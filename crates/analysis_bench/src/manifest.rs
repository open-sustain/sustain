// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The TOML manifest that drives a benchmark run.
//!
//! A manifest lists tracks to analyze. Each track is either a
//! deterministically generated [`Synthetic`] fixture (committable, CI-safe)
//! or a `path` to real audio (kept in a *gitignored* manifest — we never
//! commit private audio). Ground truth — `bpm`, `key`, and a beat grid
//! (`beat_grid` or annotated `beats`) — turns a run into a scored
//! evaluation; absent, the run still records outputs and timings as a
//! baseline.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fixtures::Synthetic;
use crate::metrics::AnnotatedBeat;

/// One analyzer capability. Selecting a subset is the point of the
/// capability-gated pipeline: a BPM-only run must not pay for key, chroma,
/// loudness, or waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task {
    /// Tempo detection and beat tracking
    /// ([`sustain_analysis::Analyzer::tempo`]).
    Bpm,
    /// Key detection ([`sustain_analysis::Analyzer::key`]).
    Key,
    /// Perceptual acoustic features ([`sustain_analysis::Analyzer::acoustics`]).
    Acoustics,
    /// Waveform tiers ([`sustain_analysis::Analyzer::waveform`]).
    Waveform,
}

impl Task {
    /// Every capability, in the order a full pass primes its caches
    /// (larger decodes first so BPM/key slice their window for free).
    pub const ALL: [Task; 4] = [Task::Waveform, Task::Acoustics, Task::Bpm, Task::Key];
}

/// Top-level manifest.
#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    /// Corpus identity and provenance.
    pub meta: Meta,
    /// Run-wide options (BPM range, default task set).
    #[serde(default)]
    pub options: Options,
    /// The tracks to analyze.
    #[serde(rename = "track", default)]
    pub tracks: Vec<TrackEntry>,
}

/// Corpus identity. Kept deliberately small; full provenance for external
/// corpora lives in a separate manifest document (see the crate README).
#[derive(Clone, Debug, Deserialize)]
pub struct Meta {
    /// Stable short name, e.g. `synthetic_v1` or `giantsteps_tempo`.
    pub corpus_id: String,
    /// Free-text description of what the corpus is for.
    #[serde(default)]
    pub description: String,
}

/// Run-wide options.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct Options {
    /// Lower bound of the octave-normalization BPM range. Defaults to the
    /// analyzer's own default when absent.
    pub min_bpm: Option<f32>,
    /// Upper bound of the octave-normalization BPM range.
    pub max_bpm: Option<f32>,
    /// Default capability set for tracks that do not override it. Absent
    /// means "all capabilities".
    pub tasks: Option<Vec<Task>>,
}

/// One track in a manifest.
#[derive(Clone, Debug, Deserialize)]
pub struct TrackEntry {
    /// Stable identifier. This — never a filesystem path — is what
    /// appears in committed results, so private corpora stay anonymous.
    pub id: String,
    /// Path to real audio. Mutually exclusive with `synthetic`.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// A synthetic fixture to generate. Mutually exclusive with `path`.
    #[serde(default)]
    pub synthetic: Option<Synthetic>,
    /// Track duration hint (seconds) the analyzer uses to center its
    /// windows. Defaults to a synthetic fixture's own length; for real
    /// audio, supplying it makes window placement deterministic.
    #[serde(default)]
    pub duration_secs: Option<f64>,
    /// Ground-truth BPM, if known.
    #[serde(default)]
    pub bpm: Option<f64>,
    /// Ground-truth beat grid, if known.
    #[serde(default)]
    pub beat_grid: Option<ReferenceGrid>,
    /// Beats a person marked, `[secs, position]` each, in time order — the
    /// other form of grid ground truth, for music whose beats a constant
    /// grid only approximates. Exclusive with `beat_grid`.
    #[serde(default)]
    pub beats: Option<Vec<AnnotatedBeat>>,
    /// Ground-truth key, if known (any form [`crate::metrics::parse_key`]
    /// accepts).
    #[serde(default)]
    pub key: Option<String>,
    /// Per-track capability override. Absent falls back to the run
    /// options, then to all capabilities.
    #[serde(default)]
    pub tasks: Option<Vec<Task>>,
}

/// A ground-truth constant beat grid, on the gapless decode timeline the
/// analyzer measures (encoder delay and padding removed).
#[derive(Clone, Copy, Debug, Deserialize)]
pub struct ReferenceGrid {
    /// Tempo the beats are laid out at.
    pub bpm: f64,
    /// Time of any beat, in seconds; conventionally the first.
    pub first_beat_secs: f64,
    /// That beat's position in its bar, 1 (the downbeat) to 4.
    pub first_beat_number: u8,
}

/// A malformed manifest.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The file could not be read.
    #[error("failed to read manifest {path}: {source}")]
    Read {
        /// Manifest path.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The TOML did not parse.
    #[error("failed to parse manifest {path}: {source}")]
    Parse {
        /// Manifest path.
        path: String,
        /// Underlying parse error.
        #[source]
        source: toml::de::Error,
    },
    /// A track had neither or both of `path` / `synthetic`.
    #[error("track {id:?}: exactly one of `path` or `synthetic` is required")]
    Source {
        /// Offending track id.
        id: String,
    },
    /// A track's beat-grid ground truth is unusable.
    #[error("track {id:?}: {problem}")]
    Beats {
        /// Offending track id.
        id: String,
        /// What is wrong with it.
        problem: &'static str,
    },
}

impl Manifest {
    /// Load and validate a manifest from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&text, &path.display().to_string())
    }

    /// Parse and validate manifest TOML; `origin` names it in errors.
    pub fn parse(text: &str, origin: &str) -> Result<Self, ManifestError> {
        let manifest: Manifest = toml::from_str(text).map_err(|source| ManifestError::Parse {
            path: origin.to_string(),
            source,
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject tracks that do not name exactly one source, or whose
    /// annotated beats cannot be scored.
    fn validate(&self) -> Result<(), ManifestError> {
        for track in &self.tracks {
            if track.path.is_some() == track.synthetic.is_some() {
                return Err(ManifestError::Source {
                    id: track.id.clone(),
                });
            }
            let problem = track
                .beats
                .as_deref()
                .and_then(|beats| annotated_beats_problem(beats, track.beat_grid.is_some()));
            if let Some(problem) = problem {
                return Err(ManifestError::Beats {
                    id: track.id.clone(),
                    problem,
                });
            }
        }
        Ok(())
    }
}

/// What makes `beats` unusable as a track's ground truth, if anything.
pub(crate) fn annotated_beats_problem(
    beats: &[AnnotatedBeat],
    has_grid: bool,
) -> Option<&'static str> {
    if has_grid {
        return Some("at most one of `beat_grid` or `beats` is allowed");
    }
    if beats.len() < 2 {
        return Some("`beats` needs at least two beats to have a tempo");
    }
    if beats.iter().any(|beat| !beat.secs.is_finite()) {
        return Some("`beats` times must be finite");
    }
    if beats.windows(2).any(|pair| pair[1].secs <= pair[0].secs) {
        return Some("`beats` must be in strictly increasing time order");
    }
    if beats.iter().any(|beat| !(1..=4).contains(&beat.position)) {
        return Some("`beats` bar positions must be 1 to 4");
    }
    None
}

impl TrackEntry {
    /// The capabilities to run for this track: its own override, else the
    /// run default, else every capability.
    pub fn effective_tasks(&self, options: &Options) -> Vec<Task> {
        self.tasks
            .clone()
            .or_else(|| options.tasks.clone())
            .unwrap_or_else(|| Task::ALL.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::{Manifest, Task};

    #[test]
    fn parses_synthetic_and_real_entries() {
        let src = r#"
            [meta]
            corpus_id = "demo"

            [options]
            min_bpm = 76.0
            max_bpm = 155.0
            tasks = ["bpm", "key"]

            [[track]]
            id = "click_120"
            synthetic = { kind = "click_train", bpm = 120.0, secs = 8.0 }
            bpm = 120.0

            [[track]]
            id = "real_a"
            path = "/music/a.flac"
            key = "Am"
            tasks = ["bpm"]
        "#;
        let manifest: Manifest = toml::from_str(src).expect("parse");
        manifest.validate().expect("valid");
        assert_eq!(manifest.tracks.len(), 2);
        assert_eq!(manifest.options.max_bpm, Some(155.0));
        // Run default applies to the first track.
        assert_eq!(
            manifest.tracks[0].effective_tasks(&manifest.options),
            vec![Task::Bpm, Task::Key]
        );
        // The second track overrides it.
        assert_eq!(
            manifest.tracks[1].effective_tasks(&manifest.options),
            vec![Task::Bpm]
        );
    }

    #[test]
    fn parses_annotated_beats() {
        let src = r#"
            [meta]
            corpus_id = "demo"
            [[track]]
            id = "clip"
            path = "/music/clip.wav"
            beats = [[0.5, 3], [1.0, 4], [1.5, 1]]
        "#;
        let manifest: Manifest = toml::from_str(src).expect("parse");
        manifest.validate().expect("valid");
        let beats = manifest.tracks[0].beats.as_ref().expect("beats");
        assert_eq!(beats.len(), 3);
        assert_eq!((beats[2].secs, beats[2].position), (1.5, 1));
    }

    #[test]
    fn rejects_unusable_annotated_beats() {
        for (beats, extra) in [
            ("[[0.5, 1]]", ""),
            ("[[0.5, 1], [0.5, 2]]", ""),
            ("[[0.5, 1], [1.0, 5]]", ""),
            (
                "[[0.5, 1], [1.0, 2]]",
                "beat_grid = { bpm = 120.0, first_beat_secs = 0.5, first_beat_number = 1 }",
            ),
        ] {
            let src = format!(
                "[meta]\ncorpus_id = \"demo\"\n[[track]]\nid = \"clip\"\n\
                 path = \"/music/clip.wav\"\nbeats = {beats}\n{extra}\n"
            );
            let manifest: Manifest = toml::from_str(&src).expect("parse");
            assert!(manifest.validate().is_err(), "{beats} {extra}");
        }
    }

    #[test]
    fn rejects_tracks_without_exactly_one_source() {
        let src = r#"
            [meta]
            corpus_id = "demo"
            [[track]]
            id = "bad"
        "#;
        let manifest: Manifest = toml::from_str(src).expect("parse");
        assert!(manifest.validate().is_err());
    }
}
