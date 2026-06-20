// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Reading tag-derived values out of an audio file.
//!
//! These are the *initial* values Sustain captures the first time a file
//! enters the library — editable metadata, star rating, and the embedded-
//! artwork bit — plus the small parsers and the shared `read_tagged_file`
//! open primitive they build on. Per Sustain's persistence policy these are
//! consulted only at first import; once a track has a library row SQLite is
//! authoritative.

use super::*;
use lofty::{config::ParseOptions, error::LoftyError, file::FileType, probe::Probe};

pub(crate) fn read_tags(
    path: &Path,
    backfill_title_from_filename: bool,
) -> MetadataResult<InitialTagRead> {
    audio_format_from_path(path)?;
    match read_tagged_file(path) {
        Ok(tagged_file) => Ok(InitialTagRead::clean(initial_tags_from_tagged_file(
            path,
            &tagged_file,
            true,
            backfill_title_from_filename,
        ))),
        Err(MetadataError::MalformedTag(kind)) => {
            let tagged_file = read_tagged_file_without_tags(path)?;
            Ok(InitialTagRead {
                tags: initial_tags_from_tagged_file(
                    path,
                    &tagged_file,
                    false,
                    backfill_title_from_filename,
                ),
                repair: Some(MetadataRepair::MalformedTag(kind)),
            })
        }
        Err(error) => Err(error),
    }
}

fn initial_tags_from_tagged_file(
    path: &Path,
    tagged_file: &TaggedFile,
    read_tags: bool,
    backfill_title_from_filename: bool,
) -> InitialTags {
    let tag = tagged_file
        .primary_tag()
        .or_else(|| tagged_file.first_tag())
        .filter(|_| read_tags);
    let properties = tagged_file.properties();

    let mut metadata = TrackMetadata {
        title: tag.and_then(|tag| tag.title().map(|value| value.into_owned())),
        artist: tag.and_then(|tag| tag.artist().map(|value| value.into_owned())),
        album: tag.and_then(|tag| tag.album().map(|value| value.into_owned())),
        album_artist: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumArtist))
            .map(ToOwned::to_owned),
        composer: tag
            .and_then(|tag| tag.get_string(ItemKey::Composer))
            .map(ToOwned::to_owned),
        grouping: tag
            .and_then(|tag| tag.get_string(ItemKey::ContentGroup))
            .map(ToOwned::to_owned),
        genre: tag.and_then(|tag| tag.genre().map(|value| value.into_owned())),
        track_number: tag.and_then(Accessor::track),
        track_total: tag.and_then(Accessor::track_total),
        disc_number: tag.and_then(Accessor::disk),
        disc_total: tag.and_then(Accessor::disk_total),
        year: tag.and_then(|tag| tag.date().map(|date| i32::from(date.year))),
        compilation: tag
            .and_then(|tag| tag.get_string(ItemKey::FlagCompilation))
            .and_then(parse_flag),
        bpm: tag
            .and_then(|tag| tag.get_string(bpm_item_key(tag.tag_type())))
            .and_then(parse_bpm),
        key: tag
            .and_then(|tag| tag.get_string(ItemKey::InitialKey))
            .map(ToOwned::to_owned),
        comments: tag.and_then(|tag| tag.comment().map(|value| value.into_owned())),
        lyrics: tag
            .and_then(|tag| tag.get_string(lyrics_item_key(tag.tag_type())))
            .map(ToOwned::to_owned),
        // Tag-derived "sort as" names (issue #13). Read once at import
        // alongside the display fields; only used for ordering and full-file
        // metadata mirrors, never displayed.
        title_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::TrackTitleSortOrder))
            .map(ToOwned::to_owned),
        artist_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::TrackArtistSortOrder))
            .map(ToOwned::to_owned),
        album_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumTitleSortOrder))
            .map(ToOwned::to_owned),
        album_artist_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::AlbumArtistSortOrder))
            .map(ToOwned::to_owned),
        composer_sort: tag
            .and_then(|tag| tag.get_string(ItemKey::ComposerSortOrder))
            .map(ToOwned::to_owned),
        duration: Some(properties.duration()),
        bitrate_kbps: properties.audio_bitrate().or(properties.overall_bitrate()),
        sample_rate_hz: properties.sample_rate(),
        channels: properties.channels(),
    };
    metadata.normalize_text_fields();
    if backfill_title_from_filename {
        metadata.ensure_title_from_filename(path);
        metadata.fill_missing_generated_sort_fields();
    }

    let rating = tag
        .and_then(|tag| tag.ratings().next())
        .and_then(|rating| Rating::new(star_rating_value(rating.rating())))
        .unwrap_or_else(Rating::unrated);
    let has_embedded_artwork = tag.and_then(valid_embedded_picture).is_some();

    InitialTags {
        metadata,
        rating,
        has_embedded_artwork,
    }
}

pub(crate) fn read_tagged_file(path: &Path) -> MetadataResult<TaggedFile> {
    // Lofty's allocation guard is thread-local. Reapply Sustain's policy at
    // every metadata entry point so worker threads and future Lofty defaults
    // cannot drift away from the application's encoded-artwork cap.
    apply_global_options(GlobalOptions::new().allocation_limit(MAX_ENCODED_ARTWORK_BYTES));
    let expected_format = audio_format_from_path(path).ok();
    lofty::read_from_path(path)
        .map_err(|error| metadata_error_from_lofty(path, expected_format, error))
}

fn read_tagged_file_without_tags(path: &Path) -> MetadataResult<TaggedFile> {
    apply_global_options(GlobalOptions::new().allocation_limit(MAX_ENCODED_ARTWORK_BYTES));
    let expected_format = audio_format_from_path(path).ok();
    Probe::open(path)
        .and_then(|probe| probe.options(ParseOptions::new().read_tags(false)).read())
        .map_err(|error| metadata_error_from_lofty(path, expected_format, error))
}

fn metadata_error_from_lofty(
    path: &Path,
    expected_format: Option<AudioFormat>,
    error: LoftyError,
) -> MetadataError {
    match error.kind() {
        ErrorKind::BadTimestamp(_) => MetadataError::MalformedTag(MalformedTagError::BadTimestamp),
        ErrorKind::TextDecode(_) => MetadataError::MalformedTag(MalformedTagError::TextDecode),
        _ => container_format_mismatch(path, expected_format).unwrap_or(MetadataError::ReadFailed),
    }
}

fn container_format_mismatch(path: &Path, expected: Option<AudioFormat>) -> Option<MetadataError> {
    let expected = expected?;
    let detected = detected_audio_format_from_content(path)?;
    (detected != expected).then_some(MetadataError::ContainerFormatMismatch { expected, detected })
}

fn detected_audio_format_from_content(path: &Path) -> Option<AudioFormat> {
    let probe = Probe::open(path).ok()?.guess_file_type().ok()?;
    audio_format_from_lofty_file_type(probe.file_type()?)
}

fn audio_format_from_lofty_file_type(file_type: FileType) -> Option<AudioFormat> {
    match file_type {
        FileType::Mpeg => Some(AudioFormat::Mp3),
        FileType::Vorbis | FileType::Opus => Some(AudioFormat::Ogg),
        FileType::Flac => Some(AudioFormat::Flac),
        FileType::Mp4 => Some(AudioFormat::Mp4),
        FileType::Wav => Some(AudioFormat::Wav),
        _ => None,
    }
}

pub(crate) fn valid_embedded_picture(tag: &Tag) -> Option<&Picture> {
    tag.get_picture_type(PictureType::CoverFront)
        .filter(|picture| validate_encoded_artwork(picture.data()).is_ok())
        .or_else(|| {
            tag.pictures()
                .iter()
                .find(|picture| validate_encoded_artwork(picture.data()).is_ok())
        })
}

pub(crate) fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}

pub(crate) fn parse_bpm(value: &str) -> Option<u32> {
    value.trim().parse::<u32>().ok().and_then(validate_bpm)
}

pub(crate) fn star_rating_value(rating: StarRating) -> u8 {
    match rating {
        StarRating::One => 1,
        StarRating::Two => 2,
        StarRating::Three => 3,
        StarRating::Four => 4,
        StarRating::Five => 5,
    }
}
