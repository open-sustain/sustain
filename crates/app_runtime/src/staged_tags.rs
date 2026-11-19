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

use sustain_domain::{Rating, TrackFields, TrackMetadata};
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
/// applied as the file's format holds it, not the library values
/// themselves: mirroring deliberately leaves a measured or imported BPM
/// or key on the file as it was, so a file whose analysis tags differ
/// from the library's verifies as long as the write left them alone, and
/// a value the format cannot hold exactly (a fractional tempo in ID3)
/// verifies as the value it does hold.
pub(crate) fn mirror_onto_staged_file(
    service: &dyn MetadataService,
    stage: &Path,
    staged: &TrackMetadata,
    metadata: &TrackMetadata,
    rating: Rating,
    artwork: &Option<Vec<u8>>,
) -> Result<(), StagedTagsFailed> {
    let change = metadata_mirror(metadata, TrackFields::TAG_METADATA);
    let mut expected = staged.clone();
    expected.apply_change(
        &service
            .carried_change(stage, change.clone())
            .map_err(|_| StagedTagsFailed)?,
    );
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

/// Whether two readings of a track carry the same value in every field
/// the file's tags hold.
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
        && left.bpm == right.bpm
        && left.key == right.key
        && left.comments == right.comments
        && left.lyrics == right.lyrics
        && left.title_sort == right.title_sort
        && left.artist_sort == right.artist_sort
        && left.album_sort == right.album_sort
        && left.album_artist_sort == right.album_artist_sort
        && left.composer_sort == right.composer_sort
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use sustain_domain::{Bpm, FieldChange, MetadataChange, Rating, TrackMetadata};
    use sustain_metadata::{InitialTags, LoftyMetadataService, MetadataResult, MetadataService};

    use super::mirror_onto_staged_file;
    use crate::tests::{mp3_frames, unique_test_directory};

    fn library_metadata() -> TrackMetadata {
        TrackMetadata {
            title: Some("Title".to_owned()),
            // A tempo the user typed, which the mirror writes.
            bpm: Bpm::from_hundredths(11_886),
            bpm_set_by_user: true,
            composer_sort: Some("Composer Sort".to_owned()),
            ..TrackMetadata::default()
        }
    }

    /// Whether [`library_metadata`] mirrors through `service` onto a fresh
    /// MP3 and verifies.
    fn mirror_through(service: &dyn MetadataService) -> bool {
        let root = unique_test_directory();
        std::fs::create_dir_all(&root).expect("create test directory");
        let stage = root.join("stage.mp3");
        std::fs::write(&stage, mp3_frames()).expect("write MP3");
        let staged = service
            .read_persisted_tags(&stage)
            .expect("read stage")
            .metadata;
        let mirrored = mirror_onto_staged_file(
            service,
            &stage,
            &staged,
            &library_metadata(),
            Rating::new(3).expect("rating"),
            &None,
        )
        .is_ok();
        std::fs::remove_dir_all(root).expect("remove test directory");
        mirrored
    }

    #[test]
    fn a_fractional_tempo_verifies_as_the_whole_beat_id3_holds() {
        assert!(mirror_through(&LoftyMetadataService));
    }

    /// The production service, except that its metadata writes leave
    /// composer sort order alone.
    struct DropsComposerSort;

    impl MetadataService for DropsComposerSort {
        fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
            LoftyMetadataService.read_initial_tags(path)
        }

        fn read_persisted_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
            LoftyMetadataService.read_persisted_tags(path)
        }

        fn write_metadata(&self, path: &Path, change: MetadataChange) -> MetadataResult<()> {
            LoftyMetadataService.write_metadata(
                path,
                MetadataChange {
                    composer_sort: FieldChange::Unchanged,
                    ..change
                },
            )
        }

        fn carried_change(
            &self,
            path: &Path,
            change: MetadataChange,
        ) -> MetadataResult<MetadataChange> {
            LoftyMetadataService.carried_change(path, change)
        }

        fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()> {
            LoftyMetadataService.write_rating(path, rating)
        }

        fn read_artwork(&self, path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            LoftyMetadataService.read_artwork(path)
        }

        fn write_artwork(&self, path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            LoftyMetadataService.write_artwork(path, artwork)
        }
    }

    #[test]
    fn a_sort_field_the_write_did_not_land_fails_verification() {
        assert!(!mirror_through(&DropsComposerSort));
    }
}
