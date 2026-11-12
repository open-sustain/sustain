// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Learn the downbeat model's weights from beat grids whose bars are known.
//!
//! The analyzer scores each of the four beats a bar can start on by
//! weighing that position's evidence ([`BarPositionFeatures`]) and starts
//! the bar on the highest. The weights are fitted as a multinomial
//! logistic regression over the four positions
//! ([`crate::softmax_regression`]), on every track whose grid the analyzer
//! places on the ground truth's beats and whose ground truth holds steady
//! bars: annotated beats a person marked, or a reference grid.
//!
//! Corpora differ — dance music's bars announce themselves differently
//! from a ballroom recording's or a blues clip's — so beside the weights
//! learned from every corpus, `analysis-bench learn-downbeat-weights`
//! reports how weights learned from all but one corpus do on the one left
//! out: the accuracy to expect on music none of them hold.

use std::time::Duration;

use sustain_analysis::{Analyzer, BAR_FEATURE_COUNT, BarPositionFeatures};

use crate::manifest::Manifest;
use crate::metrics::{self, GridAgreement};
use crate::softmax_regression::{self, Choice};

/// The ridge penalty on the squared weights, beside the mean negative log
/// likelihood of the known downbeats. Unlike the key model's, it is
/// strong enough to matter: each corpus holds a few hundred scored tracks
/// against [`BAR_FEATURE_COUNT`] weights, and held out by corpus, weaker
/// ridges fit each corpus's own habits at the others' expense.
const RIDGE: f64 = 0.5;

/// The scored tracks of one manifest.
#[derive(Clone, Debug, Default)]
pub struct CorpusExamples {
    /// The manifest's corpus id.
    pub corpus_id: String,
    /// Each scored track's four positions, the known downbeat chosen.
    pub choices: Vec<Choice>,
    /// Scored tracks whose downbeat the shipped weights already pick.
    pub shipped_right: usize,
    /// Tracks that taught nothing: no file, no grid ground truth, no grid,
    /// a grid off the ground truth's beats, or bars that do not hold.
    pub skipped: usize,
}

impl CorpusExamples {
    /// The share of scored tracks the shipped weights get right.
    pub fn shipped_accuracy(&self) -> f64 {
        self.shipped_right as f64 / self.choices.len().max(1) as f64
    }

    /// Count one track: its bar evidence, and the downbeat the ground
    /// truth puts `downbeat` beats after the grid's first beat.
    pub fn add(&mut self, bars: &BarPositionFeatures, downbeat: usize) {
        if bars.downbeat() == downbeat {
            self.shipped_right += 1;
        }
        let candidates = bars
            .positions()
            .iter()
            .map(|features| features.iter().map(|&f| f64::from(f)).collect())
            .collect();
        self.choices.push(Choice {
            candidates,
            chosen: downbeat,
        });
    }
}

/// Analyze every track of `manifest` that has a file and grid ground
/// truth. `progress` is told each track's id as it is analysed.
pub fn collect(manifest: &Manifest, mut progress: impl FnMut(&str)) -> CorpusExamples {
    let options = manifest.options.analysis_options();
    let mut corpus = CorpusExamples {
        corpus_id: manifest.meta.corpus_id.clone(),
        ..CorpusExamples::default()
    };
    for entry in &manifest.tracks {
        progress(&entry.id);
        let duration = entry
            .duration_secs
            .filter(|secs| secs.is_finite() && *secs > 0.0)
            .map(Duration::from_secs_f64);
        let known = entry.path.as_ref().zip(entry.grid_reference(duration));
        let scored = known.and_then(|(path, reference)| {
            let (grid, bars) = Analyzer::new(path.clone(), options, duration).bar_positions()?;
            // Numbered 1 on its first beat, the grid's bar starts `offset`
            // beats after the ground truth's downbeat, which therefore
            // falls `offset` beats before the first beat: `4 - offset`
            // after it, bars being four beats long.
            let predicted = metrics::Grid {
                bpm: grid.tempo().as_f64(),
                first_beat_secs: grid.first_beat().as_secs_f64(),
                first_beat_number: 1,
            };
            let comparison = reference.compare(predicted);
            let offset = comparison
                .downbeat_offset
                .filter(|_| comparison.agreement == GridAgreement::Aligned)?;
            Some((bars, (4 - usize::from(offset)) % 4))
        });
        match scored {
            Some((bars, downbeat)) => corpus.add(&bars, downbeat),
            None => corpus.skipped += 1,
        }
    }
    corpus
}

/// The weights learned from every corpus, and how each corpus fares under
/// weights learned from the others.
#[derive(Clone, Debug, PartialEq)]
pub struct DownbeatWeights {
    pub weights: Vec<f64>,
    /// Scored tracks the weights were learned from.
    pub tracks: usize,
    /// Per corpus, in the order given: its id and the accuracy of weights
    /// learned from the other corpora. Empty with a single corpus.
    pub held_out: Vec<(String, f64)>,
}

/// Learn the weights from `corpora`. `None` without a scored track.
pub fn learn(corpora: &[CorpusExamples]) -> Option<DownbeatWeights> {
    let all: Vec<Choice> = corpora
        .iter()
        .flat_map(|corpus| corpus.choices.iter().cloned())
        .collect();
    if all.is_empty() {
        return None;
    }
    let held_out = if corpora.len() < 2 {
        Vec::new()
    } else {
        corpora
            .iter()
            .enumerate()
            .map(|(index, corpus)| {
                let others: Vec<Choice> = corpora
                    .iter()
                    .enumerate()
                    .filter(|&(other, _)| other != index)
                    .flat_map(|(_, other)| other.choices.iter().cloned())
                    .collect();
                let accuracy = if others.is_empty() {
                    0.0
                } else {
                    let weights = softmax_regression::fit(&others, BAR_FEATURE_COUNT, RIDGE);
                    softmax_regression::accuracy(&corpus.choices, &weights)
                };
                (corpus.corpus_id.clone(), accuracy)
            })
            .collect()
    };
    Some(DownbeatWeights {
        weights: softmax_regression::fit(&all, BAR_FEATURE_COUNT, RIDGE),
        tracks: all.len(),
        held_out,
    })
}

impl DownbeatWeights {
    /// The weights as the Rust array the downbeat model holds.
    pub fn to_rust(&self) -> String {
        let values: Vec<String> = self.weights.iter().map(|w| format!("{w:.3}")).collect();
        format!(
            "// Learned from {} track(s).\n\
             const DOWNBEAT_WEIGHTS: [f32; BAR_FEATURE_COUNT] = [{}];\n",
            self.tracks,
            values.join(", "),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A corpus whose downbeat is the position with the largest first
    /// feature.
    fn corpus(id: &str, shift: usize) -> CorpusExamples {
        let choices = (0..40)
            .map(|example| {
                let chosen = (example + shift) % 4;
                let candidates = (0..4)
                    .map(|position| {
                        let mut x = vec![0.0; BAR_FEATURE_COUNT];
                        x[0] = if position == chosen { 1.0 } else { -1.0 / 3.0 };
                        x[1] = ((example * 5 + position) % 3) as f64 - 1.0;
                        x
                    })
                    .collect();
                Choice { candidates, chosen }
            })
            .collect();
        CorpusExamples {
            corpus_id: id.to_string(),
            choices,
            ..CorpusExamples::default()
        }
    }

    #[test]
    fn each_corpus_is_scored_by_weights_learned_from_the_others() {
        let learned = learn(&[corpus("a", 0), corpus("b", 1)]).expect("tracks");
        assert_eq!(learned.tracks, 80);
        assert_eq!(
            learned.held_out,
            vec![("a".to_string(), 1.0), ("b".to_string(), 1.0)]
        );
        assert!(learned.weights[0] > 0.0);
    }

    #[test]
    fn one_corpus_has_no_held_out_score() {
        let learned = learn(&[corpus("a", 0)]).expect("tracks");
        assert!(learned.held_out.is_empty());
        assert_eq!(learn(&[CorpusExamples::default()]), None);
    }

    #[test]
    fn weights_print_as_the_model_array() {
        let learned = DownbeatWeights {
            weights: vec![0.5, -1.25],
            tracks: 7,
            held_out: Vec::new(),
        };
        let rust = learned.to_rust();
        assert!(rust.contains("// Learned from 7 track(s)."));
        assert!(rust.contains("= [0.500, -1.250];"));
    }
}
