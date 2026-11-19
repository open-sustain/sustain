// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Reading tag-derived values out of an audio file.
//!
//! These are the *initial* values Sustain captures the first time a file
//! enters the library — editable metadata, star rating, and the embedded-
//! artwork bit — from the tag [`crate::native`] reads. Per Sustain's
//! persistence policy these are consulted only at first import; once a
//! track has a library row SQLite is authoritative.

use super::*;
use crate::native;
use lofty::{
    config::ParseOptions,
    error::{FileParseError, TextDecodingError},
    file::FileType,
    file::{AudioFile, TaggedFileExt},
    probe::Probe,
    tag::items::timestamp::TimestampParseError,
};

/// The initial values for import: a file whose tag lofty cannot parse
/// still enters the library from its audio properties and filename, with
/// the malformation reported so a scan can repair it.
pub(crate) fn read_initial_tags(path: &Path) -> MetadataResult<InitialTagRead> {
    audio_format_from_path(path)?;
    match native::parse(path, ParseOptions::new()) {
        Ok(parsed) => Ok(InitialTagRead::clean(initial_tags(path, &parsed, true))),
        Err(MetadataError::MalformedTag(kind)) => {
            let parsed = native::parse(path, ParseOptions::new().read_tags(false))?;
            Ok(InitialTagRead {
                tags: initial_tags(path, &parsed, true),
                repair: Some(MetadataRepair::MalformedTag(kind)),
            })
        }
        Err(error) => Err(error),
    }
}

/// The tag values exactly as written: no filename title, and a tag lofty
/// cannot parse is an error rather than an empty result.
pub(crate) fn read_persisted_tags(path: &Path) -> MetadataResult<InitialTags> {
    audio_format_from_path(path)?;
    native::parse(path, ParseOptions::new()).map(|parsed| initial_tags(path, &parsed, false))
}

/// The cover the file at `path` carries.
pub(crate) fn read_cover(path: &Path) -> MetadataResult<Option<Vec<u8>>> {
    audio_format_from_path(path)?;
    let parsed = native::parse(path, ParseOptions::new())?;
    Ok(parsed.with_readable_tag(|tag| {
        tag.and_then(native::EditableTag::cover)
            .map(|picture| picture.data().to_vec())
    }))
}

fn initial_tags(
    path: &Path,
    parsed: &native::ParsedFile,
    backfill_title_from_filename: bool,
) -> InitialTags {
    let (mut metadata, rating, has_embedded_artwork) = parsed.with_readable_tag(|tag| {
        tag.map_or_else(
            || (TrackMetadata::default(), Rating::unrated(), false),
            |tag| (tag.metadata(), tag.rating(), tag.cover().is_some()),
        )
    });
    let properties = parsed.rest.properties();
    // lofty guesses the length and bitrate of an MP3 that declares no
    // frame count from its first frame alone; see `mpeg_frame`.
    let mpeg_audio_extent = (parsed.rest.file_type() == FileType::Mpeg)
        .then(|| mpeg_frame::audio_extent(path))
        .flatten();

    // A BPM or key read from tags came from whichever tool wrote them,
    // not from the user's hand in Sustain.
    metadata.bpm_set_by_user = false;
    metadata.key_origin = KeyOrigin::Imported;
    metadata.duration =
        Some(mpeg_audio_extent.map_or(properties.duration(), |extent| extent.duration));
    metadata.bitrate_kbps = mpeg_audio_extent
        .and_then(|extent| extent.measured_bitrate_kbps)
        .or(properties.audio_bitrate())
        .or(properties.overall_bitrate());
    metadata.sample_rate_hz = properties.sample_rate();
    metadata.bit_depth = properties.bit_depth();
    metadata.channels = properties.channels();
    if backfill_title_from_filename {
        metadata.ensure_title_from_filename(path);
        metadata.fill_missing_generated_sort_fields();
    }

    InitialTags {
        metadata,
        rating,
        has_embedded_artwork,
    }
}

/// Applies Sustain's lofty policy for the reading thread: lofty's
/// allocation guard is thread-local, so it is reapplied at every metadata
/// entry point, keeping worker threads and future lofty defaults from
/// drifting away from the application's encoded-artwork cap.
pub(crate) fn apply_read_policy() {
    apply_global_options(GlobalOptions::new().allocation_limit(MAX_ENCODED_ARTWORK_BYTES));
}

pub(crate) fn metadata_error_from_lofty(path: &Path, error: FileParseError) -> MetadataError {
    match malformed_tag_error(&error) {
        Some(kind) => MetadataError::MalformedTag(kind),
        None => container_format_mismatch(path, audio_format_from_path(path).ok())
            .unwrap_or(MetadataError::ReadFailed),
    }
}

/// The repairable tag malformation behind a lofty read failure, if any.
pub(crate) fn malformed_tag_error(error: &FileParseError) -> Option<MalformedTagError> {
    if caused_by::<TimestampParseError>(error) {
        Some(MalformedTagError::BadTimestamp)
    } else if caused_by::<TextDecodingError>(error) {
        Some(MalformedTagError::TextDecode)
    } else {
        None
    }
}

pub(crate) fn container_format_mismatch(
    path: &Path,
    expected: Option<AudioFormat>,
) -> Option<MetadataError> {
    let expected = expected?;
    let detected = detected_audio_format_from_content(path)?;
    (detected != expected).then_some(MetadataError::ContainerFormatMismatch { expected, detected })
}

fn detected_audio_format_from_content(path: &Path) -> Option<AudioFormat> {
    let probe = Probe::open(path).ok()?.guess_file_type().ok()?;
    audio_format_from_lofty_file_type(probe.file_type()?)
}

pub(crate) fn audio_format_from_lofty_file_type(file_type: FileType) -> Option<AudioFormat> {
    match file_type {
        FileType::Mpeg => Some(AudioFormat::Mp3),
        FileType::Vorbis | FileType::Opus | FileType::Speex => Some(AudioFormat::Ogg),
        FileType::Flac => Some(AudioFormat::Flac),
        FileType::Mp4 => Some(AudioFormat::Mp4),
        FileType::Wav => Some(AudioFormat::Wav),
        FileType::Aiff => Some(AudioFormat::Aiff),
        _ => None,
    }
}

pub(crate) fn parse_flag(value: &str) -> Option<bool> {
    match value.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" => Some(true),
        "0" | "false" | "no" => Some(false),
        _ => None,
    }
}
