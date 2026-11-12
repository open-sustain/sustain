// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::PathBuf, time::Duration};

use super::{AudioExtent, audio_extent, first_audio_frame_offset, mpeg_frame_len};

/// One MPEG-1 Layer III, 128 kbps, 44.1 kHz frame is 417 bytes. The 4-byte
/// header is followed by zeroed payload — only the header is parsed.
const FRAME_LEN: usize = 417;

fn mpeg_frame() -> Vec<u8> {
    let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
    frame.resize(FRAME_LEN, 0x00);
    frame
}

fn mpeg_frames(count: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for _ in 0..count {
        bytes.extend_from_slice(&mpeg_frame());
    }
    bytes
}

fn synchsafe(mut value: u32) -> [u8; 4] {
    let mut out = [0u8; 4];
    for slot in out.iter_mut().rev() {
        *slot = (value & 0x7f) as u8;
        value >>= 7;
    }
    out
}

/// A minimal ID3v2.3 tag of `frame_payload_len` arbitrary (non-sync) bytes.
fn id3v2_tag(frame_payload_len: usize) -> Vec<u8> {
    let body = vec![0x7Eu8; frame_payload_len];
    let mut tag = Vec::new();
    tag.extend_from_slice(b"ID3");
    tag.extend_from_slice(&[0x03, 0x00, 0x00]);
    tag.extend_from_slice(&synchsafe(body.len() as u32));
    tag.extend_from_slice(&body);
    tag
}

#[test]
fn parses_a_valid_frame_length() {
    assert_eq!(mpeg_frame_len(&mpeg_frame()), Some(FRAME_LEN));
}

#[test]
fn rejects_non_frame_headers() {
    assert_eq!(mpeg_frame_len(&[0x00, 0x00, 0x00, 0x00]), None);
    assert_eq!(mpeg_frame_len(&[0xFF]), None, "too short");
    // Reserved MPEG version bits (01).
    assert_eq!(mpeg_frame_len(&[0xFF, 0xEB, 0x90, 0x00]), None);
    // "bad" bitrate index (15).
    assert_eq!(mpeg_frame_len(&[0xFF, 0xFB, 0xF0, 0x00]), None);
    // Reserved sample-rate index (3).
    assert_eq!(mpeg_frame_len(&[0xFF, 0xFB, 0x9C, 0x00]), None);
}

#[test]
fn finds_audio_at_offset_zero() {
    assert_eq!(first_audio_frame_offset(&mpeg_frames(4)), Some(0));
}

#[test]
fn finds_audio_after_a_single_id3v2_tag() {
    let tag = id3v2_tag(2000);
    let mut bytes = tag.clone();
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(tag.len()));
}

#[test]
fn finds_audio_after_stacked_id3v2_tags() {
    // Two consecutive ID3v2 tags, as some taggers leave behind — the exact
    // shape that pushes the audio past lofty's write-time detection window.
    let mut leading = id3v2_tag(1500);
    leading.extend_from_slice(&id3v2_tag(900));
    let offset = leading.len();
    let mut bytes = leading;
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(offset));
}

#[test]
fn finds_audio_after_oversized_zero_padding() {
    // A single tag followed by far more than lofty's 1024-byte junk window of
    // zero padding before the first frame.
    let tag = id3v2_tag(64);
    let mut bytes = tag.clone();
    bytes.extend_from_slice(&vec![0x00; 4096]);
    let offset = bytes.len();
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(offset));
}

#[test]
fn finds_audio_after_non_zero_junk() {
    // Some real files carry non-zero filler (e.g. a stray LAME string and
    // 0x55 padding) between the tag and the audio — the case a "skip tags and
    // zero padding" heuristic misses. Chaining still locates the real frame.
    let tag = id3v2_tag(64);
    let mut junk = b"LAME3.99".to_vec();
    junk.extend_from_slice(&vec![0x55; 2000]);
    let mut bytes = tag;
    bytes.extend_from_slice(&junk);
    let offset = bytes.len();
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(offset));
}

#[test]
fn ignores_a_lone_false_sync_that_does_not_chain() {
    // A valid-looking frame header that is not followed by further frames must
    // be rejected in favour of the real, chained audio that follows.
    let mut bytes = vec![0xFF, 0xFB, 0x90, 0x00];
    bytes.extend_from_slice(&[0x11; 20]); // not another frame
    let offset = bytes.len();
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(offset));
}

#[test]
fn returns_none_without_audio() {
    assert_eq!(first_audio_frame_offset(&id3v2_tag(500)), None);
    assert_eq!(first_audio_frame_offset(&[]), None);
}

#[test]
fn a_tag_header_claiming_more_than_the_file_holds_is_not_skipped() {
    // A truncated or corrupt ID3v2 header whose declared size runs past the
    // end of the data: nothing lies behind it to skip to. The scan starts at
    // the header instead, and the chained frames inside the claimed region
    // are still found.
    let mut bytes = b"ID3".to_vec();
    bytes.extend_from_slice(&[0x03, 0x00, 0x00]);
    bytes.extend_from_slice(&synchsafe(1_000_000));
    let offset = bytes.len();
    bytes.extend_from_slice(&mpeg_frames(4));
    assert_eq!(first_audio_frame_offset(&bytes), Some(offset));

    // The same after a tag that does fit.
    let mut stacked = id3v2_tag(300);
    stacked.extend_from_slice(b"ID3");
    stacked.extend_from_slice(&[0x04, 0x00, 0x00]);
    stacked.extend_from_slice(&synchsafe(1_000_000));
    assert_eq!(first_audio_frame_offset(&stacked), None);
}

/// One MPEG-1 Layer III, 44.1 kHz frame at the bitrate `bitrate_index`
/// stands for, with a zeroed payload.
fn mpeg_frame_at(bitrate_index: u8) -> Vec<u8> {
    let header = [0xFF, 0xFB, bitrate_index << 4, 0x00];
    let frame_len = mpeg_frame_len(&header).expect("a valid frame header");
    let mut frame = header.to_vec();
    frame.resize(frame_len, 0x00);
    frame
}

/// `frame_count` frames whose bitrate changes from one to the next,
/// opening on the lowest one (32 kbps) as a quiet intro does.
fn variable_bitrate_frames(frame_count: usize) -> Vec<u8> {
    const BITRATE_INDICES: [u8; 4] = [1, 9, 14, 5];
    (0..frame_count)
        .flat_map(|index| mpeg_frame_at(BITRATE_INDICES[index % BITRATE_INDICES.len()]))
        .collect()
}

/// A frame that carries no audio, only `header` where an MPEG-1 stereo
/// Layer III frame keeps its Xing or Info header.
fn frame_holding(offset: usize, header: &[u8]) -> Vec<u8> {
    let mut frame = mpeg_frame();
    frame[offset..offset + header.len()].copy_from_slice(header);
    frame
}

fn xing_frame(frame_count: u32) -> Vec<u8> {
    let mut xing = b"Xing".to_vec();
    xing.extend_from_slice(&1u32.to_be_bytes()); // flags: frame count present
    xing.extend_from_slice(&frame_count.to_be_bytes());
    frame_holding(4 + 32, &xing)
}

fn vbri_frame(frame_count: u32) -> Vec<u8> {
    let mut vbri = b"VBRI".to_vec();
    vbri.extend_from_slice(&[0; 6]); // version, delay, quality
    vbri.extend_from_slice(&0u32.to_be_bytes()); // byte count
    vbri.extend_from_slice(&frame_count.to_be_bytes());
    frame_holding(4 + 32, &vbri)
}

/// How long `frame_count` MPEG-1 Layer III frames play at 44.1 kHz.
fn playing_time(frame_count: u64) -> Duration {
    Duration::from_nanos(frame_count * 1152 * 1_000_000_000 / 44_100)
}

struct TemporaryFile(PathBuf);

impl TemporaryFile {
    fn holding(name: &str, bytes: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "sustain_mpeg_frame_test_{}_{name}.mp3",
            std::process::id()
        ));
        std::fs::write(&path, bytes).expect("write the test file");
        Self(path)
    }
}

impl Drop for TemporaryFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn extent_of(name: &str, bytes: &[u8]) -> Option<AudioExtent> {
    audio_extent(&TemporaryFile::holding(name, bytes).0)
}

#[test]
fn a_stream_without_a_frame_count_plays_as_long_as_its_frames() {
    let audio = variable_bitrate_frames(400);
    let extent = extent_of("counted", &audio).expect("an audio extent");

    assert_eq!(extent.duration, playing_time(400));
    // Not the 32 kbps of the frame the stream opens on.
    let kilobits = audio.len() as f64 * 8.0 / 1000.0;
    let expected_kbps = (kilobits / playing_time(400).as_secs_f64()).round() as u32;
    assert_eq!(extent.measured_bitrate_kbps, Some(expected_kbps));
    assert!(expected_kbps > 100, "{expected_kbps} kbps");
}

#[test]
fn a_declared_frame_count_is_taken_without_reading_the_stream_through() {
    for (name, header_frame) in [("xing", xing_frame(5_000)), ("vbri", vbri_frame(5_000))] {
        // The count is what the header says, not the few frames behind it.
        let mut bytes = header_frame;
        bytes.extend_from_slice(&mpeg_frames(8));
        assert_eq!(
            extent_of(name, &bytes),
            Some(AudioExtent {
                duration: playing_time(5_000),
                measured_bitrate_kbps: None,
            }),
            "{name}"
        );
    }
}

#[test]
fn a_header_frame_without_a_frame_count_has_its_stream_counted() {
    let mut xing = b"Xing".to_vec();
    xing.extend_from_slice(&0u32.to_be_bytes()); // flags: nothing present
    let mut bytes = frame_holding(4 + 32, &xing);
    bytes.extend_from_slice(&mpeg_frames(99));

    let extent = extent_of("flagless", &bytes).expect("an audio extent");
    assert_eq!(extent.duration, playing_time(100));
}

#[test]
fn tags_and_damage_around_the_frames_are_not_counted() {
    // A tag too large for the leading region, holding bytes that look like
    // frame syncs; damage in the middle of the audio; an ID3v1 tag after it.
    let mut bytes = id3v2_tag(2 * 1024 * 1024);
    bytes[10..14].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    bytes.extend_from_slice(&variable_bitrate_frames(200));
    bytes.extend_from_slice(&[0x12, 0x34, 0xFF, 0xFB, 0x90, 0x00, 0xFF, 0xFF]);
    bytes.extend_from_slice(&variable_bitrate_frames(100));
    bytes.extend_from_slice(b"TAG");
    bytes.extend_from_slice(&[0x20; 125]);

    let extent = extent_of("damaged", &bytes).expect("an audio extent");
    assert_eq!(extent.duration, playing_time(300));
}

#[test]
fn a_file_without_audio_frames_has_no_extent() {
    assert_eq!(extent_of("no_audio", &[0x7E; 4096]), None);
    assert_eq!(extent_of("empty", &[]), None);
    assert_eq!(
        audio_extent(&std::env::temp_dir().join("sustain_mpeg_frame_test_missing.mp3")),
        None
    );
}
