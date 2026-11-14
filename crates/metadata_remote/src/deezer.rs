// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Deezer album covers.
//!
//! Deezer's public catalogue API answers album searches without a key and
//! serves each album's cover at up to 1000×1000. The search is free text:
//! its field syntax (`artist:"…" album:"…"`) finds nothing for many albums
//! it holds, so the client searches for the artist and title together and
//! keeps only a result whose artist and title are the album's.

use std::sync::Arc;

use serde::Deserialize;
use sustain_artwork::MAX_ENCODED_ARTWORK_BYTES;

use crate::album_cover::{AlbumCoverQuery, validate_artwork};
use crate::client::HttpClient;
use crate::error::{RemoteError, RemoteResult};
use crate::http::url_encode;

const ALBUM_SEARCH_URL: &str = "https://api.deezer.com/search/album";

/// Search results considered; the album sits near the top when Deezer
/// holds it at all.
const SEARCH_LIMIT: u32 = 10;

#[derive(Clone)]
pub struct DeezerClient {
    http: Arc<HttpClient>,
}

impl DeezerClient {
    pub fn new(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    /// The front cover of the album `query` names. `Ok(None)` means
    /// Deezer has no such album, or no cover for it.
    pub fn fetch_album_cover(&self, query: &AlbumCoverQuery) -> RemoteResult<Option<Vec<u8>>> {
        let url = format!(
            "{ALBUM_SEARCH_URL}?q={}&limit={SEARCH_LIMIT}",
            url_encode(&format!("{} {}", query.artist, query.album))
        );
        let response: AlbumSearchResponse = self.http.get_json(&url)?;
        if response.error.is_some() {
            return Err(RemoteError::InvalidResponse);
        }
        let Some(cover_url) = cover_url(query, response) else {
            return Ok(None);
        };
        self.http
            .get_bytes(&cover_url, MAX_ENCODED_ARTWORK_BYTES)
            .and_then(validate_artwork)
    }
}

/// The largest cover of the first result that is the album.
fn cover_url(query: &AlbumCoverQuery, response: AlbumSearchResponse) -> Option<String> {
    response
        .data
        .into_iter()
        .find(|album| query.is_answered_by(&album.artist.name, &album.title))
        .and_then(|album| album.cover_xl.or(album.cover_big))
}

/// A search Deezer refused (a quota error, for instance) still answers
/// HTTP 200, with an `error` object instead of `data`. It is a failed
/// lookup, not the album missing from the catalogue.
#[derive(Deserialize)]
struct AlbumSearchResponse {
    #[serde(default)]
    data: Vec<DeezerAlbum>,
    error: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
struct DeezerAlbum {
    title: String,
    artist: DeezerArtist,
    cover_xl: Option<String>,
    cover_big: Option<String>,
}

#[derive(Deserialize)]
struct DeezerArtist {
    name: String,
}

#[cfg(test)]
mod tests {
    use super::{AlbumSearchResponse, cover_url};
    use crate::album_cover::AlbumCoverQuery;

    fn query(artist: &str, album: &str) -> AlbumCoverQuery {
        AlbumCoverQuery {
            artist: artist.to_owned(),
            album: album.to_owned(),
        }
    }

    fn response(json: &str) -> AlbumSearchResponse {
        serde_json::from_str(json).expect("deezer search response")
    }

    const DISCOVERY: &str = r#"{
        "data": [
            {
                "id": 1,
                "title": "Discovery",
                "cover_big": "https://cdn.example/other/500x500.jpg",
                "cover_xl": "https://cdn.example/other/1000x1000.jpg",
                "artist": { "id": 9, "name": "Electric Light Orchestra" }
            },
            {
                "id": 302127,
                "title": "Discovery",
                "cover_big": "https://cdn.example/daft/500x500.jpg",
                "cover_xl": "https://cdn.example/daft/1000x1000.jpg",
                "record_type": "album",
                "artist": { "id": 27, "name": "Daft Punk" }
            }
        ],
        "total": 2
    }"#;

    #[test]
    fn the_cover_comes_from_the_result_that_is_the_album() {
        assert_eq!(
            cover_url(&query("Daft Punk", "Discovery"), response(DISCOVERY)).as_deref(),
            Some("https://cdn.example/daft/1000x1000.jpg"),
            "the first result is another artist's album of the same name"
        );
    }

    #[test]
    fn no_result_that_is_the_album_means_no_cover() {
        assert_eq!(
            cover_url(&query("Daft Punk", "Homework"), response(DISCOVERY)),
            None
        );
    }

    #[test]
    fn a_smaller_cover_serves_when_the_largest_is_missing() {
        let json = r#"{"data": [{
            "title": "Discovery",
            "cover_big": "https://cdn.example/daft/500x500.jpg",
            "artist": { "name": "Daft Punk" }
        }]}"#;

        assert_eq!(
            cover_url(&query("Daft Punk", "Discovery"), response(json)).as_deref(),
            Some("https://cdn.example/daft/500x500.jpg")
        );
    }

    #[test]
    fn a_refused_search_is_told_apart_from_an_empty_one() {
        let refused = response(
            r#"{"error": {"type": "Exception", "message": "Quota limit exceeded", "code": 4}}"#,
        );
        let empty = response(r#"{"data": [], "total": 0}"#);

        assert!(refused.error.is_some());
        assert!(empty.error.is_none());
    }
}
