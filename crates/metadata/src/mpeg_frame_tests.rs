// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::{first_audio_frame_offset, mpeg_frame_len};

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
