// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Library-wide diagnostic statistics (issue #20).
//!
//! Pure aggregation over the in-memory track list — which is the
//! authoritative SQLite copy the rest of the app already holds, so these
//! figures never re-read file tags. The presentation layer renders the
//! returned struct; every selection rule (top-N folding, the
//! "most liked" minimum-sample threshold, zero-star exclusion, decade
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

use std::collections::HashMap;
use std::time::SystemTime;

use crate::Track;

/// Genres shown individually in the genre-distribution chart before the
/// long tail collapses into a single "Other" entry.
const GENRE_DISTRIBUTION_TOP_N: usize = 12;

/// Length of the "most played" / "most liked" genre rankings.
const TOP_GENRE_RANK: usize = 5;

/// A genre needs at least this many rated tracks before it can appear in
/// the "most liked" ranking, so a lone five-star track in a tiny genre
/// can't top the chart.
const MIN_RATED_TRACKS_FOR_LIKED: usize = 5;

/// The whole-library statistics shown on the Statistics screen.
#[derive(Clone, Debug, PartialEq)]
pub struct LibraryStatistics {
    /// Total number of tracks in the library (the denominator for the
    /// genre-distribution shares).
    pub total_tracks: usize,
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

/// One genre's slice of the library, by track count.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenreShare {
    /// The genre name, or `None` for tracks with no genre tag.
    pub genre: Option<String>,
    /// Number of tracks carrying this genre.
    pub track_count: usize,
}

/// The aggregated tail of the genre distribution beyond the top N.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct OtherGenres {
    /// How many distinct genres were folded together.
    pub genre_count: usize,
    /// How many tracks those genres hold in total.
    pub track_count: usize,
}

/// Genre distribution: the largest genres individually (capped at a fixed
/// count), plus an optional folded tail. `total_tracks` is the
/// whole-library count (including untagged tracks) so each share is a
/// fraction of the library, not of the charted subset.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenrePlayCount {
    pub genre: Option<String>,
    pub total_play_count: u64,
}

/// A genre ranked by how highly its rated tracks are rated.
#[derive(Clone, Debug, PartialEq)]
pub struct GenreRating {
    pub genre: Option<String>,
    /// Mean star rating across the genre's rated (one-star-or-better)
    /// tracks.
    pub average_stars: f64,
    /// How many rated tracks the average is computed over.
    pub rated_track_count: usize,
}

/// Track count for one release decade. `decade` is the decade's first
/// year (e.g. `1990` for the 1990s); `None` collects tracks with no
/// release year.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DecadeCount {
    pub decade: Option<i32>,
    pub track_count: usize,
}

/// Track count for one "year added" bucket. `None` collects tracks with
/// no recorded add date.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct YearCount {
    pub year: Option<i32>,
    pub track_count: usize,
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
    LibraryStatistics {
        total_tracks: tracks.len(),
        genre_distribution: genre_distribution(tracks),
        quality_distribution: quality_distribution(tracks),
        most_played_genres: most_played_genres(tracks),
        most_liked_genres: most_liked_genres(tracks),
        release_decades: release_decades(tracks),
        added_years: added_years(tracks, year_of_added),
    }
}

/// Normalise a track's genre into a grouping key: trimmed, with empty or
/// whitespace-only tags treated as "no genre" (`None`).
fn genre_key(track: &Track) -> Option<String> {
    track
        .metadata
        .genre
        .as_deref()
        .map(str::trim)
        .filter(|genre| !genre.is_empty())
        .map(str::to_owned)
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

fn genre_distribution(tracks: &[Track]) -> GenreDistribution {
    let mut counts: HashMap<Option<String>, usize> = HashMap::new();
    for track in tracks {
        *counts.entry(genre_key(track)).or_insert(0) += 1;
    }

    let mut ranked: Vec<GenreShare> = counts
        .into_iter()
        .map(|(genre, track_count)| GenreShare { genre, track_count })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .track_count
            .cmp(&left.track_count)
            .then_with(|| genre_tiebreak(&left.genre, &right.genre))
    });

    let other = if ranked.len() > GENRE_DISTRIBUTION_TOP_N {
        let tail = &ranked[GENRE_DISTRIBUTION_TOP_N..];
        Some(OtherGenres {
            genre_count: tail.len(),
            track_count: tail.iter().map(|share| share.track_count).sum(),
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

fn most_played_genres(tracks: &[Track]) -> Vec<GenrePlayCount> {
    let mut totals: HashMap<Option<String>, u64> = HashMap::new();
    for track in tracks {
        *totals.entry(genre_key(track)).or_insert(0) += track.statistics.play_count;
    }
    let mut ranked: Vec<GenrePlayCount> = totals
        .into_iter()
        // A genre nobody has played is not "most played".
        .filter(|(_, total)| *total > 0)
        .map(|(genre, total_play_count)| GenrePlayCount {
            genre,
            total_play_count,
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .total_play_count
            .cmp(&left.total_play_count)
            .then_with(|| genre_tiebreak(&left.genre, &right.genre))
    });
    ranked.truncate(TOP_GENRE_RANK);
    ranked
}

fn most_liked_genres(tracks: &[Track]) -> Vec<GenreRating> {
    // (sum of stars, number of rated tracks) per genre, over rated
    // tracks only — zero-star tracks are excluded per the project's
    // rating-as-exclusion convention.
    let mut sums: HashMap<Option<String>, (u64, usize)> = HashMap::new();
    for track in tracks {
        let stars = track.rating.stars();
        if stars == 0 {
            continue;
        }
        let entry = sums.entry(genre_key(track)).or_insert((0, 0));
        entry.0 += u64::from(stars);
        entry.1 += 1;
    }

    let mut ranked: Vec<GenreRating> = sums
        .into_iter()
        .filter(|(_, (_, rated_track_count))| *rated_track_count >= MIN_RATED_TRACKS_FOR_LIKED)
        .map(|(genre, (star_sum, rated_track_count))| GenreRating {
            genre,
            average_stars: star_sum as f64 / rated_track_count as f64,
            rated_track_count,
        })
        .collect();
    ranked.sort_by(|left, right| {
        right
            .average_stars
            .partial_cmp(&left.average_stars)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| right.rated_track_count.cmp(&left.rated_track_count))
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

fn release_decades(tracks: &[Track]) -> Vec<DecadeCount> {
    let mut counts: HashMap<Option<i32>, usize> = HashMap::new();
    for track in tracks {
        let decade = track.metadata.year.map(decade_of);
        *counts.entry(decade).or_insert(0) += 1;
    }
    sorted_year_buckets(counts)
        .into_iter()
        .map(|(decade, track_count)| DecadeCount {
            decade,
            track_count,
        })
        .collect()
}

fn added_years(
    tracks: &[Track],
    year_of_added: impl Fn(SystemTime) -> Option<i32>,
) -> Vec<YearCount> {
    let mut counts: HashMap<Option<i32>, usize> = HashMap::new();
    for track in tracks {
        let year = track.statistics.date_added_at.and_then(&year_of_added);
        *counts.entry(year).or_insert(0) += 1;
    }
    sorted_year_buckets(counts)
        .into_iter()
        .map(|(year, track_count)| YearCount { year, track_count })
        .collect()
}

/// Order year/decade buckets chronologically, with the "unknown"
/// (`None`) bucket last so it reads as a trailing remainder rather than
/// the earliest column.
fn sorted_year_buckets(counts: HashMap<Option<i32>, usize>) -> Vec<(Option<i32>, usize)> {
    let mut buckets: Vec<(Option<i32>, usize)> = counts.into_iter().collect();
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

    #[test]
    fn empty_library_yields_zeroed_statistics() {
        let stats = compute(&[]);
        assert_eq!(stats.total_tracks, 0);
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
        assert_eq!(dist.entries[0].track_count, 14);
        // The two smallest genres (g12 = 2 tracks, g13 = 1 track) fold.
        let other = dist.other.expect("a folded tail");
        assert_eq!(other.genre_count, 2);
        assert_eq!(other.track_count, 3);
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
        assert_eq!(untagged.track_count, 3);
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
        assert_eq!(ranked[0].total_play_count, 20);
        assert_eq!(ranked[1].genre.as_deref(), Some("House"));
        assert_eq!(ranked[1].total_play_count, 15);
    }

    #[test]
    fn most_liked_genres_need_the_minimum_rated_sample() {
        let mut tracks = Vec::new();
        let mut id = 1;
        // "Soul": five rated tracks averaging 4 stars — qualifies.
        for stars in [5, 5, 4, 3, 3] {
            tracks.push(rated(track(id, Some("Soul")), stars));
            id += 1;
        }
        // "Jazz": a single five-star track — below the threshold.
        tracks.push(rated(track(id, Some("Jazz")), 5));
        id += 1;
        // "Soul" also has a zero-star track that must not drag the mean.
        tracks.push(rated(track(id, Some("Soul")), 0));

        let ranked = compute(&tracks).most_liked_genres;

        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].genre.as_deref(), Some("Soul"));
        assert_eq!(ranked[0].rated_track_count, 5);
        assert!((ranked[0].average_stars - 4.0).abs() < f64::EPSILON);
    }

    #[test]
    fn most_liked_genres_rank_by_average_then_sample_size() {
        let mut tracks = Vec::new();
        let mut id = 1;
        // "A": five tracks averaging 5.0.
        for _ in 0..5 {
            tracks.push(rated(track(id, Some("A")), 5));
            id += 1;
        }
        // "B": six tracks averaging 4.0 — lower average, ranks below A.
        for _ in 0..6 {
            tracks.push(rated(track(id, Some("B")), 4));
            id += 1;
        }
        let ranked = compute(&tracks).most_liked_genres;

        assert_eq!(ranked.len(), 2);
        assert_eq!(ranked[0].genre.as_deref(), Some("A"));
        assert_eq!(ranked[1].genre.as_deref(), Some("B"));
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
            decades,
            vec![
                DecadeCount {
                    decade: Some(1990),
                    track_count: 2,
                },
                DecadeCount {
                    decade: Some(2000),
                    track_count: 1,
                },
                DecadeCount {
                    decade: None,
                    track_count: 1,
                },
            ]
        );
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
            years,
            vec![
                YearCount {
                    year: Some(2014),
                    track_count: 2,
                },
                YearCount {
                    year: Some(2020),
                    track_count: 1,
                },
                YearCount {
                    year: None,
                    track_count: 1,
                },
            ]
        );
    }
}
