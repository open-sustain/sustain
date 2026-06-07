// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::BTreeMap,
    fs, io,
    path::{Path, PathBuf},
};

use lofty::{
    config::WriteOptions,
    id3::v2::Id3v2Tag,
    mp4::Ilst,
    ogg::VorbisComments,
    picture::{Picture, PictureType},
    prelude::{Accessor, AudioFile, TaggedFileExt},
    tag::{ItemKey, ItemValue, Tag, TagExt, TagItem, TagType},
};
use sustain_domain::{FieldChange, MetadataChange, TrackMetadata};

use super::{
    AudioFormat, InitialTags, LibraryScanner, LoftyMetadataService, MetadataError, MetadataResult,
    MetadataService, Rating, ScanFilesystem, ScanFingerprint, StdScanFilesystem, apply_bool_change,
    apply_number_change, apply_text_change, apply_year_change, atomic_write_via_rename,
    audio_format_from_path, bpm_item_key, hash_file_content, lyrics_item_key, parse_flag,
    popularimeter_from_rating, repair_invalid_id3v2_languages, star_rating_value,
    valid_embedded_picture,
};
use sustain_domain::TrackRelativePath;

#[test]
fn detects_supported_audio_formats_case_insensitively() {
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.MP3")),
        Ok(AudioFormat::Mp3)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.ogg")),
        Ok(AudioFormat::Ogg)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.OPUS")),
        Ok(AudioFormat::Ogg)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.flac")),
        Ok(AudioFormat::Flac)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.m4a")),
        Ok(AudioFormat::Mp4)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.mp4")),
        Ok(AudioFormat::Mp4)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.WAV")),
        Ok(AudioFormat::Wav)
    );
}

#[test]
fn rejects_unsupported_audio_formats() {
    assert_eq!(
        audio_format_from_path(Path::new("/music/a.aiff")),
        Err(MetadataError::UnsupportedAudioFormat)
    );
    assert_eq!(
        audio_format_from_path(Path::new("/music/no-extension")),
        Err(MetadataError::UnsupportedAudioFormat)
    );
}

#[test]
fn year_update_replaces_the_higher_priority_recording_date() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());

    apply_year_change(&mut tag, FieldChange::Set(2015));

    assert_eq!(tag.date().map(|date| date.year), Some(2015));
    assert_eq!(tag.get_string(ItemKey::RecordingDate), Some("2015-09-30"));
    assert_eq!(tag.get_string(ItemKey::Year), None);
}

#[test]
fn year_clear_removes_recording_date_and_year_items() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());
    tag.insert_text(ItemKey::Year, "1998".to_owned());

    apply_year_change(&mut tag, FieldChange::Clear);

    assert_eq!(tag.get_string(ItemKey::RecordingDate), None);
    assert_eq!(tag.get_string(ItemKey::Year), None);
}

#[test]
fn year_update_outside_loftys_timestamp_range_keeps_a_textual_year() {
    let mut tag = Tag::new(TagType::VorbisComments);
    tag.insert_text(ItemKey::RecordingDate, "1998-09-30".to_owned());

    apply_year_change(&mut tag, FieldChange::Set(10_000));

    assert_eq!(tag.get_string(ItemKey::RecordingDate), None);
    assert_eq!(tag.get_string(ItemKey::Year), Some("10000"));
}

#[test]
fn bpm_and_lyrics_keys_follow_the_container_format() {
    // ID3v2 and MP4 only define an integer BPM (`TBPM` / `tmpo`); Vorbis
    // only a decimal `BPM`. ID3v2 only defines unsynchronized lyrics
    // (`USLT`); MP4 and Vorbis accept the plain lyrics key.
    assert_eq!(bpm_item_key(TagType::Id3v2), ItemKey::IntegerBpm);
    assert_eq!(bpm_item_key(TagType::Mp4Ilst), ItemKey::IntegerBpm);
    assert_eq!(bpm_item_key(TagType::VorbisComments), ItemKey::Bpm);

    assert_eq!(lyrics_item_key(TagType::Id3v2), ItemKey::UnsyncLyrics);
    assert_eq!(lyrics_item_key(TagType::Mp4Ilst), ItemKey::Lyrics);
    assert_eq!(lyrics_item_key(TagType::VorbisComments), ItemKey::Lyrics);
}

/// Write BPM and lyrics into a fresh `tag_type` tag exactly as
/// `write_metadata` does, round-trip it through the container-specific
/// representation — the lossy boundary lofty crosses on save + re-read,
/// supplied by `to_container_and_back` — then assert both survive.
fn assert_bpm_and_lyrics_round_trip(tag_type: TagType, to_container_and_back: impl Fn(Tag) -> Tag) {
    let mut tag = Tag::new(tag_type);
    apply_number_change(&mut tag, bpm_item_key(tag_type), FieldChange::Set(127_u32));
    apply_text_change(
        &mut tag,
        lyrics_item_key(tag_type),
        FieldChange::Set("first line\nsecond line".to_owned()),
    );

    let back = to_container_and_back(tag);

    assert_eq!(
        back.get_string(bpm_item_key(tag_type))
            .and_then(|value| value.trim().parse::<u32>().ok()),
        Some(127),
        "bpm lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(lyrics_item_key(tag_type)),
        Some("first line\nsecond line"),
        "lyrics lost for {tag_type:?}"
    );
}

#[test]
fn bpm_and_lyrics_survive_a_container_round_trip_on_every_format() {
    // Regression for #148: a survivor whose BPM/lyrics came from SQLite was
    // written with `ItemKey::Bpm`/`ItemKey::Lyrics`, which ID3v2 maps to no
    // frame at all, so the value vanished on save and consolidation's
    // read-back verification could never match. Writing the key the format
    // actually defines makes both fields round-trip everywhere.
    assert_bpm_and_lyrics_round_trip(TagType::Id3v2, |tag| Tag::from(Id3v2Tag::from(tag)));
    assert_bpm_and_lyrics_round_trip(TagType::Mp4Ilst, |tag| Tag::from(Ilst::from(tag)));
    assert_bpm_and_lyrics_round_trip(TagType::VorbisComments, |tag| {
        Tag::from(VorbisComments::from(tag))
    });
}

fn assert_full_editable_metadata_round_trip(
    tag_type: TagType,
    to_container_and_back: impl Fn(Tag) -> Tag,
) {
    let mut tag = Tag::new(tag_type);
    apply_text_change(
        &mut tag,
        ItemKey::TrackTitle,
        FieldChange::Set("Title".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::TrackArtist,
        FieldChange::Set("Artist".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::AlbumTitle,
        FieldChange::Set("Album".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::AlbumArtist,
        FieldChange::Set("Album Artist".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::Composer,
        FieldChange::Set("Composer".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::ContentGroup,
        FieldChange::Set("Grouping".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::Genre,
        FieldChange::Set("Genre".to_owned()),
    );
    apply_number_change(&mut tag, ItemKey::TrackNumber, FieldChange::Set(7_u32));
    apply_number_change(&mut tag, ItemKey::TrackTotal, FieldChange::Set(12_u32));
    apply_number_change(&mut tag, ItemKey::DiscNumber, FieldChange::Set(2_u32));
    apply_number_change(&mut tag, ItemKey::DiscTotal, FieldChange::Set(3_u32));
    apply_year_change(&mut tag, FieldChange::Set(1998));
    apply_bool_change(&mut tag, ItemKey::FlagCompilation, FieldChange::Set(true));
    apply_number_change(&mut tag, bpm_item_key(tag_type), FieldChange::Set(127_u32));
    apply_text_change(
        &mut tag,
        ItemKey::InitialKey,
        FieldChange::Set("8A".to_owned()),
    );
    apply_text_change(
        &mut tag,
        ItemKey::Comment,
        FieldChange::Set("Comment".to_owned()),
    );
    apply_text_change(
        &mut tag,
        lyrics_item_key(tag_type),
        FieldChange::Set("Lyrics".to_owned()),
    );

    let back = to_container_and_back(tag);

    assert_eq!(
        back.title().as_deref(),
        Some("Title"),
        "title lost for {tag_type:?}"
    );
    assert_eq!(
        back.artist().as_deref(),
        Some("Artist"),
        "artist lost for {tag_type:?}"
    );
    assert_eq!(
        back.album().as_deref(),
        Some("Album"),
        "album lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(ItemKey::AlbumArtist),
        Some("Album Artist"),
        "album artist lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(ItemKey::Composer),
        Some("Composer"),
        "composer lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(ItemKey::ContentGroup),
        Some("Grouping"),
        "grouping lost for {tag_type:?}"
    );
    assert_eq!(
        back.genre().as_deref(),
        Some("Genre"),
        "genre lost for {tag_type:?}"
    );
    assert_eq!(back.track(), Some(7), "track number lost for {tag_type:?}");
    assert_eq!(
        back.track_total(),
        Some(12),
        "track total lost for {tag_type:?}"
    );
    assert_eq!(back.disk(), Some(2), "disc number lost for {tag_type:?}");
    assert_eq!(
        back.disk_total(),
        Some(3),
        "disc total lost for {tag_type:?}"
    );
    assert_eq!(
        back.date().map(|date| i32::from(date.year)),
        Some(1998),
        "year lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(ItemKey::FlagCompilation)
            .and_then(parse_flag),
        Some(true),
        "compilation flag lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(bpm_item_key(tag_type))
            .and_then(|value| value.trim().parse::<u32>().ok()),
        Some(127),
        "bpm lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(ItemKey::InitialKey),
        Some("8A"),
        "key lost for {tag_type:?}"
    );
    assert_eq!(
        back.comment().as_deref(),
        Some("Comment"),
        "comment lost for {tag_type:?}"
    );
    assert_eq!(
        back.get_string(lyrics_item_key(tag_type)),
        Some("Lyrics"),
        "lyrics lost for {tag_type:?}"
    );
}

#[test]
fn editable_metadata_survives_a_container_round_trip_on_every_format() {
    assert_full_editable_metadata_round_trip(TagType::Id3v2, |tag| Tag::from(Id3v2Tag::from(tag)));
    assert_full_editable_metadata_round_trip(TagType::Mp4Ilst, |tag| Tag::from(Ilst::from(tag)));
    assert_full_editable_metadata_round_trip(TagType::VorbisComments, |tag| {
        Tag::from(VorbisComments::from(tag))
    });
}

fn assert_rating_round_trip(tag_type: TagType, to_container_and_back: impl Fn(Tag) -> Tag) {
    let mut tag = Tag::new(tag_type);
    let rating = Rating::new(4).expect("rating");
    tag.insert_text(
        ItemKey::Popularimeter,
        popularimeter_from_rating(rating, 23).to_string(),
    );

    let back = to_container_and_back(tag);
    let rating = back
        .ratings()
        .next()
        .map(|rating| star_rating_value(rating.rating()));

    assert_eq!(rating, Some(4), "rating lost for {tag_type:?}");
}

#[test]
fn rating_survives_a_container_round_trip_on_every_format() {
    assert_rating_round_trip(TagType::Id3v2, |tag| Tag::from(Id3v2Tag::from(tag)));
    assert_rating_round_trip(TagType::Mp4Ilst, |tag| Tag::from(Ilst::from(tag)));
    assert_rating_round_trip(TagType::VorbisComments, |tag| {
        Tag::from(VorbisComments::from(tag))
    });
}

#[test]
fn id3v2_drops_the_plain_bpm_and_lyrics_keys() {
    // Documents why the fix was needed: ID3v2 maps neither `ItemKey::Bpm`
    // nor `ItemKey::Lyrics`, so the old write path lost both on save.
    let mut tag = Tag::new(TagType::Id3v2);
    tag.insert_text(ItemKey::Bpm, "127".to_owned());
    tag.insert_text(ItemKey::Lyrics, "a line".to_owned());

    let back = Tag::from(Id3v2Tag::from(tag));

    assert_eq!(back.get_string(ItemKey::Bpm), None);
    assert_eq!(back.get_string(ItemKey::IntegerBpm), None);
    assert_eq!(back.get_string(ItemKey::Lyrics), None);
    assert_eq!(back.get_string(ItemKey::UnsyncLyrics), None);
}

#[test]
fn repairs_only_malformed_id3v2_language_frames() {
    // #193: COMM/USLT carry a 3-byte ISO-639-2 language lofty accepts on
    // read but refuses to write. Both surface on the abstract tag as items
    // bearing a `lang`; the repair heals the malformed ones to `XXX` and
    // leaves valid ones — and the text/description — untouched.
    let mut tag = Tag::new(TagType::Id3v2);

    let mut malformed_comment = TagItem::new(ItemKey::Comment, ItemValue::Text("note".to_owned()));
    malformed_comment.set_lang([0, 0, 0]);
    malformed_comment.set_description("desc".to_owned());
    tag.push_unchecked(malformed_comment);

    let mut valid_comment = TagItem::new(ItemKey::Comment, ItemValue::Text("keep".to_owned()));
    valid_comment.set_lang(*b"eng");
    tag.push_unchecked(valid_comment);

    let mut malformed_lyrics =
        TagItem::new(ItemKey::UnsyncLyrics, ItemValue::Text("la la".to_owned()));
    malformed_lyrics.set_lang([0, 0, 0]);
    tag.push_unchecked(malformed_lyrics);

    repair_invalid_id3v2_languages(&mut tag);

    let comments: Vec<&TagItem> = tag.get_items(ItemKey::Comment).collect();
    let healed = comments
        .iter()
        .find(|item| item.description() == "desc")
        .expect("the malformed comment survives");
    assert_eq!(healed.lang(), b"XXX", "malformed language healed to XXX");
    assert_eq!(
        healed.value().text(),
        Some("note"),
        "comment text preserved"
    );

    let untouched = comments
        .iter()
        .find(|item| item.value().text() == Some("keep"))
        .expect("the valid comment survives");
    assert_eq!(
        untouched.lang(),
        b"eng",
        "a valid language is left untouched"
    );

    let lyrics = tag
        .get_items(ItemKey::UnsyncLyrics)
        .next()
        .expect("lyrics survive");
    assert_eq!(lyrics.lang(), b"XXX", "USLT language healed too");
    assert_eq!(
        lyrics.value().text(),
        Some("la la"),
        "lyrics text preserved"
    );
}

#[test]
fn repair_leaves_valid_language_tags_byte_for_byte_unchanged() {
    // Acceptance criterion: a file without the defect must serialize exactly
    // as before, so the repair must be a true no-op on valid input.
    let mut tag = Tag::new(TagType::Id3v2);
    let mut comment = TagItem::new(ItemKey::Comment, ItemValue::Text("comment".to_owned()));
    comment.set_lang(*b"eng");
    tag.push_unchecked(comment);

    let before = dump_id3v2(tag.clone());
    repair_invalid_id3v2_languages(&mut tag);
    let after = dump_id3v2(tag);

    assert_eq!(before, after);
}

/// Serializes `tag` as ID3v2 exactly as the save path would, returning the
/// frame bytes. Panics if serialization fails, which is the very failure
/// #193 is about — so a malformed-language tag must be repaired first.
fn dump_id3v2(tag: Tag) -> Vec<u8> {
    let mut bytes = Vec::new();
    Id3v2Tag::from(tag)
        .dump_to(&mut bytes, WriteOptions::default())
        .expect("ID3v2 tag serializes");
    bytes
}

/// Builds a minimal but lofty-readable MP3: an ID3v2.3 tag carrying a single
/// `COMM` frame with the given 3-byte `language` and `text`, followed by two
/// MPEG-1 Layer III frames so lofty can read audio properties. `language` is
/// written verbatim — including malformed values like `\0\0\0` that lofty
/// accepts on read but refuses to write — which is exactly the on-disk state
/// issue #193 must heal.
fn mp3_with_comment_language(language: [u8; 3], text: &str) -> Vec<u8> {
    fn synchsafe(mut value: u32) -> [u8; 4] {
        let mut out = [0u8; 4];
        for slot in out.iter_mut().rev() {
            *slot = (value & 0x7f) as u8;
            value >>= 7;
        }
        out
    }

    // COMM content: encoding (Latin-1) + language + empty description + text.
    let mut content = vec![0x00];
    content.extend_from_slice(&language);
    content.push(0x00);
    content.extend_from_slice(text.as_bytes());

    let mut frame = Vec::new();
    frame.extend_from_slice(b"COMM");
    frame.extend_from_slice(&(content.len() as u32).to_be_bytes());
    frame.extend_from_slice(&[0x00, 0x00]); // frame flags
    frame.extend_from_slice(&content);

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"ID3");
    bytes.extend_from_slice(&[0x03, 0x00, 0x00]); // v2.3.0, no tag flags
    bytes.extend_from_slice(&synchsafe(frame.len() as u32));
    bytes.extend_from_slice(&frame);

    // Two CBR MPEG-1 Layer III frames (128 kbps, 44.1 kHz, stereo → 417
    // bytes each). lofty only parses the frame header, so zeroed audio data
    // suffices for it to report properties and accept the file.
    for _ in 0..2 {
        let mut mpeg_frame = vec![0xFF, 0xFB, 0x90, 0x00];
        mpeg_frame.resize(417, 0x00);
        bytes.extend_from_slice(&mpeg_frame);
    }

    bytes
}

/// Performs one tag-write kind against a crafted MP3 whose only `COMM` frame
/// carries the malformed `\0\0\0` language that made every mirror attempt
/// fail (#193). Asserts the write now succeeds, the comment text is
/// preserved, and the language is healed to `XXX`. Without the repair the
/// write returns `WriteFailed` and this fails.
fn assert_write_heals_invalid_language(
    write: impl Fn(&LoftyMetadataService, &Path) -> MetadataResult<()>,
) {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("malformed.mp3");
    fs::write(&path, mp3_with_comment_language([0, 0, 0], "hello")).expect("write fixture");

    write(&LoftyMetadataService, &path).expect("write heals the malformed language and succeeds");

    let back = lofty::read_from_path(&path).expect("re-read written file");
    let tag = back.primary_tag().expect("primary tag");
    assert_eq!(
        tag.comment().as_deref(),
        Some("hello"),
        "comment text preserved across the heal"
    );
    let comment = tag
        .get_items(ItemKey::Comment)
        .next()
        .expect("comment item present");
    assert_eq!(
        comment.lang(),
        b"XXX",
        "language normalized to the spec placeholder"
    );

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn write_rating_heals_invalid_comment_language() {
    assert_write_heals_invalid_language(|service, path| {
        service.write_rating(path, Rating::new(4).expect("valid rating"))
    });
}

#[test]
fn write_metadata_heals_invalid_comment_language() {
    // A metadata write that does not touch the comment still carries the
    // pre-existing malformed frame through, so it must heal it too.
    assert_write_heals_invalid_language(|service, path| {
        service.write_metadata(path, MetadataChange::default())
    });
}

#[test]
fn write_artwork_heals_invalid_comment_language() {
    assert_write_heals_invalid_language(|service, path| service.write_artwork(path, None));
}

#[test]
fn write_preserves_a_valid_comment_language() {
    // The inverse guard: a file whose comment language is already valid must
    // not be rewritten to `XXX`.
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("valid.mp3");
    fs::write(&path, mp3_with_comment_language(*b"eng", "hello")).expect("write fixture");

    LoftyMetadataService
        .write_rating(&path, Rating::new(3).expect("valid rating"))
        .expect("write a valid-language file");

    let back = lofty::read_from_path(&path).expect("re-read written file");
    let tag = back.primary_tag().expect("primary tag");
    let comment = tag
        .get_items(ItemKey::Comment)
        .next()
        .expect("comment item present");
    assert_eq!(comment.lang(), b"eng", "a valid language is left untouched");
    assert_eq!(tag.comment().as_deref(), Some("hello"));

    fs::remove_dir_all(root).expect("remove test directory");
}

fn synchsafe_size(mut value: u32) -> [u8; 4] {
    let mut out = [0u8; 4];
    for slot in out.iter_mut().rev() {
        *slot = (value & 0x7f) as u8;
        value >>= 7;
    }
    out
}

/// A minimal valid ID3v2.3 tag wrapping `payload` arbitrary body bytes.
fn id3v2_3_tag(payload: &[u8]) -> Vec<u8> {
    let mut tag = b"ID3".to_vec();
    tag.extend_from_slice(&[0x03, 0x00, 0x00]);
    tag.extend_from_slice(&synchsafe_size(payload.len() as u32));
    tag.extend_from_slice(payload);
    tag
}

/// `count` MPEG-1 Layer III, 128 kbps, 44.1 kHz frames (417 bytes each).
/// Only the 4-byte header is parsed by lofty, so zeroed payload is fine.
fn mpeg_audio_frames(count: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..count {
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
        frame.resize(417, 0x00);
        bytes.extend_from_slice(&frame);
    }
    bytes
}

/// A 1×1 PNG that passes Sustain's artwork policy.
fn tiny_png() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}

/// Builds an MP3 whose audio is pushed past lofty's write-time detection
/// window by `leading`, writes to it via `write`, and asserts the write now
/// succeeds, the audio stream is preserved byte-for-byte, the file re-reads,
/// the requested edit survived (`verify_written`), and the heal is permanent.
/// Without the leading-region heal lofty returns `UnknownFormat` and the write
/// fails (#193 follow-up).
fn assert_write_heals_leading_region(
    leading: Vec<u8>,
    write: impl Fn(&LoftyMetadataService, &Path) -> MetadataResult<()>,
    verify_written: impl Fn(&Path),
) {
    let mut fixture = leading;
    fixture.extend_from_slice(&mpeg_audio_frames(6));
    // The defect must actually exceed lofty's tolerated junk window, otherwise
    // the heal would never be exercised.
    let audio_offset =
        super::mpeg_frame::first_audio_frame_offset(&fixture).expect("fixture has audio");
    assert!(audio_offset > 1024, "fixture must reproduce the defect");
    let expected_audio = fixture[audio_offset..].to_vec();

    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("leading.mp3");
    fs::write(&path, &fixture).expect("write fixture");

    write(&LoftyMetadataService, &path).expect("write heals the leading region and succeeds");

    let healed = fs::read(&path).expect("read healed file");
    let healed_offset =
        super::mpeg_frame::first_audio_frame_offset(&healed).expect("healed file has audio");
    assert_eq!(
        &healed[healed_offset..],
        expected_audio.as_slice(),
        "audio stream preserved byte-for-byte"
    );
    assert!(
        lofty::read_from_path(&path).is_ok(),
        "healed file re-reads cleanly"
    );

    // The heal must carry the requested edit through, not merely preserve the
    // audio: a heal that silently dropped the rating or artwork would still
    // pass every check above.
    verify_written(&path);

    // The heal is permanent. lofty's own writer — the path that rejected the
    // original with `UnknownFormat` — now accepts the healed file directly, so
    // a subsequent write goes through lofty's normal path instead of compacting
    // the leading region again.
    let reopened = lofty::read_from_path(&path).expect("re-read healed file");
    let probe = root.join("probe.mp3");
    fs::copy(&path, &probe).expect("stage a copy for the lofty-native write probe");
    reopened
        .save_to_path(&probe, WriteOptions::default())
        .expect("lofty's normal write path accepts the healed file");

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn write_rating_heals_an_mp3_with_stacked_id3v2_tags() {
    // Two consecutive padding-only ID3v2 tags, as some taggers leave behind.
    // An all-zero body is valid ID3v2 padding, so lofty reads the file but
    // refuses to rewrite it because the audio sits past its detection window.
    let mut leading = id3v2_3_tag(&[0x00; 40]);
    leading.extend_from_slice(&id3v2_3_tag(&[0x00; 1500]));
    assert_write_heals_leading_region(
        leading,
        |service, path| service.write_rating(path, Rating::new(4).expect("valid rating")),
        |path| {
            let tags = LoftyMetadataService
                .read_initial_tags(path)
                .expect("re-read healed tags");
            assert_eq!(
                tags.rating,
                Rating::new(4).expect("valid rating"),
                "the written rating survived the heal"
            );
        },
    );
}

#[test]
fn write_artwork_heals_an_mp3_with_oversized_padding() {
    let mut leading = id3v2_3_tag(&[0x00; 64]);
    leading.extend_from_slice(&vec![0x00; 3000]);
    assert_write_heals_leading_region(
        leading,
        |service, path| service.write_artwork(path, Some(tiny_png())),
        |path| {
            let artwork = LoftyMetadataService
                .read_artwork(path)
                .expect("re-read healed artwork");
            assert_eq!(
                artwork.as_deref(),
                Some(tiny_png().as_slice()),
                "the written artwork survived the heal"
            );
        },
    );
}

#[test]
fn embedded_picture_selection_skips_invalid_front_cover() {
    let valid = vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];
    let mut tag = Tag::new(TagType::Id3v2);
    tag.push_picture(
        Picture::unchecked(b"not an image".to_vec())
            .pic_type(PictureType::CoverFront)
            .build(),
    );
    tag.push_picture(Picture::unchecked(valid.clone()).build());

    assert_eq!(
        valid_embedded_picture(&tag).map(Picture::data),
        Some(valid.as_slice())
    );
}

#[test]
fn scanner_recurses_and_ignores_unsupported_files() {
    let root = unique_test_directory();
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("create nested test directory");
    fs::write(root.join("one.mp3"), b"not real audio").expect("write test file");
    fs::write(nested.join("two.flac"), b"not real audio").expect("write test file");
    fs::write(root.join("notes.txt"), b"ignore").expect("write test file");

    let metadata_service =
        FakeMetadataService::for_paths([root.join("one.mp3"), nested.join("two.flac")]);
    let scan = LibraryScanner::new(&metadata_service)
        .scan(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
        )
        .expect("scan test directory");

    let scanned_paths = scan
        .tracks
        .iter()
        .map(|track| track.relative_path.as_path().to_path_buf())
        .collect::<Vec<_>>();
    assert_eq!(
        scanned_paths,
        vec![PathBuf::from("nested/two.flac"), PathBuf::from("one.mp3")]
    );
    assert_eq!(scan.skipped_unsupported_files, 1);
    assert_eq!(scan.failures, Vec::new());
    assert!(!scan.cancelled);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_returns_partial_results_when_cancellation_is_observed() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    fs::write(root.join("a.mp3"), b"audio").expect("write a.mp3");
    fs::write(root.join("b.flac"), b"audio").expect("write b.flac");

    let metadata_service =
        FakeMetadataService::for_paths([root.join("a.mp3"), root.join("b.flac")]);
    // Pre-set the cancellation flag so the very first per-entry
    // check inside the scanner trips. The walk must abort before
    // visiting any audio file and the result must report
    // `cancelled = true` so callers know not to treat unwalked
    // tracks as missing.
    let cancellation = std::sync::atomic::AtomicBool::new(true);
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &cancellation, &BTreeMap::new())
        .expect("scan test directory");

    assert!(scan.cancelled);
    assert!(!scan.complete_for_missing_reconciliation);
    assert!(scan.tracks.is_empty());

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_marks_nested_directory_read_failure_incomplete() {
    let root = unique_test_directory();
    let nested = root.join("nested");
    fs::create_dir_all(&nested).expect("create nested test directory");
    let filesystem = FaultInjectingScanFilesystem {
        unreadable_directory: Some(nested),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::default())
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_records_directory_iterator_errors_instead_of_flattening_them() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let track_path = root.join("one.mp3");
    fs::write(&track_path, b"audio").expect("write test file");
    let filesystem = FaultInjectingScanFilesystem {
        entry_error_directory: Some(root.clone()),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::for_paths([track_path]))
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert_eq!(scan.tracks.len(), 1, "safe rows remain usable");
    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_marks_stat_failures_incomplete() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let track_path = root.join("one.mp3");
    fs::write(&track_path, b"audio").expect("write test file");
    let filesystem = FaultInjectingScanFilesystem {
        unreadable_path: Some(track_path),
        ..FaultInjectingScanFilesystem::default()
    };

    let scan = LibraryScanner::new(&FakeMetadataService::default())
        .scan_with_filesystem(
            &root,
            &std::sync::atomic::AtomicBool::new(false),
            &BTreeMap::new(),
            &filesystem,
        )
        .expect("scan test directory");

    assert!(scan.tracks.is_empty());
    assert!(!scan.complete_for_missing_reconciliation);
    assert_eq!(scan.failures.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn atomic_write_keeps_open_readers_on_the_original_inode() {
    use std::io::Read;

    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("audio.bin");
    fs::write(&path, b"original-payload-bytes").expect("seed original file");

    // Open the file before the atomic write — this is the moment
    // that stands in for GStreamer holding an open fd on the
    // currently playing track.
    let mut pre_existing_reader = fs::File::open(&path).expect("open before replace");

    atomic_write_via_rename(&path, |temp_path| {
        fs::write(temp_path, b"replacement-payload").map_err(|_| MetadataError::WriteFailed)
    })
    .expect("atomic write succeeds");

    // The pre-existing reader must still see the original bytes.
    // If rename(2) were not preserving the prior inode for open
    // file descriptors, this would read either the new bytes or a
    // torn mixture — both would manifest as audio glitches in
    // GStreamer.
    let mut observed = Vec::new();
    pre_existing_reader
        .read_to_end(&mut observed)
        .expect("read pre-existing handle");
    assert_eq!(observed.as_slice(), b"original-payload-bytes");

    // A fresh open after the rename sees the replacement bytes.
    let post_swap = fs::read(&path).expect("read after replace");
    assert_eq!(post_swap.as_slice(), b"replacement-payload");

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn atomic_write_leaves_no_temp_file_when_modify_step_fails() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("audio.bin");
    fs::write(&path, b"original").expect("seed original file");

    let result =
        atomic_write_via_rename(&path, |_temp_path| Err::<(), _>(MetadataError::WriteFailed));
    assert_eq!(result, Err(MetadataError::WriteFailed));

    // The destination still holds the original content — failure
    // never replaces the user's file with partial bytes.
    let on_disk = fs::read(&path).expect("read after failure");
    assert_eq!(on_disk.as_slice(), b"original");

    // No `.sustain-*.tmp` debris lingers next to the audio file.
    #[allow(
        clippy::case_sensitive_file_extension_comparisons,
        reason = "Sustain's temporary-file suffix is the exact lowercase ASCII literal .tmp"
    )]
    let leftovers: Vec<_> = fs::read_dir(&root)
        .expect("list test directory")
        .filter_map(Result::ok)
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains("sustain-") && name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "expected no temp files, found: {leftovers:?}"
    );

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn hash_file_content_returns_sha256_hex() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("track.flac");
    fs::write(&path, b"abc").expect("write file");

    let hash = hash_file_content(&path).expect("hash file");

    assert_eq!(
        hash.as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_skips_reparsing_unchanged_files_and_reports_them_present() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let unchanged = root.join("unchanged.mp3");
    let changed = root.join("changed.mp3");
    fs::write(&unchanged, b"audio").expect("write unchanged");
    fs::write(&changed, b"audio").expect("write changed");

    // Build the "already known" fingerprints from the real on-disk stat.
    // The unchanged file's fingerprint matches exactly; the changed file's
    // recorded size is stale, so its fingerprint will not match and it must
    // be re-parsed.
    let unchanged_meta = fs::symlink_metadata(&unchanged).expect("stat unchanged");
    let changed_meta = fs::symlink_metadata(&changed).expect("stat changed");
    let unchanged_relative = TrackRelativePath::new("unchanged.mp3").expect("relative");
    let changed_relative = TrackRelativePath::new("changed.mp3").expect("relative");
    let mut known = BTreeMap::new();
    known.insert(
        unchanged_relative.clone(),
        ScanFingerprint::new(
            unchanged_meta.len(),
            unchanged_meta.modified().expect("mtime"),
        )
        .expect("fingerprint"),
    );
    known.insert(
        changed_relative.clone(),
        ScanFingerprint::new(
            changed_meta.len() + 1,
            changed_meta.modified().expect("mtime"),
        )
        .expect("fingerprint"),
    );

    let metadata_service = RecordingMetadataService::default();
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &std::sync::atomic::AtomicBool::new(false), &known)
        .expect("scan test directory");

    // Only the changed file was opened/parsed; the unchanged one was not.
    assert_eq!(metadata_service.parsed_paths(), vec![changed.clone()]);
    // The unchanged file is reported present-but-skipped, never a parsed
    // track and never a candidate for "missing".
    assert_eq!(scan.unchanged, vec![unchanged_relative]);
    let parsed: Vec<_> = scan
        .tracks
        .iter()
        .map(|track| track.relative_path.clone())
        .collect();
    assert_eq!(parsed, vec![changed_relative]);
    // The re-parsed track records a fresh mtime so the next scan can skip it.
    assert!(scan.tracks[0].file_modified_at.is_some());

    fs::remove_dir_all(root).expect("remove test directory");
}

#[test]
fn scanner_reparses_when_only_mtime_differs() {
    let root = unique_test_directory();
    fs::create_dir_all(&root).expect("create test directory");
    let path = root.join("song.mp3");
    fs::write(&path, b"audio").expect("write file");

    // Same size, but the recorded mtime is one second older than disk:
    // the fingerprint must miss and the file must be re-parsed.
    let meta = fs::symlink_metadata(&path).expect("stat");
    let stale_mtime = meta.modified().expect("mtime") - std::time::Duration::from_secs(1);
    let relative = TrackRelativePath::new("song.mp3").expect("relative");
    let mut known = BTreeMap::new();
    known.insert(
        relative.clone(),
        ScanFingerprint::new(meta.len(), stale_mtime).expect("fingerprint"),
    );

    let metadata_service = RecordingMetadataService::default();
    let scan = LibraryScanner::new(&metadata_service)
        .scan(&root, &std::sync::atomic::AtomicBool::new(false), &known)
        .expect("scan test directory");

    assert_eq!(metadata_service.parsed_paths(), vec![path]);
    assert!(scan.unchanged.is_empty());
    assert_eq!(scan.tracks.len(), 1);

    fs::remove_dir_all(root).expect("remove test directory");
}

#[derive(Default)]
struct FakeMetadataService {
    tracks: BTreeMap<PathBuf, TrackMetadata>,
}

impl FakeMetadataService {
    fn for_paths(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self {
            tracks: paths
                .into_iter()
                .map(|path| {
                    (
                        path,
                        TrackMetadata {
                            title: Some("Test".to_owned()),
                            ..TrackMetadata::default()
                        },
                    )
                })
                .collect(),
        }
    }
}

impl MetadataService for FakeMetadataService {
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        let metadata = self
            .tracks
            .get(path)
            .cloned()
            .ok_or(MetadataError::ReadFailed)?;
        Ok(InitialTags {
            metadata,
            rating: Rating::new(4).expect("valid test rating"),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, _path: &Path, _change: super::MetadataChange) -> MetadataResult<()> {
        Ok(())
    }

    fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
        Ok(())
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        Ok(())
    }
}

/// A metadata service that records every path it is asked to parse, so a
/// test can assert which files the scanner actually opened (and, by
/// omission, which it skipped via the size+mtime fingerprint). Accepts any
/// path and returns a fixed, valid tag set.
#[derive(Default)]
struct RecordingMetadataService {
    parsed: std::sync::Mutex<Vec<PathBuf>>,
}

impl RecordingMetadataService {
    fn parsed_paths(&self) -> Vec<PathBuf> {
        self.parsed
            .lock()
            .expect("recording lock not poisoned")
            .clone()
    }
}

impl MetadataService for RecordingMetadataService {
    #[allow(
        clippy::unwrap_in_result,
        reason = "the test double's mutex poisoning is a failed test invariant"
    )]
    fn read_initial_tags(&self, path: &Path) -> MetadataResult<InitialTags> {
        self.parsed
            .lock()
            .expect("recording lock not poisoned")
            .push(path.to_path_buf());
        Ok(InitialTags {
            metadata: TrackMetadata {
                title: Some("Test".to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, _path: &Path, _change: super::MetadataChange) -> MetadataResult<()> {
        Ok(())
    }

    fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
        Ok(())
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        Ok(())
    }
}

#[derive(Default)]
struct FaultInjectingScanFilesystem {
    unreadable_directory: Option<PathBuf>,
    entry_error_directory: Option<PathBuf>,
    unreadable_path: Option<PathBuf>,
}

impl ScanFilesystem for FaultInjectingScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool {
        StdScanFilesystem.is_directory(path)
    }

    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>> {
        if self.unreadable_directory.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        let entries = StdScanFilesystem.read_directory(path)?;
        if self.entry_error_directory.as_deref() == Some(path) {
            return Ok(Box::new(entries.chain(std::iter::once(Err(
                io::Error::from(io::ErrorKind::Other),
            )))));
        }
        Ok(entries)
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        if self.unreadable_path.as_deref() == Some(path) {
            return Err(io::Error::from(io::ErrorKind::PermissionDenied));
        }
        StdScanFilesystem.symlink_metadata(path)
    }
}

fn unique_test_directory() -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};

    // A wall-clock timestamp is not actually unique: two tests on
    // parallel harness threads can read the same tick (or the clock can
    // step backwards), landing in the same directory and racing each
    // other's `remove_dir_all`. Mirror the production temp-name scheme
    // (`temporary_sibling_name`) instead: a process id plus a monotonic
    // counter is collision-free within and across runs.
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sustain_metadata_test_{}_{id}", std::process::id()))
}
