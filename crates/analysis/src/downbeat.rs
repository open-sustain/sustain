// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Which beat of the bar a constant grid's bars start on.
//!
//! Music changes at bar lines: chords, bass notes and new parts enter on
//! the downbeat. Each beat's mean band spectrum is compared with the
//! beats around it, and every comparison is pooled by the bar position,
//! of the four, the beat falls on. The positions are then scored by a
//! linear model learned from downbeats people and rekordbox marked
//! (`analysis-bench learn-downbeat-weights`), and the highest scoring
//! position starts the bar.
//!
//! The evidence at each position, [`BarPositionFeatures`]:
//!
//! * per band, the squared change of the beat's level from the previous
//!   beat's — a downbeat changes most, though a fill or a riser on the
//!   beat before it changes a lot too;
//! * the broadband onset at the beat;
//! * the novelty of the beat against two and against four beats: how far
//!   the mean spectrum of the beats from it differs from that of the
//!   beats before it. A section starts where the longer view changes,
//!   which a one-beat fill does not move much;
//! * whether the music starts on the beat: a track's first bar is most
//!   often a whole one, so the first beat that is not near silence
//!   (`MUSIC_START_LEVEL`) tends to be a downbeat.
//!
//! Every feature is read relative to its own spread across the four
//! positions — centered on their mean and divided by their mean absolute
//! deviation — so a track's loudness, density or mix sets no scale: only
//! which positions stand out does.

use sustain_domain::BEATS_PER_BAR;

use crate::beat_tracking::BAND_COUNT;

const BAR: usize = BEATS_PER_BAR as usize;

/// The novelty spans, in beats either side of the beat.
const NOVELTY_SPANS: [usize; 2] = [2, 4];

/// The music starts on the first beat whose level — its band levels
/// summed — has come this share of the way from the quietest beat's to
/// the median beat's.
const MUSIC_START_LEVEL: f32 = 0.25;

/// Features per bar position: one change per band, the onset, one
/// novelty per span, and the music's start.
pub const BAR_FEATURE_COUNT: usize = BAND_COUNT + 1 + NOVELTY_SPANS.len() + 1;

const ONSET_FEATURE: usize = BAND_COUNT;
const NOVELTY_FEATURES: usize = BAND_COUNT + 1;
const MUSIC_START_FEATURE: usize = NOVELTY_FEATURES + NOVELTY_SPANS.len();

/// The downbeat model's weight on each feature, in [`BarPositionFeatures`]
/// order: the 24 bands' changes, low to high, the onset, the two-beat and
/// four-beat novelties, the music's start. Learned by
/// `analysis-bench learn-downbeat-weights` from 1,365 tracks whose grid
/// sits on known beats and bars: 434 GTZAN and 272 Ballroom clips with
/// human annotations, and 659 tracks of the maintainer's library against
/// rekordbox's grids.
const DOWNBEAT_WEIGHTS: [f32; BAR_FEATURE_COUNT] = [
    0.033, 0.130, 0.103, 0.041, 0.067, 0.054, 0.052, 0.019, 0.074, 0.044, 0.050, 0.013, 0.010,
    0.009, -0.002, 0.018, 0.008, -0.040, 0.018, 0.019, 0.031, 0.036, 0.046, 0.078, 0.053, 0.118,
    0.162, 0.200,
];

/// The evidence that a bar starts at each of the four beats from a grid's
/// first beat, index 0 being that beat.
#[derive(Clone, Debug, PartialEq)]
pub struct BarPositionFeatures {
    positions: [[f32; BAR_FEATURE_COUNT]; BAR],
}

impl BarPositionFeatures {
    /// The features of the four positions from `beat_spectra`, each beat's
    /// mean band levels, and `beat_onsets`, each beat's broadband onset,
    /// beat 0 first. `None` when there are too few beats for a novelty at
    /// every position.
    pub(crate) fn of(beat_spectra: &[[f32; BAND_COUNT]], beat_onsets: &[f32]) -> Option<Self> {
        let longest_span = NOVELTY_SPANS[NOVELTY_SPANS.len() - 1];
        if beat_spectra.len() < 2 * longest_span + BAR || beat_onsets.len() != beat_spectra.len() {
            return None;
        }
        let mut pooled = [[PositionMean::default(); BAR_FEATURE_COUNT]; BAR];
        for (beat, pair) in beat_spectra.windows(2).enumerate() {
            let position = &mut pooled[(beat + 1) % BAR];
            for (band, mean) in position.iter_mut().take(BAND_COUNT).enumerate() {
                mean.add(f64::from(pair[1][band] - pair[0][band]).powi(2));
            }
        }
        for (beat, &onset) in beat_onsets.iter().enumerate() {
            pooled[beat % BAR][ONSET_FEATURE].add(f64::from(onset));
        }
        let prefix = prefix_sums(beat_spectra);
        for (index, &span) in NOVELTY_SPANS.iter().enumerate() {
            for beat in span..=beat_spectra.len() - span {
                pooled[beat % BAR][NOVELTY_FEATURES + index].add(novelty(&prefix, beat, span));
            }
        }
        let start = music_start(beat_spectra);
        for (position, mean) in pooled.iter_mut().enumerate() {
            mean[MUSIC_START_FEATURE].add(if position == start % BAR { 1.0 } else { 0.0 });
        }
        let mut positions = [[0.0_f32; BAR_FEATURE_COUNT]; BAR];
        for feature in 0..BAR_FEATURE_COUNT {
            let values: [f64; BAR] =
                std::array::from_fn(|position| pooled[position][feature].mean());
            for (position, value) in relative_to_spread(values).into_iter().enumerate() {
                positions[position][feature] = value as f32;
            }
        }
        Some(Self { positions })
    }

    /// Each position's features, index 0 being the grid's first beat.
    pub fn positions(&self) -> &[[f32; BAR_FEATURE_COUNT]; BAR] {
        &self.positions
    }

    /// The position, 0–3 beats from the grid's first, that the shipped
    /// model scores highest: the bar's first beat. Ties go to the earliest.
    pub fn downbeat(&self) -> usize {
        let score = |position: &[f32; BAR_FEATURE_COUNT]| -> f32 {
            position
                .iter()
                .zip(&DOWNBEAT_WEIGHTS)
                .map(|(feature, weight)| feature * weight)
                .sum()
        };
        (0..BAR).fold(0, |best, position| {
            if score(&self.positions[position]) > score(&self.positions[best]) {
                position
            } else {
                best
            }
        })
    }
}

/// A running mean.
#[derive(Clone, Copy, Debug, Default)]
struct PositionMean {
    sum: f64,
    count: usize,
}

impl PositionMean {
    fn add(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
    }

    fn mean(self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.sum / self.count as f64
        }
    }
}

/// The first beat of `beat_spectra` whose level has come
/// [`MUSIC_START_LEVEL`] of the way from the quietest beat's to the
/// median beat's.
fn music_start(beat_spectra: &[[f32; BAND_COUNT]]) -> usize {
    let levels: Vec<f32> = beat_spectra
        .iter()
        .map(|spectrum| spectrum.iter().sum())
        .collect();
    let mut sorted = levels.clone();
    sorted.sort_by(f32::total_cmp);
    let (Some(&quietest), Some(&median)) = (sorted.first(), sorted.get(sorted.len() / 2)) else {
        return 0;
    };
    let threshold = quietest + MUSIC_START_LEVEL * (median - quietest);
    levels
        .iter()
        .position(|&level| level >= threshold)
        .unwrap_or(0)
}

/// Per-band running sums of `beat_spectra`, entry `i` summing the first
/// `i` beats.
fn prefix_sums(beat_spectra: &[[f32; BAND_COUNT]]) -> Vec<[f64; BAND_COUNT]> {
    let mut prefix = Vec::with_capacity(beat_spectra.len() + 1);
    prefix.push([0.0; BAND_COUNT]);
    for spectrum in beat_spectra {
        let last = prefix[prefix.len() - 1];
        prefix.push(std::array::from_fn(|band| {
            last[band] + f64::from(spectrum[band])
        }));
    }
    prefix
}

/// The squared distance between the mean spectrum of the `span` beats
/// from `beat` and that of the `span` beats before it.
fn novelty(prefix: &[[f64; BAND_COUNT]], beat: usize, span: usize) -> f64 {
    (0..BAND_COUNT)
        .map(|band| {
            let after = prefix[beat + span][band] - prefix[beat][band];
            let before = prefix[beat][band] - prefix[beat - span][band];
            ((after - before) / span as f64).powi(2)
        })
        .sum()
}

/// `values` centered on their mean and divided by their mean absolute
/// deviation; all zero when they do not vary.
fn relative_to_spread(values: [f64; BAR]) -> [f64; BAR] {
    let mean = values.iter().sum::<f64>() / BAR as f64;
    let centered = values.map(|value| value - mean);
    let spread = centered.iter().map(|value| value.abs()).sum::<f64>() / BAR as f64;
    if spread > f64::EPSILON * mean.abs().max(1.0) {
        centered.map(|value| value / spread)
    } else {
        [0.0; BAR]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn features_are_relative_to_their_spread() {
        assert_eq!(
            relative_to_spread([1.0, 3.0, 1.0, 3.0]),
            [-1.0, 1.0, -1.0, 1.0]
        );
        assert_eq!(relative_to_spread([2.0; BAR]), [0.0; BAR]);
        // Scaling a track scales nothing it is read by.
        assert_eq!(
            relative_to_spread([0.5, 2.0, 1.0, 0.5]),
            relative_to_spread([5.0, 20.0, 10.0, 5.0])
        );
    }

    #[test]
    fn novelty_compares_the_beats_after_with_the_beats_before() {
        let spectra: Vec<[f32; BAND_COUNT]> = (0..8)
            .map(|beat| [if beat < 4 { 0.0 } else { 2.0 }; BAND_COUNT])
            .collect();
        let prefix = prefix_sums(&spectra);
        assert_eq!(novelty(&prefix, 4, 4), 4.0 * BAND_COUNT as f64);
        assert_eq!(novelty(&prefix, 4, 2), 4.0 * BAND_COUNT as f64);
        assert_eq!(novelty(&prefix, 2, 2), 0.0);
    }

    #[test]
    fn a_change_every_bar_marks_its_position() {
        // The spectrum steps every fourth beat from beat 1.
        let spectra: Vec<[f32; BAND_COUNT]> = (0..32)
            .map(|beat: usize| [((beat + 3) / BAR % 2) as f32; BAND_COUNT])
            .collect();
        let features = BarPositionFeatures::of(&spectra, &[1.0; 32]).expect("features");
        let positions = features.positions();
        for feature in [0, NOVELTY_FEATURES, NOVELTY_FEATURES + 1] {
            let strongest = (0..BAR)
                .max_by(|&a, &b| positions[a][feature].total_cmp(&positions[b][feature]))
                .expect("four positions");
            assert_eq!(strongest, 1, "feature {feature}");
        }
        // A steady onset says nothing.
        assert!(
            positions
                .iter()
                .all(|position| position[ONSET_FEATURE] == 0.0)
        );
    }

    #[test]
    fn the_music_starts_after_the_silence() {
        let spectra: Vec<[f32; BAND_COUNT]> = (0..24)
            .map(|beat| {
                [if beat < 3 {
                    0.0
                } else {
                    1.0 + (beat % 2) as f32
                }; BAND_COUNT]
            })
            .collect();
        assert_eq!(music_start(&spectra), 3);
        let features = BarPositionFeatures::of(&spectra, &[1.0; 24]).expect("features");
        let start = features
            .positions()
            .map(|position| position[MUSIC_START_FEATURE]);
        assert_eq!(start, [-2.0 / 3.0, -2.0 / 3.0, -2.0 / 3.0, 2.0]);
    }

    #[test]
    fn too_few_beats_give_no_features() {
        let spectra = [[0.0; BAND_COUNT]; 2 * 4 + BAR - 1];
        assert_eq!(BarPositionFeatures::of(&spectra, &[0.0; 11]), None);
    }
}
