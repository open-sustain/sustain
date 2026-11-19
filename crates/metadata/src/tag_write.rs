// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Writing Sustain's edits back into an audio file's tags.
//!
//! [`crate::native`] edits a file's tag; these helpers persist it via the
//! durable atomic replace-by-rename in [`crate::atomic_file`], and repair
//! the malformed ID3v2 frames that keep lofty from reading a tag at all.
//! The MP3 leading-region compaction here is the write-side counterpart
//! to [`crate::mpeg_frame`].

use super::*;
use lofty::{
    error::{FileEncodingError, UnknownFormatError},
    file::FileType,
    probe::Probe,
};

pub(crate) fn repair_malformed_tag_read_error(
    path: &Path,
    repair: MetadataRepair,
) -> MetadataResult<bool> {
    if audio_format_from_path(path) != Ok(AudioFormat::Mp3) {
        return Ok(false);
    }

    let bytes = fs::read(path).map_err(|_| MetadataError::ReadFailed)?;
    let Some(repaired) = repair_id3v2_bytes(&bytes, repair)? else {
        return Ok(false);
    };
    atomic_write_via_rename(path, |temp_path| {
        fs::write(temp_path, &repaired).map_err(|_| MetadataError::WriteFailed)
    })?;
    Ok(true)
}

fn repair_id3v2_bytes(bytes: &[u8], repair: MetadataRepair) -> MetadataResult<Option<Vec<u8>>> {
    let Some(header) = Id3v2RawHeader::parse(bytes)? else {
        return Ok(None);
    };
    let tag_end = header
        .content_offset
        .checked_add(header.content_size)
        .ok_or(MetadataError::ReadFailed)?;
    if tag_end > bytes.len() {
        return Err(MetadataError::ReadFailed);
    }

    let mut frames = Vec::with_capacity(header.content_size);
    let mut offset = header.content_offset;
    let mut changed = false;
    while offset < tag_end {
        let remaining = &bytes[offset..tag_end];
        if remaining.iter().all(|byte| *byte == 0) {
            break;
        }
        if remaining.len() < ID3V2_FRAME_HEADER_SIZE {
            return Ok(None);
        }

        let frame_header = &remaining[..ID3V2_FRAME_HEADER_SIZE];
        if !is_id3v2_frame_id(&frame_header[..4]) {
            return Ok(None);
        }
        let frame_size = match header.version {
            Id3v2RawVersion::V23 => u32::from_be_bytes([
                frame_header[4],
                frame_header[5],
                frame_header[6],
                frame_header[7],
            ]) as usize,
            Id3v2RawVersion::V24 => parse_synchsafe_u32(&frame_header[4..8])?,
        };
        let frame_end = offset
            .checked_add(ID3V2_FRAME_HEADER_SIZE)
            .and_then(|value| value.checked_add(frame_size))
            .ok_or(MetadataError::ReadFailed)?;
        if frame_size == 0 || frame_end > tag_end {
            return Ok(None);
        }
        let frame = &bytes[offset..frame_end];
        if should_remove_frame_for_repair(frame, header.version, repair) {
            changed = true;
        } else {
            frames.extend_from_slice(frame);
        }
        offset = frame_end;
    }

    if !changed {
        return Ok(None);
    }
    if frames.len() > MAX_ID3V2_SYNCHSAFE_SIZE {
        return Err(MetadataError::WriteFailed);
    }

    let mut repaired =
        Vec::with_capacity(bytes.len() - (tag_end - header.content_offset) + frames.len());
    repaired.extend_from_slice(&bytes[..6]);
    repaired.extend_from_slice(&synchsafe_u32(frames.len() as u32));
    repaired.extend_from_slice(&frames);
    repaired.extend_from_slice(&bytes[tag_end..]);
    Ok(Some(repaired))
}

/// Whether `frame` (header included) is one that makes lofty fail to read
/// the tag with the malformation `repair` heals. lofty itself is the judge:
/// the frame is parsed alone in a minimal tag, with the options every
/// Sustain read uses, so exactly the offending frames are dropped and every
/// valid one — a good recording date next to a bad release date — is kept.
///
/// Chapter frames (`CHAP`, `CTOC`) are never removed: they embed whole
/// sub-frames, and dropping one for a bad child would lose every chapter
/// boundary and title it holds. Such a malformation stays for the user's
/// own tagger to fix.
fn should_remove_frame_for_repair(
    frame: &[u8],
    version: Id3v2RawVersion,
    repair: MetadataRepair,
) -> bool {
    if frame.starts_with(b"CHAP") || frame.starts_with(b"CTOC") {
        return false;
    }
    let MetadataRepair::MalformedTag(kind) = repair;
    // Zero padding ends the probe buffer so lofty's footer searches (APE,
    // ID3v1) never mistake the frame's own trailing bytes — an attached
    // file's tag inside a `GEOB`, say — for a footer of the probe.
    let Ok(tag_len) = u32::try_from(frame.len() + PROBE_PADDING_SIZE) else {
        return false;
    };
    let mut tag = b"ID3".to_vec();
    tag.push(match version {
        Id3v2RawVersion::V23 => 3,
        Id3v2RawVersion::V24 => 4,
    });
    tag.extend_from_slice(&[0, 0]);
    tag.extend_from_slice(&synchsafe_u32(tag_len));
    tag.extend_from_slice(frame);
    tag.resize(tag.len() + PROBE_PADDING_SIZE, 0);
    let read = Probe::new(std::io::Cursor::new(tag))
        .set_file_type(FileType::Mpeg)
        .read();
    read.err()
        .and_then(|error| read::malformed_tag_error(&error))
        == Some(kind)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Id3v2RawVersion {
    V23,
    V24,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Id3v2RawHeader {
    version: Id3v2RawVersion,
    content_offset: usize,
    content_size: usize,
}

impl Id3v2RawHeader {
    fn parse(bytes: &[u8]) -> MetadataResult<Option<Self>> {
        if bytes.len() < ID3V2_HEADER_SIZE || &bytes[..3] != b"ID3" {
            return Ok(None);
        }
        let version = match bytes[3] {
            3 => Id3v2RawVersion::V23,
            4 => Id3v2RawVersion::V24,
            _ => return Ok(None),
        };
        let flags = bytes[5];
        if flags != 0 {
            return Ok(None);
        }
        Ok(Some(Self {
            version,
            content_offset: ID3V2_HEADER_SIZE,
            content_size: parse_synchsafe_u32(&bytes[6..10])?,
        }))
    }
}

fn is_id3v2_frame_id(bytes: &[u8]) -> bool {
    bytes.len() == 4
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
}

fn parse_synchsafe_u32(bytes: &[u8]) -> MetadataResult<usize> {
    if bytes.len() != 4 || bytes.iter().any(|byte| byte & 0x80 != 0) {
        return Err(MetadataError::ReadFailed);
    }
    let value = bytes
        .iter()
        .fold(0usize, |value, byte| (value << 7) | usize::from(*byte));
    Ok(value)
}

fn synchsafe_u32(mut value: u32) -> [u8; 4] {
    let mut bytes = [0; 4];
    for byte in bytes.iter_mut().rev() {
        *byte = (value & 0x7f) as u8;
        value >>= 7;
    }
    bytes
}

const ID3V2_HEADER_SIZE: usize = 10;
/// Opens every APEv2 header and footer.
const APE_PREAMBLE: &[u8] = b"APETAGEX";
const ID3V2_FRAME_HEADER_SIZE: usize = 10;
/// Covers the largest footer lofty looks for at a file's end (ID3v1).
const PROBE_PADDING_SIZE: usize = 128;
const MAX_ID3V2_SYNCHSAFE_SIZE: usize = 0x0fff_ffff;

// Persists the one tag Sustain edits over `path` through `save`, which
// writes it into the file at the path it is given. The file's other tag
// blocks (an MP3's ID3v1 or APE tag, a WAV's RIFF INFO list, an AIFF's
// native text chunks) are not rewritten.
//
// The save is an atomic replace-by-rename: the
// new bytes land in an exclusive sibling temp file, retain the source
// filesystem metadata, get fsync'd to disk, atomically replace the
// pathname, and are made durable by syncing the parent directory. The key
// property this buys us is that GStreamer (or any other reader holding an open
// file descriptor on `path`) keeps seeing the *original* inode's bytes
// until it closes the descriptor — Linux/POSIX `rename` only swaps the
// directory entry, the prior inode is kept alive by outstanding fds.
// That eliminates the audio glitch caused by lofty's default in-place
// rewrite happening underneath an active playback read.
pub(crate) fn atomic_save_lofty(
    path: &Path,
    mut save: impl FnMut(&Path) -> Result<(), FileEncodingError>,
) -> MetadataResult<()> {
    let is_mp3 = audio_format_from_path(path) == Ok(AudioFormat::Mp3);
    atomic_write_via_rename(path, |temp_path| {
        match save(temp_path) {
            Ok(()) => return Ok(()),
            // lofty re-detects the format on write and reports `UnknownFormat`
            // when it cannot find the first MPEG frame within its small window
            // of tolerated leading junk. That — and only that — is the defect
            // healed below; every other save failure is surfaced verbatim so
            // an unrelated error never triggers a leading-region rewrite.
            Err(error) if caused_by::<UnknownFormatError>(&error) => {}
            Err(_) => return Err(MetadataError::WriteFailed),
        }

        // An MP3 that carries stacked ID3v2 tags or oversized padding between
        // its tag and its audio pushes the first frame past lofty's window, so
        // lofty refuses to rewrite it even though it read it fine via the file
        // extension. Heal such a file by compacting its leading region down to
        // the audio stream, then let lofty write a single clean tag (#193
        // follow-up). A non-MP3 that somehow reports `UnknownFormat` is not
        // something this MPEG-specific compaction can heal; an MP3 whose audio
        // already starts at offset 0 failed for some other reason.
        if !is_mp3 || !compact_mpeg_leading_region(temp_path)? {
            return Err(MetadataError::WriteFailed);
        }

        save(temp_path).map_err(|_| MetadataError::WriteFailed)
    })
}

/// Compacts the MP3 at `path` in place down to its first MPEG audio frame,
/// dropping the leading region — stacked ID3v2 tags, oversized padding, and
/// any other non-standard junk some taggers leave behind — while preserving
/// the audio (and any trailing tags) byte-for-byte. The ID3v2 frames lofty
/// read from the region (it merges consecutive tags) survive in the tag the
/// caller rewrites. A leading APE tag would not — Sustain writes only the tag
/// it edits — so a region holding one is never dropped; a stray `APETAGEX`
/// elsewhere in the region errs on the same safe side.
///
/// Returns `Ok(true)` when a leading region was removed, `Ok(false)` when the
/// first frame is already at offset 0 (so there was nothing to heal and the
/// caller's prior write failure had a different cause), and `Err` when no
/// audio frame could be located or the region holds an APE tag. The first frame is found by validating a
/// chained run of frames (see [`mpeg_frame`]) rather than scanning for a lone
/// sync, so the cut never lands inside the audio; the byte-exact tail copy is
/// what makes this a heal rather than a re-encode.
fn compact_mpeg_leading_region(path: &Path) -> MetadataResult<bool> {
    let bytes = fs::read(path).map_err(|_| MetadataError::WriteFailed)?;
    let audio_start =
        mpeg_frame::first_audio_frame_offset(&bytes).ok_or(MetadataError::WriteFailed)?;
    if audio_start == 0 {
        return Ok(false);
    }
    if bytes[..audio_start]
        .windows(APE_PREAMBLE.len())
        .any(|window| window == APE_PREAMBLE)
    {
        return Err(MetadataError::WriteFailed);
    }
    fs::write(path, &bytes[audio_start..]).map_err(|_| MetadataError::WriteFailed)?;
    Ok(true)
}
