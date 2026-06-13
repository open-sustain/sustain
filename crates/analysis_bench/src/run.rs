// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Driving the analyzer over a manifest and assembling a [`Report`].
//!
//! A report has three layers: a per-track record of outputs, timings, and
//! scores; an aggregate [`Summary`]; and an [`Env`] capture (toolchain,
//! profile, CPU). Two serialization modes share the type:
//!
//! * **Full** (`reproducible = false`) — everything, for a working run.
//! * **Reproducible** (`reproducible = true`) — outputs and scores only,
//!   with the machine-specific [`Env`] and all wall-clock timings omitted,
//!   so the file is byte-stable across machines and meaningful to diff.
//!   This is the form committed as a synthetic baseline.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use sustain_analysis::{AcousticFeatures, AnalysisOptions, Analyzer, WaveformSegments};

use crate::manifest::{Manifest, Options, Task, TrackEntry};
use crate::metrics::{self, DEFAULT_BPM_TOLERANCE, KeyCategory};

/// A complete benchmark report.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    /// Capture of the machine/toolchain. Omitted in reproducible mode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub env: Option<Env>,
    /// The corpus this report covers.
    pub corpus_id: String,
    /// Resolved BPM range the analyzer ran with.
    pub bpm_range: [f32; 2],
    /// Aggregate metrics across the run.
    pub summary: Summary,
    /// Per-track records, in manifest order.
    pub tracks: Vec<TrackResult>,
}

/// Toolchain/host capture for a run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Env {
    /// `sustain-analysis-bench` package version.
    pub bench_version: String,
    /// `sustain_analysis::ANALYZER_VERSION` the outputs were produced under.
    pub analyzer_version: u32,
    /// Build profile, `"debug"` or `"release"`.
    pub profile: String,
    /// Short git commit, if the working tree is a git checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_commit: Option<String>,
    /// Whether the working tree had uncommitted changes when the run started.
    /// `git_commit` names HEAD only; without this flag a run from a
    /// modified-but-uncommitted tree records HEAD's hash and is otherwise
    /// indistinguishable from a clean run — exactly how an earlier post-fix
    /// baseline came to carry a stale commit. `None` outside a git checkout.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_dirty: Option<bool>,
    /// Wall-clock seconds since the Unix epoch when the run started.
    pub timestamp_unix: u64,
    /// CPU model string from `/proc/cpuinfo`, if readable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_model: Option<String>,
    /// Logical core count.
    pub logical_cores: usize,
}

impl Env {
    /// Capture the current environment.
    fn capture() -> Self {
        Self {
            bench_version: env!("CARGO_PKG_VERSION").to_string(),
            analyzer_version: sustain_analysis::ANALYZER_VERSION,
            profile: if cfg!(debug_assertions) {
                "debug"
            } else {
                "release"
            }
            .to_string(),
            git_commit: git_short_commit(),
            git_dirty: git_tree_dirty(),
            timestamp_unix: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            cpu_model: cpu_model(),
            logical_cores: std::thread::available_parallelism().map_or(0, std::num::NonZero::get),
        }
    }
}

/// Per-track outputs, timings, and scores.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrackResult {
    /// Manifest id (never a filesystem path — keeps private corpora anonymous).
    pub id: String,
    /// Where the audio came from: `synthetic:<kind>` or `file`.
    pub source: String,
    /// Capabilities actually run for this track.
    pub tasks: Vec<Task>,
    /// Detected tempo (BPM), if the BPM capability ran and produced one.
    pub bpm: Option<f64>,
    /// Detected key as a short code (`Am`, `F#m`, …).
    pub key: Option<String>,
    /// Perceptual acoustic features.
    pub acoustics: Option<Acoustics>,
    /// Waveform tier summary (counts + content hashes).
    pub waveform: Option<WaveformSummary>,
    /// BPM scoring vs ground truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bpm_score: Option<BpmScore>,
    /// Key scoring vs ground truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key_score: Option<KeyScore>,
    /// Per-band wall-clock timing. Omitted in reproducible mode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timings: Option<BandTimings>,
    /// Set when the audio source could not be opened.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub error: Option<String>,
}

/// Mirror of [`AcousticFeatures`] owned by this crate, so the committed
/// baseline schema does not couple to the domain layer's serde form.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Acoustics {
    /// Integrated (gated) loudness, LUFS.
    pub integrated_lufs: f32,
    /// Maximum short-term loudness, LUFS.
    pub short_term_lufs_max: f32,
    /// Loudness range, LU.
    pub loudness_range_lu: f32,
    /// Onset density, events/second.
    pub onset_rate_hz: f32,
    /// Low-band energy fraction.
    pub low_band_ratio: f32,
    /// Mid-band energy fraction.
    pub mid_band_ratio: f32,
    /// High-band energy fraction.
    pub high_band_ratio: f32,
    /// Low-band temporal variation.
    pub low_band_variation: f32,
    /// Tonalness in `[0, 1]`.
    pub tonalness: f32,
}

impl From<&AcousticFeatures> for Acoustics {
    fn from(features: &AcousticFeatures) -> Self {
        Self {
            integrated_lufs: features.integrated_lufs,
            short_term_lufs_max: features.short_term_lufs_max,
            loudness_range_lu: features.loudness_range_lu,
            onset_rate_hz: features.onset_rate_hz,
            low_band_ratio: features.low_band_ratio,
            mid_band_ratio: features.mid_band_ratio,
            high_band_ratio: features.high_band_ratio,
            low_band_variation: features.low_band_variation,
            tonalness: features.tonalness,
        }
    }
}

/// Counts and content hashes for the two waveform tiers. Hashing the
/// quantized segment bytes pins waveform output without storing it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WaveformSummary {
    /// Preview-tier segment count.
    pub preview_segments: usize,
    /// Detail-tier segment count.
    pub detail_segments: usize,
    /// Milliseconds covered by each preview segment.
    pub preview_duration_ms: f32,
    /// Milliseconds covered by each detail segment.
    pub detail_duration_ms: f32,
    /// FNV-1a hash (hex) of the preview segment bytes.
    pub preview_hash: String,
    /// FNV-1a hash (hex) of the detail segment bytes.
    pub detail_hash: String,
}

impl WaveformSummary {
    fn of(preview: &WaveformSegments, detail: &WaveformSegments) -> Self {
        Self {
            preview_segments: preview.segments.len(),
            detail_segments: detail.segments.len(),
            preview_duration_ms: preview.segment_duration_ms,
            detail_duration_ms: detail.segment_duration_ms,
            preview_hash: hash_segments(preview),
            detail_hash: hash_segments(detail),
        }
    }
}

/// BPM score against ground truth.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpmScore {
    /// Ground-truth BPM from the manifest.
    pub ground_truth: f64,
    /// Absolute error, or `None` if the BPM capability produced nothing.
    pub abs_error: Option<f64>,
    /// Whether the prediction landed within ±2 BPM.
    pub within_tolerance: bool,
    /// Metrical ratio bucket (`1x`, `2x`, `1/2x`, …, `other`, `n/a`).
    pub ratio_bucket: String,
}

/// Key score against ground truth.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeyScore {
    /// Ground-truth key string from the manifest.
    pub ground_truth: String,
    /// MIREX agreement category.
    pub category: KeyCategory,
    /// MIREX weighted score.
    pub score: f32,
}

/// Per-band wall-clock timings (milliseconds).
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct BandTimings {
    /// BPM band.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bpm_ms: Option<f64>,
    /// Key band.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key_ms: Option<f64>,
    /// Acoustics band.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub acoustics_ms: Option<f64>,
    /// Waveform band.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub waveform_ms: Option<f64>,
    /// Sum across the bands that ran.
    pub total_ms: f64,
}

/// Aggregate metrics across a run.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Summary {
    /// Number of tracks in the run.
    pub track_count: usize,
    /// Number of tracks whose source failed to open.
    pub error_count: usize,
    /// BPM aggregate, present when any track carried BPM ground truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bpm: Option<BpmSummary>,
    /// Key aggregate, present when any track carried key ground truth.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key: Option<KeySummary>,
    /// Timing aggregate. Omitted in reproducible mode.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub timing: Option<TimingSummary>,
}

/// Aggregate BPM accuracy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BpmSummary {
    /// Tracks with BPM ground truth.
    pub scored: usize,
    /// Percentage within ±2 BPM.
    pub within_tolerance_pct: f32,
    /// Mean absolute error over predictions that produced a value.
    pub mean_abs_error: f64,
    /// Histogram of metrical ratio buckets.
    pub ratio_buckets: BTreeMap<String, usize>,
}

/// Aggregate key accuracy.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct KeySummary {
    /// Tracks with key ground truth.
    pub scored: usize,
    /// MIREX weighted score as a percentage. Kept as the research/regression
    /// metric, comparable to prior MIR work.
    pub weighted_pct: f32,
    /// Strict harmonic-compatible rate (percent): `correct + fifth +
    /// relative`. The **product-facing key headline** — for DJ /
    /// Rekordbox-style filtering and mixing, a prediction in this set lands in
    /// a harmonically usable neighbourhood of the true key (exact, a fifth
    /// away, or the relative major/minor, all adjacent on the Camelot wheel).
    #[serde(default)]
    pub strict_compatible_pct: f32,
    /// Loose harmonic-compatible rate (percent): strict plus `parallel` (same
    /// tonic, opposite mode). Diagnostic only until we confirm whether Pioneer
    /// treats the parallel key as compatible; reported alongside strict so the
    /// parallel contribution stays visible.
    #[serde(default)]
    pub loose_compatible_pct: f32,
    /// Histogram of MIREX categories.
    pub categories: BTreeMap<String, usize>,
}

/// Aggregate per-band timing.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TimingSummary {
    /// Sum of all band times across all tracks.
    pub total_ms: f64,
    /// BPM band stats.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub bpm: Option<BandStat>,
    /// Key band stats.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub key: Option<BandStat>,
    /// Acoustics band stats.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub acoustics: Option<BandStat>,
    /// Waveform band stats.
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub waveform: Option<BandStat>,
}

/// Timing statistics for a single band over the tracks that ran it.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct BandStat {
    /// Tracks that ran this band.
    pub runs: usize,
    /// Total time across those tracks.
    pub total_ms: f64,
    /// Mean time per track.
    pub mean_ms: f64,
    /// Slowest single track.
    pub max_ms: f64,
}

/// Run a whole manifest, generating any synthetic fixtures under
/// `fixtures_dir`. In `reproducible` mode the env capture and all timings
/// are dropped from the report.
pub fn run_manifest(manifest: &Manifest, fixtures_dir: &Path, reproducible: bool) -> Report {
    let analysis_options = analysis_options(&manifest.options);
    let tracks: Vec<TrackResult> = manifest
        .tracks
        .iter()
        .map(|entry| {
            run_track(
                entry,
                &manifest.options,
                analysis_options,
                fixtures_dir,
                reproducible,
            )
        })
        .collect();
    let summary = summarize(&tracks, reproducible);
    Report {
        env: (!reproducible).then(Env::capture),
        corpus_id: manifest.meta.corpus_id.clone(),
        bpm_range: [analysis_options.min_bpm, analysis_options.max_bpm],
        summary,
        tracks,
    }
}

/// Resolve manifest options into analyzer options, falling back to the
/// analyzer's own defaults.
fn analysis_options(options: &Options) -> AnalysisOptions {
    let defaults = AnalysisOptions::default();
    AnalysisOptions {
        min_bpm: options.min_bpm.unwrap_or(defaults.min_bpm),
        max_bpm: options.max_bpm.unwrap_or(defaults.max_bpm),
    }
}

/// Analyze a single track entry.
fn run_track(
    entry: &TrackEntry,
    options: &Options,
    analysis_options: AnalysisOptions,
    fixtures_dir: &Path,
    reproducible: bool,
) -> TrackResult {
    let tasks = entry.effective_tasks(options);
    let (path, source, duration_hint) = match resolve_source(entry, fixtures_dir) {
        Ok(resolved) => resolved,
        Err(message) => {
            return TrackResult {
                id: entry.id.clone(),
                source: "error".to_string(),
                tasks,
                bpm: None,
                key: None,
                acoustics: None,
                waveform: None,
                bpm_score: None,
                key_score: None,
                timings: None,
                error: Some(message),
            };
        }
    };

    // A real audio source that cannot even be opened is a hard failure;
    // record it rather than silently reporting empty bands.
    if entry.path.is_some() {
        if let Err(err) = std::fs::File::open(&path) {
            return TrackResult {
                id: entry.id.clone(),
                source,
                tasks,
                bpm: None,
                key: None,
                acoustics: None,
                waveform: None,
                bpm_score: None,
                key_score: None,
                timings: None,
                error: Some(format!("open failed: {err}")),
            };
        }
    }

    let analyzer = Analyzer::new(path, analysis_options, duration_hint);

    // Run the requested capabilities in priming order (larger decodes
    // first) so a full pass measures the same cache-sharing the
    // production scheduler gets.
    let mut bpm = None;
    let mut key = None;
    let mut acoustics = None;
    let mut waveform = None;
    let mut timings = BandTimings {
        bpm_ms: None,
        key_ms: None,
        acoustics_ms: None,
        waveform_ms: None,
        total_ms: 0.0,
    };
    for task in Task::ALL {
        if !tasks.contains(&task) {
            continue;
        }
        let started = Instant::now();
        match task {
            Task::Waveform => {
                waveform = analyzer
                    .waveform()
                    .map(|tiers| WaveformSummary::of(&tiers.preview, &tiers.detail));
            }
            Task::Acoustics => {
                acoustics = analyzer
                    .acoustics()
                    .map(|features| Acoustics::from(&features));
            }
            Task::Bpm => {
                bpm = analyzer.bpm().map(f64::from);
            }
            Task::Key => {
                key = analyzer.key().map(|k| k.short_code().to_string());
            }
        }
        let elapsed = millis(started.elapsed());
        timings.total_ms += elapsed;
        match task {
            Task::Waveform => timings.waveform_ms = Some(elapsed),
            Task::Acoustics => timings.acoustics_ms = Some(elapsed),
            Task::Bpm => timings.bpm_ms = Some(elapsed),
            Task::Key => timings.key_ms = Some(elapsed),
        }
    }

    let bpm_score = entry.bpm.map(|gt| score_bpm(gt, bpm));
    let key_score = entry
        .key
        .as_deref()
        .and_then(|gt| score_key(gt, key.as_deref()));

    TrackResult {
        id: entry.id.clone(),
        source,
        tasks,
        bpm,
        key,
        acoustics,
        waveform,
        bpm_score,
        key_score,
        timings: (!reproducible).then_some(timings),
        error: None,
    }
}

/// Resolve a track entry to a concrete audio file, generating a synthetic
/// fixture when needed. Returns the path, a source label, and the
/// duration hint to feed the analyzer.
fn resolve_source(
    entry: &TrackEntry,
    fixtures_dir: &Path,
) -> Result<(PathBuf, String, Option<Duration>), String> {
    if let Some(synthetic) = &entry.synthetic {
        let path = fixtures_dir.join(format!("{}.wav", sanitize(&entry.id)));
        synthetic
            .write_wav(&path)
            .map_err(|err| format!("failed to write fixture: {err}"))?;
        let label = synthetic_label(synthetic);
        let hint = duration_hint(entry.duration_secs.or(Some(synthetic.secs())));
        return Ok((path, label, hint));
    }
    if let Some(path) = &entry.path {
        return Ok((
            path.clone(),
            "file".to_string(),
            duration_hint(entry.duration_secs),
        ));
    }
    Err("track has neither `path` nor `synthetic`".to_string())
}

/// Build a non-negative, finite duration hint.
fn duration_hint(secs: Option<f64>) -> Option<Duration> {
    secs.filter(|s| s.is_finite() && *s >= 0.0)
        .map(Duration::from_secs_f64)
}

/// `synthetic:<kind>` label for a fixture.
fn synthetic_label(synthetic: &crate::fixtures::Synthetic) -> String {
    use crate::fixtures::Synthetic;
    let kind = match synthetic {
        Synthetic::Silence { .. } => "silence",
        Synthetic::Tone { .. } => "tone",
        Synthetic::Ramp { .. } => "ramp",
        Synthetic::ClickTrain { .. } => "click_train",
        Synthetic::Triad { .. } => "triad",
    };
    format!("synthetic:{kind}")
}

/// Score BPM against ground truth.
fn score_bpm(ground_truth: f64, predicted: Option<f64>) -> BpmScore {
    match predicted {
        Some(pred) => BpmScore {
            ground_truth,
            abs_error: metrics::bpm_absolute_error(pred, ground_truth),
            within_tolerance: metrics::bpm_within(pred, ground_truth, DEFAULT_BPM_TOLERANCE),
            ratio_bucket: metrics::tempo_ratio_bucket(pred, ground_truth).to_string(),
        },
        None => BpmScore {
            ground_truth,
            abs_error: None,
            within_tolerance: false,
            ratio_bucket: "n/a".to_string(),
        },
    }
}

/// Score key against ground truth. Returns `None` when the ground-truth
/// string is not a recognizable key (so a typo doesn't pollute the
/// summary).
fn score_key(ground_truth: &str, predicted: Option<&str>) -> Option<KeyScore> {
    let gt = metrics::parse_key(ground_truth)?;
    let category = match predicted.and_then(metrics::parse_key) {
        Some(pred) => metrics::evaluate_key(pred, gt),
        None => KeyCategory::Other,
    };
    Some(KeyScore {
        ground_truth: ground_truth.to_string(),
        category,
        score: category.score(),
    })
}

/// Build the aggregate summary.
fn summarize(tracks: &[TrackResult], reproducible: bool) -> Summary {
    let error_count = tracks.iter().filter(|t| t.error.is_some()).count();
    Summary {
        track_count: tracks.len(),
        error_count,
        bpm: summarize_bpm(tracks),
        key: summarize_key(tracks),
        timing: (!reproducible).then(|| summarize_timing(tracks)),
    }
}

fn summarize_bpm(tracks: &[TrackResult]) -> Option<BpmSummary> {
    let scores: Vec<&BpmScore> = tracks.iter().filter_map(|t| t.bpm_score.as_ref()).collect();
    if scores.is_empty() {
        return None;
    }
    let within = scores.iter().filter(|s| s.within_tolerance).count();
    let errors: Vec<f64> = scores.iter().filter_map(|s| s.abs_error).collect();
    let mean_abs_error = if errors.is_empty() {
        0.0
    } else {
        errors.iter().sum::<f64>() / errors.len() as f64
    };
    let mut ratio_buckets: BTreeMap<String, usize> = BTreeMap::new();
    for score in &scores {
        *ratio_buckets.entry(score.ratio_bucket.clone()).or_default() += 1;
    }
    Some(BpmSummary {
        scored: scores.len(),
        within_tolerance_pct: pct(within, scores.len()),
        mean_abs_error,
        ratio_buckets,
    })
}

fn summarize_key(tracks: &[TrackResult]) -> Option<KeySummary> {
    let scores: Vec<&KeyScore> = tracks.iter().filter_map(|t| t.key_score.as_ref()).collect();
    if scores.is_empty() {
        return None;
    }
    let score_sum: f32 = scores.iter().map(|s| s.score).sum();
    let mut categories: BTreeMap<String, usize> = BTreeMap::new();
    for score in &scores {
        *categories
            .entry(category_label(score.category))
            .or_default() += 1;
    }
    let (strict_compatible_pct, loose_compatible_pct) = compatible_rates(&categories, scores.len());
    Some(KeySummary {
        scored: scores.len(),
        weighted_pct: score_sum / scores.len() as f32 * 100.0,
        strict_compatible_pct,
        loose_compatible_pct,
        categories,
    })
}

/// Strict and loose harmonic-compatible key rates (percent) from a MIREX
/// category histogram. Strict = `correct + fifth + relative`; loose adds
/// `parallel`. Derived from the histogram rather than stored separately, so it
/// is computable from any report — including ones recorded before these rates
/// were summarized — which is what lets [`crate::run`] consumers compare an
/// older baseline against a newer candidate.
pub fn compatible_rates(categories: &BTreeMap<String, usize>, scored: usize) -> (f32, f32) {
    if scored == 0 {
        return (0.0, 0.0);
    }
    let count = |name: &str| categories.get(name).copied().unwrap_or(0);
    let strict = count("correct") + count("fifth") + count("relative");
    let loose = strict + count("parallel");
    let denom = scored as f32;
    (strict as f32 / denom * 100.0, loose as f32 / denom * 100.0)
}

fn summarize_timing(tracks: &[TrackResult]) -> TimingSummary {
    let timings: Vec<&BandTimings> = tracks.iter().filter_map(|t| t.timings.as_ref()).collect();
    TimingSummary {
        total_ms: timings.iter().map(|t| t.total_ms).sum(),
        bpm: band_stat(timings.iter().filter_map(|t| t.bpm_ms)),
        key: band_stat(timings.iter().filter_map(|t| t.key_ms)),
        acoustics: band_stat(timings.iter().filter_map(|t| t.acoustics_ms)),
        waveform: band_stat(timings.iter().filter_map(|t| t.waveform_ms)),
    }
}

/// Stats over one band's per-track times, or `None` if the band never ran.
fn band_stat(values: impl Iterator<Item = f64>) -> Option<BandStat> {
    let values: Vec<f64> = values.collect();
    if values.is_empty() {
        return None;
    }
    let total_ms: f64 = values.iter().sum();
    let max_ms = values.iter().copied().fold(0.0_f64, f64::max);
    Some(BandStat {
        runs: values.len(),
        total_ms,
        mean_ms: total_ms / values.len() as f64,
        max_ms,
    })
}

fn category_label(category: KeyCategory) -> String {
    match category {
        KeyCategory::Correct => "correct",
        KeyCategory::Fifth => "fifth",
        KeyCategory::Relative => "relative",
        KeyCategory::Parallel => "parallel",
        KeyCategory::Other => "other",
    }
    .to_string()
}

fn pct(part: usize, whole: usize) -> f32 {
    if whole == 0 {
        0.0
    } else {
        part as f32 / whole as f32 * 100.0
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// FNV-1a hash of the quantized segment bytes, as zero-padded hex.
fn hash_segments(segments: &WaveformSegments) -> String {
    let mut hash = 0xcbf2_9ce4_8422_2325_u64;
    for segment in &segments.segments {
        for byte in [
            segment.amplitude,
            segment.low_band,
            segment.mid_band,
            segment.high_band,
        ] {
            hash ^= u64::from(byte);
            hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    format!("{hash:016x}")
}

/// Replace anything outside `[A-Za-z0-9._-]` with `_` for a safe filename.
fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// First `model name` line from `/proc/cpuinfo`, if readable.
fn cpu_model() -> Option<String> {
    let contents = std::fs::read_to_string("/proc/cpuinfo").ok()?;
    contents
        .lines()
        .find_map(|line| {
            line.split_once(':')
                .filter(|(key, _)| key.trim() == "model name")
        })
        .map(|(_, value)| value.trim().to_string())
}

/// `git rev-parse --short HEAD`, if this is a git checkout.
fn git_short_commit() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let commit = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!commit.is_empty()).then_some(commit)
}

/// Whether the working tree has uncommitted changes (`git status --porcelain`
/// reports a non-empty diff), or `None` if this is not a git checkout. Recorded
/// next to [`git_short_commit`] so a baseline taken from a dirty tree is
/// self-evident: the commit hash alone cannot reveal uncommitted edits, so a
/// recorded baseline must be run from a clean tree to be trustworthy.
fn git_tree_dirty() -> Option<bool> {
    let output = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(!output.stdout.is_empty())
}

#[cfg(test)]
mod tests {
    use super::{compatible_rates, score_bpm, score_key};
    use crate::metrics::KeyCategory;
    use std::collections::BTreeMap;

    #[test]
    fn compatible_rates_partition_categories() {
        let categories: BTreeMap<String, usize> = [
            ("correct", 5),
            ("fifth", 3),
            ("relative", 2),
            ("parallel", 4),
            ("other", 6),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        // 20 scored: strict = 5+3+2 = 10 (50%), loose = +4 = 14 (70%).
        let (strict, loose) = compatible_rates(&categories, 20);
        assert_eq!(strict, 50.0);
        assert_eq!(loose, 70.0);
        // Empty / zero-scored is finite zero, not a divide-by-zero.
        assert_eq!(compatible_rates(&BTreeMap::new(), 0), (0.0, 0.0));
    }

    #[test]
    fn bpm_score_marks_a_close_hit() {
        let score = score_bpm(120.0, Some(121.0));
        assert!(score.within_tolerance);
        assert_eq!(score.ratio_bucket, "1x");
        assert_eq!(score.abs_error, Some(1.0));
    }

    #[test]
    fn bpm_score_records_a_missing_prediction() {
        let score = score_bpm(120.0, None);
        assert!(!score.within_tolerance);
        assert_eq!(score.ratio_bucket, "n/a");
        assert_eq!(score.abs_error, None);
    }

    #[test]
    fn key_score_uses_mirex_category() {
        let score = score_key("C", Some("Am")).expect("scored");
        assert_eq!(score.category, KeyCategory::Relative);
        let bad_gt = score_key("not-a-key", Some("Am"));
        assert!(bad_gt.is_none());
    }
}
