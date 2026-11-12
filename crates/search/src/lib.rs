// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::collections::HashMap;

use sustain_domain::{Track, TrackId, normalize_search_text};

/// Joins a track's searchable fields in its precomputed document. A
/// query word holds no whitespace, so no word can match across two
/// fields: each word is found inside one field or not at all.
const FIELD_SEPARATOR: char = '\n';

/// A precomputed, normalized search document per track, keyed by id.
///
/// Each track contributes one lowercased string concatenating its
/// searchable fields, built once on insert/rebuild rather than re-cloned
/// and re-lowercased on every query. Matching a query is then one
/// substring test per query word with no per-track allocation — the
/// index, not the query path, owns the normalization cost. The owner keeps it in sync with the
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
    /// [`normalize_query`]): the track matches when each of the query's
    /// words is found in one of its fields, whichever field that is, so
    /// "massive mezzanine" finds an artist's album. A track absent from
    /// the index never matches.
    pub fn matches(&self, track_id: TrackId, normalized_query: &str) -> bool {
        self.documents
            .get(&track_id)
            .is_some_and(|document| every_word_is_found(normalized_query, document))
    }
}

/// Normalize a raw search query once so it can be reused across many track
/// tests. The same normalization is baked into each indexed document.
pub fn normalize_query(query: &str) -> String {
    normalize_search_text(query)
}

/// Whether `text` contains each whitespace-separated word of
/// `normalized_query`. A query with no words is found in any text.
fn every_word_is_found(normalized_query: &str, text: &str) -> bool {
    normalized_query
        .split_whitespace()
        .all(|word| text.contains(word))
}

/// Album-level search: each word of the search is looked for in the
/// album's title, artist, and year.
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
    let mut document = String::new();
    push_field(&mut document, album_title);
    push_field(&mut document, album_artist);
    if let Some(year) = album_year {
        push_field(&mut document, &year.to_string());
    }
    every_word_is_found(&normalized_search, &document)
}

/// Build a track's normalized search document: each non-empty searchable
/// field, trimmed and lowercased, joined by [`FIELD_SEPARATOR`]. This is
/// the single source of truth for "what text a track is searchable by".
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

    use super::{SearchIndex, album_matches_search_text, normalize_query};
    use crate::Track;

    #[test]
    fn index_matches_every_searchable_field() {
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        track.metadata.album_artist = Some("Various Artists".to_owned());
        track.metadata.composer = Some("Robert Del Naja".to_owned());
        track.metadata.genre = Some("Trip Hop".to_owned());
        track.location = track_location("Bristol/track.flac");
        let index = index_of(std::slice::from_ref(&track));

        for query in [
            "angel",
            "massive attack",
            "mezzanine",
            "various",
            "del naja",
            "trip hop",
            "bristol",
        ] {
            assert!(
                index.matches(track.id, &normalize_query(query)),
                "index should match {query:?}"
            );
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
    fn index_matches_words_found_in_different_fields() {
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        let index = index_of(std::slice::from_ref(&track));
        for query in ["massive mezzanine", "mezzanine  angel attack", "mass mezz"] {
            assert!(index.matches(track.id, &normalize_query(query)), "{query}");
        }
    }

    #[test]
    fn index_requires_every_word_of_the_query() {
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        let index = index_of(std::slice::from_ref(&track));
        assert!(!index.matches(track.id, &normalize_query("massive protection")));
    }

    #[test]
    fn index_does_not_match_a_word_spanning_two_fields() {
        // "attack" ends the artist and "mezzanine" starts the album: no
        // single word runs from one field into the next.
        let mut track = track(1, "Angel", "Massive Attack");
        track.metadata.album = Some("Mezzanine".to_owned());
        let index = index_of(std::slice::from_ref(&track));
        assert!(!index.matches(track.id, &normalize_query("attackmezzanine")));
    }

    #[test]
    fn album_search_matches_words_found_in_different_fields() {
        assert!(album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "massive mezzanine 1998"
        ));
        assert!(!album_matches_search_text(
            "Mezzanine",
            "Massive Attack",
            Some(1998),
            "massive protection"
        ));
    }

    #[test]
    fn a_blank_query_normalizes_to_no_search() {
        // Callers skip the index for an empty normalized query and keep
        // every track.
        assert!(normalize_query("   ").is_empty());
        assert_eq!(normalize_query("  Massive Attack  "), "massive attack");
    }

    #[test]
    fn index_matches_case_insensitively() {
        let track = track(1, "Angel", "Massive Attack");
        let index = index_of(std::slice::from_ref(&track));

        assert!(index.matches(track.id, &normalize_query("angel")));
        assert!(index.matches(track.id, &normalize_query("ANGEL")));
    }

    #[test]
    fn index_folds_accents_in_queries_and_indexed_fields() {
        let track = track(1, "Déjà Vu", "Beyoncé");
        let index = index_of(std::slice::from_ref(&track));

        for query in ["deja vu", "déjà vu", "beyonce", "BEYONCÉ"] {
            assert!(index.matches(track.id, &normalize_query(query)));
        }
    }

    #[test]
    fn index_keeps_only_tracks_with_a_matching_field() {
        let tracks = vec![
            track(1, "Angel", "Massive Attack"),
            track(2, "Roads", "Portishead"),
        ];
        let index = index_of(&tracks);
        let query = normalize_query("port");

        let matching: Vec<TrackId> = tracks
            .iter()
            .map(|track| track.id)
            .filter(|track_id| index.matches(*track_id, &query))
            .collect();
        assert_eq!(matching, vec![track_id(2)]);
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

    fn index_of(tracks: &[Track]) -> SearchIndex {
        let mut index = SearchIndex::new();
        index.rebuild(tracks);
        index
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

    fn track_location(path: &str) -> TrackLocation {
        TrackLocation::available(relative_path(path))
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(PathBuf::from(path)).expect("test path is relative")
    }
}
