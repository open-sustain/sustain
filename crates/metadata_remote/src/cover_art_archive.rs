// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Cover Art Archive client.
//!
//! Cover Art Archive is the canonical artwork source associated with
//! MusicBrainz. We use it through two redirect endpoints:
//!
//! * `release/<release-mbid>/front` — the front cover of a specific
//!   release. This is what we hit first because a release's cover is
//!   the most specific match available: the user's file most likely
//!   refers to a particular pressing/edition.
//! * `release-group/<release-group-mbid>/front` — the front cover of
//!   the release *group*. This falls back when a specific release
//!   doesn't have artwork: most releases share a release group with
//!   one another (different editions of the same album), and the
//!   group-level front is normally available even when an individual
//!   release isn't.
//!
//! Both endpoints respond with a redirect to `archive.org`; the
//! [`crate::client::HttpClient`] follows redirects transparently. A
//! 404 means "no artwork on file" and is *not* an error in our model
//! — the caller decides whether to fall back to a different MBID,
//! try a different match, or surface "no cover found" to the user.

use std::sync::Arc;

use sustain_artwork::{MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

use crate::client::HttpClient;
use crate::error::{RemoteError, RemoteResult};
use crate::mbid::is_well_formed;

const RELEASE_FRONT_BASE: &str = "https://coverartarchive.org/release";
const RELEASE_GROUP_FRONT_BASE: &str = "https://coverartarchive.org/release-group";

#[derive(Clone)]
pub struct CoverArtArchiveClient {
    http: Arc<HttpClient>,
}

impl CoverArtArchiveClient {
    pub fn new(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    /// Fetch the front cover for a release MBID. `Ok(None)` means
    /// "no front cover on file"; `Err(_)` means the lookup itself
    /// failed and the caller should not interpret the absence of
    /// bytes as authoritative.
    pub fn fetch_release_front(&self, release_mbid: &str) -> RemoteResult<Option<Vec<u8>>> {
        if !is_well_formed(release_mbid) {
            return Ok(None);
        }
        let url = format!("{RELEASE_FRONT_BASE}/{release_mbid}/front");
        self.http
            .get_bytes(&url, MAX_ENCODED_ARTWORK_BYTES)
            .and_then(validate_artwork)
    }

    /// Fetch the front cover for a release-group MBID. Used as a
    /// fallback when no release-specific cover is present.
    pub fn fetch_release_group_front(
        &self,
        release_group_mbid: &str,
    ) -> RemoteResult<Option<Vec<u8>>> {
        if !is_well_formed(release_group_mbid) {
            return Ok(None);
        }
        let url = format!("{RELEASE_GROUP_FRONT_BASE}/{release_group_mbid}/front");
        self.http
            .get_bytes(&url, MAX_ENCODED_ARTWORK_BYTES)
            .and_then(validate_artwork)
    }
}

fn validate_artwork(bytes: Option<Vec<u8>>) -> RemoteResult<Option<Vec<u8>>> {
    bytes
        .map(|bytes| {
            validate_encoded_artwork(&bytes).map_err(RemoteError::ArtworkRejected)?;
            Ok(bytes)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::validate_artwork;

    #[test]
    fn absent_artwork_is_a_normal_outcome() {
        // A 404 from Cover Art Archive arrives here as `None`, and "no cover
        // on file" must not be reported as an error.
        assert_eq!(validate_artwork(None), Ok(None));
    }

    #[test]
    fn valid_image_bytes_pass_through_unchanged() {
        let bytes = include_bytes!("../tests/fixtures/cover_2x2.png").to_vec();
        assert_eq!(validate_artwork(Some(bytes.clone())), Ok(Some(bytes)));
    }

    #[test]
    fn corrupt_image_bytes_are_rejected() {
        assert!(validate_artwork(Some(vec![0xff, 0x00, 0x01, 0x02])).is_err());
    }
}
