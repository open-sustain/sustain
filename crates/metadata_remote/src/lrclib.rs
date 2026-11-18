// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! LRClib lyrics provider client.
//!
//! LRClib (<https://lrclib.net>) is a community-maintained, open lyrics
//! database with an extremely simple HTTP API and no authentication.
//! Two endpoints matter for Sustain today:
//!
//! 1. `GET /api/get` — exact lookup by artist and title, plus album
//!    and duration when the track has them. Returns plain and/or
//!    synced lyrics when the given fields match a record; 404
//!    otherwise. This is the cheap,
//!    deterministic path and the only one we use here today.
//! 2. `GET /api/search` — fuzzy search when the exact lookup misses.
//!    Not wired in this pass; the exact endpoint covers the
//!    overwhelming majority of well-tagged libraries and we want to
//!    establish the baseline behavior before adding heuristics.
//!
//! The client is intentionally thin: it owns the network call and the
//! JSON parse, nothing else. Persistence, deduplication, and the
//! decision of which capability to attempt all live in the runtime.
//!
//! Output shape: [`LrcLibClient::fetch`] returns a single
//! [`crate::service::FetchedLyrics`] containing whatever the provider
//! served — plain lyrics, synced LRC text, both, or neither. The
//! caller decides what to do with each.

use std::sync::Arc;

use serde::Deserialize;

use crate::client::HttpClient;
use crate::error::{RemoteError, RemoteResult};
use crate::http::url_encode;
use crate::service::{FetchedLyrics, TrackQuery};

const GET_BASE: &str = "https://lrclib.net/api/get";

#[derive(Clone)]
pub struct LrcLibClient {
    http: Arc<HttpClient>,
}

impl LrcLibClient {
    pub fn new(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    /// Look up lyrics for the given track query. Requires non-blank
    /// artist and title — the LRClib `/api/get` endpoint matches on
    /// exact fields and silently returns 404 if either is missing,
    /// which we treat the same as a true miss. Album and duration are
    /// passed through when present; LRClib uses them for tie-breaking
    /// but they are not strictly required.
    ///
    /// Returns `Ok(None)` for a 404 (no entry on file), `Ok(Some(_))`
    /// when at least one of `plain`/`synced` came back populated, and
    /// `Err(_)` for transport or parse failures.
    pub fn fetch(&self, query: &TrackQuery) -> RemoteResult<Option<FetchedLyrics>> {
        let Some((artist, title)) = required_terms(query) else {
            return Ok(None);
        };

        let url = build_get_url(artist, title, query);
        let payload: GetPayload = match self.http.get_json(&url) {
            Ok(payload) => payload,
            Err(RemoteError::BadStatus(404)) => return Ok(None),
            Err(error) => return Err(error),
        };

        let plain = normalise(payload.plain_lyrics);
        let synced = normalise(payload.synced_lyrics);
        if plain.is_none() && synced.is_none() {
            // LRClib will occasionally return a 200 with both fields
            // empty for a placeholder record. Treat that as "no
            // result" — there is nothing for Sustain to persist.
            return Ok(None);
        }
        Ok(Some(FetchedLyrics {
            plain,
            synced_lrc: synced,
        }))
    }
}

fn required_terms(query: &TrackQuery) -> Option<(&str, &str)> {
    let artist = query.artist.as_deref().filter(is_non_blank)?;
    let title = query.title.as_deref().filter(is_non_blank)?;
    Some((artist, title))
}

fn build_get_url(artist: &str, title: &str, query: &TrackQuery) -> String {
    let mut url = format!(
        "{GET_BASE}?artist_name={artist}&track_name={title}",
        artist = url_encode(artist),
        title = url_encode(title),
    );
    if let Some(album) = query.album.as_deref().filter(is_non_blank) {
        url.push_str("&album_name=");
        url.push_str(&url_encode(album));
    }
    if let Some(duration_ms) = query.duration_ms {
        // LRClib expects duration in whole seconds.
        let seconds = duration_ms / 1_000;
        url.push_str(&format!("&duration={seconds}"));
    }
    url
}

fn normalise(value: Option<String>) -> Option<String> {
    let trimmed = value?;
    if trimmed.trim().is_empty() {
        None
    } else {
        Some(trimmed)
    }
}

fn is_non_blank(value: &&str) -> bool {
    !value.trim().is_empty()
}

#[derive(Debug, Deserialize)]
struct GetPayload {
    #[serde(rename = "plainLyrics")]
    plain_lyrics: Option<String>,
    #[serde(rename = "syncedLyrics")]
    synced_lyrics: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_omits_album_and_duration_when_absent() {
        let query = TrackQuery {
            artist: Some("Beatles".to_owned()),
            title: Some("Yesterday".to_owned()),
            ..Default::default()
        };
        let url = build_get_url("Beatles", "Yesterday", &query);
        assert!(
            url.starts_with(GET_BASE),
            "URL should target the get endpoint"
        );
        assert!(url.contains("artist_name=Beatles"));
        assert!(url.contains("track_name=Yesterday"));
        assert!(!url.contains("album_name="));
        assert!(!url.contains("duration="));
    }

    #[test]
    fn build_url_encodes_special_characters() {
        let query = TrackQuery {
            artist: Some("Sigur Rós".to_owned()),
            title: Some("Untitled #4 / Ný batterí".to_owned()),
            album: Some("( )".to_owned()),
            duration_ms: Some(187_500),
            ..Default::default()
        };
        let url = build_get_url(
            query.artist.as_deref().expect("artist"),
            query.title.as_deref().expect("title"),
            &query,
        );
        // Spaces, slashes, and # all percent-encoded.
        assert!(url.contains("Sigur%20R%C3%B3s"));
        assert!(url.contains("Untitled%20%234%20%2F%20N%C3%BD%20batter%C3%AD"));
        // Album and duration appended; duration converted to seconds.
        assert!(url.contains("album_name=%28%20%29"));
        assert!(url.contains("duration=187"));
    }

    #[test]
    fn required_terms_filters_blank_fields() {
        let blank_artist = TrackQuery {
            artist: Some("  ".to_owned()),
            title: Some("Title".to_owned()),
            ..Default::default()
        };
        assert!(required_terms(&blank_artist).is_none());

        let missing_title = TrackQuery {
            artist: Some("A".to_owned()),
            title: None,
            ..Default::default()
        };
        assert!(required_terms(&missing_title).is_none());

        let usable = TrackQuery {
            artist: Some("A".to_owned()),
            title: Some("T".to_owned()),
            ..Default::default()
        };
        assert!(required_terms(&usable).is_some());
    }

    #[test]
    fn normalise_drops_empty_and_whitespace_only_strings() {
        assert_eq!(normalise(None), None);
        assert_eq!(normalise(Some(String::new())), None);
        assert_eq!(normalise(Some("   \n\n  ".to_owned())), None);
        assert_eq!(normalise(Some("Real".to_owned())), Some("Real".to_owned()));
    }
}
