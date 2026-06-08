// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::cmp::Ordering;
use std::collections::HashMap;

pub use sustain_domain::{
    LibraryQuery, SortDirection, Track, TrackId, TrackSort, TrackSortColumn, compare_optional_text,
    normalize_search_text,
};

/// Joins a track's searchable fields in its precomputed document. Chosen
/// because a single-line search query never contains it, so
/// `document.contains(query)` can never span two fields — preserving the
/// "matches some single field" semantics of the per-field scan it replaced.
const FIELD_SEPARATOR: char = '\n';

/// A precomputed, normalized search document per track, keyed by id.
///
/// Each track contributes one lowercased string concatenating its
/// searchable fields, built once on insert/rebuild rather than re-cloned
/// and re-lowercased on every query. Matching a query is then a single
/// substring test with no per-track allocation — the index, not the query
/// path, owns the normalization cost. The owner keeps it in sync with the
/// library (rebuild on a wholesale change, insert on a per-track update,
/// remove on deletion).
#[derive(Debug, Default)]
pub struct SearchIndex {
    documents: HashMap<TrackId, String>,
}

impl SearchIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Replace the whole index from `tracks` (after a scan/import/reconcile).
    pub fn rebuild(&mut self, tracks: &[Track]) {
        self.documents.clear();
        self.documents.reserve(tracks.len());
        for track in tracks {
            self.documents.insert(track.id, build_document(track));
        }
    }

    /// Insert or replace one track's document (after a per-track update).
    pub fn insert(&mut self, track: &Track) {
        self.documents.insert(track.id, build_document(track));
    }

    /// Drop a track's document (after deletion).
    pub fn remove(&mut self, track_id: TrackId) {
        self.documents.remove(&track_id);
    }

    pub fn len(&self) -> usize {
        self.documents.len()
    }

    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Test a track against an ALREADY-normalized query (see
    /// [`normalize_query`]). A track absent from the index never matches; an
    /// empty query is the caller's responsibility to short-circuit.
    pub fn matches(&self, track_id: TrackId, normalized_query: &str) -> bool {
        self.documents
            .get(&track_id)
            .is_some_and(|document| document.contains(normalized_query))
    }
}

/// Normalize a raw search query once so it can be reused across many track
/// tests. The same normalization is baked into each indexed document.
pub fn normalize_query(query: &str) -> String {
    normalize_search_text(query)
}

pub fn filter_tracks_by_search_text(tracks: &[Track], search_text: &str) -> Vec<Track> {
    let normalized_search = normalize_query(search_text);
    if normalized_search.is_empty() {
        return tracks.to_vec();
    }

    tracks
        .iter()
        .filter(|track| build_document(track).contains(&normalized_search))
        .cloned()
        .collect()
}

pub fn track_matches_search_text(track: &Track, search_text: &str) -> bool {
    let normalized_search = normalize_query(search_text);
    normalized_search.is_empty() || build_document(track).contains(&normalized_search)
}

/// Album-level search: matches against the album's title, artist, and year.
/// Used by the Albums grid view, which intentionally does NOT search track
/// titles — typing a track title in Albums view returning no albums is the
/// agreed behavior (the user can switch to Songs view for that).
///
/// Caller passes the raw album-level fields so this function does not have
/// to know about the GTK view-model type.
pub fn album_matches_search_text(
    album_title: &str,
    album_artist: &str,
    album_year: Option<i32>,
    search_text: &str,
) -> bool {
    let normalized_search = normalize_query(search_text);
    if normalized_search.is_empty() {
        return true;
    }
    let year_text = album_year.map(|year| year.to_string()).unwrap_or_default();
    [album_title, album_artist, year_text.as_str()]
        .iter()
        .any(|field| normalize_query(field).contains(&normalized_search))
}

pub fn sort_tracks(mut tracks: Vec<Track>, sort: TrackSort) -> Vec<Track> {
    if sort.column == TrackSortColumn::PlaylistPosition {
        return tracks;
    }

    tracks.sort_by(|left, right| compare_tracks(left, right, sort));
    tracks
}

fn compare_tracks(left: &Track, right: &Track, sort: TrackSort) -> Ordering {
    let ordering = match sort.column {
        TrackSortColumn::PlaylistPosition => Ordering::Equal,
        TrackSortColumn::Title => compare_optional_text(
            left.metadata.title.as_deref(),
            right.metadata.title.as_deref(),
        ),
        TrackSortColumn::Artist => compare_optional_text(
            left.metadata.artist.as_deref(),
            right.metadata.artist.as_deref(),
        ),
        TrackSortColumn::Album => compare_optional_text(
            left.metadata.album.as_deref(),
            right.metadata.album.as_deref(),
        ),
        TrackSortColumn::Genre => compare_optional_text(
            left.metadata.genre.as_deref(),
            right.metadata.genre.as_deref(),
        ),
        TrackSortColumn::Rating => left.rating.cmp(&right.rating),
        TrackSortColumn::PlayCount => left.statistics.play_count.cmp(&right.statistics.play_count),
        TrackSortColumn::LastPlayed => left
            .statistics
            .last_played_at
            .cmp(&right.statistics.last_played_at),
        TrackSortColumn::Duration => left.metadata.duration.cmp(&right.metadata.duration),
        TrackSortColumn::DateAdded => left
            .statistics
            .date_added_at
            .cmp(&right.statistics.date_added_at),
    };

    let ordering = match sort.direction {
        SortDirection::Ascending => ordering,
        SortDirection::Descending => ordering.reverse(),
    };

    ordering.then_with(|| left.id.cmp(&right.id))
}

/// Build a track's normalized search document: each non-empty searchable
/// field, trimmed and lowercased, joined by [`FIELD_SEPARATOR`]. This is
/// the single source of truth for "what text a track is searchable by",
/// shared by [`SearchIndex`] and the free match functions.
fn build_document(track: &Track) -> String {
    let metadata = &track.metadata;
    let mut document = String::new();
    for field in [
        metadata.title.as_deref(),
        metadata.artist.as_deref(),
        metadata.album.as_deref(),
        metadata.album_artist.as_deref(),
        metadata.composer.as_deref(),
        metadata.genre.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        push_field(&mut document, field);
    }
    push_field(&mut document, &track.location.path().to_string_lossy());
    document
}

fn push_field(document: &mut String, field: &str) {
    let trimmed = field.trim();
    if trimmed.is_empty() {
        return;
    }
    if !document.is_empty() {
        document.push(FIELD_SEPARATOR);
    }
    document.push_str(&normalize_search_text(trimmed));
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sustain_domain::{
        PlayStatistics, Rating, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::{
        SearchIndex, album_matches_search_text, filter_tracks_by_search_text, normalize_query,
        sort_tracks, track_matches_search_text,
    };
    use crate::Track;
    use crate::{SortDirection, TrackSort, TrackSortColumn};

    #[test]
    fn index_matches_the_same_fields_as_the_free_function() {
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        track.metadata.genre = Some("Trip Hop".to_owned());
        track.location = track_location("Massive Attack/track.flac");

        let mut index = SearchIndex::new();
        index.insert(&track);

        for query in ["angel", "MEZZANINE", "trip hop", "massive attack"] {
            assert!(
                index.matches(track.id, &normalize_query(query)),
                "index should match {query:?}"
            );
            // Parity with the per-track free function it replaces.
            assert!(track_matches_search_text(&track, query));
        }
        assert!(!index.matches(track.id, &normalize_query("portishead")));
    }

    #[test]
    fn index_insert_reflects_edited_metadata_immediately() {
        let mut track = track(1, "Before", "Artist");
        // Neutral path so the title is the only carrier of "before"/"after".
        track.location = track_location("song.flac");
        let mut index = SearchIndex::new();
        index.insert(&track);
        assert!(index.matches(track.id, &normalize_query("before")));

        track.metadata.title = Some("After".to_owned());
        index.insert(&track);
        assert!(index.matches(track.id, &normalize_query("after")));
        assert!(
            !index.matches(track.id, &normalize_query("before")),
            "the stale title must no longer match after an edit"
        );
    }

    #[test]
    fn index_rebuild_and_remove_track_documents() {
        let tracks = vec![
            track(1, "Angel", "Massive Attack"),
            track(2, "Roads", "Portishead"),
        ];
        let mut index = SearchIndex::new();
        index.rebuild(&tracks);
        assert_eq!(index.len(), 2);
        assert!(index.matches(track_id(2), &normalize_query("portishead")));

        index.remove(track_id(2));
        assert_eq!(index.len(), 1);
        assert!(
            !index.matches(track_id(2), &normalize_query("portishead")),
            "a removed track must never match"
        );
    }

    #[test]
    fn index_does_not_match_a_query_spanning_two_fields() {
        // "attack" (artist) and "mezzanine" (album) are distinct fields; a
        // query straddling them must not match, matching the per-field
        // semantics of the scan the index replaced.
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        let mut index = SearchIndex::new();
        index.insert(&track);
        assert!(!index.matches(track.id, &normalize_query("attack mezzanine")));
    }

    #[test]
    fn blank_search_returns_all_tracks() {
        let tracks = vec![track(1, "Angel", "Massive Attack")];

        assert_eq!(filter_tracks_by_search_text(&tracks, "   "), tracks);
    }

    #[test]
    fn search_matches_track_title_case_insensitively() {
        let track = track(1, "Angel", "Massive Attack");

        assert!(track_matches_search_text(&track, "angel"));
        assert!(track_matches_search_text(&track, "ANGEL"));
    }

    #[test]
    fn search_folds_accents_in_queries_and_indexed_fields() {
        let track = track(1, "Déjà Vu", "Beyoncé");
        let mut index = SearchIndex::new();
        index.insert(&track);

        for query in ["deja vu", "déjà vu", "beyonce", "BEYONCÉ"] {
            assert!(track_matches_search_text(&track, query));
            assert!(index.matches(track.id, &normalize_query(query)));
        }
    }

    #[test]
    fn search_matches_artist_album_genre_and_path() {
        let mut track = track(1, "Untitled", "Unknown");
        track.metadata.album = Some("Mezzanine".to_owned());
        track.metadata.genre = Some("Trip Hop".to_owned());
        track.location = track_location("Massive Attack/track.flac");

        assert!(track_matches_search_text(&track, "mezzanine"));
        assert!(track_matches_search_text(&track, "trip hop"));
        assert!(track_matches_search_text(&track, "massive attack"));
    }

    #[test]
    fn search_excludes_tracks_without_a_matching_field() {
        let tracks = vec![
            track(1, "Angel", "Massive Attack"),
            track(2, "Roads", "Portishead"),
        ];

        assert_eq!(
            filter_tracks_by_search_text(&tracks, "port"),
            vec![track(2, "Roads", "Portishead")]
        );
    }

    #[test]
    fn album_blank_search_matches_anything() {
        assert!(album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "   "
        ));
    }

    #[test]
    fn album_search_matches_title_case_insensitively() {
        assert!(album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "MEZZ",
        ));
    }

    #[test]
    fn album_search_matches_artist() {
        assert!(album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "massive",
        ));
    }

    #[test]
    fn album_search_folds_accents() {
        assert!(album_matches_search_text(
            "Déjà Vu",
            "Beyoncé",
            Some(2006),
            "beyonce",
        ));
    }

    #[test]
    fn album_search_matches_year() {
        assert!(album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "1998",
        ));
    }

    #[test]
    fn album_search_does_not_match_track_titles() {
        // The caller deliberately does not pass track-level info; this
        // function only knows about album-level fields. Confirms the
        // documented contract.
        assert!(!album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "angel",
        ));
    }

    #[test]
    fn album_search_excludes_non_matching_album() {
        assert!(!album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "portishead",
        ));
    }

    #[test]
    fn sort_orders_tracks_by_text_columns_case_insensitively() {
        let tracks = vec![
            track(1, "zebra", "Artist"),
            track(2, "Alpha", "Artist"),
            track(3, "middle", "Artist"),
        ];

        assert_eq!(
            sort_tracks(
                tracks,
                TrackSort {
                    column: TrackSortColumn::Title,
                    direction: SortDirection::Ascending
                }
            ),
            vec![
                track(2, "Alpha", "Artist"),
                track(3, "middle", "Artist"),
                track(1, "zebra", "Artist")
            ]
        );
    }

    #[test]
    fn sort_supports_descending_rating() {
        let mut low = track(1, "Low", "Artist");
        low.rating = rating(1);
        let mut high = track(2, "High", "Artist");
        high.rating = rating(5);

        assert_eq!(
            sort_tracks(
                vec![low.clone(), high.clone()],
                TrackSort {
                    column: TrackSortColumn::Rating,
                    direction: SortDirection::Descending
                }
            ),
            vec![high, low]
        );
    }

    #[test]
    fn sort_orders_tracks_by_date_added_chronologically() {
        use std::time::{Duration, UNIX_EPOCH};

        let mut older = track(1, "Older", "Artist");
        older.statistics.date_added_at = Some(UNIX_EPOCH + Duration::from_secs(1_000));
        let mut newer = track(2, "Newer", "Artist");
        newer.statistics.date_added_at = Some(UNIX_EPOCH + Duration::from_secs(2_000));

        assert_eq!(
            sort_tracks(
                vec![newer.clone(), older.clone()],
                TrackSort {
                    column: TrackSortColumn::DateAdded,
                    direction: SortDirection::Ascending
                }
            ),
            vec![older, newer]
        );
    }

    fn track(id: i64, title: &str, artist: &str) -> Track {
        Track {
            id: track_id(id),
            location: track_location(&format!("{title}.flac")),
            metadata: TrackMetadata {
                title: Some(title.to_owned()),
                artist: Some(artist.to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn track_id(value: i64) -> TrackId {
        match TrackId::new(value) {
            Some(track_id) => track_id,
            None => unreachable!("test helper only constructs positive ids"),
        }
    }

    fn rating(stars: u8) -> Rating {
        match Rating::new(stars) {
            Some(rating) => rating,
            None => unreachable!("test helper only constructs valid ratings"),
        }
    }

    fn track_location(path: &str) -> TrackLocation {
        TrackLocation::available(relative_path(path))
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(PathBuf::from(path)).expect("test path is relative")
    }
}
