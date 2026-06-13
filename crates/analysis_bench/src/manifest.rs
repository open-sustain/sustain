// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The TOML manifest that drives a benchmark run.
//!
//! A manifest lists tracks to analyze. Each track is either a
//! deterministically generated [`Synthetic`] fixture (committable, CI-safe)
//! or a `path` to real audio (kept in a *gitignored* manifest — we never
//! commit private audio). Ground-truth `bpm` / `key`, when present, turn a
//! run into a scored evaluation; absent, the run still records outputs and
//! timings as a baseline.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::fixtures::Synthetic;

/// One analyzer capability. Selecting a subset is the point of the
/// capability-gated pipeline: a BPM-only run must not pay for key, chroma,
/// loudness, or waveform.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Task {
    /// Tempo detection ([`sustain_analysis::Analyzer::bpm`]).
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
    /// Ground-truth key, if known (any form [`crate::metrics::parse_key`]
    /// accepts).
    #[serde(default)]
    pub key: Option<String>,
    /// Per-track capability override. Absent falls back to the run
    /// options, then to all capabilities.
    #[serde(default)]
    pub tasks: Option<Vec<Task>>,
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
}

impl Manifest {
    /// Load and validate a manifest from a TOML file.
    pub fn load(path: &std::path::Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.display().to_string(),
            source,
        })?;
        let manifest: Manifest = toml::from_str(&text).map_err(|source| ManifestError::Parse {
            path: path.display().to_string(),
            source,
        })?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Reject tracks that do not name exactly one source.
    fn validate(&self) -> Result<(), ManifestError> {
        for track in &self.tracks {
            if track.path.is_some() == track.synthetic.is_some() {
                return Err(ManifestError::Source {
                    id: track.id.clone(),
                });
            }
        }
        Ok(())
    }
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
