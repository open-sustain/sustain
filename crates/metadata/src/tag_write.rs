// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Writing Sustain's edits back into an audio file's tags.
//!
//! These helpers apply a [`MetadataChange`] / rating edit to an in-memory
//! tag, heal structurally broken ID3v2 language fields some other taggers
//! leave behind, and persist the result via the durable atomic
//! replace-by-rename in [`crate::atomic_file`]. The MP3 leading-region
//! compaction here is the write-side counterpart to [`crate::mpeg_frame`].

use super::*;

pub(crate) fn ensure_primary_tag(tagged_file: &mut TaggedFile) {
    if tagged_file.primary_tag().is_some() {
        return;
    }

    tagged_file.insert_tag(Tag::new(tagged_file.primary_tag_type()));
}

/// lofty's exact write-time rule for an ID3v2 language field
/// (`id3/v2/items/language_frame.rs`): each of the three bytes must be
/// ASCII-alphabetic. The field is a fixed `[u8; 3]`, so its length is never
/// in question. A value like `\0\0\0` — which some taggers emit and lofty
/// accepts on read but rejects on write — fails this rule and makes the
/// whole frame unserializable.
fn is_valid_id3v2_language(language: &Lang) -> bool {
    language.iter().all(u8::is_ascii_alphabetic)
}

/// De-corrupts malformed ID3v2 language codes on `tag` so a structurally
/// broken tag inherited from another tagger becomes writable again (#193).
///
/// `COMM` and `USLT` are the only ID3v2 frames whose three-byte ISO-639-2
/// language lofty validates when serializing; both surface on the abstract
/// tag as items carrying a `lang`, and a single invalid field (e.g.
/// `\0\0\0`) makes every save of the file fail with `WriteFailed`. Sustain
/// already owns the file at the moment it rewrites it — SQLite is
/// authoritative — so a write is the natural, safe point to heal the
/// container field it inherited.
///
/// Only the malformed language is touched: it is reset to the spec's
/// "undefined" placeholder `XXX` (ID3v2 §4) while the comment/lyrics text
/// and description are preserved verbatim. Valid languages are left
/// untouched, so a file without the defect serializes exactly as before.
/// Non-ID3v2 formats carry no such field and are skipped.
pub(crate) fn repair_invalid_id3v2_languages(tag: &mut Tag) {
    if tag.tag_type() != TagType::Id3v2 {
        return;
    }

    for item_key in [ItemKey::Comment, ItemKey::UnsyncLyrics] {
        // `take_filter` pulls out only the malformed items and preserves the
        // order of the rest, so valid frames keep their position and an
        // already-clean file is left undisturbed. Each extracted item is
        // re-appended with a healed language via `push_unchecked`, which —
        // unlike `insert` — never deduplicates, so two comments that differ
        // only by their (now-healed) language both survive.
        let repaired: Vec<TagItem> = tag
            .take_filter(item_key, |item| !is_valid_id3v2_language(item.lang()))
            .map(|mut item| {
                item.set_lang(UNKNOWN_LANGUAGE);
                item
            })
            .collect();
        for item in repaired {
            tag.push_unchecked(item);
        }
    }
}

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
        let Some(frame_id) = Id3v2FrameId::parse(&frame_header[..4]) else {
            return Ok(None);
        };
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
        let content = &frame[ID3V2_FRAME_HEADER_SIZE..];
        if should_remove_frame_for_repair(frame_id, content, header.version, repair) {
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

fn should_remove_frame_for_repair(
    frame_id: Id3v2FrameId,
    content: &[u8],
    version: Id3v2RawVersion,
    repair: MetadataRepair,
) -> bool {
    match repair {
        MetadataRepair::MalformedTag(MalformedTagError::BadTimestamp) => {
            is_id3v2_timestamp_frame(frame_id, version)
        }
        MetadataRepair::MalformedTag(MalformedTagError::TextDecode) => {
            is_malformed_optional_text_frame(frame_id, content)
        }
    }
}

fn is_id3v2_timestamp_frame(frame_id: Id3v2FrameId, version: Id3v2RawVersion) -> bool {
    match frame_id.as_str() {
        "TDRC" | "TDOR" | "TDRL" | "TDTG" | "TDEN" => true,
        "TYER" | "TDAT" | "TIME" | "TRDA" | "TORY" if version == Id3v2RawVersion::V23 => true,
        _ => false,
    }
}

fn is_malformed_optional_text_frame(frame_id: Id3v2FrameId, content: &[u8]) -> bool {
    let Some((&encoding, payload)) = content.split_first() else {
        return frame_id.is_text_information() || frame_id.is_language_text();
    };
    if !matches!(encoding, 0x00..=0x03) {
        return frame_id.is_text_information() || frame_id.is_language_text();
    }

    let utf16_payload = match frame_id.as_str() {
        _ if frame_id.is_text_information() => payload,
        "COMM" | "USLT" if payload.len() >= 3 => &payload[3..],
        "COMM" | "USLT" => return true,
        _ => return false,
    };
    if !matches!(encoding, 0x01 | 0x02) {
        return false;
    }
    utf16_payload.len() < 2 || utf16_payload.len() % 2 != 0
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Id3v2FrameId([u8; 4]);

impl Id3v2FrameId {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let id: [u8; 4] = bytes.try_into().ok()?;
        if id
            .iter()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit())
        {
            Some(Self(id))
        } else {
            None
        }
    }

    fn as_str(self) -> &'static str {
        match &self.0 {
            b"TDRC" => "TDRC",
            b"TDOR" => "TDOR",
            b"TDRL" => "TDRL",
            b"TDTG" => "TDTG",
            b"TDEN" => "TDEN",
            b"TYER" => "TYER",
            b"TDAT" => "TDAT",
            b"TIME" => "TIME",
            b"TRDA" => "TRDA",
            b"TORY" => "TORY",
            b"COMM" => "COMM",
            b"USLT" => "USLT",
            _ => "",
        }
    }

    fn is_text_information(self) -> bool {
        self.0[0] == b'T'
    }

    fn is_language_text(self) -> bool {
        matches!(&self.0, b"COMM" | b"USLT")
    }
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
const ID3V2_FRAME_HEADER_SIZE: usize = 10;
const MAX_ID3V2_SYNCHSAFE_SIZE: usize = 0x0fff_ffff;

// Persists `tagged_file` over `path` via atomic replace-by-rename: the
// new bytes land in an exclusive sibling temp file, retain the source
// filesystem metadata, get fsync'd to disk, atomically replace the
// pathname, and are made durable by syncing the parent directory. The key
// property this buys us is that GStreamer (or any other reader holding an open
// file descriptor on `path`) keeps seeing the *original* inode's bytes
// until it closes the descriptor — Linux/POSIX `rename` only swaps the
// directory entry, the prior inode is kept alive by outstanding fds.
// That eliminates the audio glitch caused by lofty's default in-place
// rewrite happening underneath an active playback read.
pub(crate) fn atomic_save_to_path(
    tagged_file: &lofty::file::TaggedFile,
    path: &Path,
    options: WriteOptions,
) -> MetadataResult<()> {
    atomic_save_lofty(path, |temp_path| {
        tagged_file.save_to_path(temp_path, options)
    })
}

pub(crate) fn atomic_save_id3v2_to_path(
    id3v2: &Id3v2Tag,
    path: &Path,
    options: WriteOptions,
) -> MetadataResult<()> {
    atomic_save_lofty(path, |temp_path| id3v2.save_to_path(temp_path, options))
}

fn atomic_save_lofty(
    path: &Path,
    mut save: impl FnMut(&Path) -> lofty::error::Result<()>,
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
            Err(error) if matches!(error.kind(), ErrorKind::UnknownFormat) => {}
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
/// the audio (and any trailing tags) byte-for-byte.
///
/// Returns `Ok(true)` when a leading region was removed, `Ok(false)` when the
/// first frame is already at offset 0 (so there was nothing to heal and the
/// caller's prior write failure had a different cause), and `Err` when no
/// audio frame could be located. The first frame is found by validating a
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
    fs::write(path, &bytes[audio_start..]).map_err(|_| MetadataError::WriteFailed)?;
    Ok(true)
}

pub(crate) fn apply_text_change(tag: &mut Tag, item_key: ItemKey, change: FieldChange<String>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            if value.trim().is_empty() {
                let _removed = tag.take(item_key).count();
            } else {
                tag.insert_text(item_key, value);
            }
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

pub(crate) fn apply_number_change<T>(tag: &mut Tag, item_key: ItemKey, change: FieldChange<T>)
where
    T: ToString,
{
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            tag.insert_text(item_key, value.to_string());
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

pub(crate) fn apply_year_change(tag: &mut Tag, change: FieldChange<i32>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(year) => {
            if let Some(year) = u16::try_from(year).ok().filter(|year| *year <= 9999) {
                let mut date = tag.date().unwrap_or_default();
                date.year = year;
                tag.set_date(date);
            } else {
                // Lofty's timestamp representation is unsigned. Retain the
                // prior textual behavior for values outside that range while
                // still removing any higher-priority recording date.
                tag.remove_date();
                tag.insert_text(ItemKey::Year, year.to_string());
            }
        }
        FieldChange::Clear => tag.remove_date(),
    }
}

pub(crate) fn apply_bool_change(tag: &mut Tag, item_key: ItemKey, change: FieldChange<bool>) {
    match change {
        FieldChange::Unchanged => {}
        FieldChange::Set(value) => {
            tag.insert_text(item_key, if value { "1" } else { "0" }.to_owned());
        }
        FieldChange::Clear => {
            let _removed = tag.take(item_key).count();
        }
    }
}

pub(crate) fn popularimeter_from_rating(
    rating: Rating,
    play_counter: u64,
) -> Popularimeter<'static> {
    match rating.stars() {
        1 => Popularimeter::musicbee(StarRating::One, play_counter),
        2 => Popularimeter::musicbee(StarRating::Two, play_counter),
        3 => Popularimeter::musicbee(StarRating::Three, play_counter),
        4 => Popularimeter::musicbee(StarRating::Four, play_counter),
        5 => Popularimeter::musicbee(StarRating::Five, play_counter),
        _ => unreachable!("unrated ratings are removed before conversion"),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PreservedPopularimeter {
    email: String,
    pub(crate) play_counter: u64,
}

impl PreservedPopularimeter {
    pub(crate) fn from_parts(email: String, play_counter: u64) -> Self {
        Self {
            email,
            play_counter,
        }
    }
}

pub(crate) fn clear_rating(tag: &mut Tag) {
    let _removed = tag.take(ItemKey::Popularimeter).count();
}

pub(crate) fn id3v2_tag_clearing_rating_preserving_counter(
    tag: &mut Tag,
    preserved_popularimeter: Option<PreservedPopularimeter>,
) -> Option<Id3v2Tag> {
    let preserved = preserved_popularimeter.filter(|popularimeter| {
        tag.tag_type() == TagType::Id3v2 && popularimeter.play_counter > 0
    })?;

    clear_rating(tag);
    repair_invalid_id3v2_languages(tag);
    let mut id3v2 = Id3v2Tag::from(tag.clone());
    id3v2.insert(Frame::Popularimeter(PopularimeterFrame::new(
        preserved.email,
        0,
        preserved.play_counter,
    )));
    Some(id3v2)
}
