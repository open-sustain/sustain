// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The Smart Shuffle *index*: the prepared, library-dependent state
//! the picker needs that cannot be derived from a single track in
//! isolation.
//!
//! This is the artefact the design brief (§12) calls "the index" and
//! deliberately *not* a trained model — there is none. With fixed,
//! hand-set perceptual weights the only genuinely library-dependent
//! work is:
//!
//!   * the **genre-token vocabulary and its IDF weights** — "Rock" on
//!     40% of a library is nearly uninformative; "Shoegaze" on 2% is
//!     highly informative, and only a sweep of the whole library can
//!     tell the two apart (§6.1);
//!   * the **robust normalization statistics** for the timbral features
//!     whose meaningful scale depends on the collection (§8);
//!   * the **cached per-track acoustic features** themselves, so the
//!     picker can score and guard a pair without re-reading the store.
//!
//! The index is rebuilt on the user's chosen cadence, on the "Rebuild
//! index" button, and once in the background after launch. It is
//! milliseconds of work on a 10 000-track library but it is real, and
//! it is what makes the fixed weights honest. It is persisted as an
//! opaque blob in a singleton row, tagged with [`INDEX_SCHEMA_VERSION`]
//! so a stale shape is silently discarded rather than fed mismatched
//! inputs.

use std::collections::{BTreeMap, HashSet};

use serde::{Deserialize, Serialize};
use sustain_domain::{AcousticFeatures, Track, TrackId};

use crate::SmartShuffleError;

/// Bump when the *shape* of the index — the set of features it
/// prepares, the genre tokenisation, or the normalization scheme —
/// changes in a way that invalidates a previously-persisted blob. The
/// runtime compares this against the stored value and discards a
/// mismatched blob (no migration: pre-release, the index is cheap to
/// rebuild from scratch).
///
/// Version 1: genre-token vocabulary + IDF only (metadata-feature
/// scorer; the DSP/timbral normalization parameters arrive in a later
/// schema version).
///
/// Version 2: the acoustic features arrived — the index now caches each
/// analysed track's [`AcousticFeatures`] plus the library-derived robust
/// normalization ranges (§8) for the collection-scaled timbral terms. A
/// version-1 blob has neither, so it is discarded and rebuilt.
pub const INDEX_SCHEMA_VERSION: u32 = 2;

/// Lower percentile for the robust normalization ranges (§8). The 5th
/// percentile, paired with [`ROBUST_UPPER_PERCENTILE`], ignores the
/// outliers a single mistagged track would otherwise inject.
const ROBUST_LOWER_PERCENTILE: f32 = 0.05;

/// Upper percentile for the robust normalization ranges (§8).
const ROBUST_UPPER_PERCENTILE: f32 = 0.95;

/// Hard cap on the genre-token vocabulary, a safety net against a
/// pathologically exotic library blowing up the blob. 4096 is an
/// order of magnitude beyond any plausible curated collection's
/// distinct genre tokens; the truncation keeps the most frequent
/// tokens (the rare ones it drops carry the *highest* IDF, but a
/// token appearing on a single track contributes to no pair's
/// similarity anyway, so dropping the long tail is harmless).
const MAX_GENRE_TOKENS: usize = 4096;

/// A robust `[low, high]` range — the 5th/95th library percentiles of
/// one collection-scaled acoustic feature — used to map a raw value
/// onto `[0, 1]` before it is compared (§8). Mapping onto a
/// library-relative range is what keeps a feature *alive* in a
/// homogeneous collection: in an all-techno library the onset-density
/// spread is narrow, so a fixed absolute scale would flatten every
/// track to the same value, but the percentile range stretches that
/// narrow spread back across `[0, 1]` and preserves the contrast.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RobustRange {
    low: f32,
    high: f32,
}

impl RobustRange {
    /// The robust range of `values` (mutated in place by the sort).
    /// A collection with fewer than two distinct values yields a
    /// degenerate range whose [`Self::normalize`] returns the neutral
    /// `0.5` for every input.
    fn from_values(values: &mut [f32]) -> Self {
        Self {
            low: percentile(values, ROBUST_LOWER_PERCENTILE),
            high: percentile(values, ROBUST_UPPER_PERCENTILE),
        }
    }

    /// Map `raw` onto `[0, 1]` against this range, clamping outliers to
    /// the ends. A degenerate (`high <= low`) range — too few tracks to
    /// estimate a spread — returns `0.5`, so the feature contributes a
    /// neutral, non-discriminating value rather than a spurious one.
    pub fn normalize(self, raw: f32) -> f32 {
        if self.high <= self.low {
            return 0.5;
        }
        ((raw - self.low) / (self.high - self.low)).clamp(0.0, 1.0)
    }
}

/// Library-derived robust normalization ranges for the collection-scaled
/// acoustic features (§8). Loudness, tempo, key, year and date-added use
/// *fixed* perceptual scales and so are absent here; only the features
/// whose meaningful spread depends on the collection are normalized
/// against the library.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct AcousticNormalization {
    /// Onset density (events/second).
    pub onset_rate: RobustRange,
    /// Loudness range (LRA, in LU).
    pub loudness_range: RobustRange,
    /// Low-band temporal variation (the "kick-drum check").
    pub low_band_variation: RobustRange,
    /// Tonalness (pitched↔noisy).
    pub tonalness: RobustRange,
}

/// The prepared, persisted Smart Shuffle index. Cheap to clone (a
/// `BTreeMap` of short strings to `f32`, a per-track acoustics map, and
/// a few scalars).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SmartShuffleIndex {
    /// Schema version the blob was written under. Checked on load.
    schema_version: u32,
    /// Genre token → inverse-document-frequency weight. A token's IDF
    /// reflects how *rare* it is across the library, so shared rare
    /// genres count for far more than shared common ones (§6.1).
    /// `BTreeMap` for deterministic serialisation.
    genre_idf: BTreeMap<String, f32>,
    /// Cached acoustic features for every analysed, *available* track,
    /// keyed by raw track id. The acoustic affinity terms and the
    /// loudness guard look a pair of tracks up here; a track with no
    /// entry (not yet analysed) is masked (§5). `BTreeMap` for
    /// deterministic serialisation.
    acoustics: BTreeMap<i64, AcousticFeatures>,
    /// Robust normalization ranges for the collection-scaled acoustic
    /// features, derived from `acoustics` (§8).
    normalization: AcousticNormalization,
    /// Number of (available) tracks the index was built from. Surfaced
    /// in the preferences status caption ("Library indexed: N tracks").
    indexed_track_count: u32,
    /// Fraction in `[0.0, 1.0]` of indexed tracks that carry the DSP
    /// acoustic features the timbral affinity terms need. Surfaced as
    /// "Analysis coverage: NN%".
    analysis_coverage: f32,
    /// Wall-clock unix timestamp of the rebuild that produced this
    /// index, stamped by the runtime (this crate never reads the
    /// clock). Surfaced as "Last index rebuild: …".
    built_at_unix: i64,
}

impl SmartShuffleIndex {
    /// Build a fresh index from the live library and whatever acoustic
    /// analysis exists for it. Missing-file tracks are excluded — they
    /// can never be picked, so they must not skew the IDF document
    /// frequencies, the normalization ranges, or the coverage figure.
    /// `acoustics` is the store's full set of analysed tracks; entries
    /// for unavailable or unknown tracks are ignored. `built_at_unix` is
    /// supplied by the caller's clock.
    pub fn build(
        tracks: &[Track],
        acoustics: &[(TrackId, AcousticFeatures)],
        built_at_unix: i64,
    ) -> Self {
        // Document frequency: how many tracks carry each genre token.
        let mut document_frequency: BTreeMap<String, u32> = BTreeMap::new();
        let mut indexed_track_count: u32 = 0;
        let mut available_ids: HashSet<i64> = HashSet::new();
        for track in tracks.iter().filter(|t| !t.location.is_missing()) {
            indexed_track_count = indexed_track_count.saturating_add(1);
            available_ids.insert(track.id.get());
            // De-duplicate tokens within a track so a genre tag of
            // "Rock/Rock" counts "rock" once toward its frequency.
            let mut seen: BTreeMap<String, ()> = BTreeMap::new();
            for token in genre_tokens(track.metadata.genre.as_deref()) {
                seen.entry(token).or_default();
            }
            for token in seen.into_keys() {
                *document_frequency.entry(token).or_default() += 1;
            }
        }

        // Keep only the most frequent tokens if the vocabulary is
        // absurdly large (see MAX_GENRE_TOKENS).
        if document_frequency.len() > MAX_GENRE_TOKENS {
            let mut ranked: Vec<(String, u32)> = document_frequency.into_iter().collect();
            ranked.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
            ranked.truncate(MAX_GENRE_TOKENS);
            document_frequency = ranked.into_iter().collect();
        }

        let genre_idf = document_frequency
            .into_iter()
            .map(|(token, df)| (token, idf(indexed_track_count, df)))
            .collect();

        // Cache acoustics for available tracks only; a duplicate id
        // (which the store's primary key forbids) would simply overwrite.
        let acoustics: BTreeMap<i64, AcousticFeatures> = acoustics
            .iter()
            .filter(|(id, _)| available_ids.contains(&id.get()))
            .map(|(id, features)| (id.get(), *features))
            .collect();

        let normalization = derive_normalization(&acoustics);
        let analysis_coverage = if indexed_track_count == 0 {
            0.0
        } else {
            acoustics.len() as f32 / indexed_track_count as f32
        };

        Self {
            schema_version: INDEX_SCHEMA_VERSION,
            genre_idf,
            acoustics,
            normalization,
            indexed_track_count,
            analysis_coverage,
            built_at_unix,
        }
    }

    /// Cached acoustic features for a track, or `None` when it was not
    /// analysed (so the acoustic terms and the loudness guard mask it).
    pub fn acoustics(&self, track_id: TrackId) -> Option<&AcousticFeatures> {
        self.acoustics.get(&track_id.get())
    }

    /// The library-derived robust normalization ranges (§8).
    pub fn acoustic_normalization(&self) -> &AcousticNormalization {
        &self.normalization
    }

    /// IDF weight for a genre token. Tokens the index has never seen
    /// (a track added since the last rebuild) are treated as maximally
    /// informative for the current library size, which is the honest
    /// default for "rare enough that we have not catalogued it yet".
    pub fn genre_token_idf(&self, token: &str) -> f32 {
        self.genre_idf
            .get(token)
            .copied()
            .unwrap_or_else(|| idf(self.indexed_track_count, 0))
    }

    pub fn schema_version(&self) -> u32 {
        self.schema_version
    }

    pub fn indexed_track_count(&self) -> u32 {
        self.indexed_track_count
    }

    pub fn analysis_coverage(&self) -> f32 {
        self.analysis_coverage
    }

    pub fn built_at_unix(&self) -> i64 {
        self.built_at_unix
    }

    pub fn to_blob(&self) -> Result<Vec<u8>, SmartShuffleError> {
        serde_json::to_vec(self).map_err(|_| SmartShuffleError::IndexSerialisationFailed)
    }

    pub fn from_blob(blob: &[u8]) -> Result<Self, SmartShuffleError> {
        serde_json::from_slice(blob).map_err(|_| SmartShuffleError::IndexDeserialisationFailed)
    }
}

/// Robust normalization ranges over the cached acoustics. Each range is
/// the 5th/95th percentile of one feature across the analysed tracks;
/// loudness/tempo/key/year/date-added are *not* here because they use
/// fixed perceptual scales (§8).
fn derive_normalization(acoustics: &BTreeMap<i64, AcousticFeatures>) -> AcousticNormalization {
    let mut onset: Vec<f32> = acoustics.values().map(|a| a.onset_rate_hz).collect();
    let mut loudness_range: Vec<f32> = acoustics.values().map(|a| a.loudness_range_lu).collect();
    let mut low_band_variation: Vec<f32> =
        acoustics.values().map(|a| a.low_band_variation).collect();
    let mut tonalness: Vec<f32> = acoustics.values().map(|a| a.tonalness).collect();
    AcousticNormalization {
        onset_rate: RobustRange::from_values(&mut onset),
        loudness_range: RobustRange::from_values(&mut loudness_range),
        low_band_variation: RobustRange::from_values(&mut low_band_variation),
        tonalness: RobustRange::from_values(&mut tonalness),
    }
}

/// Linear-interpolation-free percentile: sort ascending and take the
/// nearest-rank element. `fraction` is clamped to `[0, 1]`; an empty
/// slice yields `0.0` (the caller's degenerate range then normalizes
/// every input to the neutral `0.5`).
fn percentile(values: &mut [f32], fraction: f32) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let rank = (fraction.clamp(0.0, 1.0) * (values.len() - 1) as f32).round() as usize;
    values[rank.min(values.len() - 1)]
}

/// Smoothed inverse document frequency. `+1` smoothing keeps the
/// weight strictly positive even for a token present on every track
/// (so the weighted Jaccard denominator never collapses) and bounded
/// for a token present on none. With `total = 0` the library is empty
/// and every token gets the same neutral weight.
fn idf(total: u32, document_frequency: u32) -> f32 {
    let total = f32::from(u16::try_from(total.min(u32::from(u16::MAX))).unwrap_or(u16::MAX));
    let df =
        f32::from(u16::try_from(document_frequency.min(u32::from(u16::MAX))).unwrap_or(u16::MAX));
    ((total + 1.0) / (df + 1.0)).ln() + 1.0
}

/// Slugify-and-explode a raw `genre` tag into comparable tokens.
/// ASCII-folded, lowercase; every run of non-alphanumerics becomes a
/// single separator, the result is split on it, and single-character
/// tokens are dropped to keep noise low. `"House/Tech"` → `["house",
/// "tech"]`; `"Alternative Rock"` → `["alternative", "rock"]`;
/// `"R&B"` → `[]` (both single-char after slugification). Empty for
/// `None`/blank input.
pub fn genre_tokens(genre: Option<&str>) -> Vec<String> {
    let Some(genre) = genre else {
        return Vec::new();
    };
    let mut slug = String::with_capacity(genre.len());
    let mut last_was_separator = true;
    for character in genre.chars() {
        let lowered = character.to_ascii_lowercase();
        if lowered.is_ascii_alphanumeric() {
            slug.push(lowered);
            last_was_separator = false;
        } else if !last_was_separator {
            slug.push('-');
            last_was_separator = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug.split('-')
        .filter(|token| token.len() >= 2)
        .map(str::to_owned)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{INDEX_SCHEMA_VERSION, RobustRange, SmartShuffleIndex, genre_tokens, idf};
    use sustain_domain::{
        AcousticFeatures, PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata,
        TrackRelativePath,
    };

    fn acoustics(onset: f32) -> AcousticFeatures {
        AcousticFeatures {
            integrated_lufs: -14.0,
            short_term_lufs_max: -10.0,
            loudness_range_lu: 6.0,
            onset_rate_hz: onset,
            low_band_ratio: 0.3,
            mid_band_ratio: 0.5,
            high_band_ratio: 0.2,
            low_band_variation: 0.4,
            tonalness: 0.7,
        }
    }

    fn track(id: i64, genre: Option<&str>) -> Track {
        Track {
            id: TrackId::new(id).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("t/{id}.flac")).expect("relative path"),
            ),
            metadata: TrackMetadata {
                genre: genre.map(str::to_owned),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    #[test]
    fn genre_tokens_slugify_and_explode() {
        assert_eq!(genre_tokens(Some("House/Tech")), vec!["house", "tech"]);
        assert_eq!(
            genre_tokens(Some("Alternative Rock")),
            vec!["alternative", "rock"]
        );
        assert_eq!(genre_tokens(Some("R&B")), Vec::<String>::new());
        assert_eq!(genre_tokens(None), Vec::<String>::new());
    }

    #[test]
    fn rarer_genre_token_earns_a_higher_idf() {
        // "rock" appears on 9 tracks, "shoegaze" on 1.
        let mut tracks: Vec<Track> = (0..9).map(|i| track(i + 1, Some("Rock"))).collect();
        tracks.push(track(10, Some("Shoegaze")));
        let index = SmartShuffleIndex::build(&tracks, &[], 0);
        assert_eq!(index.indexed_track_count(), 10);
        assert!(
            index.genre_token_idf("shoegaze") > index.genre_token_idf("rock"),
            "the rare token must weigh more"
        );
    }

    #[test]
    fn missing_tracks_are_excluded_from_the_index() {
        let mut present = track(1, Some("Rock"));
        present.location =
            TrackLocation::missing(TrackRelativePath::new("t/1.flac").expect("relative path"));
        let index = SmartShuffleIndex::build(&[present], &[], 0);
        assert_eq!(index.indexed_track_count(), 0);
    }

    #[test]
    fn idf_is_strictly_positive_even_for_ubiquitous_tokens() {
        // Present on every one of 1000 tracks → still > 0.
        assert!(idf(1000, 1000) > 0.0);
        // Present on none → finite and larger.
        assert!(idf(1000, 0) > idf(1000, 1000));
    }

    #[test]
    fn blob_round_trips() {
        let index = SmartShuffleIndex::build(
            &[track(1, Some("Rock"))],
            &[(TrackId::new(1).expect("id"), acoustics(2.0))],
            123,
        );
        let blob = index.to_blob().expect("serialise");
        let restored = SmartShuffleIndex::from_blob(&blob).expect("deserialise");
        assert_eq!(restored, index);
        assert_eq!(restored.schema_version(), INDEX_SCHEMA_VERSION);
        assert_eq!(restored.built_at_unix(), 123);
    }

    #[test]
    fn caches_acoustics_for_available_tracks_only() {
        let mut missing = track(2, Some("Rock"));
        missing.location =
            TrackLocation::missing(TrackRelativePath::new("t/2.flac").expect("relative path"));
        let tracks = [track(1, Some("Rock")), missing];
        let index = SmartShuffleIndex::build(
            &tracks,
            &[
                (TrackId::new(1).expect("id"), acoustics(1.0)),
                // An acoustics row for a missing track is ignored.
                (TrackId::new(2).expect("id"), acoustics(9.0)),
                // …as is one for a track not in the library at all.
                (TrackId::new(99).expect("id"), acoustics(9.0)),
            ],
            0,
        );
        assert!(index.acoustics(TrackId::new(1).expect("id")).is_some());
        assert!(index.acoustics(TrackId::new(2).expect("id")).is_none());
        assert!(index.acoustics(TrackId::new(99).expect("id")).is_none());
    }

    #[test]
    fn analysis_coverage_is_the_analysed_fraction_of_available_tracks() {
        let tracks: Vec<Track> = (1..=4).map(|i| track(i, Some("Rock"))).collect();
        // Two of the four available tracks carry acoustics.
        let index = SmartShuffleIndex::build(
            &tracks,
            &[
                (TrackId::new(1).expect("id"), acoustics(1.0)),
                (TrackId::new(2).expect("id"), acoustics(2.0)),
            ],
            0,
        );
        assert!((index.analysis_coverage() - 0.5).abs() < 1e-6);
    }

    #[test]
    fn robust_range_normalizes_and_degenerates_to_neutral() {
        let mut values = vec![0.0, 1.0, 2.0, 3.0, 4.0];
        let range = RobustRange::from_values(&mut values);
        // A mid value maps near the middle; out-of-range clamps to the ends.
        assert!((range.normalize(2.0) - 0.5).abs() < 0.2);
        assert_eq!(range.normalize(-100.0), 0.0);
        assert_eq!(range.normalize(100.0), 1.0);
        // A single distinct value → degenerate range → neutral 0.5.
        let mut flat = vec![5.0, 5.0, 5.0];
        let degenerate = RobustRange::from_values(&mut flat);
        assert_eq!(degenerate.normalize(5.0), 0.5);
        assert_eq!(degenerate.normalize(999.0), 0.5);
    }
}
