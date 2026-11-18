// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Mirroring a track's library values onto a staged audio file, and
//! proving they landed, before the file replaces the track's audio.
//!
//! Duplicate consolidation and YouTube audio replacement both publish a
//! staged file under a track the library already describes. The library
//! wins (see `CLAUDE.md`): the staged file takes the library's mirrored
//! tags, rating, and artwork, and is published only once reading it back
//! shows exactly what the write was meant to leave there.

use std::path::Path;

use sustain_domain::{Bpm, Rating, TrackMetadata};
use sustain_metadata::MetadataService;

use crate::metadata_writer::metadata_mirror;

/// The staged file could not be written, could not be read back, or reads
/// back other than the mirror meant to leave it.
#[derive(Debug)]
pub(crate) struct StagedTagsFailed;

/// Write `metadata`'s mirror, `rating`, and `artwork` onto the file at
/// `stage`, whose tags currently read as `staged`, then read it back and
/// check every editable field.
///
/// The expected tags are the staged file's own tags with the mirror
/// applied, not the library values themselves: mirroring deliberately
/// leaves a measured or imported BPM or key on the file as it was, so a
/// file whose analysis tags differ from the library's verifies as long
/// as the write left them alone.
pub(crate) fn mirror_onto_staged_file(
    service: &dyn MetadataService,
    stage: &Path,
    staged: &TrackMetadata,
    metadata: &TrackMetadata,
    rating: Rating,
    artwork: &Option<Vec<u8>>,
) -> Result<(), StagedTagsFailed> {
    let change = metadata_mirror(metadata);
    let mut expected = staged.clone();
    expected.apply_change(&change);
    service
        .write_metadata(stage, change)
        .and_then(|()| service.write_rating(stage, rating))
        .and_then(|()| service.write_artwork(stage, artwork.clone()))
        .map_err(|_| StagedTagsFailed)?;

    let written = service
        .read_persisted_tags(stage)
        .map_err(|_| StagedTagsFailed)?;
    let written_artwork = service.read_artwork(stage).map_err(|_| StagedTagsFailed)?;
    if editable_metadata_matches(&written.metadata, &expected)
        && written.rating == rating
        && &written_artwork == artwork
    {
        Ok(())
    } else {
        Err(StagedTagsFailed)
    }
}

/// Whether two readings of a track carry the same editable metadata.
///
/// BPM compares in whole beats: ID3 `TBPM` and MP4 `tmpo` hold no
/// fraction, so a tag written from a fractional library tempo reads back
/// as its nearest whole beat.
fn editable_metadata_matches(left: &TrackMetadata, right: &TrackMetadata) -> bool {
    left.title == right.title
        && left.artist == right.artist
        && left.album == right.album
        && left.album_artist == right.album_artist
        && left.composer == right.composer
        && left.grouping == right.grouping
        && left.genre == right.genre
        && left.track_number == right.track_number
        && left.track_total == right.track_total
        && left.disc_number == right.disc_number
        && left.disc_total == right.disc_total
        && left.year == right.year
        && left.compilation == right.compilation
        && left.bpm.map(Bpm::whole) == right.bpm.map(Bpm::whole)
        && left.key == right.key
        && left.comments == right.comments
        && left.lyrics == right.lyrics
}

#[cfg(test)]
mod tests {
    use sustain_domain::{Bpm, TrackMetadata};

    use super::editable_metadata_matches;

    #[test]
    fn editable_metadata_matches_compares_bpm_in_whole_beats() {
        let with_bpm = |bpm| TrackMetadata {
            bpm,
            ..TrackMetadata::default()
        };
        let library = with_bpm(Bpm::from_hundredths(11_886));
        // An ID3 or MP4 tag written from 118.86 reads back as 119.
        assert!(editable_metadata_matches(
            &with_bpm(Bpm::from_whole(119)),
            &library
        ));
        assert!(!editable_metadata_matches(
            &with_bpm(Bpm::from_whole(118)),
            &library
        ));
        assert!(!editable_metadata_matches(&with_bpm(None), &library));
    }
}
