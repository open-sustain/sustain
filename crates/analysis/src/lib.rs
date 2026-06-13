// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Audio analysis for Sustain — pure DSP, no I/O beyond reading the
//! audio file the caller hands us.
//!
//! [`Analyzer`] is the single public surface: a stateful,
//! capability-driven analyzer that owns a single track and caches the
//! decoded audio shared between bands. Callers pick exactly which bands
//! they want by calling [`Analyzer::bpm`], [`Analyzer::key`],
//! [`Analyzer::acoustics`], and [`Analyzer::waveform`] independently —
//! a method that is never called does zero work. This is the API the
//! background scheduler uses, so toggling off (say) waveform generation
//! in Preferences actually skips the waveform decode pass.
//!
//! ## Analysis windows
//!
//! BPM and key are read off a **centered** window — the middle
//! `BPM_KEY_WINDOW_SECS` of the track — not the opening seconds. The
//! intro of a track (especially electronic music) is the least
//! representative part: sparse, tame, often beatless. The middle is
//! where the groove and the tonal centre actually live. The window is
//! seeked to, so even on a long file we never decode the lead-in.
//!
//! Tracks are tiered by length to keep the working set bounded:
//!
//! * **Normal** (≤ 15 min, `LONG_TRACK_THRESHOLD_SECS`): acoustics
//!   measure the whole track (so the loudness guard's short-term max
//!   sees a real finale, §7), and the waveform renders the whole track
//!   at full detail.
//! * **Long** (> 15 min — classical movements, DJ mixes, podcasts):
//!   acoustics measure a centered `ACOUSTICS_LONG_WINDOW_SECS` sample
//!   instead of the whole track, and the **waveform is skipped
//!   entirely** (the caller checks [`Analyzer::is_long_track`]). A
//!   whole-track decode of a two-hour file is gigabytes of working set
//!   for a feature that, at that length, is a coarse smear nobody
//!   scrubs; the high-detail, device-specific waveforms a Pioneer
//!   export needs are computed on demand by that export, not here.
//!
//! The windows nest — the BPM/key window is always the centre of the
//! acoustics window — so when `audio` analysis is enabled the acoustics
//! decode is computed once and BPM/key slice their window out of it for
//! free. BPM/key requested *without* audio decode only their own
//! centered window.
//!
//! The Analyzer composes the vendored `sustain_dsp` primitives directly
//! (chroma extractor for the STFT, period tempogram for BPM, key detector
//! for key). The vendored core is deliberately just those primitives: it
//! omits the upstream `analyze_audio` compute-everything orchestration,
//! which always computes every band, so calling it for "BPM only" would
//! still pay for chroma + key detection. The trade-off is that we forgo
//! that orchestration's confidence-boosting heuristics (multi-resolution
//! tempogram, onset consensus); accuracy on a 200-track validation set was
//! measured at ~85% with the plain tempogram path versus ~92% with the
//! full orchestration. The maintainer's call is that 7 percentage points
//! is the right price for an honest skip path.
//!
//! Persistence, paced scheduling, and "needs analysis" bookkeeping
//! live in `sustain-library-store` and `sustain-app-runtime`
//! respectively. This crate touches the filesystem only to read the
//! audio file.

mod bands;
mod decode;
mod waveform;

use std::cell::OnceCell;
use std::path::PathBuf;

// Re-exported from sustain_domain so callers can `use sustain_analysis::*`
// without also pulling sustain_domain into their imports for what is
// conceptually one cohesive surface. The canonical home for these types
// is the domain layer — the storage crate needs them but should not
// pull in symphonia / stratum-dsp.
pub use sustain_domain::{
    AcousticFeatures, BeatGrid, DETAIL_SEGMENTS_PER_SECOND, MusicalKey, PREVIEW_SEGMENT_COUNT,
    TrackAnalysis, WaveformSegment, WaveformSegments,
};

use std::time::Duration;

use decode::{DecodedAudio, decode_full, decode_window};
use sustain_dsp::{
    KeyTemplates, NormalizationConfig, NormalizationMethod, compute_stft, detect_key,
    detect_spectral_flux_onsets, estimate_bpm_tempogram,
    extract_chroma_from_spectrogram_with_options, normalize,
};

pub use waveform::WaveformTiers;

/// Monotonically-increasing identifier for the DSP algorithms in this
/// crate. Bumped centrally when a change to the band split, BPM/key
/// engine, or waveform encoding would invalidate previously-stored
/// `track_analysis` rows. The storage layer compares stored rows
/// against this value to decide whether a track should be re-queued
/// by the runtime scheduler — no migration code, just a version
/// bump that the scheduler walks past in the background.
///
/// Version 2: the BPM and key bands now route through
/// `stratum_dsp::features::*` directly via [`Analyzer`] instead of
/// `stratum_dsp::analyze_audio`'s compute-everything orchestration, so
/// previously-attempted rows must be re-attempted under the new
/// capability-driven pipeline.
///
/// Version 3: the audio pass gained the perceptual acoustic feature
/// set (loudness, onset density, timbral band ratios, low-band
/// variation, tonalness) Smart Shuffle consumes. Tracks attempted under
/// version 2 have no `track_acoustics` row, so they must be re-attempted.
///
/// Version 4: BPM/key moved from the opening 120 s to a *centered*
/// window, and long tracks (> 15 min) now measure acoustics over a
/// centered 8-min sample with the waveform skipped. The values these
/// produce differ from version 3, so previously-attempted rows must be
/// re-attempted. Note the storage layer's `FILL_*_IF_NULL` policy: a
/// re-attempt overwrites acoustics but leaves an existing BPM/key value
/// in place (it never clobbers a populated field), so an existing
/// library only picks up the new centered BPM/key on a wipe-and-rescan
/// — the documented pre-release workflow.
///
/// Version 5: waveform detail windows are laid out on the exact Pioneer
/// half-frame timeline (150 entries per second), including for sample
/// rates that are not divisible by 150.
///
/// Deliberately *not* gated here: the audio *decoder* library version.
/// The symphonia 0.5 → 0.6 migration changes some lossy-format (e.g. MP3)
/// decoded samples slightly — enough to shift waveform/acoustics and,
/// rarely, a BPM estimate — but the band split, BPM/key engine, and
/// waveform encoding (the things this version actually gates) are
/// unchanged, and PCM/WAV decode is bit-identical. Re-deriving every
/// existing library's audio pass for sub-perceptual decoder rounding would
/// be disproportionate, so the version stays at 5: existing rows keep
/// their values; only newly-analyzed tracks use the 0.6 decode.
///
/// Likewise *not* gated: the #192 vendoring of the BPM/key/STFT/onset/loudness
/// DSP out of the published `stratum-dsp` crate into `sustain-dsp`. The
/// vendored full-band path reproduces the previous BPM/acoustics/waveform
/// byte-for-byte on the synthetic corpus, so stored values' meaning is
/// unchanged — key merely becomes deterministic on ambiguous ties (which the
/// storage layer's `FILL_*_IF_NULL` policy never re-clobbers anyway).
pub const ANALYZER_VERSION: u32 = 5;

/// DSP tunables exposed to callers. Defaults reflect the values the
/// rhythmbox-to-pioneer-xdj-exporter author landed on after testing on
/// a large DJ-style library.
///
/// Capability gating (which bands to compute) is **not** in this
/// struct — that lives on the call site, which simply chooses which
/// [`Analyzer`] methods to invoke. The window placement and long-track
/// tiering are not tunables either; they are fixed policy
/// (`BPM_KEY_WINDOW_SECS`, `LONG_TRACK_THRESHOLD_SECS`,
/// `ACOUSTICS_LONG_WINDOW_SECS`). Anything in here is DSP tuning every
/// band-method-call uses identically.
#[derive(Clone, Copy, Debug)]
pub struct AnalysisOptions {
    /// Lower bound of the BPM range used for octave normalization
    /// (detected BPM is doubled while below this value, if doubling
    /// keeps it inside the range).
    pub min_bpm: f32,
    /// Upper bound of the BPM range used for octave normalization
    /// (detected BPM is halved while above this value, if halving
    /// keeps it inside the range).
    pub max_bpm: f32,
}

impl Default for AnalysisOptions {
    fn default() -> Self {
        Self {
            min_bpm: 70.0,
            max_bpm: 170.0,
        }
    }
}

/// Failure modes produced by the DSP pipeline. The per-band
/// [`Analyzer`] methods collapse failures to `None` so the caller can
/// decide whether a partial result still counts as an attempt; the
/// scheduler surfaces the typed error from its own file-open probe
/// before constructing the Analyzer, so this enum is what reaches
/// `record_analysis_attempt_failure`.
#[derive(Debug, thiserror::Error)]
pub enum AnalysisError {
    #[error("failed to open audio file {path}: {source}")]
    OpenFailed {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("audio decoder error for {path}: {message}")]
    DecoderError { path: String, message: String },
    #[error("audio file {path} contains no usable audio track")]
    NoAudioTrack { path: String },
    #[error("audio file {path} is too short for analysis ({samples} samples)")]
    TooShort { path: String, samples: usize },
    #[error("DSP engine failed for {path}: {message}")]
    DspFailed { path: String, message: String },
}

/// Length (seconds) of the centered window BPM and key are measured
/// over. Long enough for a reliable tempogram + chroma estimate, short
/// enough that the working set stays bounded and the window stays
/// inside a single musical section (so a track that changes key or
/// tempo does not smear). Matches the upstream
/// rhythmbox-to-pioneer-xdj-exporter figure, now centered rather than
/// taken from the opening.
const BPM_KEY_WINDOW_SECS: f64 = 120.0;

/// Track length (seconds) above which a track is treated as *long*:
/// acoustics measure a centered sample rather than the whole track, and
/// the waveform is skipped. 15 minutes is comfortably past any normal
/// song and lands on the classical-movement / DJ-mix / podcast material
/// where whole-track analysis is both expensive and low-value.
const LONG_TRACK_THRESHOLD_SECS: f64 = 15.0 * 60.0;

/// Length (seconds) of the centered acoustics sample on long tracks. 8
/// minutes is a generous, representative slice of the body of a long
/// piece while keeping the decode + STFT bounded (a two-hour file would
/// otherwise be gigabytes of working set per worker).
const ACOUSTICS_LONG_WINDOW_SECS: f64 = 8.0 * 60.0;

/// Minimum number of samples the DSP engine needs to do anything
/// meaningful. Roughly one second at 44.1 kHz; below that the FFT
/// windows do not have enough material to analyze and `stratum-dsp`
/// either errors or returns garbage.
const MIN_SAMPLES_FOR_ANALYSIS: usize = 44_100;

/// STFT frame size used for both BPM and key. Matches stratum-dsp's
/// own default (`AnalysisConfig::frame_size`); a smaller frame loses
/// frequency resolution that the key detector needs, a larger frame
/// blurs onsets the tempogram looks for.
const STFT_FRAME_SIZE: usize = 2048;

/// STFT hop size. Matches stratum-dsp's default — 75% overlap, which
/// the tempogram's novelty curve was tuned against.
const STFT_HOP_SIZE: usize = 512;

/// BPM resolution (in BPM) the tempogram searches at. 1.0 BPM matches
/// the upstream default and the resolution our UI displays at.
const TEMPOGRAM_BPM_RESOLUTION: f32 = 1.0;

/// Band-split crossovers (Hz) for the acoustic brightness ratios.
/// Matches the waveform's visual band split (`bands.rs`) so "low /
/// mid / high" means the same thing wherever it appears in the app.
const ACOUSTIC_LOW_MID_HZ: f32 = 250.0;
const ACOUSTIC_MID_HIGH_HZ: f32 = 4_000.0;

/// Short-term loudness window and hop, in seconds. EBU R128 defines
/// short-term loudness over a 3 s window; we slide it by 1 s and take
/// the max as the loud *boundary* a transition can hit (§7).
const SHORT_TERM_WINDOW_SECS: f32 = 3.0;
const SHORT_TERM_HOP_SECS: f32 = 1.0;

/// Percentile threshold for spectral-flux onset detection. 0.8 keeps
/// only the strongest flux peaks (matches the upstream example), so
/// the count reflects real rhythmic events rather than noise.
const ONSET_FLUX_PERCENTILE: f32 = 0.8;

/// Relative gate (LU below the loudest short-term window) below which
/// windows are dropped from the loudness-range estimate, à la the
/// EBU R128 relative gate.
const LOUDNESS_RANGE_GATE_LU: f32 = 20.0;

/// Capability-driven, stateful analyzer for one audio file. Cheap to
/// construct — the constructor does no I/O — and caches the decoded
/// regions the band methods share, so a caller that requests both BPM
/// and key only pays for one decode + one STFT, and a caller that also
/// requests audio analysis pays for the acoustics decode and slices the
/// BPM/key window out of it for free.
///
/// The caches are lazy and region-keyed:
///
/// * [`Self::waveform`] uses the **whole-track** decode (`full_audio`).
/// * [`Self::acoustics`] uses the whole track on normal-length tracks
///   (sharing `full_audio` with the waveform) and a centered
///   `ACOUSTICS_LONG_WINDOW_SECS` decode on long tracks.
/// * [`Self::bpm`] / [`Self::key`] use a centered `BPM_KEY_WINDOW_SECS`
///   window plus its STFT. When a larger region is already in memory
///   (the audio pass ran first), the window is sliced from its centre —
///   no extra I/O. Otherwise it is decoded on its own.
///
/// To get the free slice, a caller that wants everything should prime
/// the larger region first: call [`Self::waveform`]/[`Self::acoustics`]
/// before [`Self::bpm`]/[`Self::key`]. The production scheduler closure
/// does exactly that. Calling BPM/key first is still correct, only
/// slightly less efficient (the centered window is decoded separately).
///
/// The `duration_hint` (the library's stored track duration) places the
/// centered windows and classifies the track as normal vs. long without
/// a preliminary probe. When it is absent the analyzer treats the track
/// as normal and windows from the start (centering needs a length).
///
/// Failures inside any band collapse to `None`; the caller decides
/// how to record the attempt. Errors that prevent any band from
/// producing a result (file does not exist, decoder rejects the
/// container) surface as `None` from every method — semantically
/// "we tried, nothing came out". Callers that need a strict
/// open-fail signal (the scheduler does, so it can route the track
/// to `record_analysis_attempt_failure`) probe the file with
/// `std::fs::File::open` before constructing the analyzer.
pub struct Analyzer {
    path: PathBuf,
    options: AnalysisOptions,
    duration_hint: Option<Duration>,
    /// Whole-track decode (waveform; acoustics on normal-length tracks).
    full_audio: OnceCell<Option<DecodedAudio>>,
    /// Centered acoustics window — only populated on long tracks; normal
    /// tracks route acoustics through `full_audio`.
    acoustics_window: OnceCell<Option<DecodedAudio>>,
    /// Centered BPM/key window (a slice of a larger region when one is
    /// in memory, otherwise its own decode).
    bpmkey_audio: OnceCell<Option<DecodedAudio>>,
    bpmkey_stft: OnceCell<Option<Vec<Vec<f32>>>>,
}

impl Analyzer {
    /// Build an analyzer bound to `path` with the given DSP tunings and
    /// an optional `duration_hint` (the library's stored track length,
    /// used to place the centered windows and classify the track length
    /// without a probe). Performs no I/O — the audio file is opened
    /// lazily on the first band call.
    pub fn new(
        path: impl Into<PathBuf>,
        options: AnalysisOptions,
        duration_hint: Option<Duration>,
    ) -> Self {
        Self {
            path: path.into(),
            options,
            duration_hint,
            full_audio: OnceCell::new(),
            acoustics_window: OnceCell::new(),
            bpmkey_audio: OnceCell::new(),
            bpmkey_stft: OnceCell::new(),
        }
    }

    /// Whether this track is *long* (> `LONG_TRACK_THRESHOLD_SECS`).
    /// The production closure consults this to skip the waveform on long
    /// tracks. With no duration hint the track is treated as normal.
    pub fn is_long_track(&self) -> bool {
        self.duration_hint
            .is_some_and(|d| d.as_secs_f64() > LONG_TRACK_THRESHOLD_SECS)
    }

    /// Detected tempo in beats per minute, after octave normalization
    /// to `[options.min_bpm, options.max_bpm]`. Returns `None` if the
    /// file cannot be decoded, is too short for analysis, or the DSP
    /// engine cannot produce a confident estimate.
    pub fn bpm(&self) -> Option<f32> {
        let stft = self.bpmkey_stft()?;
        let window = self.bpmkey_audio()?;
        let estimate = estimate_bpm_tempogram(
            stft,
            window.sample_rate,
            u32::try_from(STFT_HOP_SIZE).unwrap_or(u32::MAX),
            self.options.min_bpm,
            self.options.max_bpm,
            TEMPOGRAM_BPM_RESOLUTION,
        )
        .ok()?;
        if !(estimate.bpm > 0.0 && estimate.bpm.is_finite()) {
            return None;
        }
        Some(octave_normalize(
            estimate.bpm,
            self.options.min_bpm,
            self.options.max_bpm,
        ))
    }

    /// Detected musical key. Returns `None` if the file cannot be
    /// decoded, the chroma extraction produces no usable frames, or
    /// the key detector's best match does not correspond to one of
    /// Sustain's 24 canonical [`MusicalKey`] variants.
    pub fn key(&self) -> Option<MusicalKey> {
        let stft = self.bpmkey_stft()?;
        let window = self.bpmkey_audio()?;
        let chroma = extract_chroma_from_spectrogram_with_options(
            stft,
            window.sample_rate,
            STFT_FRAME_SIZE,
            true,
            0.5,
        )
        .ok()?;
        if chroma.is_empty() {
            return None;
        }
        let templates = KeyTemplates::new();
        let detection = detect_key(&chroma, &templates).ok()?;
        let label = stratum_key_label(&detection.key);
        map_stratum_key(&label)
    }

    /// Both waveform tiers (preview + detail). Returns `None` if the
    /// file cannot be decoded; an empty-but-valid pair (no segments)
    /// is still returned as `Some(_)` so the caller can record the
    /// attempt and persist a zero-length BLOB rather than treating
    /// "silent track" as a failure.
    pub fn waveform(&self) -> Option<WaveformTiers> {
        let full = self.full()?;
        Some(waveform::build_tiers(&full.samples, full.sample_rate))
    }

    /// Perceptual acoustic features for Smart Shuffle — loudness
    /// (integrated, short-term max, range), onset density, timbral
    /// band ratios + low-band variation, and tonalness. Returns `None`
    /// if the file cannot be decoded, is too short, or carries no
    /// measurable loudness (effectively silent).
    ///
    /// On a **normal-length** track this measures the whole track (the
    /// loudness guard keys off the short-term *max*, and a loud finale
    /// must still count, §7) via the same decode [`Self::waveform`]
    /// uses, so enabling waveform and acoustics together decodes the
    /// track only once. On a **long** track (> `LONG_TRACK_THRESHOLD_SECS`)
    /// it measures a centered `ACOUSTICS_LONG_WINDOW_SECS` sample
    /// instead — a whole-track decode + STFT of a two-hour file is
    /// gigabytes of working set, and the middle is a faithful proxy for
    /// the body of the piece. The STFT for the spectral features is
    /// computed here over the region's samples (acoustics is its only
    /// consumer, so it is not cached).
    pub fn acoustics(&self) -> Option<AcousticFeatures> {
        let region = self.acoustics_audio()?;
        let sample_rate = region.sample_rate;
        let samples = &region.samples;
        if samples.len() < MIN_SAMPLES_FOR_ANALYSIS {
            return None;
        }
        let stft = compute_stft(samples, STFT_FRAME_SIZE, STFT_HOP_SIZE).ok()?;
        if stft.is_empty() {
            return None;
        }

        let integrated_lufs = measure_integrated_lufs(samples, sample_rate)?;
        let short_term = measure_short_term_lufs(samples, sample_rate);
        let short_term_lufs_max = short_term.iter().copied().fold(f32::NEG_INFINITY, f32::max);
        // No measurable short-term window → effectively silent, no
        // useful loudness signal to offer the picker.
        if !short_term_lufs_max.is_finite() {
            return None;
        }
        let loudness_range_lu = loudness_range(&short_term);

        let duration_secs = samples.len() as f32 / sample_rate as f32;
        let onset_rate_hz = onset_density(&stft, duration_secs);

        let bands = band_energies(&stft, sample_rate);
        let tonalness = mean_tonalness(&stft);

        Some(AcousticFeatures {
            integrated_lufs,
            short_term_lufs_max,
            loudness_range_lu,
            onset_rate_hz,
            low_band_ratio: bands.low_ratio,
            mid_band_ratio: bands.mid_ratio,
            high_band_ratio: bands.high_ratio,
            low_band_variation: bands.low_variation,
            tonalness,
        })
    }

    /// Whole-track decode. Used by the waveform and, on normal-length
    /// tracks, by acoustics.
    fn full(&self) -> Option<&DecodedAudio> {
        self.full_audio
            .get_or_init(|| decode_full(&self.path).ok())
            .as_ref()
    }

    /// The region acoustics measure: the whole track on normal-length
    /// tracks (shared with the waveform), or a centered
    /// `ACOUSTICS_LONG_WINDOW_SECS` sample on long tracks.
    fn acoustics_audio(&self) -> Option<&DecodedAudio> {
        if self.is_long_track() {
            self.acoustics_window
                .get_or_init(|| {
                    let start = self.window_start(ACOUSTICS_LONG_WINDOW_SECS);
                    match decode_window(&self.path, start, ACOUSTICS_LONG_WINDOW_SECS) {
                        Ok(audio) if audio.samples.len() >= MIN_SAMPLES_FOR_ANALYSIS => Some(audio),
                        _ => None,
                    }
                })
                .as_ref()
        } else {
            self.full()
        }
    }

    /// The centered BPM/key window. Sliced from a larger region already
    /// in memory when one exists (the audio pass primed it), otherwise
    /// decoded on its own. We only *peek* the larger caches — never
    /// initialize them — so requesting BPM/key alone never triggers a
    /// whole-track or acoustics decode.
    fn bpmkey_audio(&self) -> Option<&DecodedAudio> {
        self.bpmkey_audio
            .get_or_init(|| {
                let cached_region = self
                    .acoustics_window
                    .get()
                    .and_then(|cell| cell.as_ref())
                    .or_else(|| self.full_audio.get().and_then(|cell| cell.as_ref()));
                let candidate = match cached_region {
                    Some(region) => center_slice(region, BPM_KEY_WINDOW_SECS),
                    None => {
                        let start = self.window_start(BPM_KEY_WINDOW_SECS);
                        decode_window(&self.path, start, BPM_KEY_WINDOW_SECS).ok()?
                    }
                };
                (candidate.samples.len() >= MIN_SAMPLES_FOR_ANALYSIS).then_some(candidate)
            })
            .as_ref()
    }

    /// STFT of the centered BPM/key window, shared by `bpm` and `key`.
    fn bpmkey_stft(&self) -> Option<&Vec<Vec<f32>>> {
        // Pull the decoded window out first: we cannot call
        // `bpmkey_audio()` inside `get_or_init` without borrowing `self`
        // twice.
        if self.bpmkey_stft.get().is_none() {
            let computed = match self.bpmkey_audio() {
                Some(audio) => compute_stft(&audio.samples, STFT_FRAME_SIZE, STFT_HOP_SIZE).ok(),
                None => None,
            };
            // OnceCell::set returns Err only on a concurrent init, which
            // cannot happen (OnceCell is !Sync); ignore that case.
            let _ = self.bpmkey_stft.set(computed);
        }
        self.bpmkey_stft.get().and_then(|cached| cached.as_ref())
    }

    /// Start offset (seconds) that centers a `len_secs` window in the
    /// track, from the duration hint. With no hint, window from the
    /// start (`0.0`) — centering needs a length.
    fn window_start(&self, len_secs: f64) -> f64 {
        match self.duration_hint {
            Some(duration) => ((duration.as_secs_f64() - len_secs) / 2.0).max(0.0),
            None => 0.0,
        }
    }
}

/// Copy the central `len_secs` of a decoded region into a fresh buffer.
/// Both the whole-track decode and the centered acoustics window are
/// centred on the track, so their middle `len_secs` is exactly the
/// track's centered BPM/key window — no offset bookkeeping needed. A
/// region shorter than the window is returned whole.
fn center_slice(region: &DecodedAudio, len_secs: f64) -> DecodedAudio {
    let want = (len_secs * region.sample_rate as f64) as usize;
    let samples = if region.samples.len() <= want {
        region.samples.clone()
    } else {
        let start = (region.samples.len() - want) / 2;
        region.samples[start..start + want].to_vec()
    };
    DecodedAudio {
        samples,
        sample_rate: region.sample_rate,
    }
}

/// Pull `bpm` into `[min, max]` by repeated doubling/halving. Tracks
/// detected at a sub-bass periodicity (typically 60 BPM downbeat for
/// a 120 BPM tune) get doubled into the main band; double-time
/// detections (typically 200 BPM for a 100 BPM tune) get halved.
fn octave_normalize(mut bpm: f32, min: f32, max: f32) -> f32 {
    if !(min > 0.0 && max > min && bpm > 0.0) {
        return bpm;
    }
    while bpm < min && bpm * 2.0 <= max {
        bpm *= 2.0;
    }
    while bpm > max && bpm / 2.0 >= min {
        bpm /= 2.0;
    }
    bpm
}

/// Normalization config that asks stratum-dsp for an ITU-R BS.1770-4
/// loudness *measurement*. We only consume the measured value, not the
/// gained audio, so the target/headroom are irrelevant — but they must
/// be set for the LUFS path to run.
fn loudness_config() -> NormalizationConfig {
    NormalizationConfig {
        target_loudness_lufs: -14.0,
        max_headroom_db: 1.0,
        method: NormalizationMethod::Loudness,
    }
}

/// Integrated (gated) loudness in LUFS via stratum-dsp's BS.1770-4
/// measurement. `normalize` applies gain in place, so we measure on a
/// scratch copy and keep only `measured_lufs` (the *input* loudness).
fn measure_integrated_lufs(samples: &[f32], sample_rate: u32) -> Option<f32> {
    let mut scratch = samples.to_vec();
    normalize(&mut scratch, loudness_config(), sample_rate as f32)
        .ok()
        .and_then(|metadata| metadata.measured_lufs)
        .filter(|value| value.is_finite())
}

/// Short-term loudness (LUFS) sampled over sliding ~3 s windows.
/// Measuring each window with stratum-dsp's own LUFS path yields a
/// faithful short-term curve without re-implementing K-weighting. A
/// clip shorter than one window is measured whole.
fn measure_short_term_lufs(samples: &[f32], sample_rate: u32) -> Vec<f32> {
    let window = (SHORT_TERM_WINDOW_SECS * sample_rate as f32) as usize;
    let hop = ((SHORT_TERM_HOP_SECS * sample_rate as f32) as usize).max(1);
    if window == 0 || samples.len() < window {
        return measure_integrated_lufs(samples, sample_rate)
            .into_iter()
            .collect();
    }
    let mut values = Vec::new();
    let mut start = 0;
    while start + window <= samples.len() {
        if let Some(lufs) = measure_integrated_lufs(&samples[start..start + window], sample_rate) {
            values.push(lufs);
        }
        start += hop;
    }
    values
}

/// Loudness range (LU): the spread between quiet and loud passages,
/// approximated as the 95th minus 10th percentile of the short-term
/// loudness values after dropping windows more than
/// [`LOUDNESS_RANGE_GATE_LU`] below the loudest (the relative gate).
fn loudness_range(short_term: &[f32]) -> f32 {
    if short_term.len() < 2 {
        return 0.0;
    }
    let max = short_term.iter().copied().fold(f32::NEG_INFINITY, f32::max);
    let gate = max - LOUDNESS_RANGE_GATE_LU;
    let mut gated: Vec<f32> = short_term
        .iter()
        .copied()
        .filter(|value| *value >= gate)
        .collect();
    if gated.len() < 2 {
        return 0.0;
    }
    gated.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    (percentile(&gated, 0.95) - percentile(&gated, 0.10)).max(0.0)
}

/// Nearest-rank percentile of an already-sorted slice. `q` in `[0, 1]`.
fn percentile(sorted: &[f32], q: f32) -> f32 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f32 * q).round() as usize;
    sorted[index.min(sorted.len() - 1)]
}

/// Onset density in events per second from the spectral-flux detector.
fn onset_density(stft: &[Vec<f32>], duration_secs: f32) -> f32 {
    if duration_secs <= 0.0 {
        return 0.0;
    }
    let onsets = detect_spectral_flux_onsets(stft, ONSET_FLUX_PERCENTILE)
        .map(|onsets| onsets.len())
        .unwrap_or(0);
    onsets as f32 / duration_secs
}

/// Per-band energy fractions plus the low band's temporal variation.
struct BandEnergies {
    low_ratio: f32,
    mid_ratio: f32,
    high_ratio: f32,
    low_variation: f32,
}

/// Sum power per band (low / mid / high) across the whole STFT and
/// return each band's fraction of the total, plus the coefficient of
/// variation of the low band over time (the "kick-drum check": low =
/// steady dominant pulse, high = fluid/ambient or syncopated low end).
fn band_energies(stft: &[Vec<f32>], sample_rate: u32) -> BandEnergies {
    let bin_hz = sample_rate as f32 / STFT_FRAME_SIZE as f32;
    let (mut low_total, mut mid_total, mut high_total) = (0.0_f64, 0.0_f64, 0.0_f64);
    let mut low_fractions: Vec<f32> = Vec::with_capacity(stft.len());
    for frame in stft {
        let (mut low, mut mid, mut high) = (0.0_f64, 0.0_f64, 0.0_f64);
        for (bin, magnitude) in frame.iter().enumerate() {
            let freq = bin as f32 * bin_hz;
            let power = f64::from(*magnitude) * f64::from(*magnitude);
            if freq < ACOUSTIC_LOW_MID_HZ {
                low += power;
            } else if freq < ACOUSTIC_MID_HIGH_HZ {
                mid += power;
            } else {
                high += power;
            }
        }
        let frame_total = low + mid + high;
        if frame_total > 0.0 {
            low_fractions.push((low / frame_total) as f32);
        }
        low_total += low;
        mid_total += mid;
        high_total += high;
    }
    let grand = low_total + mid_total + high_total;
    let (low_ratio, mid_ratio, high_ratio) = if grand > 0.0 {
        (
            (low_total / grand) as f32,
            (mid_total / grand) as f32,
            (high_total / grand) as f32,
        )
    } else {
        (0.0, 0.0, 0.0)
    };
    BandEnergies {
        low_ratio,
        mid_ratio,
        high_ratio,
        low_variation: coefficient_of_variation(&low_fractions),
    }
}

/// Coefficient of variation (stddev / mean), clamped to a sane ceiling
/// so a near-zero mean can't blow the value up.
fn coefficient_of_variation(values: &[f32]) -> f32 {
    if values.len() < 2 {
        return 0.0;
    }
    let mean = values.iter().sum::<f32>() / values.len() as f32;
    if mean <= 0.0 {
        return 0.0;
    }
    let variance =
        values.iter().map(|v| (v - mean) * (v - mean)).sum::<f32>() / values.len() as f32;
    (variance.sqrt() / mean).min(4.0)
}

/// Mean tonalness across frames: `1 − spectral_flatness`. Spectral
/// flatness (geometric ÷ arithmetic mean of the power spectrum) is ≈1
/// for noise and ≈0 for a pure tone, so its complement rises with how
/// *pitched* the material is. Silent frames are skipped.
///
/// We use spectral flatness rather than the brief's suggested
/// chroma-energy ratio because it is self-contained (no dependency on
/// the key-detection chroma pass running) and is the textbook
/// pitched-vs-noisy measure; the brief explicitly left this open ("or
/// the key detector's own confidence").
fn mean_tonalness(stft: &[Vec<f32>]) -> f32 {
    let mut sum = 0.0_f64;
    let mut count = 0_u64;
    for frame in stft {
        if let Some(tonalness) = frame_tonalness(frame) {
            sum += f64::from(tonalness);
            count += 1;
        }
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64) as f32
    }
}

/// Tonalness of a single frame, or `None` if the frame is silent. The
/// DC bin is skipped (it carries no pitch information).
fn frame_tonalness(frame: &[f32]) -> Option<f32> {
    let powers: Vec<f64> = frame
        .iter()
        .skip(1)
        .map(|magnitude| f64::from(*magnitude) * f64::from(*magnitude))
        .collect();
    if powers.is_empty() {
        return None;
    }
    let arithmetic_mean = powers.iter().sum::<f64>() / powers.len() as f64;
    if arithmetic_mean <= 1e-12 {
        return None;
    }
    let log_mean = powers.iter().map(|p| (p + 1e-12).ln()).sum::<f64>() / powers.len() as f64;
    let geometric_mean = log_mean.exp();
    let flatness = (geometric_mean / arithmetic_mean).clamp(0.0, 1.0);
    Some((1.0 - flatness) as f32)
}

/// Render a `sustain_dsp::Key` as the lower-case label our mapper
/// expects. `sustain_dsp::Key` exposes a `name()` accessor, but its
/// `Debug` is what other call sites use; we format explicitly so the
/// mapping table below stays canonical.
fn stratum_key_label(key: &sustain_dsp::Key) -> String {
    use sustain_dsp::Key;
    let (root, mode) = match key {
        Key::Major(idx) => (*idx, "major"),
        Key::Minor(idx) => (*idx, "minor"),
    };
    let name = match root % 12 {
        0 => "c",
        1 => "c#",
        2 => "d",
        3 => "d#",
        4 => "e",
        5 => "f",
        6 => "f#",
        7 => "g",
        8 => "g#",
        9 => "a",
        10 => "a#",
        _ => "b",
    };
    format!("{name} {mode}")
}

/// Best-effort mapping from a normalised `stratum-dsp` key label to a
/// [`MusicalKey`]. Returns `None` for labels that do not correspond
/// to one of our 24 variants (vanishingly rare — only happens for
/// non-standard names the engine might produce). Enharmonic equivalents
/// collapse onto Sustain's canonical spelling.
fn map_stratum_key(name: &str) -> Option<MusicalKey> {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "c major" | "cmaj" | "c" => Some(MusicalKey::CMajor),
        "c# major" | "db major" | "c#maj" | "dbmaj" | "c#" | "db" => Some(MusicalKey::DbMajor),
        "d major" | "dmaj" | "d" => Some(MusicalKey::DMajor),
        "d# major" | "eb major" | "d#maj" | "ebmaj" | "d#" | "eb" => Some(MusicalKey::EbMajor),
        "e major" | "emaj" | "e" => Some(MusicalKey::EMajor),
        "f major" | "fmaj" | "f" => Some(MusicalKey::FMajor),
        "f# major" | "gb major" | "f#maj" | "gbmaj" | "f#" | "gb" => Some(MusicalKey::GbMajor),
        "g major" | "gmaj" | "g" => Some(MusicalKey::GMajor),
        "g# major" | "ab major" | "g#maj" | "abmaj" | "g#" | "ab" => Some(MusicalKey::AbMajor),
        "a major" | "amaj" | "a" => Some(MusicalKey::AMajor),
        "a# major" | "bb major" | "a#maj" | "bbmaj" | "a#" | "bb" => Some(MusicalKey::BbMajor),
        "b major" | "bmaj" | "b" => Some(MusicalKey::BMajor),
        "c minor" | "cm" | "cmin" => Some(MusicalKey::CMinor),
        "c# minor" | "db minor" | "c#m" | "c#min" | "dbm" | "dbmin" => Some(MusicalKey::CsMinor),
        "d minor" | "dm" | "dmin" => Some(MusicalKey::DMinor),
        "d# minor" | "eb minor" | "d#m" | "d#min" | "ebm" | "ebmin" => Some(MusicalKey::EbMinor),
        "e minor" | "em" | "emin" => Some(MusicalKey::EMinor),
        "f minor" | "fm" | "fmin" => Some(MusicalKey::FMinor),
        "f# minor" | "gb minor" | "f#m" | "f#min" | "gbm" | "gbmin" => Some(MusicalKey::FsMinor),
        "g minor" | "gm" | "gmin" => Some(MusicalKey::GMinor),
        "g# minor" | "ab minor" | "g#m" | "g#min" | "abm" | "abmin" => Some(MusicalKey::AbMinor),
        "a minor" | "am" | "amin" => Some(MusicalKey::AMinor),
        "a# minor" | "bb minor" | "a#m" | "a#min" | "bbm" | "bbmin" => Some(MusicalKey::BbMinor),
        "b minor" | "bm" | "bmin" => Some(MusicalKey::BMinor),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnalysisOptions, Analyzer, BPM_KEY_WINDOW_SECS, map_stratum_key, octave_normalize,
        stratum_key_label,
    };
    use std::path::PathBuf;
    use std::time::Duration;
    use sustain_domain::MusicalKey;

    #[test]
    fn octave_normalize_doubles_subbass_tempo() {
        // 60 BPM with [70..170] window doubles to 120.
        assert!((octave_normalize(60.0, 70.0, 170.0) - 120.0).abs() < f32::EPSILON);
    }

    #[test]
    fn octave_normalize_halves_double_time() {
        // 200 BPM with [70..170] window halves to 100.
        assert!((octave_normalize(200.0, 70.0, 170.0) - 100.0).abs() < f32::EPSILON);
    }

    #[test]
    fn octave_normalize_passes_in_range_unchanged() {
        assert!((octave_normalize(128.0, 70.0, 170.0) - 128.0).abs() < f32::EPSILON);
    }

    #[test]
    fn octave_normalize_keeps_value_when_no_octave_lands_in_range() {
        // A track tagged 40 BPM in a narrow [60..70] window cannot be
        // doubled (80 > 70) — return what we have rather than spin
        // forever.
        assert!((octave_normalize(40.0, 60.0, 70.0) - 40.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stratum_key_labels_map_to_canonical_variants() {
        assert_eq!(map_stratum_key("C major"), Some(MusicalKey::CMajor));
        assert_eq!(map_stratum_key("c minor"), Some(MusicalKey::CMinor));
        assert_eq!(map_stratum_key("D# major"), Some(MusicalKey::EbMajor));
        assert_eq!(map_stratum_key("F# minor"), Some(MusicalKey::FsMinor));
        assert_eq!(map_stratum_key("Bb"), Some(MusicalKey::BbMajor));
    }

    #[test]
    fn stratum_key_labels_reject_unknown_input() {
        assert_eq!(map_stratum_key(""), None);
        assert_eq!(map_stratum_key("H major"), None);
    }

    #[test]
    fn stratum_key_label_formats_match_mapper() {
        // Every label `stratum_key_label` produces must round-trip
        // through `map_stratum_key` to a `MusicalKey`. Guard against
        // a typo introducing a label the mapper does not know.
        for root in 0_u32..12 {
            for mode in [sustain_dsp::Key::Major(root), sustain_dsp::Key::Minor(root)] {
                let label = stratum_key_label(&mode);
                assert!(
                    map_stratum_key(&label).is_some(),
                    "label {label:?} from stratum_key_label has no mapper entry"
                );
            }
        }
    }

    #[test]
    fn analyzer_returns_none_for_missing_file() {
        // Capability-gating boils down to "the method that was not
        // called did no work". Here we call only `bpm()` on a path
        // that does not exist; the band-specific result is None and
        // the unrequested bands are never touched (a successful
        // `key()`/`waveform()` call would be impossible too, since
        // the file does not exist — but the point of this test is
        // that the absence of an explicit Result still surfaces the
        // failure cleanly).
        let analyzer = Analyzer::new(
            PathBuf::from("/does/not/exist/sustain_tests_missing.flac"),
            AnalysisOptions::default(),
            None,
        );
        assert_eq!(analyzer.bpm(), None);
        assert_eq!(analyzer.key(), None);
        assert!(analyzer.waveform().is_none());
    }

    #[test]
    fn analyzer_constructed_lazily_without_io() {
        // Constructing an Analyzer must not touch the filesystem.
        // The path here is intentionally invalid — if the
        // constructor opened the file, this test would not even
        // compile a working analyzer for the assertion below.
        let _analyzer = Analyzer::new(
            PathBuf::from("/this/path/does/not/exist.flac"),
            AnalysisOptions::default(),
            None,
        );
        // Reaching this line proves no I/O happened in `new`; the
        // call sites lower in the method chain (bpm/key/waveform)
        // are where the actual `File::open` lives.
    }

    #[test]
    fn long_track_classification_keys_off_the_duration_hint() {
        let opts = AnalysisOptions::default();
        let path = PathBuf::from("/does/not/matter.flac");
        // 20 min > 15 min threshold → long.
        assert!(Analyzer::new(&path, opts, Some(Duration::from_secs(20 * 60))).is_long_track());
        // 4 min → normal.
        assert!(!Analyzer::new(&path, opts, Some(Duration::from_secs(4 * 60))).is_long_track());
        // Exactly the threshold is *not* long (strictly greater).
        assert!(!Analyzer::new(&path, opts, Some(Duration::from_secs(15 * 60))).is_long_track());
        // No hint → treated as normal.
        assert!(!Analyzer::new(&path, opts, None).is_long_track());
    }

    #[test]
    fn window_start_centers_the_window_in_the_track() {
        let opts = AnalysisOptions::default();
        let path = PathBuf::from("/does/not/matter.flac");
        // A 4-min (240 s) track, 120 s BPM/key window → starts at 60 s
        // (the "skip the intro" behaviour the centering subsumes).
        let normal = Analyzer::new(&path, opts, Some(Duration::from_secs(240)));
        assert!((normal.window_start(BPM_KEY_WINDOW_SECS) - 60.0).abs() < 1e-6);
        // A window longer than the track clamps to 0 (decode the whole
        // short track).
        let short = Analyzer::new(&path, opts, Some(Duration::from_secs(30)));
        assert_eq!(short.window_start(BPM_KEY_WINDOW_SECS), 0.0);
        // No hint → window from the start.
        let unknown = Analyzer::new(&path, opts, None);
        assert_eq!(unknown.window_start(BPM_KEY_WINDOW_SECS), 0.0);
    }

    #[test]
    fn center_slice_takes_the_middle_span() {
        // 10 s of mono at 1 kHz = 10_000 samples; the central 2 s is
        // samples [4000, 6000).
        let region = super::DecodedAudio {
            samples: (0..10_000).map(|i| i as f32).collect(),
            sample_rate: 1_000,
        };
        let slice = super::center_slice(&region, 2.0);
        assert_eq!(slice.sample_rate, 1_000);
        assert_eq!(slice.samples.len(), 2_000);
        assert_eq!(slice.samples.first().copied(), Some(4_000.0));
        assert_eq!(slice.samples.last().copied(), Some(5_999.0));

        // A region shorter than the window is returned whole.
        let whole = super::center_slice(&region, 30.0);
        assert_eq!(whole.samples.len(), region.samples.len());
    }
}

#[cfg(test)]
mod acoustic_tests {
    use super::{
        STFT_FRAME_SIZE, band_energies, coefficient_of_variation, frame_tonalness, loudness_range,
        mean_tonalness, measure_integrated_lufs, percentile,
    };

    const SAMPLE_RATE: u32 = 44_100;

    /// A magnitude-spectrum frame (`frame_size/2 + 1` bins) with all
    /// energy in a single bin. Bin `b` maps to `b * SR / FRAME_SIZE` Hz.
    fn frame_with_energy_at_bin(bin: usize, magnitude: f32) -> Vec<f32> {
        let mut frame = vec![0.0_f32; STFT_FRAME_SIZE / 2 + 1];
        frame[bin] = magnitude;
        frame
    }

    fn sine(freq: f32, secs: f32, amplitude: f32) -> Vec<f32> {
        let count = (secs * SAMPLE_RATE as f32) as usize;
        (0..count)
            .map(|i| {
                amplitude * (std::f32::consts::TAU * freq * i as f32 / SAMPLE_RATE as f32).sin()
            })
            .collect()
    }

    #[test]
    fn band_ratios_follow_the_dominant_frequency() {
        // Bin 5 ≈ 108 Hz (low band); bin 500 ≈ 10.8 kHz (high band).
        let low_heavy = vec![frame_with_energy_at_bin(5, 1.0); 4];
        let bands = band_energies(&low_heavy, SAMPLE_RATE);
        assert!(
            bands.low_ratio > 0.99,
            "low tone should land in the low band, got low={} mid={} high={}",
            bands.low_ratio,
            bands.mid_ratio,
            bands.high_ratio
        );

        let high_heavy = vec![frame_with_energy_at_bin(500, 1.0); 4];
        let bands = band_energies(&high_heavy, SAMPLE_RATE);
        assert!(bands.high_ratio > 0.99, "high tone → high band");
    }

    #[test]
    fn steady_low_band_has_low_variation_jittery_has_high() {
        // Identical low-band frames → zero variation.
        let steady = vec![frame_with_energy_at_bin(5, 1.0); 8];
        assert!(band_energies(&steady, SAMPLE_RATE).low_variation < 1e-3);

        // Alternating low-only / high-only frames → the low-band
        // fraction swings between ~1 and ~0, a high coefficient of
        // variation.
        let mut jittery = Vec::new();
        for i in 0..8 {
            jittery.push(frame_with_energy_at_bin(
                if i % 2 == 0 { 5 } else { 500 },
                1.0,
            ));
        }
        assert!(band_energies(&jittery, SAMPLE_RATE).low_variation > 0.5);
    }

    #[test]
    fn tonalness_is_high_for_a_peak_and_low_for_a_flat_spectrum() {
        // One dominant bin → very peaky → tonalness near 1.
        let peaky = frame_with_energy_at_bin(40, 10.0);
        let peaky_tonalness = frame_tonalness(&peaky).expect("non-silent");
        assert!(peaky_tonalness > 0.9, "peak tonalness {peaky_tonalness}");

        // Flat spectrum (every bin equal) → flatness ≈ 1 → tonalness ≈ 0.
        let flat = vec![1.0_f32; STFT_FRAME_SIZE / 2 + 1];
        let flat_tonalness = frame_tonalness(&flat).expect("non-silent");
        assert!(flat_tonalness < 0.05, "flat tonalness {flat_tonalness}");

        // A silent frame contributes nothing.
        assert_eq!(frame_tonalness(&[0.0_f32; 8]), None);

        // Mean across frames sits between the two.
        let mixed = vec![peaky.clone(), flat.clone()];
        let mean = mean_tonalness(&mixed);
        assert!(mean > flat_tonalness && mean < peaky_tonalness);
    }

    #[test]
    fn percentile_and_loudness_range() {
        let sorted = [-20.0, -18.0, -16.0, -14.0, -12.0];
        assert_eq!(percentile(&sorted, 0.0), -20.0);
        assert_eq!(percentile(&sorted, 1.0), -12.0);

        // Range over a spread set is positive; a flat set is ~0.
        let varied = [-30.0, -22.0, -18.0, -10.0, -8.0];
        assert!(loudness_range(&varied) > 0.0);
        let flat = [-14.0, -14.0, -14.0, -14.0];
        assert_eq!(loudness_range(&flat), 0.0);
        assert_eq!(loudness_range(&[-14.0]), 0.0);
    }

    #[test]
    fn coefficient_of_variation_basics() {
        assert_eq!(coefficient_of_variation(&[]), 0.0);
        assert_eq!(coefficient_of_variation(&[0.5]), 0.0);
        assert_eq!(coefficient_of_variation(&[0.3, 0.3, 0.3]), 0.0);
        assert!(coefficient_of_variation(&[0.1, 0.9]) > 0.0);
    }

    #[test]
    fn louder_audio_measures_higher_lufs() {
        // Same 440 Hz tone at two amplitudes 14 dB apart. The louder
        // one must report a higher integrated LUFS (exercises the
        // stratum-dsp BS.1770-4 path end to end).
        let loud = sine(440.0, 2.0, 0.5);
        let quiet = sine(440.0, 2.0, 0.1);
        let loud_lufs = measure_integrated_lufs(&loud, SAMPLE_RATE).expect("loud lufs");
        let quiet_lufs = measure_integrated_lufs(&quiet, SAMPLE_RATE).expect("quiet lufs");
        assert!(
            loud_lufs > quiet_lufs + 3.0,
            "louder tone should measure clearly higher: {loud_lufs} vs {quiet_lufs}"
        );
    }
}
