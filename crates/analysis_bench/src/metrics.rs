// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Scoring metrics for analyzer outputs against ground truth.
//!
//! These are a faithful Rust port of the stratum-dsp validation suite's
//! `_metrics.py` and `_keys.py` — the same definitions the fork's quality
//! numbers were measured with, so Sustain's baselines are comparable to
//! that prior work:
//!
//! * **BPM** — absolute error, a ±tolerance hit/miss (default ±2 BPM), and
//!   the *metrical ratio bucket* that exposes octave/harmonic confusions
//!   (`2x`, `1/2x`, `3/2x`, …) rather than scoring them as plain misses.
//! * **Key** — MIREX-style categories with the standard weights
//!   (correct 1.0, fifth 0.5, relative 0.3, parallel 0.2, other 0.0), on
//!   pitch classes so enharmonic spellings never matter.
//!
//! Beat grids are Sustain's own: a constant grid is compared either with
//! another constant grid ([`compare_grids`]) or with beats a person marked
//! ([`compare_with_beats`]), for its phase and the bar position of its
//! downbeats.

use serde::{Deserialize, Serialize};
use sustain_domain::MusicalKey;

/// A key reduced to a pitch class (0 = C … 11 = B) plus mode. MIREX
/// scoring works on pitch classes, so enharmonic spellings (Db/C#) and
/// notation differences collapse away.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeyClass {
    /// Tonic pitch class, 0 = C … 11 = B.
    pub tonic: u8,
    /// `true` for a major key, `false` for minor.
    pub major: bool,
}

impl From<MusicalKey> for KeyClass {
    fn from(key: MusicalKey) -> Self {
        // `MusicalKey` is declared chromatically — majors C..B at
        // discriminants 0..11, minors Cm..Bm at 12..23 — so the
        // discriminant mod 12 is exactly the tonic pitch class.
        Self {
            tonic: (key as u8) % 12,
            major: key.is_major(),
        }
    }
}

/// Parse a ground-truth key string into a [`KeyClass`]. Accepts Sustain's
/// `short_code` forms (`C`, `Am`, `C#m`, `Ebm`), verbose forms
/// (`C major`, `F# minor`), flats/sharps including Unicode accidentals
/// (♭/♯), and `maj`/`min` shorthand. Returns `None` for anything that is
/// not a recognizable Western key.
pub fn parse_key(raw: &str) -> Option<KeyClass> {
    let s = raw.trim().replace('\u{266d}', "b").replace('\u{266f}', "#");
    if s.is_empty() {
        return None;
    }
    let lower = s.to_ascii_lowercase();

    let is_minor = lower.contains("minor")
        || lower.ends_with("min")
        || (lower.ends_with('m') && !lower.contains("maj"));

    // Strip the mode descriptor to isolate the note token. Longest
    // suffixes first so "minor" is not mistaken for a trailing "m".
    let mut note = lower.as_str();
    for suffix in [
        " minor", "minor", " min", "min", " major", "major", " maj", "maj", "m",
    ] {
        if let Some(stripped) = note.strip_suffix(suffix) {
            note = stripped;
            break;
        }
    }
    let note: String = note.chars().filter(|c| !c.is_whitespace()).collect();

    let mut chars = note.chars();
    let letter = chars.next()?;
    let base = match letter {
        'c' => 0_i16,
        'd' => 2,
        'e' => 4,
        'f' => 5,
        'g' => 7,
        'a' => 9,
        'b' => 11,
        _ => return None,
    };
    let accidental = match chars.next() {
        Some('#') => 1,
        Some('b') => -1,
        None => 0,
        Some(_) => return None,
    };
    // No trailing garbage after note + optional accidental.
    if chars.next().is_some() {
        return None;
    }

    let tonic = (base + accidental).rem_euclid(12) as u8;
    Some(KeyClass {
        tonic,
        major: !is_minor,
    })
}

/// MIREX-style key agreement category between a prediction and a
/// reference. Carries the standard weighted scores.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeyCategory {
    /// Exact tonic + mode match.
    Correct,
    /// Same mode, a perfect fifth away (ascending or descending).
    Fifth,
    /// Relative major/minor (e.g. C major ↔ A minor).
    Relative,
    /// Parallel major/minor (same tonic, opposite mode).
    Parallel,
    /// Any other disagreement.
    Other,
}

impl KeyCategory {
    /// MIREX weighted score for this category.
    pub fn score(self) -> f32 {
        match self {
            KeyCategory::Correct => 1.0,
            KeyCategory::Fifth => 0.5,
            KeyCategory::Relative => 0.3,
            KeyCategory::Parallel => 0.2,
            KeyCategory::Other => 0.0,
        }
    }
}

/// The relative key's tonic: a major key's relative minor is a minor
/// third down (−3), a minor key's relative major is a minor third up
/// (+3).
fn relative_tonic(key: KeyClass) -> u8 {
    let shift = if key.major { -3 } else { 3 };
    (i16::from(key.tonic) + shift).rem_euclid(12) as u8
}

/// Score a predicted key against a reference key with MIREX categories.
pub fn evaluate_key(predicted: KeyClass, reference: KeyClass) -> KeyCategory {
    if predicted == reference {
        return KeyCategory::Correct;
    }
    let same_mode = predicted.major == reference.major;
    let up_fifth = (reference.tonic + 7) % 12;
    let down_fifth = (reference.tonic + 5) % 12; // −7 mod 12
    if same_mode && (predicted.tonic == up_fifth || predicted.tonic == down_fifth) {
        return KeyCategory::Fifth;
    }
    if !same_mode && predicted.tonic == relative_tonic(reference) {
        return KeyCategory::Relative;
    }
    if !same_mode && predicted.tonic == reference.tonic {
        return KeyCategory::Parallel;
    }
    KeyCategory::Other
}

/// Default BPM tolerance for the ±-accuracy metric, in BPM.
pub const DEFAULT_BPM_TOLERANCE: f64 = 2.0;

/// Tolerance for matching a tempo ratio to a metrical factor.
const RATIO_BUCKET_TOLERANCE: f64 = 0.08;

/// Metrical factors a predicted/reference tempo ratio is bucketed into,
/// in priority order. Mirrors the fork's `TEMPO_RATIO_FACTORS`.
const RATIO_FACTORS: [(&str, f64); 7] = [
    ("1x", 1.0),
    ("2x", 2.0),
    ("1/2x", 0.5),
    ("3/2x", 1.5),
    ("2/3x", 2.0 / 3.0),
    ("4/3x", 4.0 / 3.0),
    ("3/4x", 3.0 / 4.0),
];

/// Absolute BPM error, or `None` if either value is non-finite.
pub fn bpm_absolute_error(predicted: f64, reference: f64) -> Option<f64> {
    (predicted.is_finite() && reference.is_finite()).then(|| (predicted - reference).abs())
}

/// Whether the prediction is within `tolerance` BPM of the reference.
pub fn bpm_within(predicted: f64, reference: f64, tolerance: f64) -> bool {
    bpm_absolute_error(predicted, reference).is_some_and(|err| err <= tolerance)
}

/// Predicted ÷ reference tempo ratio, or `None` unless both are finite
/// and positive.
pub fn tempo_ratio(predicted: f64, reference: f64) -> Option<f64> {
    (predicted.is_finite() && reference.is_finite() && predicted > 0.0 && reference > 0.0)
        .then_some(predicted / reference)
}

/// Bucket the tempo ratio into a metrical factor label (`1x`, `2x`,
/// `1/2x`, …), `other` if it matches none, or `n/a` if the ratio is
/// undefined.
pub fn tempo_ratio_bucket(predicted: f64, reference: f64) -> &'static str {
    match tempo_ratio(predicted, reference) {
        None => "n/a",
        Some(ratio) => RATIO_FACTORS
            .iter()
            .find(|(_, factor)| (ratio - factor).abs() <= RATIO_BUCKET_TOLERANCE)
            .map_or("other", |(label, _)| label),
    }
}

/// A constant beat grid as the bench compares it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Grid {
    /// Tempo the beats are laid out at.
    pub bpm: f64,
    /// Time of one beat, in seconds.
    pub first_beat_secs: f64,
    /// That beat's position in its bar, 1–4.
    pub first_beat_number: u8,
}

impl Grid {
    fn period(&self) -> f64 {
        60.0 / self.bpm
    }

    /// Index (from the first beat) of the beat nearest `time`.
    fn nearest_beat(&self, time: f64) -> i64 {
        ((time - self.first_beat_secs) / self.period()).round() as i64
    }

    fn beat_time(&self, index: i64) -> f64 {
        self.first_beat_secs + index as f64 * self.period()
    }

    fn beat_number(&self, index: i64) -> u8 {
        (i64::from(self.first_beat_number) - 1 + index).rem_euclid(4) as u8 + 1
    }
}

/// Phase errors at or under this many milliseconds count as on the beat:
/// below the ~20 ms at which two percussive hits stop sounding as one.
pub const GRID_ALIGNED_MS: f64 = 20.0;
/// Tempos further apart than this ratio are a tempo error, not a phase
/// one (0.1 %, about 0.13 BPM at 128 BPM).
pub const GRID_TEMPO_TOLERANCE: f64 = 0.001;

/// How a predicted grid relates to the reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GridAgreement {
    /// Same tempo, on the reference's beats.
    Aligned,
    /// Same tempo, but locked half a beat away (on the off-beats).
    OffBeat,
    /// Same tempo, phase neither aligned nor off-beat.
    Misaligned,
    /// A different tempo: phase is meaningless.
    TempoMismatch,
    /// The annotated beats follow no constant tempo (a live performance
    /// speeding up, a rubato), so no constant grid can be judged against
    /// them.
    Unsteady,
}

/// The outcome of comparing a predicted grid with the reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridComparison {
    /// The verdict.
    pub agreement: GridAgreement,
    /// Signed phase error, in milliseconds (positive: predicted beats come
    /// late), when the tempos match.
    pub phase_error_ms: Option<f64>,
    /// For aligned grids, how many beats after the reference's downbeat
    /// the predicted bar starts: 0 when the bars agree, 3 when the
    /// predicted bar starts a beat early. `None` when the reference's own
    /// bars are not steady on the grid.
    pub downbeat_offset: Option<u8>,
}

impl GridComparison {
    fn tempo_mismatch() -> Self {
        Self {
            agreement: GridAgreement::TempoMismatch,
            phase_error_ms: None,
            downbeat_offset: None,
        }
    }
}

/// How many beats after the reference's downbeat a grid's bar starts,
/// from the two bar positions of one beat (each 1–4).
fn downbeat_offset(reference_position: u8, predicted_position: u8) -> u8 {
    (i16::from(reference_position) - i16::from(predicted_position)).rem_euclid(4) as u8
}

/// Compare `predicted` with `reference` at each probe time (seconds):
/// for every probe, the reference beat nearest it against the predicted
/// beat nearest that.
/// Aligned means every probed beat within [`GRID_ALIGNED_MS`].
pub fn compare_grids(predicted: Grid, reference: Grid, probes: &[f64]) -> GridComparison {
    if (predicted.bpm / reference.bpm - 1.0).abs() > GRID_TEMPO_TOLERANCE || probes.is_empty() {
        return GridComparison::tempo_mismatch();
    }
    let errors: Vec<(f64, i64, i64)> = probes
        .iter()
        .map(|&probe| {
            let reference_index = reference.nearest_beat(probe);
            let beat = reference.beat_time(reference_index);
            let predicted_index = predicted.nearest_beat(beat);
            let error_ms = (predicted.beat_time(predicted_index) - beat) * 1_000.0;
            (error_ms, reference_index, predicted_index)
        })
        .collect();
    let (middle_error, reference_index, predicted_index) = errors[errors.len() / 2];
    let half_beat_ms = reference.period() * 500.0;
    let agreement = if errors
        .iter()
        .all(|(error, _, _)| error.abs() <= GRID_ALIGNED_MS)
    {
        GridAgreement::Aligned
    } else if (middle_error.abs() - half_beat_ms).abs() <= GRID_ALIGNED_MS {
        GridAgreement::OffBeat
    } else {
        GridAgreement::Misaligned
    };
    GridComparison {
        agreement,
        phase_error_ms: Some(middle_error),
        downbeat_offset: (agreement == GridAgreement::Aligned).then(|| {
            downbeat_offset(
                reference.beat_number(reference_index),
                predicted.beat_number(predicted_index),
            )
        }),
    }
}

/// A beat a person marked: its time, and its position in the bar, 1 (the
/// downbeat) to 4. Written `[secs, position]` in a manifest.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(from = "(f64, u8)", into = "(f64, u8)")]
pub struct AnnotatedBeat {
    /// Time of the beat, in seconds.
    pub secs: f64,
    /// Its position in the bar, 1–4.
    pub position: u8,
}

impl From<(f64, u8)> for AnnotatedBeat {
    fn from((secs, position): (f64, u8)) -> Self {
        Self { secs, position }
    }
}

impl From<AnnotatedBeat> for (f64, u8) {
    fn from(beat: AnnotatedBeat) -> Self {
        (beat.secs, beat.position)
    }
}

/// A grid whose period is further than this ratio from the annotated
/// beats' median interval has another tempo (4 %: the tolerance of the
/// standard "Acc1" tempo accuracy).
pub const ANNOTATED_TEMPO_TOLERANCE: f64 = 0.04;
/// Annotated beats whose phases against the grid cohere less than this
/// (their mean resultant length, 1 when every beat sits at the same
/// phase) follow no constant tempo. 0.9 allows a spread of about 0.07 of
/// a beat, 35 ms at 120 BPM: a person's tapping, not a tempo change.
pub const ANNOTATED_MIN_COHERENCE: f64 = 0.9;
/// Within this fraction of a beat of the annotated beats is aligned, and
/// within it of the off-beat is off-beat: the eighth of a beat either side
/// that belongs to the beat rather than to the quarter-beat between.
pub const ANNOTATED_PHASE_REACH: f64 = 0.125;
/// The bars agree with the grid's when at least this share of the
/// annotated beats give the same bar offset.
pub const ANNOTATED_BAR_AGREEMENT: f64 = 0.75;

/// Compare `predicted` with beats a person marked. The tempo must match
/// the beats' median interval; the phase is the beats' mean phase against
/// the grid, so a person's timing averages out; the downbeat is the bar
/// offset most annotated beats agree on.
pub fn compare_with_beats(predicted: Grid, beats: &[AnnotatedBeat]) -> GridComparison {
    let mut intervals: Vec<f64> = beats
        .windows(2)
        .map(|pair| pair[1].secs - pair[0].secs)
        .collect();
    intervals.sort_by(f64::total_cmp);
    let period = predicted.period();
    let Some(&median) = intervals.get(intervals.len() / 2) else {
        return GridComparison::tempo_mismatch();
    };
    if (period / median - 1.0).abs() > ANNOTATED_TEMPO_TOLERANCE {
        return GridComparison::tempo_mismatch();
    }
    let (sin, cos) = beats.iter().fold((0.0, 0.0), |(sin, cos), beat| {
        let angle = std::f64::consts::TAU * (beat.secs - predicted.first_beat_secs) / period;
        (sin + angle.sin(), cos + angle.cos())
    });
    let coherence = sin.hypot(cos) / beats.len() as f64;
    if coherence < ANNOTATED_MIN_COHERENCE {
        return GridComparison {
            agreement: GridAgreement::Unsteady,
            phase_error_ms: None,
            downbeat_offset: None,
        };
    }
    // Where the annotated beats sit after the grid's, in beats.
    let lag = sin.atan2(cos) / std::f64::consts::TAU;
    let agreement = if lag.abs() <= ANNOTATED_PHASE_REACH {
        GridAgreement::Aligned
    } else if lag.abs() >= 0.5 - ANNOTATED_PHASE_REACH {
        GridAgreement::OffBeat
    } else {
        GridAgreement::Misaligned
    };
    let downbeat_offset = (agreement == GridAgreement::Aligned)
        .then(|| {
            let mut votes = [0_usize; 4];
            for beat in beats {
                let position = predicted.beat_number(predicted.nearest_beat(beat.secs));
                votes[usize::from(downbeat_offset(beat.position, position))] += 1;
            }
            let (offset, &count) = votes
                .iter()
                .enumerate()
                .max_by_key(|&(_, count)| count)
                .expect("four offsets");
            (count as f64 >= ANNOTATED_BAR_AGREEMENT * beats.len() as f64).then_some(offset as u8)
        })
        .flatten();
    GridComparison {
        agreement,
        phase_error_ms: Some(-lag * period * 1_000.0),
        downbeat_offset,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AnnotatedBeat, Grid, GridAgreement, KeyCategory, KeyClass, compare_grids,
        compare_with_beats, evaluate_key, parse_key, tempo_ratio_bucket,
    };
    use sustain_domain::MusicalKey;

    fn key(tonic: u8, major: bool) -> KeyClass {
        KeyClass { tonic, major }
    }

    #[test]
    fn musical_key_maps_to_pitch_class() {
        assert_eq!(KeyClass::from(MusicalKey::CMajor), key(0, true));
        assert_eq!(KeyClass::from(MusicalKey::AMinor), key(9, false));
        assert_eq!(KeyClass::from(MusicalKey::DbMajor), key(1, true));
        assert_eq!(KeyClass::from(MusicalKey::FsMinor), key(6, false));
    }

    #[test]
    fn parse_key_handles_short_codes_and_verbose_forms() {
        assert_eq!(parse_key("C major"), Some(key(0, true)));
        assert_eq!(parse_key("Am"), Some(key(9, false)));
        assert_eq!(parse_key("c minor"), Some(key(0, false)));
        assert_eq!(parse_key("Ebm"), Some(key(3, false)));
        assert_eq!(parse_key("F# minor"), Some(key(6, false)));
        assert_eq!(parse_key("Bb"), Some(key(10, true)));
        // Enharmonic spellings collapse to the same pitch class.
        assert_eq!(parse_key("Db"), parse_key("C#"));
    }

    #[test]
    fn parse_key_rejects_nonsense() {
        assert_eq!(parse_key(""), None);
        assert_eq!(parse_key("H major"), None);
        assert_eq!(parse_key("C#x"), None);
    }

    #[test]
    fn mirex_categories_follow_the_circle_of_fifths() {
        let c_major = key(0, true);
        assert_eq!(evaluate_key(c_major, c_major), KeyCategory::Correct);
        // G major is a fifth above C; F major a fifth below.
        assert_eq!(evaluate_key(key(7, true), c_major), KeyCategory::Fifth);
        assert_eq!(evaluate_key(key(5, true), c_major), KeyCategory::Fifth);
        // A minor is C major's relative.
        assert_eq!(evaluate_key(key(9, false), c_major), KeyCategory::Relative);
        // C minor is C major's parallel.
        assert_eq!(evaluate_key(key(0, false), c_major), KeyCategory::Parallel);
        // D major is unrelated.
        assert_eq!(evaluate_key(key(2, true), c_major), KeyCategory::Other);
    }

    #[test]
    fn tempo_ratio_buckets_catch_metrical_errors() {
        assert_eq!(tempo_ratio_bucket(120.0, 120.0), "1x");
        assert_eq!(tempo_ratio_bucket(240.0, 120.0), "2x");
        assert_eq!(tempo_ratio_bucket(60.0, 120.0), "1/2x");
        assert_eq!(tempo_ratio_bucket(180.0, 120.0), "3/2x");
        assert_eq!(tempo_ratio_bucket(160.0, 120.0), "4/3x");
        assert_eq!(tempo_ratio_bucket(90.0, 120.0), "3/4x");
        // 100/120 ≈ 0.833 matches no metrical factor within tolerance.
        assert_eq!(tempo_ratio_bucket(100.0, 120.0), "other");
        assert_eq!(tempo_ratio_bucket(0.0, 120.0), "n/a");
    }

    fn grid(bpm: f64, first_beat_secs: f64, first_beat_number: u8) -> Grid {
        Grid {
            bpm,
            first_beat_secs,
            first_beat_number,
        }
    }

    #[test]
    fn grids_compare_by_phase_and_bar_position() {
        let reference = grid(120.0, 0.2, 1);
        let probes = [10.0, 60.0, 110.0];
        // Ten milliseconds late, same bars — written from another beat.
        let late = compare_grids(grid(120.0, 0.21 + 2.0, 1), reference, &probes);
        assert_eq!(late.agreement, GridAgreement::Aligned);
        assert!((late.phase_error_ms.expect("error") - 10.0).abs() < 1e-6);
        assert_eq!(late.downbeat_offset, Some(0));
        // One beat later, still numbered 1: aligned, the bar a beat late.
        let shifted = compare_grids(grid(120.0, 0.7, 1), reference, &probes);
        assert_eq!(shifted.agreement, GridAgreement::Aligned);
        assert_eq!(shifted.downbeat_offset, Some(1));
        // Numbered 2 on the reference's downbeat: the bar a beat early.
        let early = compare_grids(grid(120.0, 0.2, 2), reference, &probes);
        assert_eq!(early.downbeat_offset, Some(3));
        // Half a beat over.
        let off_beat = compare_grids(grid(120.0, 0.45, 1), reference, &probes);
        assert_eq!(off_beat.agreement, GridAgreement::OffBeat);
        assert_eq!(off_beat.downbeat_offset, None);
        // A tempo 1 % off is not a phase question.
        let tempo = compare_grids(grid(121.2, 0.2, 1), reference, &probes);
        assert_eq!(tempo.agreement, GridAgreement::TempoMismatch);
        assert_eq!(tempo.phase_error_ms, None);
    }

    /// Annotated beats at `bpm` from `first_secs`, the first at bar
    /// position `first_position`, each nudged by the matching `jitter`
    /// (seconds, cycled) as a person's tapping would be.
    fn tapped(bpm: f64, first_secs: f64, first_position: u8, count: usize) -> Vec<AnnotatedBeat> {
        let jitter = [0.012, -0.008, 0.0, -0.015, 0.006];
        (0..count)
            .map(|index| AnnotatedBeat {
                secs: first_secs + index as f64 * 60.0 / bpm + jitter[index % jitter.len()],
                position: ((usize::from(first_position) - 1 + index) % 4 + 1) as u8,
            })
            .collect()
    }

    #[test]
    fn annotated_beats_judge_phase_and_bars() {
        let beats = tapped(120.0, 0.5, 1, 60);
        let aligned = compare_with_beats(grid(120.0, 0.51, 1), &beats);
        assert_eq!(aligned.agreement, GridAgreement::Aligned);
        assert_eq!(aligned.downbeat_offset, Some(0));
        let error = aligned.phase_error_ms.expect("error");
        assert!((error - 10.0).abs() < 5.0, "{error}");
        // The grid's bar starts on the annotated beat 4: a beat early.
        let early = compare_with_beats(grid(120.0, 0.5 + 1.5, 1), &beats);
        assert_eq!(early.downbeat_offset, Some(3));
        let off_beat = compare_with_beats(grid(120.0, 0.75, 1), &beats);
        assert_eq!(off_beat.agreement, GridAgreement::OffBeat);
        assert_eq!(off_beat.downbeat_offset, None);
        let quarter = compare_with_beats(grid(120.0, 0.625, 1), &beats);
        assert_eq!(quarter.agreement, GridAgreement::Misaligned);
        // 5 % slower than the beats' tempo.
        let slower = compare_with_beats(grid(114.0, 0.5, 1), &beats);
        assert_eq!(slower.agreement, GridAgreement::TempoMismatch);
    }

    #[test]
    fn annotated_beats_that_drift_are_unsteady() {
        // Thirty beats at 120 BPM, then thirty at 126: no constant grid
        // holds both halves.
        let mut beats = tapped(120.0, 0.5, 1, 30);
        let resume = beats[29].secs + 60.0 / 126.0;
        beats.extend(tapped(126.0, resume, 3, 30));
        let comparison = compare_with_beats(grid(123.0, 0.5, 1), &beats);
        assert_eq!(comparison.agreement, GridAgreement::Unsteady);
    }

    #[test]
    fn bars_that_do_not_hold_on_the_grid_give_no_downbeat() {
        // A bar of two beats halfway through shifts every later bar.
        let mut beats = tapped(120.0, 0.5, 1, 32);
        for beat in &mut beats[16..] {
            beat.position = (beat.position + 1) % 4 + 1;
        }
        let comparison = compare_with_beats(grid(120.0, 0.5, 1), &beats);
        assert_eq!(comparison.agreement, GridAgreement::Aligned);
        assert_eq!(comparison.downbeat_offset, None);
    }
}
