// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Per-track *acoustic* features — the perceptual measurements Smart
//! Shuffle's continuity terms are built on (§6.1 / §7 of the design
//! brief), produced by the DSP analyzer and persisted by the storage
//! layer.
//!
//! These are deliberately raw, physical values (LUFS, Hz, band energy
//! fractions, …), not pre-bucketed moods: the scorer combines and
//! normalizes them, so the stored form stays lossless and re-tunable.
//! Like [`crate::TrackAnalysis`], this is a behaviour-free bag of
//! values living in the domain layer so the storage crate can persist
//! it without pulling in the DSP stack.
//!
//! Every field is computed only when the user opts into acoustic
//! analysis (it is the genuinely heavy decode + STFT + band-split
//! work); Smart Shuffle masks any track that lacks it (§5) and works
//! fine without it.

use serde::{Deserialize, Serialize};

/// The acoustic measurements for one track.
///
/// All loudness values are in LUFS / LU (ITU-R BS.1770-4); band ratios
/// are fractions of total band energy summing to ≈1.0; `tonalness` is
/// in `[0, 1]`. A `serde` round-trip is supported because Smart
/// Shuffle caches these inside its persisted index blob.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcousticFeatures {
    /// Integrated (whole-pass, gated) loudness in LUFS — the track's
    /// overall level. Drives the *soft* loudness-continuity distance.
    pub integrated_lufs: f32,
    /// Maximum short-term (≈3 s window) loudness in LUFS. This is the
    /// loud *boundary* a transition can hit — a track with a quiet
    /// intro and a brickwalled finish has a deceptively low integrated
    /// score, so the asymmetric loudness *guard* keys off this instead
    /// (§7).
    pub short_term_lufs_max: f32,
    /// Loudness range (LRA) in LU — the spread between quiet and loud
    /// passages. Distinguishes compressed-flat from dynamic-punchy
    /// material.
    pub loudness_range_lu: f32,
    /// Onset density in events per second — rhythmic busyness. Tells a
    /// sparse 120-BPM ambient piece from a busy 120-BPM drum-and-bass
    /// track, which BPM alone cannot.
    pub onset_rate_hz: f32,
    /// Fraction of total energy in the low band (≤ ~250 Hz).
    pub low_band_ratio: f32,
    /// Fraction of total energy in the mid band (~250 Hz – 4 kHz).
    pub mid_band_ratio: f32,
    /// Fraction of total energy in the high band (≥ ~4 kHz). Together
    /// the three ratios describe timbral *shape* (dark↔bright, plus EQ
    /// curve) — a spectral-centroid-free brightness representation.
    pub high_band_ratio: f32,
    /// Temporal variation of the low band — the "kick-drum check".
    /// Coefficient of variation of per-window low-band energy; low
    /// means a steady dominant pulse (four-on-the-floor), high means a
    /// fluid/ambient or syncopated low end at the same BPM.
    pub low_band_variation: f32,
    /// Tonalness in `[0, 1]`: how pitched-vs-noisy the material is
    /// (clean piano ≈ high, white-noise pad / heavily distorted ≈
    /// low). Derived from the chroma pass already run for key
    /// detection, so it is nearly free.
    pub tonalness: f32,
}

impl AcousticFeatures {
    /// The three band-energy ratios as an array, in low/mid/high order.
    /// Handy for the brightness similarity, which compares the two
    /// tracks' ratio vectors.
    pub fn band_ratios(&self) -> [f32; 3] {
        [
            self.low_band_ratio,
            self.mid_band_ratio,
            self.high_band_ratio,
        ]
    }

    /// `true` when every measurement is finite. Both `LibraryStore`
    /// backends gate persistence on this so a NaN or ∞ from a degenerate
    /// decode never reaches the database or the Smart Shuffle index.
    pub fn all_finite(&self) -> bool {
        self.integrated_lufs.is_finite()
            && self.short_term_lufs_max.is_finite()
            && self.loudness_range_lu.is_finite()
            && self.onset_rate_hz.is_finite()
            && self.low_band_ratio.is_finite()
            && self.mid_band_ratio.is_finite()
            && self.high_band_ratio.is_finite()
            && self.low_band_variation.is_finite()
            && self.tonalness.is_finite()
    }
}
