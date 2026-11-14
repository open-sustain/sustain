// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! What the artwork providers share: the album a cover is looked up for,
//! the rule that says a catalogue's album is that album, and the check
//! every fetched image passes.
//!
//! Catalogues searched by text (Deezer, the iTunes Search API) return
//! whatever their search ranks first, which for a common album title can
//! be someone else's record. A cover is embedded in the user's files and
//! never replaced by a later lookup, so a wrong one is worse than none:
//! a candidate is accepted only when both its artist and its title are
//! the album's, compared by [`same_name`].

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::normalize_search_text;

use crate::error::{RemoteError, RemoteResult};

/// The album a cover is looked up for, as the library's tags name it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AlbumCoverQuery {
    /// The album artist, or the track artist when the album has none.
    pub artist: String,
    pub album: String,
}

impl AlbumCoverQuery {
    /// Whether a catalogue album by `artist` titled `title` is this album.
    pub(crate) fn is_answered_by(&self, artist: &str, title: &str) -> bool {
        same_name(&self.artist, artist) && same_name(&self.album, title)
    }
}

/// Whether two names are the same once case, accents, punctuation and
/// spacing are set aside, along with bracketed qualifiers: "Discovery
/// (Remastered)" names the album "Discovery", whose cover it shares.
/// A name made only of qualifiers or punctuation matches nothing.
pub(crate) fn same_name(left: &str, right: &str) -> bool {
    let left = name_key(left);
    !left.is_empty() && left == name_key(right)
}

fn name_key(name: &str) -> String {
    let mut depth = 0usize;
    normalize_search_text(name)
        .chars()
        .filter(|character| {
            match character {
                '(' | '[' => depth += 1,
                ')' | ']' => depth = depth.saturating_sub(1),
                _ => return depth == 0 && character.is_alphanumeric(),
            }
            false
        })
        .collect()
}

/// Accept fetched image bytes only when they decode as artwork Sustain
/// can embed. `None`, the provider having no image, passes as is.
pub(crate) fn validate_artwork(bytes: Option<Vec<u8>>) -> RemoteResult<Option<Vec<u8>>> {
    bytes
        .map(|bytes| {
            validate_encoded_artwork(&bytes).map_err(RemoteError::ArtworkRejected)?;
            Ok(bytes)
        })
        .transpose()
}

#[cfg(test)]
mod tests {
    use super::{AlbumCoverQuery, same_name, validate_artwork};

    #[test]
    fn names_match_across_case_accents_punctuation_and_qualifiers() {
        assert!(same_name("Discovery", "discovery"));
        assert!(same_name("Sigur Rós", "Sigur Ros"));
        assert!(same_name("AC/DC", "AC-DC"));
        assert!(same_name("Discovery", "Discovery (Remastered)"));
        assert!(same_name("Abbey Road", "Abbey Road [2019 Mix]"));
    }

    #[test]
    fn different_names_do_not_match() {
        assert!(!same_name("Discovery", "Homework"));
        assert!(!same_name("Discovery", "Discovery Live"));
        assert!(!same_name("", ""));
        assert!(!same_name("(Untitled)", "(Untitled)"), "only qualifiers");
    }

    #[test]
    fn an_album_needs_both_its_artist_and_its_title() {
        let query = AlbumCoverQuery {
            artist: "Daft Punk".to_owned(),
            album: "Discovery".to_owned(),
        };

        assert!(query.is_answered_by("Daft Punk", "Discovery"));
        assert!(!query.is_answered_by("Electric Light Orchestra", "Discovery"));
        assert!(!query.is_answered_by("Daft Punk", "Homework"));
    }

    #[test]
    fn absent_artwork_is_a_normal_outcome() {
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
