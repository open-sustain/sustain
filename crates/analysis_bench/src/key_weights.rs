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
//! penalty. The fit is convex, so Newton's method finds its one optimum
//! in a few steps, from any start.
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

/// The ridge penalty on the squared weights, beside the mean negative log
/// likelihood of the labelled keys. Small enough to change no decision,
/// large enough to pin the one direction the likelihood cannot see.
const RIDGE: f64 = 1e-4;

/// The fit stops once a Newton step promises to lower the objective by
/// less than this (half the Newton decrement): far below any change to a
/// learned weight's printed digits, yet above the rounding of a sum over
/// thousands of tracks.
const DECREASE_TOLERANCE: f64 = 1e-14;

/// More Newton steps than a fit this small ever needs.
const MAX_NEWTON_STEPS: usize = 100;

/// Both modes' weights, major first: index `12 · mode + interval`.
type Weights = [f64; 24];

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
        let weights = fit(&self.tracks);
        Some(KeyWeights {
            major: std::array::from_fn(|interval| weights[interval]),
            minor: std::array::from_fn(|interval| weights[12 + interval]),
            major_tracks,
            minor_tracks,
        })
    }
}

/// The weights minimizing [`objective`], by Newton's method with a
/// backtracking line search.
fn fit(tracks: &[LabelledTrack]) -> Weights {
    let mut weights = [0.0; 24];
    for _ in 0..MAX_NEWTON_STEPS {
        let (value, gradient, hessian) = derivatives(tracks, &weights);
        let step = solve_positive_definite(&hessian, gradient.map(|g| -g));
        // The objective's slope along the step: minus the Newton decrement.
        let slope: f64 = gradient.iter().zip(&step).map(|(g, s)| g * s).sum();
        if -slope / 2.0 <= DECREASE_TOLERANCE {
            break;
        }
        // Halve the step until it decreases the objective enough (Armijo's
        // condition); far from the optimum the full step can overshoot.
        let mut scale = 1.0;
        loop {
            let candidate: Weights = std::array::from_fn(|i| weights[i] + scale * step[i]);
            if objective(tracks, &candidate) <= value + 1e-4 * scale * slope {
                weights = candidate;
                break;
            }
            scale /= 2.0;
            if scale < 1e-12 {
                // No step decreases it: rounding has the last word.
                return weights;
            }
        }
    }
    weights
}

/// The score of each of the 24 keys for `track`: the weights of the key's
/// mode against the log shares rotated to the key's tonic.
fn scores(track: &LabelledTrack, weights: &Weights) -> [f64; 24] {
    std::array::from_fn(|key| {
        let (mode, tonic) = (key / 12, key % 12);
        (0..12)
            .map(|interval| {
                weights[12 * mode + interval] * track.log_shares[(tonic + interval) % 12]
            })
            .sum()
    })
}

/// The softmax of `scores`, and the log of its normalizer.
fn probabilities(scores: &[f64; 24]) -> ([f64; 24], f64) {
    let highest = scores.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let exponentials = scores.map(|score| (score - highest).exp());
    let total: f64 = exponentials.iter().sum();
    (exponentials.map(|e| e / total), highest + total.ln())
}

/// The mean negative log likelihood of the labelled keys plus the ridge
/// penalty.
fn objective(tracks: &[LabelledTrack], weights: &Weights) -> f64 {
    let likelihood: f64 = tracks
        .iter()
        .map(|track| {
            let scores = scores(track, weights);
            probabilities(&scores).1 - scores[track.key]
        })
        .sum();
    likelihood / tracks.len() as f64 + ridge_penalty(weights)
}

fn ridge_penalty(weights: &Weights) -> f64 {
    RIDGE * weights.iter().map(|w| w * w).sum::<f64>()
}

/// The [`objective`], its gradient and its Hessian at `weights`.
fn derivatives(tracks: &[LabelledTrack], weights: &Weights) -> (f64, Weights, [Weights; 24]) {
    let mut value = 0.0;
    let mut gradient = [0.0; 24];
    let mut hessian = [[0.0; 24]; 24];
    for track in tracks {
        let scores = scores(track, weights);
        let (probabilities, log_normalizer) = probabilities(&scores);
        value += log_normalizer - scores[track.key];
        // Each key's feature vector: its tonic's rotation of the log
        // shares, in its mode's half.
        let features = |key: usize| -> Weights {
            let (mode, tonic) = (key / 12, key % 12);
            let mut x = [0.0; 24];
            for interval in 0..12 {
                x[12 * mode + interval] = track.log_shares[(tonic + interval) % 12];
            }
            x
        };
        let mut expected = [0.0; 24];
        for (key, &p) in probabilities.iter().enumerate() {
            let x = features(key);
            for i in 0..24 {
                expected[i] += p * x[i];
                for j in 0..24 {
                    hessian[i][j] += p * x[i] * x[j];
                }
            }
        }
        let labelled = features(track.key);
        for i in 0..24 {
            gradient[i] += expected[i] - labelled[i];
            for j in 0..24 {
                hessian[i][j] -= expected[i] * expected[j];
            }
        }
    }
    let n = tracks.len() as f64;
    value = value / n + ridge_penalty(weights);
    for i in 0..24 {
        gradient[i] = gradient[i] / n + 2.0 * RIDGE * weights[i];
        for entry in &mut hessian[i] {
            *entry /= n;
        }
        hessian[i][i] += 2.0 * RIDGE;
    }
    (value, gradient, hessian)
}

/// `x` with `matrix · x = rhs`, for a symmetric positive definite
/// `matrix`, by Cholesky factorization. The objective's Hessian is one:
/// the likelihood's is positive semidefinite, and the ridge adds a
/// positive multiple of the identity.
fn solve_positive_definite(matrix: &[Weights; 24], rhs: Weights) -> Weights {
    // matrix = L · Lᵀ, L lower triangular.
    let mut lower = [[0.0; 24]; 24];
    for i in 0..24 {
        for j in 0..=i {
            let dot: f64 = (0..j).map(|k| lower[i][k] * lower[j][k]).sum();
            lower[i][j] = if i == j {
                (matrix[i][i] - dot).sqrt()
            } else {
                (matrix[i][j] - dot) / lower[j][j]
            };
        }
    }
    // L · y = rhs, then Lᵀ · x = y.
    let mut y = [0.0; 24];
    for i in 0..24 {
        let dot: f64 = (0..i).map(|k| lower[i][k] * y[k]).sum();
        y[i] = (rhs[i] - dot) / lower[i][i];
    }
    let mut x = [0.0; 24];
    for i in (0..24).rev() {
        let dot: f64 = (i + 1..24).map(|k| lower[k][i] * x[k]).sum();
        x[i] = (y[i] - dot) / lower[i][i];
    }
    x
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
        let all: Weights = std::array::from_fn(|i| {
            if i < 12 {
                weights.major[i]
            } else {
                weights.minor[i - 12]
            }
        });
        for track in &learner.tracks {
            let scores = scores(track, &all);
            let best = (0..24)
                .max_by(|&a, &b| scores[a].total_cmp(&scores[b]))
                .expect("24 keys");
            assert_eq!(best, track.key);
        }
    }

    #[test]
    fn the_fit_reaches_the_optimum() {
        let learner = triad_learner();
        let weights = fit(&learner.tracks);
        let (value, gradient, _) = derivatives(&learner.tracks, &weights);
        assert!(gradient.iter().all(|g| g.abs() <= 1e-8), "{gradient:?}");
        // Any small move away does no better.
        for i in 0..24 {
            for delta in [-1e-3, 1e-3] {
                let mut moved = weights;
                moved[i] += delta;
                assert!(objective(&learner.tracks, &moved) >= value);
            }
        }
    }

    #[test]
    fn the_cholesky_solve_inverts_its_matrix() {
        let matrix: [Weights; 24] = std::array::from_fn(|i| {
            std::array::from_fn(|j| {
                if i == j {
                    3.0
                } else {
                    1.0 / (1 + i + j) as f64
                }
            })
        });
        let x: Weights = std::array::from_fn(|i| i as f64 - 11.5);
        let rhs: Weights = std::array::from_fn(|i| (0..24).map(|j| matrix[i][j] * x[j]).sum());
        let solved = solve_positive_definite(&matrix, rhs);
        for i in 0..24 {
            assert!((solved[i] - x[i]).abs() < 1e-9);
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
