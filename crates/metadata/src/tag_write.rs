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
            tag.insert_text(item_key, value);
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
