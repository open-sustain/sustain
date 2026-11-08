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
    /// Same tempo; every probed beat within [`GRID_ALIGNED_MS`].
    Aligned,
    /// Same tempo, but locked half a beat away (on the off-beats).
    OffBeat,
    /// Same tempo, phase neither aligned nor off-beat.
    Misaligned,
    /// A different tempo: phase is meaningless.
    TempoMismatch,
}

/// The outcome of comparing a predicted grid with the reference.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GridComparison {
    /// The verdict.
    pub agreement: GridAgreement,
    /// Signed phase error at the middle probe, in milliseconds (positive:
    /// predicted beats come late), when the tempos match.
    pub phase_error_ms: Option<f64>,
    /// Whether the predicted bar position agrees, for aligned grids.
    pub downbeat_correct: Option<bool>,
}

/// Compare `predicted` with `reference` at each probe time (seconds):
/// for every probe, the reference beat nearest it against the predicted
/// beat nearest that.
pub fn compare_grids(predicted: Grid, reference: Grid, probes: &[f64]) -> GridComparison {
    if (predicted.bpm / reference.bpm - 1.0).abs() > GRID_TEMPO_TOLERANCE || probes.is_empty() {
        return GridComparison {
            agreement: GridAgreement::TempoMismatch,
            phase_error_ms: None,
            downbeat_correct: None,
        };
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
        downbeat_correct: (agreement == GridAgreement::Aligned).then(|| {
            predicted.beat_number(predicted_index) == reference.beat_number(reference_index)
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        Grid, GridAgreement, KeyCategory, KeyClass, compare_grids, evaluate_key, parse_key,
        tempo_ratio_bucket,
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
        assert_eq!(late.downbeat_correct, Some(true));
        // One beat over: aligned, wrong bar position.
        let shifted = compare_grids(grid(120.0, 0.7, 1), reference, &probes);
        assert_eq!(shifted.agreement, GridAgreement::Aligned);
        assert_eq!(shifted.downbeat_correct, Some(false));
        // Half a beat over.
        let off_beat = compare_grids(grid(120.0, 0.45, 1), reference, &probes);
        assert_eq!(off_beat.agreement, GridAgreement::OffBeat);
        assert_eq!(off_beat.downbeat_correct, None);
        // A tempo 1 % off is not a phase question.
        let tempo = compare_grids(grid(121.2, 0.2, 1), reference, &probes);
        assert_eq!(tempo.agreement, GridAgreement::TempoMismatch);
        assert_eq!(tempo.phase_error_ms, None);
    }
}
