// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Library-wide diagnostic statistics (issue #20).
//!
//! Pure aggregation over the in-memory track list — which is the
//! authoritative SQLite copy the rest of the app already holds, so these
//! figures never re-read file tags. The presentation layer renders the
//! returned struct; every selection rule (top-N folding, weighted
//! "most liked" ranking, zero-star exclusion, decade
//! bucketing) lives here so it can be tested without a UI.
//!
//! Two time domains appear: a track's *release year*
//! (`TrackMetadata::year`) is already a calendar year, so it is bucketed
//! arithmetically; the *date a track was added* is a [`SystemTime`] whose
//! calendar year requires a timezone-aware calendar this crate
//! deliberately does not depend on. Callers pass a `year_of_added`
//! closure (the GTK frontend backs it with `glib::DateTime`) so the
//! bucketing rule still lives here while the calendar dependency stays at
//! the edge.

use std::cmp::Ordering;
use std::collections::HashMap;
use std::time::SystemTime;

use crate::{Spellings, Track};

/// Genres shown individually in the genre-distribution chart before the
/// long tail collapses into a single "Other" entry.
const GENRE_DISTRIBUTION_TOP_N: usize = 12;

/// Length of the "most played" / "most liked" genre rankings.
const TOP_GENRE_RANK: usize = 5;

/// The whole-library statistics shown on the Statistics screen.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryStatistics {
    /// Total number of tracks in the library (the denominator for the
    /// genre-distribution shares).
    pub total_tracks: usize,
    /// Plays recorded across the whole library.
    pub total_plays: u64,
    /// Share of tracks per genre, largest first, with the long tail
    /// folded into a single "Other" bucket.
    pub genre_distribution: GenreDistribution,
    /// Share of tracks per bitrate range.
    pub quality_distribution: QualityDistribution,
    /// The five genres with the highest total play count.
    pub most_played_genres: Vec<GenrePlayCount>,
    /// The five most highly-rated genres (zero-star tracks excluded).
    pub most_liked_genres: Vec<GenreRating>,
    /// Track counts grouped by release decade.
    pub release_decades: Vec<DecadeCount>,
    /// Track counts grouped by the calendar year each track was added.
    pub added_years: Vec<YearCount>,
}

/// One genre's slice of the library, by weighted track contribution.
#[derive(Clone, Debug, PartialEq)]
pub struct GenreShare {
    /// The genre name, or `None` for tracks with no genre tag.
    pub genre: Option<String>,
    /// Track contribution carried by this genre. A track tagged
    /// `House/Deep` contributes `0.5` to `House` and `0.5` to `Deep`.
    pub track_weight: f64,
}

/// The aggregated tail of the genre distribution beyond the top N.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OtherGenres {
    /// How many distinct genres were folded together.
    pub genre_count: usize,
    /// Weighted track contribution held by those folded genres.
    pub track_weight: f64,
}

/// Genre distribution: the largest genres individually (capped at a fixed
/// count), plus an optional folded tail. `total_tracks` is the
/// whole-library count (including untagged tracks) so each share is a
/// fraction of the library, not of the charted subset.
#[derive(Clone, Debug, PartialEq)]
pub struct GenreDistribution {
    pub entries: Vec<GenreShare>,
    pub other: Option<OtherGenres>,
    pub total_tracks: usize,
}

/// A bitrate range and how many tracks fall in it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QualityBucket {
    pub range: QualityRange,
    pub track_count: usize,
}

/// The bitrate ranges reported by the quality-distribution widget, in
/// ascending order. Boundaries are fixed by issue #20.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QualityRange {
    /// `<= 128 kbps`.
    UpTo128,
    /// `> 128 kbps and < 256 kbps`.
    Between129And255,
    /// `>= 256 kbps and <= 320 kbps`.
    Between256And320,
    /// `> 320 kbps`.
    Above320,
}

impl QualityRange {
    /// Every range, ascending — the fixed row order of the widget.
    pub const ALL: [QualityRange; 4] = [
        QualityRange::UpTo128,
        QualityRange::Between129And255,
        QualityRange::Between256And320,
        QualityRange::Above320,
    ];

    fn contains(self, bitrate_kbps: u32) -> bool {
        match self {
            QualityRange::UpTo128 => bitrate_kbps <= 128,
            QualityRange::Between129And255 => bitrate_kbps > 128 && bitrate_kbps < 256,
            QualityRange::Between256And320 => (256..=320).contains(&bitrate_kbps),
            QualityRange::Above320 => bitrate_kbps > 320,
        }
    }
}

/// Quality distribution: the four bitrate buckets plus the count of
/// tracks that carry a bitrate at all (the shares' denominator — tracks
/// with no bitrate are excluded rather than invented into a bucket).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct QualityDistribution {
    pub buckets: [QualityBucket; 4],
    pub total_with_bitrate: usize,
}

/// A genre ranked by total play count.
#[derive(Clone, Debug, PartialEq)]
pub struct GenrePlayCount {
    pub genre: Option<String>,
    /// Weighted play count. A ten-play track tagged `House/Deep`
    /// contributes five plays to each genre.
    pub total_play_count: f64,
}

/// A genre ranked by its total rating points across rated tracks.
#[derive(Clone, Debug, PartialEq)]
pub struct GenreRating {
    pub genre: Option<String>,
    /// Weighted score used for ranking: the sum of stars across every rated
    /// track in the genre. This naturally gives a genre with broad library
    /// support more weight than a one-track marginal genre.
    pub total_stars: f64,
    /// Mean star rating across the genre's rated (one-star-or-better)
    /// tracks.
    pub average_stars: f64,
    /// Weighted rated-track contribution the average is computed over.
    pub rated_track_weight: f64,
}

/// Track count for one release decade. `decade` is the decade's first
/// year (e.g. `1990` for the 1990s); `None` collects tracks with no
/// release year.
#[derive(Clone, Debug, PartialEq)]
pub struct DecadeCount {
    pub decade: Option<i32>,
    pub track_count: usize,
    /// How the decade's tracks split across the genre distribution.
    pub genres: GenreBreakdown,
}

/// Track count for one "year added" bucket. `None` collects tracks with
/// no recorded add date.
#[derive(Clone, Debug, PartialEq)]
pub struct YearCount {
    pub year: Option<i32>,
    pub track_count: usize,
    /// How the year's tracks split across the genre distribution.
    pub genres: GenreBreakdown,
}

/// A subset of the library split across the slices of the
/// [`GenreDistribution`], so a chart of that subset can colour its genres
/// exactly as the genre chart does. Contributions are weighted the same
/// way — a track tagged `House/Deep` gives `0.5` to each — so they sum to
/// the subset's track count.
#[derive(Clone, Debug, PartialEq)]
pub struct GenreBreakdown {
    /// Weighted track contribution per charted genre, index-aligned with
    /// [`GenreDistribution::entries`].
    pub charted: Vec<f64>,
    /// Weighted track contribution of the genres folded into
    /// [`GenreDistribution::other`].
    pub other: f64,
}

/// Compute the whole-library statistics from `tracks`.
///
/// `year_of_added` maps a track's add timestamp to its local calendar
/// year; it returns `None` for instants the calendar can't place (e.g.
/// before the Unix epoch), which fold into the "unknown" year bucket.
pub fn compute_library_statistics(
    tracks: &[Track],
    year_of_added: impl Fn(SystemTime) -> Option<i32>,
) -> LibraryStatistics {
    let spellings = GenreSpellings::of(tracks);
    let genre_distribution = genre_distribution(tracks, &spellings);
    let slots = GenreSlots::of(&genre_distribution);
    let release_decades = release_decades(tracks, &spellings, &slots);
    let added_years = added_years(tracks, &spellings, &slots, year_of_added);
    LibraryStatistics {
        total_tracks: tracks.len(),
        total_plays: tracks.iter().map(|track| track.statistics.play_count).sum(),
        genre_distribution,
        quality_distribution: quality_distribution(tracks),
        most_played_genres: most_played_genres(tracks, &spellings),
        most_liked_genres: most_liked_genres(tracks, &spellings),
        release_decades,
        added_years,
    }
}

/// The genre names a track's tag holds: a slash-delimited tag names
/// several, and blank components name none.
fn tagged_genres(track: &Track) -> impl Iterator<Item = &str> {
    track
        .metadata
        .genre
        .as_deref()
        .into_iter()
        .flat_map(|tag| tag.split('/'))
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
}

/// The spelling each genre is counted and shown under: "Deep House" and
/// "Deep house" are one genre here, as they are everywhere else in the
/// library.
struct GenreSpellings<'a>(Spellings<'a>);

impl<'a> GenreSpellings<'a> {
    fn of(tracks: &'a [Track]) -> Self {
        Self(Spellings::of(tracks.iter().flat_map(tagged_genres)))
    }

    /// The grouping keys of a track: its genres under their shown
    /// spelling, each once, or the untagged bucket (`None`) when its tag
    /// names no genre. The keys borrow from the library: the aggregations
    /// count every track, and own a genre name only once it is ranked.
    fn keys(&self, track: &'a Track) -> Vec<Option<&'a str>> {
        let mut genres = Vec::new();
        for genre in tagged_genres(track) {
            let genre = Some(self.0.shown(genre).unwrap_or(genre));
            if !genres.contains(&genre) {
                genres.push(genre);
            }
        }

        if genres.is_empty() {
            vec![None]
        } else {
            genres
        }
    }

    /// A track's genres with the share of the track each one carries: a
    /// track tagged with several genres counts equally toward each.
    fn weighted(&self, track: &'a Track) -> Vec<(Option<&'a str>, f64)> {
        let keys = self.keys(track);
        let weight = 1.0 / keys.len() as f64;
        keys.into_iter().map(|genre| (genre, weight)).collect()
    }
}

/// Where each genre lands in the genre distribution: the index of its
/// charted slice, or — absent from the map — the folded "Other" tail.
struct GenreSlots<'a>(HashMap<Option<&'a str>, usize>);

impl<'a> GenreSlots<'a> {
    fn of(distribution: &'a GenreDistribution) -> Self {
        Self(
            distribution
                .entries
                .iter()
                .enumerate()
                .map(|(index, share)| (share.genre.as_deref(), index))
                .collect(),
        )
    }

    fn empty_breakdown(&self) -> GenreBreakdown {
        GenreBreakdown {
            charted: vec![0.0; self.0.len()],
            other: 0.0,
        }
    }

    /// Add `track`'s weighted genre contributions to `breakdown`.
    fn add(
        &self,
        breakdown: &mut GenreBreakdown,
        spellings: &GenreSpellings<'a>,
        track: &'a Track,
    ) {
        for (genre, weight) in spellings.weighted(track) {
            match self.0.get(&genre) {
                Some(&index) => breakdown.charted[index] += weight,
                None => breakdown.other += weight,
            }
        }
    }
}

fn compare_weight_desc(left: f64, right: f64) -> Ordering {
    right.partial_cmp(&left).unwrap_or(Ordering::Equal)
}

/// Sort a per-genre `(metric, genre)` ranking in descending metric order,
/// breaking ties by genre so the output is deterministic. Untagged
/// (`None`) genres sort after named ones on a tie.
fn genre_tiebreak(left: &Option<String>, right: &Option<String>) -> std::cmp::Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    }
}

fn genre_distribution<'a>(
    tracks: &'a [Track],
    spellings: &GenreSpellings<'a>,
) -> GenreDistribution {
    let mut counts: HashMap<Option<&str>, f64> = HashMap::new();
    for track in tracks {
        for (genre, weight) in spellings.weighted(track) {
            *counts.entry(genre).or_insert(0.0) += weight;
        }
    }

    let mut ranked: Vec<GenreShare> = counts
        .into_iter()
        .map(|(genre, track_weight)| GenreShare {
            genre: genre.map(str::to_owned),
            track_weight,
        })
        .collect();
    ranked.sort_by(|left, right| {
        compare_weight_desc(left.track_weight, right.track_weight)
            .then_with(|| genre_tiebreak(&left.genre, &right.genre))
    });

    let other = if ranked.len() > GENRE_DISTRIBUTION_TOP_N {
        let tail = &ranked[GENRE_DISTRIBUTION_TOP_N..];
        Some(OtherGenres {
            genre_count: tail.len(),
            track_weight: tail.iter().map(|share| share.track_weight).sum(),
        })
    } else {
        None
    };
    ranked.truncate(GENRE_DISTRIBUTION_TOP_N);

    GenreDistribution {
        entries: ranked,
        other,
        total_tracks: tracks.len(),
    }
}

fn quality_distribution(tracks: &[Track]) -> QualityDistribution {
    let mut buckets = QualityRange::ALL.map(|range| QualityBucket {
        range,
        track_count: 0,
    });
    let mut total_with_bitrate = 0;
    for track in tracks {
        let Some(bitrate) = track.metadata.bitrate_kbps else {
            continue;
        };
        total_with_bitrate += 1;
        if let Some(bucket) = buckets
            .iter_mut()
            .find(|bucket| bucket.range.contains(bitrate))
        {
            bucket.track_count += 1;
        }
    }
    QualityDistribution {
        buckets,
        total_with_bitrate,
    }
}

fn most_played_genres<'a>(
    tracks: &'a [Track],
    spellings: &GenreSpellings<'a>,
) -> Vec<GenrePlayCount> {
    let mut totals: HashMap<Option<&str>, f64> = HashMap::new();
    for track in tracks {
        for (genre, weight) in spellings.weighted(track) {
            *totals.entry(genre).or_insert(0.0) += track.statistics.play_count as f64 * weight;
        }
    }
    let mut ranked: Vec<GenrePlayCount> = totals
        .into_iter()
        // A genre nobody has played is not "most played".
        .filter(|(_, total)| *total > 0.0)
        .map(|(genre, total_play_count)| GenrePlayCount {
            genre: genre.map(str::to_owned),
            total_play_count,
        })
        .collect();
    ranked.sort_by(|left, right| {
        compare_weight_desc(left.total_play_count, right.total_play_count)
            .then_with(|| genre_tiebreak(&left.genre, &right.genre))
    });
    ranked.truncate(TOP_GENRE_RANK);
    ranked
}

fn most_liked_genres<'a>(tracks: &'a [Track], spellings: &GenreSpellings<'a>) -> Vec<GenreRating> {
    // (sum of stars, rated-track weight) per genre, over rated
    // tracks only — zero-star tracks are excluded per the project's
    // rating-as-exclusion convention.
    let mut sums: HashMap<Option<&str>, (f64, f64)> = HashMap::new();
    for track in tracks {
        let stars = track.rating.stars();
        if stars == 0 {
            continue;
        }
        for (genre, weight) in spellings.weighted(track) {
            let entry = sums.entry(genre).or_insert((0.0, 0.0));
            entry.0 += f64::from(stars) * weight;
            entry.1 += weight;
        }
    }

    let mut ranked: Vec<GenreRating> = sums
        .into_iter()
        .map(|(genre, (star_sum, rated_track_weight))| GenreRating {
            genre: genre.map(str::to_owned),
            total_stars: star_sum,
            average_stars: star_sum / rated_track_weight,
            rated_track_weight,
        })
        .collect();
    ranked.sort_by(|left, right| {
        compare_weight_desc(left.total_stars, right.total_stars)
            .then_with(|| {
                right
                    .average_stars
                    .partial_cmp(&left.average_stars)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| compare_weight_desc(left.rated_track_weight, right.rated_track_weight))
            .then_with(|| genre_tiebreak(&left.genre, &right.genre))
    });
    ranked.truncate(TOP_GENRE_RANK);
    ranked
}

/// Fold a calendar year down to its decade's first year (1995 → 1990).
/// `Euclid` division keeps the math correct for any conceivable year.
fn decade_of(year: i32) -> i32 {
    year - year.rem_euclid(10)
}

/// The first year sound was recorded (Scott's phonautograph, 1860): a
/// release year before it is a mistyped tag, not a release.
const FIRST_RECORDING_YEAR: i32 = 1860;

fn release_decades<'a>(
    tracks: &'a [Track],
    spellings: &GenreSpellings<'a>,
    slots: &GenreSlots<'a>,
) -> Vec<DecadeCount> {
    year_buckets(tracks, spellings, slots, |track| {
        track
            .metadata
            .year
            .filter(|year| *year >= FIRST_RECORDING_YEAR)
            .map(decade_of)
    })
    .into_iter()
    .map(|(decade, (track_count, genres))| DecadeCount {
        decade,
        track_count,
        genres,
    })
    .collect()
}

fn added_years<'a>(
    tracks: &'a [Track],
    spellings: &GenreSpellings<'a>,
    slots: &GenreSlots<'a>,
    year_of_added: impl Fn(SystemTime) -> Option<i32>,
) -> Vec<YearCount> {
    year_buckets(tracks, spellings, slots, |track| {
        track.statistics.date_added_at.and_then(&year_of_added)
    })
    .into_iter()
    .map(|(year, (track_count, genres))| YearCount {
        year,
        track_count,
        genres,
    })
    .collect()
}

/// Count tracks per year bucket (`bucket_of`, `None` for "unknown"),
/// splitting each bucket across the genre distribution, in chronological
/// order with the "unknown" bucket last so it reads as a trailing
/// remainder rather than the earliest column.
fn year_buckets<'a>(
    tracks: &'a [Track],
    spellings: &GenreSpellings<'a>,
    slots: &GenreSlots<'a>,
    bucket_of: impl Fn(&Track) -> Option<i32>,
) -> Vec<(Option<i32>, (usize, GenreBreakdown))> {
    let mut counts: HashMap<Option<i32>, (usize, GenreBreakdown)> = HashMap::new();
    for track in tracks {
        let (count, genres) = counts
            .entry(bucket_of(track))
            .or_insert_with(|| (0, slots.empty_breakdown()));
        *count += 1;
        slots.add(genres, spellings, track);
    }
    let mut buckets: Vec<_> = counts.into_iter().collect();
    buckets.sort_by(|(left, _), (right, _)| match (left, right) {
        (Some(left), Some(right)) => left.cmp(right),
        (Some(_), None) => std::cmp::Ordering::Less,
        (None, Some(_)) => std::cmp::Ordering::Greater,
        (None, None) => std::cmp::Ordering::Equal,
    });
    buckets
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use super::*;
    use crate::{PlayStatistics, Rating, TrackLocation, TrackMetadata, TrackRelativePath};
    use crate::{Track, TrackId};

    fn track(id: i64, genre: Option<&str>) -> Track {
        Track {
            id: TrackId::new(id).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("{id}.mp3")).expect("valid path"),
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

    fn rated(track: Track, stars: u8) -> Track {
        Track {
            rating: Rating::new(stars).expect("valid rating"),
            ..track
        }
    }

    fn played(track: Track, plays: u64) -> Track {
        let mut track = track;
        track.statistics.play_count = plays;
        track
    }

    fn with_year(track: Track, year: i32) -> Track {
        let mut track = track;
        track.metadata.year = Some(year);
        track
    }

    fn added_at(track: Track, secs: u64) -> Track {
        let mut track = track;
        track.statistics.date_added_at = Some(UNIX_EPOCH + Duration::from_secs(secs));
        track
    }

    fn with_bitrate(track: Track, kbps: u32) -> Track {
        let mut track = track;
        track.metadata.bitrate_kbps = Some(kbps);
        track
    }

    /// A calendar that treats each whole `secs` value as its own "year"
    /// for deterministic, timezone-free tests.
    fn fake_year(time: SystemTime) -> Option<i32> {
        let secs = time.duration_since(UNIX_EPOCH).ok()?.as_secs();
        i32::try_from(secs).ok()
    }

    fn compute(tracks: &[Track]) -> LibraryStatistics {
        compute_library_statistics(tracks, fake_year)
    }

    fn counts_per_decade(decades: &[DecadeCount]) -> Vec<(Option<i32>, usize)> {
        decades
            .iter()
            .map(|decade| (decade.decade, decade.track_count))
            .collect()
    }

    fn assert_near(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < f64::EPSILON,
            "expected {expected}, got {actual}",
        );
    }

    #[test]
    fn empty_library_yields_zeroed_statistics() {
        let stats = compute(&[]);
        assert_eq!(stats.total_tracks, 0);
        assert_eq!(stats.total_plays, 0);
        assert!(stats.genre_distribution.entries.is_empty());
        assert_eq!(stats.genre_distribution.other, None);
        assert_eq!(stats.quality_distribution.total_with_bitrate, 0);
        assert!(stats.most_played_genres.is_empty());
        assert!(stats.most_liked_genres.is_empty());
        assert!(stats.release_decades.is_empty());
        assert!(stats.added_years.is_empty());
    }

    #[test]
    fn genre_distribution_ranks_by_count_and_folds_the_tail() {
        // 14 distinct genres so the 12-entry cap leaves a two-genre tail.
        let mut tracks = Vec::new();
        let mut id = 1;
        // Genre "g00" gets the most tracks, "g13" the fewest, so rank is
        // predictable.
        for genre_index in 0..14 {
            let copies = 14 - genre_index;
            for _ in 0..copies {
                tracks.push(track(id, Some(&format!("g{genre_index:02}"))));
                id += 1;
            }
        }
        let dist = compute(&tracks).genre_distribution;

        assert_eq!(dist.total_tracks, tracks.len());
        assert_eq!(dist.entries.len(), GENRE_DISTRIBUTION_TOP_N);
        assert_eq!(dist.entries[0].genre.as_deref(), Some("g00"));
        assert_near(dist.entries[0].track_weight, 14.0);
        // The two smallest genres (g12 = 2 tracks, g13 = 1 track) fold.
        let other = dist.other.expect("a folded tail");
        assert_eq!(other.genre_count, 2);
        assert_near(other.track_weight, 3.0);
    }

    #[test]
    fn genre_distribution_groups_untagged_tracks_as_none() {
        let tracks = [
            track(1, Some("Rock")),
            track(2, None),
            track(3, Some("   ")),
            track(4, None),
        ];
        let dist = compute(&tracks).genre_distribution;

        let untagged = dist
            .entries
            .iter()
            .find(|share| share.genre.is_none())
            .expect("an untagged bucket");
        // Both the `None` genre and the whitespace-only genre collapse
        // into the single untagged bucket.
        assert_near(untagged.track_weight, 3.0);
    }

    #[test]
    fn genre_distribution_splits_slash_delimited_genres() {
        let tracks = [
            track(1, Some("House/Deep")),
            track(2, Some("House")),
            track(3, Some("Techno / Ambient / ")),
            track(4, Some("House/House")),
        ];
        let dist = compute(&tracks).genre_distribution;
        let weight = |genre_name: &str| {
            dist.entries
                .iter()
                .find(|share| share.genre.as_deref() == Some(genre_name))
                .expect("genre present")
                .track_weight
        };

        assert_near(weight("House"), 2.5);
        assert_near(weight("Deep"), 0.5);
        assert_near(weight("Techno"), 0.5);
        assert_near(weight("Ambient"), 0.5);
    }

    #[test]
    fn spellings_of_one_genre_count_together_under_the_most_used_one() {
        let tracks = [
            played(track(1, Some("Deep House")), 3),
            played(track(2, Some("Deep House")), 1),
            rated(played(track(3, Some("deep house")), 4), 5),
            track(4, Some("DEEP HOUSE / Deep House")),
            // Equally used spellings: the first in byte order is shown.
            track(5, Some("trip hop")),
            track(6, Some("Trip Hop")),
            // An accent makes a different word, not a different spelling.
            track(7, Some("Deep Hous\u{e9}")),
        ];
        let stats = compute(&tracks);
        let shares: Vec<(Option<&str>, f64)> = stats
            .genre_distribution
            .entries
            .iter()
            .map(|share| (share.genre.as_deref(), share.track_weight))
            .collect();

        assert_eq!(
            shares,
            [
                (Some("Deep House"), 4.0),
                (Some("Trip Hop"), 2.0),
                (Some("Deep Hous\u{e9}"), 1.0),
            ]
        );
        assert_eq!(stats.most_played_genres.len(), 1);
        assert_eq!(
            stats.most_played_genres[0].genre.as_deref(),
            Some("Deep House")
        );
        assert_near(stats.most_played_genres[0].total_play_count, 8.0);
        assert_eq!(
            stats.most_liked_genres[0].genre.as_deref(),
            Some("Deep House")
        );
    }

    #[test]
    fn quality_distribution_respects_the_bitrate_boundaries() {
        let tracks = [
            with_bitrate(track(1, None), 128), // UpTo128 (inclusive upper)
            with_bitrate(track(2, None), 64),  // UpTo128
            with_bitrate(track(3, None), 129), // Between129And255 (lower)
            with_bitrate(track(4, None), 255), // Between129And255 (upper)
            with_bitrate(track(5, None), 256), // Between256And320 (lower)
            with_bitrate(track(6, None), 320), // Between256And320 (upper)
            with_bitrate(track(7, None), 321), // Above320
            with_bitrate(track(8, None), 1411),
            track(9, None), // no bitrate — excluded
        ];
        let dist = compute(&tracks).quality_distribution;

        assert_eq!(dist.total_with_bitrate, 8);
        let count = |range: QualityRange| {
            dist.buckets
                .iter()
                .find(|bucket| bucket.range == range)
                .expect("bucket present")
                .track_count
        };
        assert_eq!(count(QualityRange::UpTo128), 2);
        assert_eq!(count(QualityRange::Between129And255), 2);
        assert_eq!(count(QualityRange::Between256And320), 2);
        assert_eq!(count(QualityRange::Above320), 2);
    }

    #[test]
    fn most_played_genres_rank_by_total_plays_and_exclude_unplayed() {
        let tracks = [
            played(track(1, Some("House")), 10),
            played(track(2, Some("House")), 5),
            played(track(3, Some("Techno")), 20),
            track(4, Some("Ambient")), // zero plays — excluded
        ];
        let ranked = compute(&tracks).most_played_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("Techno"));
        assert_near(ranked[0].total_play_count, 20.0);
        assert_eq!(ranked[1].genre.as_deref(), Some("House"));
        assert_near(ranked[1].total_play_count, 15.0);
    }

    #[test]
    fn most_played_genres_split_track_plays_across_slash_genres() {
        let tracks = [
            played(track(1, Some("House/Deep")), 10),
            played(track(2, Some("Deep")), 1),
        ];
        let ranked = compute(&tracks).most_played_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("Deep"));
        assert_near(ranked[0].total_play_count, 6.0);
        assert_eq!(ranked[1].genre.as_deref(), Some("House"));
        assert_near(ranked[1].total_play_count, 5.0);
    }

    #[test]
    fn most_liked_genres_weight_rating_points_without_a_sample_cutoff() {
        let mut tracks = Vec::new();
        let mut id = 1;
        // "Soul": five rated tracks averaging 4 stars.
        for stars in [5, 5, 4, 3, 3] {
            tracks.push(rated(track(id, Some("Soul")), stars));
            id += 1;
        }
        // "Jazz": a single five-star track remains visible but ranks below
        // the broadly liked genre.
        tracks.push(rated(track(id, Some("Jazz")), 5));
        id += 1;
        // "Soul" also has a zero-star track that must not drag the mean.
        tracks.push(rated(track(id, Some("Soul")), 0));

        let ranked = compute(&tracks).most_liked_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("Soul"));
        assert_near(ranked[0].total_stars, 20.0);
        assert_near(ranked[0].rated_track_weight, 5.0);
        assert_near(ranked[0].average_stars, 4.0);
        assert_eq!(ranked[1].genre.as_deref(), Some("Jazz"));
        assert_near(ranked[1].total_stars, 5.0);
    }

    #[test]
    fn most_liked_genres_rank_by_total_rating_points() {
        let mut tracks = Vec::new();
        let mut id = 1;
        // "A": five tracks averaging 5.0, 25 total rating points.
        for _ in 0..5 {
            tracks.push(rated(track(id, Some("A")), 5));
            id += 1;
        }
        // "B": seven tracks averaging 4.0, 28 total rating points. Its
        // broader support wins despite the lower average.
        for _ in 0..7 {
            tracks.push(rated(track(id, Some("B")), 4));
            id += 1;
        }
        let ranked = compute(&tracks).most_liked_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("B"));
        assert_eq!(ranked[1].genre.as_deref(), Some("A"));
    }

    #[test]
    fn most_liked_genres_split_rating_points_without_lowering_the_average() {
        let tracks = [
            rated(track(1, Some("House/Deep")), 4),
            rated(track(2, Some("Deep")), 5),
        ];
        let ranked = compute(&tracks).most_liked_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("Deep"));
        assert_near(ranked[0].total_stars, 7.0);
        assert_near(ranked[0].rated_track_weight, 1.5);
        assert_near(ranked[0].average_stars, 7.0 / 1.5);
        assert_eq!(ranked[1].genre.as_deref(), Some("House"));
        assert_near(ranked[1].total_stars, 2.0);
        assert_near(ranked[1].rated_track_weight, 0.5);
        assert_near(ranked[1].average_stars, 4.0);
    }

    #[test]
    fn release_decades_bucket_by_decade_with_unknown_last() {
        let tracks = [
            with_year(track(1, None), 1991),
            with_year(track(2, None), 1999),
            with_year(track(3, None), 2003),
            track(4, None), // no year
        ];
        let decades = compute(&tracks).release_decades;

        assert_eq!(
            counts_per_decade(&decades),
            [(Some(1990), 2), (Some(2000), 1), (None, 1)]
        );
    }

    #[test]
    fn release_years_before_recorded_sound_are_unknown() {
        let tracks = [
            with_year(track(1, None), 203),
            with_year(track(2, None), 1859),
            with_year(track(3, None), 1860),
        ];
        let decades = compute(&tracks).release_decades;

        assert_eq!(counts_per_decade(&decades), [(Some(1860), 1), (None, 2)]);
    }

    #[test]
    fn total_plays_sum_every_track() {
        let tracks = [played(track(1, None), 3), played(track(2, None), 4)];
        assert_eq!(compute(&tracks).total_plays, 7);
    }

    #[test]
    fn added_years_bucket_per_year_via_the_calendar_closure() {
        let tracks = [
            added_at(track(1, None), 2014),
            added_at(track(2, None), 2014),
            added_at(track(3, None), 2020),
            track(4, None), // no add date
        ];
        let years = compute(&tracks).added_years;

        assert_eq!(
            years
                .iter()
                .map(|year| (year.year, year.track_count))
                .collect::<Vec<_>>(),
            [(Some(2014), 2), (Some(2020), 1), (None, 1)]
        );
    }

    #[test]
    fn year_buckets_split_across_the_genre_distribution_slices() {
        // Fourteen genres, so the two smallest (g12, g13) fold into Other;
        // every track is released in the 1990s except one in the 2000s.
        let mut tracks = Vec::new();
        let mut id = 1;
        for genre_index in 0..14 {
            for _ in 0..(14 - genre_index) {
                tracks.push(with_year(
                    track(id, Some(&format!("g{genre_index:02}"))),
                    1995,
                ));
                id += 1;
            }
        }
        tracks.push(with_year(track(id, Some("g00/g13")), 2001));
        let stats = compute(&tracks);
        let slot = |genre: &str| {
            stats
                .genre_distribution
                .entries
                .iter()
                .position(|share| share.genre.as_deref() == Some(genre))
                .expect("charted genre")
        };

        let nineties = &stats.release_decades[0].genres;
        assert_eq!(nineties.charted.len(), GENRE_DISTRIBUTION_TOP_N);
        assert_near(nineties.charted[slot("g00")], 14.0);
        assert_near(nineties.charted[slot("g11")], 3.0);
        // g12 (2 tracks) and g13 (1 track) fold together.
        assert_near(nineties.other, 3.0);

        // The multi-genre track splits between a charted slice and Other.
        let noughties = &stats.release_decades[1].genres;
        assert_near(noughties.charted[slot("g00")], 0.5);
        assert_near(noughties.other, 0.5);
        assert_near(noughties.charted.iter().sum::<f64>() + noughties.other, 1.0);
    }

    #[test]
    fn year_buckets_count_untagged_tracks_in_the_unknown_slice() {
        let tracks = [
            added_at(track(1, None), 2014),
            added_at(track(2, Some("House")), 2014),
        ];
        let stats = compute(&tracks);
        let unknown = stats
            .genre_distribution
            .entries
            .iter()
            .position(|share| share.genre.is_none())
            .expect("untagged slice");

        let genres = &stats.added_years[0].genres;
        assert_near(genres.charted[unknown], 1.0);
        assert_near(genres.charted.iter().sum::<f64>(), 2.0);
        assert_near(genres.other, 0.0);
    }
}
