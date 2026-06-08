// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Per-feature, seed-conditioned similarity functions — the heart of
//! the perceptual transition metric.
//!
//! Every function answers one question: *given the track playing now
//! (`seed`) and a candidate (`cand`), how well does this one feature
//! say the candidate continues the seed?* Each returns a value in
//! `[0.0, 1.0]` (1.0 = identical / perfect continuation) **or `None`
//! when the feature is absent on either side**.
//!
//! `None` is the load-bearing detail (§5 of the design brief): a
//! missing feature is *masked* — it drops out of both the numerator
//! and the denominator of the affinity sum. It is never imputed to a
//! neutral 0.5, never penalised, never bonused. A hole is not a
//! signal. The affinity combiner in [`crate::affinity`] is what
//! weights and sums the present features and applies the coverage
//! correction for thin evidence.
//!
//! Most functions are pure functions of the two tracks (plus, for
//! genre, the IDF table from the prepared index). The DSP/timbral
//! functions (loudness, onset density, brightness, …) additionally read
//! each track's cached [`AcousticFeatures`] and the library-derived
//! normalization ranges from the index — the values do not live on a
//! `Track` — but they slot into the same masked-sum framework: absent
//! acoustics (or no index) mask the term exactly like an absent tag.

use std::time::SystemTime;

use sustain_domain::{AcousticFeatures, MusicalKey, Track, normalize_search_text};

use crate::index::SmartShuffleIndex;
use crate::index::genre_tokens;

/// IDF-weighted Jaccard over the seed's and candidate's genre tokens.
///
/// Plain Jaccard treats every shared token equally; that is wrong —
/// sharing "Shoegaze" (rare) says far more about continuation than
/// sharing "Rock" (ubiquitous). We weight each token by its inverse
/// document frequency: `sim = Σ idf(t) over shared tokens / Σ idf(t)
/// over the union`. Masked (`None`) when either side has no genre
/// tokens at all.
///
/// `index` is the prepared Smart Shuffle index, the source of the IDF
/// weights. When it is `None` (no index built yet) every token weighs
/// `1.0`, degrading gracefully to plain Jaccard rather than refusing
/// to score.
pub fn genre_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let seed_tokens = dedup(genre_tokens(seed.metadata.genre.as_deref()));
    let cand_tokens = dedup(genre_tokens(cand.metadata.genre.as_deref()));
    if seed_tokens.is_empty() || cand_tokens.is_empty() {
        return None;
    }
    let weight = |token: &str| -> f32 {
        index
            .map(|i| i.genre_token_idf(token))
            .unwrap_or(1.0)
            .max(0.0)
    };

    let mut union_weight = 0.0_f32;
    let mut shared_weight = 0.0_f32;
    // Union = seed ∪ cand; iterate seed tokens then cand-only tokens.
    for token in &seed_tokens {
        let w = weight(token);
        union_weight += w;
        if cand_tokens.iter().any(|c| c == token) {
            shared_weight += w;
        }
    }
    for token in &cand_tokens {
        if !seed_tokens.iter().any(|s| s == token) {
            union_weight += weight(token);
        }
    }
    if union_weight <= 0.0 {
        return None;
    }
    Some((shared_weight / union_weight).clamp(0.0, 1.0))
}

fn dedup(mut tokens: Vec<String>) -> Vec<String> {
    tokens.sort();
    tokens.dedup();
    tokens
}

/// Tempo similarity on a log scale with one-octave folding.
///
/// Half-time and double-time of a tempo *feel* the same to a dancer —
/// 90 BPM flows perfectly into 180 BPM — so we fold the ratio into a
/// single octave before measuring distance. The falloff is Gaussian
/// in log-tempo (a fixed perceptual scale, not library-derived):
/// small percentage differences are nearly free, larger ones fall off
/// super-linearly. Masked when either side lacks a BPM.
pub fn tempo_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let (seed_bpm, cand_bpm) = (seed.metadata.bpm?, cand.metadata.bpm?);
    if seed_bpm == 0 || cand_bpm == 0 {
        return None;
    }
    let mut ratio = cand_bpm as f32 / seed_bpm as f32;
    // Fold into [1/√2, √2] — the octave centred on 1.0 — so 2× / ½×
    // collapse onto a unison match.
    #[allow(
        clippy::while_float,
        reason = "positive finite BPM inputs and power-of-two steps guarantee termination"
    )]
    while ratio > std::f32::consts::SQRT_2 {
        ratio /= 2.0;
    }
    #[allow(
        clippy::while_float,
        reason = "positive finite BPM inputs and power-of-two steps guarantee termination"
    )]
    while ratio < std::f32::consts::FRAC_1_SQRT_2 {
        ratio *= 2.0;
    }
    let log_distance = ratio.log2().abs(); // in [0, 0.5]
    Some(gaussian(log_distance, TEMPO_SIGMA_LOG2))
}

/// Width (in log2-tempo units) of the tempo Gaussian. 0.10 ≈ a 7%
/// tempo difference scoring ~0.6; a perfect fifth-ish tempo lurch
/// (≈1.25×, 0.32 log2) scores near zero.
const TEMPO_SIGMA_LOG2: f32 = 0.10;

/// Duration similarity on a log scale. A 3-minute song and a 6-minute
/// one are one doubling apart (still reasonably close); a 30-second
/// skit after a 4-minute ballad is far. Low-weight by design — jarring
/// but rare. Masked when either side lacks a duration.
pub fn duration_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let seed_secs = seed.metadata.duration?.as_secs_f32().max(1.0);
    let cand_secs = cand.metadata.duration?.as_secs_f32().max(1.0);
    let log_distance = (cand_secs / seed_secs).log2().abs();
    Some(gaussian(log_distance, DURATION_SIGMA_LOG2))
}

/// Width of the duration Gaussian, in log2 units. 1.0 means a 2×
/// length ratio scores ~0.61 and a 4× ratio ~0.14.
const DURATION_SIGMA_LOG2: f32 = 1.0;

/// Release-year proximity on a fixed calendar scale (the *era of
/// creation* thread). Gaussian in years: same half-decade is nearly
/// free, ~12 years apart scores ~0.61, a generation apart is far.
/// Masked when either side lacks a year.
pub fn year_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let (seed_year, cand_year) = (seed.metadata.year?, cand.metadata.year?);
    let delta = (seed_year - cand_year).unsigned_abs() as f32;
    Some(gaussian(delta, YEAR_SIGMA))
}

/// Width of the release-year Gaussian, in years.
const YEAR_SIGMA: f32 = 12.0;

/// Date-added proximity — the *era of discovery* thread (§6.2). Two
/// tracks imported the same season belong to the same chapter of the
/// listener's life regardless of when they were *released*: a 1975
/// soul cut and a 2019 track both added one spring share that
/// discovery context. Gaussian in days on a fixed scale. Masked when
/// either side lacks an add-date.
pub fn date_added_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let seed_added = seed.statistics.date_added_at?;
    let cand_added = cand.statistics.date_added_at?;
    let delta_days = absolute_days_between(seed_added, cand_added);
    Some(gaussian(delta_days, DATE_ADDED_SIGMA_DAYS))
}

/// Width of the date-added Gaussian, in days. ~180 days ≈ one season's
/// breadth around 0.61; a year apart scores ~0.37.
const DATE_ADDED_SIGMA_DAYS: f32 = 180.0;

/// Musical-key compatibility for *sequencing* (not DJ beat-matching):
/// circle-of-fifths position plus a mode-compatibility term (§9).
///
/// `key_sim = γ · fifths_proximity + (1 − γ) · mode_compat`, with
/// `γ ≈ 0.6` (fifths proximity matters more than mode, but mode
/// matters). Minor keys are mapped to their *relative major's*
/// position on the circle of fifths, so a key and its relative
/// (C major ↔ A minor) land at the same harmonic place and score as a
/// close, natural move — while a tritone apart (C ↔ F♯) scores far.
/// Masked when either side's key is absent or unparseable.
pub fn key_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let seed_key = parse_key(seed.metadata.key.as_deref())?;
    let cand_key = parse_key(cand.metadata.key.as_deref())?;

    let seed_pos = circle_of_fifths_position(seed_key);
    let cand_pos = circle_of_fifths_position(cand_key);
    let steps = seed_pos.abs_diff(cand_pos);
    let circular_steps = steps.min(12 - steps);
    let theta = std::f32::consts::TAU * circular_steps as f32 / 12.0;
    let fifths_proximity = (1.0 + theta.cos()) / 2.0;

    let mode_compat = if seed_key.is_major() == cand_key.is_major() {
        1.0
    } else {
        MODE_CROSS_COMPAT
    };

    Some(KEY_FIFTHS_GAMMA * fifths_proximity + (1.0 - KEY_FIFTHS_GAMMA) * mode_compat)
}

/// Relative weighting of circle-of-fifths proximity vs mode in the key
/// term. 0.6 = fifths proximity dominates, mode still counts.
const KEY_FIFTHS_GAMMA: f32 = 0.6;

/// Mode-compatibility for a major↔minor transition (same-mode is
/// always 1.0). 0.6 keeps a mode change a mild, not disqualifying,
/// difference. The open knob noted in §9 — whether *parallel* should
/// out-rank *relative* — is settled by the relative-major mapping in
/// [`circle_of_fifths_position`], which already places relatives
/// closest; tune this constant from the debug log if needed.
const MODE_CROSS_COMPAT: f32 = 0.6;

/// Position on the circle of fifths in `[0, 12)`. Major keys map their
/// tonic pitch class directly (`pc · 7 mod 12`); minor keys map to
/// their relative major (tonic + 3 semitones) so a relative pair
/// shares a position.
fn circle_of_fifths_position(key: MusicalKey) -> u32 {
    let pitch_class = u32::from(key as u8 % 12);
    let major_pitch_class = if key.is_major() {
        pitch_class
    } else {
        (pitch_class + 3) % 12
    };
    (major_pitch_class * 7) % 12
}

fn parse_key(value: Option<&str>) -> Option<MusicalKey> {
    MusicalKey::from_short_code(value?.trim())
}

/// Same-artist relation, computed at pick time (never embedded or
/// hashed). 1.0 when the `artist` tags match case-insensitively, 0.0
/// when they differ, masked when either is absent. Low weight: a mild
/// nudge, deliberately *less* artist-clumpy than Pure shuffle.
pub fn same_artist(seed: &Track, cand: &Track) -> Option<f32> {
    relation(
        seed.metadata.artist.as_deref(),
        cand.metadata.artist.as_deref(),
    )
}

/// Same-album-artist relation (compilations / reissues). Same shape as
/// [`same_artist`], even lower weight.
pub fn same_album_artist(seed: &Track, cand: &Track) -> Option<f32> {
    relation(
        seed.metadata.album_artist.as_deref(),
        cand.metadata.album_artist.as_deref(),
    )
}

/// Grouping overlap — the user *explicitly tagging the thread*
/// ("Workout", "Late night", "Dinner"). Token/set overlap on the
/// free-form `grouping` field, scored as plain Jaccard. Sparse but
/// high-signal when present; the coverage term handles the sparsity.
/// Masked when either side's grouping is blank.
pub fn grouping_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    let seed_tokens = dedup(free_text_tokens(seed.metadata.grouping.as_deref()));
    let cand_tokens = dedup(free_text_tokens(cand.metadata.grouping.as_deref()));
    if seed_tokens.is_empty() || cand_tokens.is_empty() {
        return None;
    }
    let shared = seed_tokens
        .iter()
        .filter(|t| cand_tokens.iter().any(|c| &c == t))
        .count();
    let union = seed_tokens.len() + cand_tokens.len() - shared;
    if union == 0 {
        return None;
    }
    Some(shared as f32 / union as f32)
}

/// Same-composer indicator — load-bearing for classical and jazz,
/// where the composer is often the real identity of the work, and
/// near-irrelevant elsewhere (hence its low weight). Masked when
/// either composer is blank.
pub fn composer_similarity(seed: &Track, cand: &Track) -> Option<f32> {
    relation(
        seed.metadata.composer.as_deref(),
        cand.metadata.composer.as_deref(),
    )
}

fn relation(seed: Option<&str>, cand: Option<&str>) -> Option<f32> {
    let seed = normalized_non_blank(seed)?;
    let cand = normalized_non_blank(cand)?;
    Some(if seed == cand { 1.0 } else { 0.0 })
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

fn normalized_non_blank(value: Option<&str>) -> Option<String> {
    let normalized = normalize_search_text(value?);
    (!normalized.is_empty()).then_some(normalized)
}

// --- Acoustic (DSP) similarities (§6.1, §7, §8) -----------------------------
//
// These read the pair's cached `AcousticFeatures` from the prepared
// index. When the index is absent, or either track has not been
// analysed, the term is masked (`None`) — identical to a missing tag.
// Loudness uses a fixed perceptual scale (absolute LUFS); the
// collection-scaled features (onset, LRA, low-band variation, tonalness)
// are mapped onto `[0, 1]` against the library's robust range first, so
// the fixed σ below is expressed in those normalized units.

/// Width of the integrated-loudness Gaussian, in LUFS (§7). ~σ = 4.5
/// means a ±3 LUFS difference is nearly free and the similarity falls
/// off super-linearly as the gap widens.
const LOUDNESS_SIGMA_LUFS: f32 = 4.5;

/// Width of the Gaussian for the library-normalized acoustic features,
/// in `[0, 1]` units. A quarter-of-the-library-spread difference scores
/// ~0.61; a full-spread difference scores near zero.
const ACOUSTIC_NORM_SIGMA: f32 = 0.25;

/// Look both tracks' cached acoustics up in the index. `None` (masking
/// the term) when there is no index or either track was not analysed.
fn acoustic_pair<'a>(
    seed: &Track,
    cand: &Track,
    index: Option<&'a SmartShuffleIndex>,
) -> Option<(&'a AcousticFeatures, &'a AcousticFeatures)> {
    let index = index?;
    Some((index.acoustics(seed.id)?, index.acoustics(cand.id)?))
}

/// Loudness continuity on the *integrated* (whole-track) level, a fixed
/// perceptual Gaussian in LUFS (§7). This is the soft distance term; the
/// hard asymmetric guard (which keys off short-term max) lives in the
/// picker. Masked when either track lacks acoustics.
pub fn loudness_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let delta = (cand_a.integrated_lufs - seed_a.integrated_lufs).abs();
    Some(gaussian(delta, LOUDNESS_SIGMA_LUFS))
}

/// Onset-density (rhythmic-busyness) continuity. Library-normalized
/// (§8) so a homogeneous collection keeps its contrast, then a Gaussian
/// on the normalized difference. Masked when either track lacks
/// acoustics. Tells a sparse 120-BPM ambient piece from a busy 120-BPM
/// drum-and-bass track, which tempo alone cannot.
pub fn onset_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let range = index?.acoustic_normalization().onset_rate;
    let delta =
        (range.normalize(seed_a.onset_rate_hz) - range.normalize(cand_a.onset_rate_hz)).abs();
    Some(gaussian(delta, ACOUSTIC_NORM_SIGMA))
}

/// Spectral-brightness (timbral shape) continuity from the low/mid/high
/// band-energy ratios. The ratios already sum to ≈1, so they are
/// inherently collection-independent and compared directly: the L1
/// distance between the two ratio vectors lies in `[0, 2]`, mapped to a
/// `[0, 1]` similarity. Captures dark↔bright *and* EQ curve (V-shape vs
/// mid-forward) that a single centroid would miss. Masked when either
/// track lacks acoustics.
pub fn brightness_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let [sl, sm, sh] = seed_a.band_ratios();
    let [cl, cm, ch] = cand_a.band_ratios();
    let l1 = (sl - cl).abs() + (sm - cm).abs() + (sh - ch).abs();
    Some((1.0 - 0.5 * l1).clamp(0.0, 1.0))
}

/// Tonalness (pitched↔noisy) continuity. Library-normalized (§8) then a
/// Gaussian on the normalized difference. Separates a clean piano from a
/// distorted guitar or a white-noise pad — material that brightness and
/// onset density alone confuse. Masked when either track lacks
/// acoustics.
pub fn tonalness_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let range = index?.acoustic_normalization().tonalness;
    let delta = (range.normalize(seed_a.tonalness) - range.normalize(cand_a.tonalness)).abs();
    Some(gaussian(delta, ACOUSTIC_NORM_SIGMA))
}

/// Low-band-variation (the "kick-drum check") continuity. Separates a
/// steady four-on-the-floor pulse from a syncopated or fluid low end at
/// the *same* BPM and onset density. Library-normalized (§8). Masked
/// when either track lacks acoustics.
pub fn low_band_variation_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let range = index?.acoustic_normalization().low_band_variation;
    let delta = (range.normalize(seed_a.low_band_variation)
        - range.normalize(cand_a.low_band_variation))
    .abs();
    Some(gaussian(delta, ACOUSTIC_NORM_SIGMA))
}

/// Dynamic-range (LRA) continuity — compressed-flat vs dynamic-punchy
/// material. Low weight (§10): partially overlaps onset density and band
/// energy, but the independent cases are real. Library-normalized (§8).
/// Masked when either track lacks acoustics.
pub fn dynamic_range_similarity(
    seed: &Track,
    cand: &Track,
    index: Option<&SmartShuffleIndex>,
) -> Option<f32> {
    let (seed_a, cand_a) = acoustic_pair(seed, cand, index)?;
    let range = index?.acoustic_normalization().loudness_range;
    let delta = (range.normalize(seed_a.loudness_range_lu)
        - range.normalize(cand_a.loudness_range_lu))
    .abs();
    Some(gaussian(delta, ACOUSTIC_NORM_SIGMA))
}

/// Lowercase whitespace/punctuation-delimited tokens for free-text
/// fields like `grouping`. Single-character tokens are dropped, same
/// as genre.
fn free_text_tokens(value: Option<&str>) -> Vec<String> {
    let Some(value) = non_blank(value) else {
        return Vec::new();
    };
    normalize_search_text(value)
        .split(|character: char| !character.is_alphanumeric())
        .filter(|token| token.chars().count() >= 2)
        .map(str::to_owned)
        .collect()
}

/// `exp(−x² / 2σ²)` — the unnormalised Gaussian kernel used by every
/// distance-based similarity. `x = 0` → 1.0, falling off
/// super-linearly. A non-positive σ collapses to an exact-match
/// indicator (defensive; the named σ constants are all positive).
fn gaussian(distance: f32, sigma: f32) -> f32 {
    if sigma <= 0.0 {
        return if distance == 0.0 { 1.0 } else { 0.0 };
    }
    (-(distance * distance) / (2.0 * sigma * sigma)).exp()
}

/// Absolute number of days between two instants, tolerant of either
/// ordering and of clock anomalies.
fn absolute_days_between(a: SystemTime, b: SystemTime) -> f32 {
    let delta = a.duration_since(b).or_else(|_| b.duration_since(a));
    delta.map(|d| d.as_secs_f32() / 86_400.0).unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime};

    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::*;

    fn track(metadata: TrackMetadata) -> Track {
        Track {
            id: TrackId::new(1).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new("a/b.flac").expect("relative path"),
            ),
            metadata,
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn with_genre(genre: &str) -> Track {
        track(TrackMetadata {
            genre: Some(genre.to_owned()),
            ..TrackMetadata::default()
        })
    }

    fn with_bpm(bpm: u32) -> Track {
        track(TrackMetadata {
            bpm: Some(bpm),
            ..TrackMetadata::default()
        })
    }

    fn with_key(key: &str) -> Track {
        track(TrackMetadata {
            key: Some(key.to_owned()),
            ..TrackMetadata::default()
        })
    }

    fn with_year(year: i32) -> Track {
        track(TrackMetadata {
            year: Some(year),
            ..TrackMetadata::default()
        })
    }

    #[test]
    fn genre_is_masked_when_either_side_is_untagged() {
        assert_eq!(
            genre_similarity(&with_genre("Rock"), &track(TrackMetadata::default()), None),
            None
        );
        assert_eq!(
            genre_similarity(&track(TrackMetadata::default()), &with_genre("Rock"), None),
            None
        );
    }

    #[test]
    fn genre_overlap_beats_no_overlap() {
        let seed = with_genre("House/Tech");
        let close = with_genre("House");
        let far = with_genre("Classical");
        assert!(genre_similarity(&seed, &close, None) > genre_similarity(&seed, &far, None));
    }

    #[test]
    fn tempo_folds_the_octave() {
        let seed = with_bpm(90);
        let double = with_bpm(180);
        let off = with_bpm(110);
        // 90↔180 is a double-time match → near 1.0, beats 90↔110.
        assert!(tempo_similarity(&seed, &double).expect("both have bpm") > 0.95);
        assert!(tempo_similarity(&seed, &double) > tempo_similarity(&seed, &off));
    }

    #[test]
    fn tempo_masked_without_bpm() {
        assert_eq!(
            tempo_similarity(&with_bpm(120), &track(TrackMetadata::default())),
            None
        );
    }

    #[test]
    fn artist_relation_folds_accents_and_case() {
        let seed = track(TrackMetadata {
            artist: Some("Björk".to_owned()),
            ..TrackMetadata::default()
        });
        let candidate = track(TrackMetadata {
            artist: Some("BJORK".to_owned()),
            ..TrackMetadata::default()
        });

        assert_eq!(same_artist(&seed, &candidate), Some(1.0));
    }

    #[test]
    fn grouping_tokens_fold_accents_before_matching() {
        let seed = track(TrackMetadata {
            grouping: Some("Déjà Vu".to_owned()),
            ..TrackMetadata::default()
        });
        let candidate = track(TrackMetadata {
            grouping: Some("deja".to_owned()),
            ..TrackMetadata::default()
        });

        assert_eq!(grouping_similarity(&seed, &candidate), Some(0.5));
    }

    #[test]
    fn key_places_relatives_close_and_tritone_far() {
        let c_major = with_key("C");
        let a_minor = with_key("Am");
        let fs_major = with_key("Gb"); // F#/Gb major, a tritone from C
        let g_major = with_key("G");

        let relative = key_similarity(&c_major, &a_minor).expect("both keyed");
        let tritone = key_similarity(&c_major, &fs_major).expect("both keyed");
        let dominant = key_similarity(&c_major, &g_major).expect("both keyed");
        let unison = key_similarity(&c_major, &c_major).expect("both keyed");

        assert!(
            relative > tritone,
            "relative {relative} should beat tritone {tritone}"
        );
        assert!(
            dominant > relative,
            "dominant {dominant} should beat relative {relative}"
        );
        assert!(unison >= dominant);
        assert!(
            tritone < 0.5,
            "tritone should be clearly far, got {tritone}"
        );
    }

    #[test]
    fn year_falls_off_with_distance() {
        let seed = with_year(2010);
        assert!(
            year_similarity(&seed, &with_year(2012)) > year_similarity(&seed, &with_year(1980))
        );
        assert_eq!(
            year_similarity(&seed, &track(TrackMetadata::default())),
            None
        );
    }

    #[test]
    fn date_added_groups_by_discovery_era_not_release_year() {
        let spring_2014 = SystemTime::UNIX_EPOCH + Duration::from_secs(1_396_310_400);
        let mut soul_1975 = with_year(1975);
        soul_1975.statistics.date_added_at = Some(spring_2014);
        let mut track_2019 = with_year(2019);
        track_2019.statistics.date_added_at = Some(spring_2014 + Duration::from_secs(86_400 * 10));
        // 44 years apart in release, ten days apart in discovery → high.
        assert!(date_added_similarity(&soul_1975, &track_2019).expect("both dated") > 0.9);
    }

    #[test]
    fn identity_relations_are_masked_when_absent() {
        let named = track(TrackMetadata {
            artist: Some("Radiohead".to_owned()),
            ..TrackMetadata::default()
        });
        assert_eq!(same_artist(&named, &track(TrackMetadata::default())), None);
        assert_eq!(same_artist(&named, &named), Some(1.0));
        let other = track(TrackMetadata {
            artist: Some("Aphex Twin".to_owned()),
            ..TrackMetadata::default()
        });
        assert_eq!(same_artist(&named, &other), Some(0.0));
    }

    #[test]
    fn grouping_overlap_is_jaccard_and_masked_when_blank() {
        let workout = track(TrackMetadata {
            grouping: Some("Workout Morning".to_owned()),
            ..TrackMetadata::default()
        });
        let workout2 = track(TrackMetadata {
            grouping: Some("Workout".to_owned()),
            ..TrackMetadata::default()
        });
        let dinner = track(TrackMetadata {
            grouping: Some("Dinner".to_owned()),
            ..TrackMetadata::default()
        });
        assert!(grouping_similarity(&workout, &workout2) > grouping_similarity(&workout, &dinner));
        assert_eq!(
            grouping_similarity(&workout, &track(TrackMetadata::default())),
            None
        );
    }
}
