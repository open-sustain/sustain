// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Learn the key detector's major and minor weights from labelled audio.
//!
//! The detector scores each of the 24 keys by weighing the log share each
//! pitch class takes of a track's chroma ([`Analyzer::key_log_shares`])
//! with that key's template, and the key scoring highest wins. The
//! templates of a mode are one weight vector rotated onto each tonic, so
//! the model has 24 weights. They are fitted as a multinomial logistic
//! regression over the 24 keys: the weights that make the labelled keys
//! most probable under the softmax of the scores, with a small ridge
//! penalty ([`crate::softmax_regression`]).
//!
//! The ridge also makes the optimum unique. Adding one constant to every
//! weight adds the same amount to all 24 scores of a track — the sum of
//! its log shares times the constant — which no softmax can see.
//!
//! `analysis-bench learn-key-weights` runs this over key manifests and
//! prints the weights in the form `sustain_dsp`'s key templates hold.

use std::path::Path;
use std::time::Duration;

use sustain_analysis::{AnalysisOptions, Analyzer};

use crate::manifest::Manifest;
use crate::metrics::{KeyClass, parse_key};
use crate::softmax_regression::{self, Choice};

/// The ridge penalty on the squared weights, beside the mean negative log
/// likelihood of the labelled keys. Small enough to change no decision,
/// large enough to pin the one direction the likelihood cannot see.
const RIDGE: f64 = 1e-4;

/// The candidate keys, and the weights: both modes', major first, index
/// `12 · mode + interval`.
const KEYS: usize = 24;

/// The labelled tracks to learn from.
#[derive(Clone, Debug, Default)]
pub struct WeightLearner {
    tracks: Vec<LabelledTrack>,
}

#[derive(Clone, Debug)]
struct LabelledTrack {
    log_shares: [f64; 12],
    /// The labelled key's index among the 24 candidates: `12 · mode + tonic`.
    key: usize,
}

/// Learned weights, tonic at index 0.
#[derive(Clone, Debug, PartialEq)]
pub struct KeyWeights {
    pub major: [f64; 12],
    pub minor: [f64; 12],
    /// Tracks of each mode the weights were learned from.
    pub major_tracks: usize,
    pub minor_tracks: usize,
}

impl WeightLearner {
    /// Add one track's pitch-class log shares under its labelled `key`.
    /// Returns whether the track counted: a non-finite share says nothing
    /// about a key.
    pub fn add(&mut self, log_shares: [f32; 12], key: KeyClass) -> bool {
        if !log_shares.iter().all(|share| share.is_finite()) {
            return false;
        }
        self.tracks.push(LabelledTrack {
            log_shares: log_shares.map(f64::from),
            key: 12 * usize::from(!key.major) + usize::from(key.tonic % 12),
        });
        true
    }

    /// The learned weights, or `None` until both modes have a track.
    pub fn weights(&self) -> Option<KeyWeights> {
        let minor_tracks = self.tracks.iter().filter(|track| track.key >= 12).count();
        let major_tracks = self.tracks.len() - minor_tracks;
        if major_tracks == 0 || minor_tracks == 0 {
            return None;
        }
        let choices: Vec<Choice> = self.tracks.iter().map(LabelledTrack::choice).collect();
        let weights = softmax_regression::fit(&choices, KEYS, RIDGE);
        Some(KeyWeights {
            major: std::array::from_fn(|interval| weights[interval]),
            minor: std::array::from_fn(|interval| weights[12 + interval]),
            major_tracks,
            minor_tracks,
        })
    }
}

impl LabelledTrack {
    /// The track as a choice among the 24 keys, each key's features its
    /// tonic's rotation of the log shares, in its mode's half.
    fn choice(&self) -> Choice {
        let candidates = (0..KEYS)
            .map(|key| {
                let (mode, tonic) = (key / 12, key % 12);
                let mut x = vec![0.0; KEYS];
                for interval in 0..12 {
                    x[12 * mode + interval] = self.log_shares[(tonic + interval) % 12];
                }
                x
            })
            .collect();
        Choice {
            candidates,
            chosen: self.key,
        }
    }
}

/// What [`learn`] went through, beside the weights.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LearnReport {
    /// Tracks whose chroma the weights were learned from.
    pub learned: usize,
    /// Tracks without a readable key label, a file path, or tonal chroma.
    pub skipped: usize,
}

/// Learn the weights from every labelled audio file in `manifests`.
/// `progress` is told each track's id as it is analysed.
pub fn learn(
    manifests: &[Manifest],
    mut progress: impl FnMut(&str),
) -> (Option<KeyWeights>, LearnReport) {
    let mut learner = WeightLearner::default();
    let mut report = LearnReport::default();
    for entry in manifests.iter().flat_map(|manifest| &manifest.tracks) {
        progress(&entry.id);
        let key = entry.key.as_deref().and_then(parse_key);
        let counted = match (key, &entry.path) {
            (Some(key), Some(path)) => log_shares(path, entry.duration_secs)
                .is_some_and(|log_shares| learner.add(log_shares, key)),
            _ => false,
        };
        if counted {
            report.learned += 1;
        } else {
            report.skipped += 1;
        }
    }
    (learner.weights(), report)
}

fn log_shares(path: &Path, duration_secs: Option<f64>) -> Option<[f32; 12]> {
    let hint = duration_secs
        .filter(|secs| secs.is_finite() && *secs > 0.0)
        .map(Duration::from_secs_f64);
    Analyzer::new(path.to_path_buf(), AnalysisOptions::default(), hint).key_log_shares()
}

impl KeyWeights {
    /// The weights as the Rust arrays the key templates hold.
    pub fn to_rust(&self) -> String {
        format!(
            "// Major: {} tracks. Minor: {} tracks.\n\
             const MAJOR_WEIGHTS: [f32; 12] = {};\n\
             const MINOR_WEIGHTS: [f32; 12] = {};\n",
            self.major_tracks,
            self.minor_tracks,
            rust_array(&self.major),
            rust_array(&self.minor),
        )
    }
}

fn rust_array(weights: &[f64; 12]) -> String {
    let values: Vec<String> = weights.iter().map(|value| format!("{value:.3}")).collect();
    format!("[{}]", values.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The log shares of a chroma holding `notes` (pitch class, level),
    /// as the detector reads them.
    fn log_shares(notes: &[(usize, f32)]) -> [f32; 12] {
        let mut chroma = [0.0; 12];
        for &(pitch_class, level) in notes {
            chroma[pitch_class] = level;
        }
        // As `sustain_dsp::pitch_class_log_shares` reads them.
        let total: f32 = chroma.iter().sum();
        std::array::from_fn(|pitch_class| (chroma[pitch_class] / total + 1e-3).ln())
    }

    fn key(tonic: u8, major: bool) -> KeyClass {
        KeyClass { tonic, major }
    }

    /// Major and minor triads, with the tonic louder, on every tonic.
    fn triad_learner() -> WeightLearner {
        let mut learner = WeightLearner::default();
        for tonic in 0..12_u8 {
            let t = usize::from(tonic);
            for (third, major) in [(4, true), (3, false)] {
                let notes = [(t, 2.0), ((t + third) % 12, 1.0), ((t + 7) % 12, 1.0)];
                assert!(learner.add(log_shares(&notes), key(tonic, major)));
            }
        }
        learner
    }

    #[test]
    fn learned_weights_name_the_keys_they_were_learned_from() {
        let learner = triad_learner();
        let weights = learner.weights().expect("both modes have tracks");
        assert_eq!((weights.major_tracks, weights.minor_tracks), (12, 12));
        let all: Vec<f64> = weights
            .major
            .iter()
            .chain(&weights.minor)
            .copied()
            .collect();
        for track in &learner.tracks {
            let choice = track.choice();
            assert_eq!(
                softmax_regression::best(&choice.candidates, &all),
                track.key
            );
        }
    }

    #[test]
    fn one_mode_teaches_nothing() {
        let mut learner = WeightLearner::default();
        assert!(learner.add(log_shares(&[(0, 2.0), (4, 1.0), (7, 1.0)]), key(0, true)));
        assert!(!learner.add([f32::NEG_INFINITY; 12], key(9, false)));
        assert_eq!(learner.weights(), None);
    }

    #[test]
    fn weights_print_as_template_arrays() {
        let weights = KeyWeights {
            major: [
                2.75, -1.25, 0.5, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0,
            ],
            minor: [1.0; 12],
            major_tracks: 3,
            minor_tracks: 4,
        };
        let rust = weights.to_rust();
        assert!(rust.contains("// Major: 3 tracks. Minor: 4 tracks."));
        assert!(rust.contains(
            "const MAJOR_WEIGHTS: [f32; 12] = [2.750, -1.250, 0.500, 0.000, 0.000, \
             0.000, 0.000, 0.000, 0.000, 0.000, 0.000, 0.000];"
        ));
    }
}
