// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! iTunes Search API album covers.
//!
//! Apple's public search API answers album searches without a key. Each
//! result names its cover as a 100×100 thumbnail whose URL ends in the
//! size it renders at (`…/100x100bb.jpg`); Apple's image server renders
//! the same artwork at any size asked for there, and the client asks for
//! 1200×1200. A thumbnail URL without that ending is not used: a
//! 100-pixel cover embedded in the user's files would never be replaced.

use std::sync::Arc;

use serde::Deserialize;
use sustain_artwork::MAX_ENCODED_ARTWORK_BYTES;

use crate::album_cover::{AlbumCoverQuery, validate_artwork};
use crate::client::HttpClient;
use crate::error::RemoteResult;
use crate::http::url_encode;

const SEARCH_URL: &str = "https://itunes.apple.com/search";

/// Search results considered; the album sits near the top when the
/// store sells it at all.
const SEARCH_LIMIT: u32 = 10;

/// The thumbnail ending each result's cover URL has.
const THUMBNAIL_SIZE: &str = "/100x100bb.jpg";

/// The cover size the client asks Apple's image server for.
const COVER_SIZE: &str = "/1200x1200bb.jpg";

#[derive(Clone)]
pub struct ItunesClient {
    http: Arc<HttpClient>,
}

impl ItunesClient {
    pub fn new(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    /// The front cover of the album `query` names. `Ok(None)` means the
    /// store has no such album, or no usable cover for it.
    pub fn fetch_album_cover(&self, query: &AlbumCoverQuery) -> RemoteResult<Option<Vec<u8>>> {
        let url = format!(
            "{SEARCH_URL}?term={}&entity=album&media=music&limit={SEARCH_LIMIT}",
            url_encode(&format!("{} {}", query.artist, query.album))
        );
        let response: SearchResponse = self.http.get_json(&url)?;
        let Some(cover_url) = cover_url(query, response) else {
            return Ok(None);
        };
        self.http
            .get_bytes(&cover_url, MAX_ENCODED_ARTWORK_BYTES)
            .and_then(validate_artwork)
    }
}

/// The full-size cover of the first result that is the album.
fn cover_url(query: &AlbumCoverQuery, response: SearchResponse) -> Option<String> {
    let album = response
        .results
        .into_iter()
        .find(|album| query.is_answered_by(&album.artist_name, &album.collection_name))?;
    let thumbnail = album.artwork_url_100?;
    let base = thumbnail.strip_suffix(THUMBNAIL_SIZE)?;
    Some(format!("{base}{COVER_SIZE}"))
}

#[derive(Deserialize)]
struct SearchResponse {
    #[serde(default)]
    results: Vec<StoreAlbum>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoreAlbum {
    artist_name: String,
    collection_name: String,
    artwork_url_100: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::{SearchResponse, cover_url};
    use crate::album_cover::AlbumCoverQuery;

    fn query(artist: &str, album: &str) -> AlbumCoverQuery {
        AlbumCoverQuery {
            artist: artist.to_owned(),
            album: album.to_owned(),
        }
    }

    fn response(json: &str) -> SearchResponse {
        serde_json::from_str(json).expect("itunes search response")
    }

    const DAFT_PUNK: &str = r#"{
        "resultCount": 2,
        "results": [
            {
                "wrapperType": "collection",
                "artistName": "Daft Punk",
                "collectionName": "Random Access Memories",
                "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/ram.jpg/100x100bb.jpg"
            },
            {
                "wrapperType": "collection",
                "artistName": "Daft Punk",
                "collectionName": "Discovery",
                "artworkUrl60": "https://is1-ssl.mzstatic.com/image/thumb/disc.jpg/60x60bb.jpg",
                "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/disc.jpg/100x100bb.jpg"
            }
        ]
    }"#;

    #[test]
    fn the_full_size_cover_comes_from_the_result_that_is_the_album() {
        assert_eq!(
            cover_url(&query("Daft Punk", "Discovery"), response(DAFT_PUNK)).as_deref(),
            Some("https://is1-ssl.mzstatic.com/image/thumb/disc.jpg/1200x1200bb.jpg")
        );
    }

    #[test]
    fn no_result_that_is_the_album_means_no_cover() {
        assert_eq!(
            cover_url(&query("Daft Punk", "Homework"), response(DAFT_PUNK)),
            None
        );
    }

    #[test]
    fn a_thumbnail_that_cannot_be_enlarged_is_not_used() {
        let json = r#"{"results": [{
            "artistName": "Daft Punk",
            "collectionName": "Discovery",
            "artworkUrl100": "https://is1-ssl.mzstatic.com/image/thumb/disc.jpg/source.png"
        }]}"#;

        assert_eq!(
            cover_url(&query("Daft Punk", "Discovery"), response(json)),
            None
        );
    }
}
