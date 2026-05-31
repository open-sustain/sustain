// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The affinity score: a transparent, masked, weighted sum of the
//! per-feature similarities, with an explicit coverage correction for
//! thin evidence (§5 of the design brief).
//!
//! ```text
//!             Σ  wᵢ · simᵢ(X, Y)     over features present in BOTH X and Y
//! affinity =  ─────────────────────
//!             Σ  wᵢ                  over features present in BOTH X and Y
//! ```
//!
//! Missing features are *masked* — dropped from both sums — never
//! imputed. But a score over two shared features is weaker evidence
//! than one over ten, so we blend toward a neutral prior by how much
//! of the total possible weight actually voted:
//!
//! ```text
//! coverage       = Σ wᵢ (shared) / Σ wᵢ (all features)
//! final_affinity = affinity · coverage + NEUTRAL_PRIOR · (1 − coverage)
//! ```
//!
//! Weights are fixed, hand-set, perceptually-grounded named constants
//! (§10), ordered by how jarring the worst-case violation of each
//! feature is: physical comfort first, then groove, then
//! harmony/timbre, then cultural/era context, last identity. They are
//! *starting points committed as named constants, not magic numbers* —
//! the debug log (§14) is the real calibration instrument.

use sustain_domain::Track;

use crate::index::SmartShuffleIndex;
use crate::similarity;

/// Neutral value a thin-evidence score is blended toward. Slightly
/// above 0.5 because, in an all-liked library, the prior on "this is a
/// fine continuation" is mildly positive (§5).
pub const NEUTRAL_PRIOR: f32 = 0.55;

// --- Affinity weights (§10) -------------------------------------------------
//
// Ordered by how jarring the worst-case violation of each feature is:
// physical comfort (loudness) first, then the cultural/groove core
// (genre, tempo, onset, brightness), then user intent and harmony/timbre
// (grouping, key, tonalness), then era and rhythmic character, then the
// marginal/identity tail. The DSP/timbral terms are masked (and so drop
// out of the coverage denominator) on tracks the user has not run audio
// analysis on — exactly like any other absent feature.

const W_LOUDNESS: f32 = 1.40;
const W_GENRE: f32 = 1.10;
const W_TEMPO: f32 = 1.10;
const W_ONSET: f32 = 1.00;
const W_BRIGHTNESS: f32 = 0.80;
const W_GROUPING: f32 = 0.70;
const W_KEY: f32 = 0.60;
const W_TONALNESS: f32 = 0.60;
const W_YEAR: f32 = 0.60;
const W_LOW_BAND: f32 = 0.50;
const W_DATE_ADDED: f32 = 0.50;
const W_DYNAMIC_RANGE: f32 = 0.40;
const W_COMPOSER: f32 = 0.30;
const W_DURATION: f32 = 0.30;
const W_SAME_ARTIST: f32 = 0.20;
const W_SAME_ALBUM_ARTIST: f32 = 0.15;

/// The affinity features in scoring order, each a stable label for the
/// debug log plus its weight. The order is the one printed in the
/// `SUSTAIN_LOG_SMART_SHUFFLE=1` trace.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AffinityFeature {
    Loudness,
    Genre,
    Tempo,
    OnsetDensity,
    Brightness,
    Grouping,
    Key,
    Tonalness,
    Year,
    LowBandVariation,
    DateAdded,
    DynamicRange,
    Composer,
    Duration,
    SameArtist,
    SameAlbumArtist,
}

impl AffinityFeature {
    /// Every feature, in scoring/printing order (the §10 weight rank).
    pub const ALL: [Self; 16] = [
        Self::Loudness,
        Self::Genre,
        Self::Tempo,
        Self::OnsetDensity,
        Self::Brightness,
        Self::Grouping,
        Self::Key,
        Self::Tonalness,
        Self::Year,
        Self::LowBandVariation,
        Self::DateAdded,
        Self::DynamicRange,
        Self::Composer,
        Self::Duration,
        Self::SameArtist,
        Self::SameAlbumArtist,
    ];

    pub const fn weight(self) -> f32 {
        match self {
            Self::Loudness => W_LOUDNESS,
            Self::Genre => W_GENRE,
            Self::Tempo => W_TEMPO,
            Self::OnsetDensity => W_ONSET,
            Self::Brightness => W_BRIGHTNESS,
            Self::Grouping => W_GROUPING,
            Self::Key => W_KEY,
            Self::Tonalness => W_TONALNESS,
            Self::Year => W_YEAR,
            Self::LowBandVariation => W_LOW_BAND,
            Self::DateAdded => W_DATE_ADDED,
            Self::DynamicRange => W_DYNAMIC_RANGE,
            Self::Composer => W_COMPOSER,
            Self::Duration => W_DURATION,
            Self::SameArtist => W_SAME_ARTIST,
            Self::SameAlbumArtist => W_SAME_ALBUM_ARTIST,
        }
    }

    /// Short label for the debug log.
    pub const fn label(self) -> &'static str {
        match self {
            Self::Loudness => "loudness",
            Self::Genre => "genre",
            Self::Tempo => "tempo",
            Self::OnsetDensity => "onset_density",
            Self::Brightness => "brightness",
            Self::Grouping => "grouping",
            Self::Key => "key",
            Self::Tonalness => "tonalness",
            Self::Year => "year",
            Self::LowBandVariation => "low_band_variation",
            Self::DateAdded => "date_added",
            Self::DynamicRange => "dynamic_range",
            Self::Composer => "composer",
            Self::Duration => "duration",
            Self::SameArtist => "same_artist",
            Self::SameAlbumArtist => "same_album_artist",
        }
    }

    fn similarity(
        self,
        seed: &Track,
        cand: &Track,
        index: Option<&SmartShuffleIndex>,
    ) -> Option<f32> {
        match self {
            Self::Loudness => similarity::loudness_similarity(seed, cand, index),
            Self::Genre => similarity::genre_similarity(seed, cand, index),
            Self::Tempo => similarity::tempo_similarity(seed, cand),
            Self::OnsetDensity => similarity::onset_similarity(seed, cand, index),
            Self::Brightness => similarity::brightness_similarity(seed, cand, index),
            Self::Grouping => similarity::grouping_similarity(seed, cand),
            Self::Key => similarity::key_similarity(seed, cand),
            Self::Tonalness => similarity::tonalness_similarity(seed, cand, index),
            Self::Year => similarity::year_similarity(seed, cand),
            Self::LowBandVariation => similarity::low_band_variation_similarity(seed, cand, index),
            Self::DateAdded => similarity::date_added_similarity(seed, cand),
            Self::DynamicRange => similarity::dynamic_range_similarity(seed, cand, index),
            Self::Composer => similarity::composer_similarity(seed, cand),
            Self::Duration => similarity::duration_similarity(seed, cand),
            Self::SameArtist => similarity::same_artist(seed, cand),
            Self::SameAlbumArtist => similarity::same_album_artist(seed, cand),
        }
    }
}

/// Total weight if every feature were present — the coverage
/// denominator. Computed from [`AffinityFeature::ALL`] so it can never
/// drift out of sync with the feature set.
pub fn total_affinity_weight() -> f32 {
    AffinityFeature::ALL.iter().map(|f| f.weight()).sum()
}

/// One feature's contribution to a pair's affinity, retained for the
/// debug log. `similarity` is `None` when the feature was masked.
#[derive(Clone, Copy, Debug)]
pub struct FeatureContribution {
    pub feature: AffinityFeature,
    pub similarity: Option<f32>,
    pub weight: f32,
}

/// The full, transparent decomposition of one pair's affinity.
#[derive(Clone, Debug)]
pub struct AffinityBreakdown {
    /// Per-feature contributions, in [`AffinityFeature::ALL`] order.
    pub contributions: Vec<FeatureContribution>,
    /// Weighted mean over the *present* features (the raw masked sum).
    pub affinity: f32,
    /// Share of total possible weight that actually voted, in `[0, 1]`.
    pub coverage: f32,
    /// `affinity · coverage + NEUTRAL_PRIOR · (1 − coverage)`.
    pub final_affinity: f32,
}

/// Compute the affinity of candidate `cand` as a continuation of
/// `seed`. Returns `None` in the degenerate case where the two tracks
/// share *no* perceptual feature at all — the caller treats that as a
/// uniform (Pure-style) draw for this one pick rather than scoring on
/// nothing (§5).
pub fn compute_affinity(
    index: Option<&SmartShuffleIndex>,
    seed: &Track,
    cand: &Track,
) -> Option<AffinityBreakdown> {
    let mut contributions = Vec::with_capacity(AffinityFeature::ALL.len());
    let mut shared_weight = 0.0_f32;
    let mut weighted_sum = 0.0_f32;

    for feature in AffinityFeature::ALL {
        let weight = feature.weight();
        let similarity = feature.similarity(seed, cand, index);
        if let Some(value) = similarity {
            shared_weight += weight;
            weighted_sum += weight * value;
        }
        contributions.push(FeatureContribution {
            feature,
            similarity,
            weight,
        });
    }

    if shared_weight <= 0.0 {
        return None;
    }

    let affinity = weighted_sum / shared_weight;
    let coverage = (shared_weight / total_affinity_weight()).clamp(0.0, 1.0);
    let final_affinity = affinity * coverage + NEUTRAL_PRIOR * (1.0 - coverage);

    Some(AffinityBreakdown {
        contributions,
        affinity,
        coverage,
        final_affinity,
    })
}

#[cfg(test)]
mod tests {
    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::{NEUTRAL_PRIOR, compute_affinity, total_affinity_weight};

    fn track(id: i64, metadata: TrackMetadata) -> Track {
        Track {
            id: TrackId::new(id).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("t/{id}.flac")).expect("relative path"),
            ),
            metadata,
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    #[test]
    fn no_shared_feature_yields_none() {
        // Seed has only a genre; candidate has only a BPM → no feature
        // present on both sides.
        let seed = track(
            1,
            TrackMetadata {
                genre: Some("Rock".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let cand = track(
            2,
            TrackMetadata {
                bpm: Some(120),
                ..TrackMetadata::default()
            },
        );
        assert!(compute_affinity(None, &seed, &cand).is_none());
    }

    #[test]
    fn thin_evidence_is_pulled_toward_the_neutral_prior() {
        // Both share only genre (perfect match). Coverage is low, so
        // even a perfect single-feature score lands well below 1.0 and
        // above the neutral prior.
        let seed = track(
            1,
            TrackMetadata {
                genre: Some("Shoegaze".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let cand = track(
            2,
            TrackMetadata {
                genre: Some("Shoegaze".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let breakdown = compute_affinity(None, &seed, &cand).expect("shared genre");
        assert!(
            (breakdown.affinity - 1.0).abs() < 1e-6,
            "perfect raw affinity"
        );
        assert!(breakdown.coverage < 0.3, "single feature → low coverage");
        assert!(
            breakdown.final_affinity > NEUTRAL_PRIOR && breakdown.final_affinity < 1.0,
            "blended toward but above neutral: {}",
            breakdown.final_affinity
        );
    }

    #[test]
    fn richer_agreement_scores_higher_than_thin_agreement() {
        let base = TrackMetadata {
            genre: Some("House".to_owned()),
            bpm: Some(124),
            key: Some("Am".to_owned()),
            year: Some(2014),
            ..TrackMetadata::default()
        };
        let seed = track(1, base.clone());
        let rich_match = track(2, base);
        let thin_match = track(
            3,
            TrackMetadata {
                genre: Some("House".to_owned()),
                ..TrackMetadata::default()
            },
        );
        let rich = compute_affinity(None, &seed, &rich_match)
            .expect("rich match shares features")
            .final_affinity;
        let thin = compute_affinity(None, &seed, &thin_match)
            .expect("thin match shares genre")
            .final_affinity;
        assert!(
            rich > thin,
            "more agreeing features should win: {rich} vs {thin}"
        );
    }

    #[test]
    fn total_weight_matches_the_feature_table() {
        // Guards against a feature being added to ALL without a weight
        // (or vice versa): the sum must equal the documented constants
        // (§10), in rank order.
        let expected = 1.40 // loudness
            + 1.10 // genre
            + 1.10 // tempo
            + 1.00 // onset density
            + 0.80 // brightness
            + 0.70 // grouping
            + 0.60 // key
            + 0.60 // tonalness
            + 0.60 // year
            + 0.50 // low-band variation
            + 0.50 // date added
            + 0.40 // dynamic range (LRA)
            + 0.30 // composer
            + 0.30 // duration
            + 0.20 // same artist
            + 0.15; // same album-artist
        assert!((total_affinity_weight() - expected).abs() < 1e-6);
    }
}
