// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Minimal MPEG audio frame parsing, used only to locate where the audio
//! stream of an MP3 begins.
//!
//! lofty reads MP3s by trusting the file extension and scanning the whole
//! file for the first frame, but its *write* path re-detects the format from
//! content and only tolerates a small window (`DEFAULT_MAX_JUNK_BYTES`, 1024)
//! of leading junk before the first frame sync. Files that carry stacked
//! ID3v2 tags or oversized padding between the tag and the audio push the
//! first frame past that window, so lofty refuses to write them
//! (`UnknownFormat`). To heal such a file Sustain compacts its leading region
//! down to the first audio frame; doing that safely means locating that frame
//! without cutting into the audio, which is what this module provides.
//!
//! Detection is deliberately conservative: a candidate offset is only
//! accepted when its frame header is valid *and* the following frames chain
//! to it at their computed lengths, which rejects the stray `0xFF Ex` byte
//! pairs that occur inside tag or padding bytes. A simpler "skip the tags and
//! zero padding" heuristic is not enough — some files carry non-zero junk
//! (e.g. a stray LAME string and filler) between the tag and the audio.

/// How many consecutive frames must chain before an offset is accepted as the
/// real start of the audio stream. One valid header can occur by chance
/// inside junk; several in a row at their exact computed lengths effectively
/// cannot.
const REQUIRED_CHAINED_FRAMES: usize = 3;

/// Upper bound on how far into the file we look for the first frame. The
/// audio of a real MP3 begins within at most a few tens of kilobytes of the
/// start (tag + padding); a file with no frame sync in the first mebibyte is
/// not something we will rewrite.
const MAX_LEADING_REGION_BYTES: usize = 1024 * 1024;

/// Returns the byte offset of the first MPEG audio frame in `bytes`, or
/// `None` if no chained run of valid frames is found within
/// [`MAX_LEADING_REGION_BYTES`].
pub(crate) fn first_audio_frame_offset(bytes: &[u8]) -> Option<usize> {
    // Skip any stacked ID3v2 tags up front so their frame contents are not
    // scanned for false syncs. Whatever remains (padding, junk, or audio) is
    // searched frame-by-frame; the chaining check is what actually validates
    // the start, so this skip is an optimisation, not a correctness crutch.
    let scan_start = skip_leading_id3v2_tags(bytes);
    let scan_limit = bytes.len().min(MAX_LEADING_REGION_BYTES);

    let mut offset = scan_start;
    while offset < scan_limit {
        if frames_chain_from(bytes, offset) {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

/// Total on-disk length of consecutive ID3v2 tags at the start of `bytes`.
/// Returns 0 when the data does not begin with an ID3v2 tag.
fn skip_leading_id3v2_tags(bytes: &[u8]) -> usize {
    let mut offset = 0;
    while let Some(tag_len) = id3v2_tag_len(&bytes[offset..]) {
        offset += tag_len;
    }
    offset
}

/// On-disk length of an ID3v2 tag at the start of `bytes` (10-byte header +
/// declared size + optional 10-byte footer), or `None` if `bytes` does not
/// start with a valid-looking ID3v2 header.
fn id3v2_tag_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 10 || &bytes[0..3] != b"ID3" {
        return None;
    }
    // A real ID3v2 version byte is 2, 3, or 4; 0xFF is reserved and is the
    // signature of an MPEG frame sync, so rejecting it keeps a frame that
    // happens to follow "ID3"-like bytes from being mistaken for a tag.
    if bytes[3] == 0xFF || bytes[4] == 0xFF {
        return None;
    }
    let size = synchsafe_u32(&bytes[6..10])? as usize;
    let has_footer = bytes[5] & 0x10 != 0;
    Some(10 + size + if has_footer { 10 } else { 0 })
}

/// Decodes a 4-byte ID3v2 synchsafe integer (7 bits per byte). Returns `None`
/// if any byte has its high bit set, which a valid synchsafe integer never
/// does.
fn synchsafe_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.iter().any(|byte| byte & 0x80 != 0) {
        return None;
    }
    Some(
        (u32::from(bytes[0]) << 21)
            | (u32::from(bytes[1]) << 14)
            | (u32::from(bytes[2]) << 7)
            | u32::from(bytes[3]),
    )
}

/// True when a valid MPEG frame begins at `offset` and at least
/// [`REQUIRED_CHAINED_FRAMES`] frames follow at their computed lengths
/// (tolerating a final frame truncated by end-of-file).
fn frames_chain_from(bytes: &[u8], offset: usize) -> bool {
    let mut position = offset;
    for index in 0..REQUIRED_CHAINED_FRAMES {
        let Some(frame_len) = mpeg_frame_len(&bytes[position..]) else {
            return false;
        };
        position += frame_len;
        if position > bytes.len() {
            // The trailing frame runs to EOF: accept once at least one earlier
            // frame in the run has already validated, which still rules out a
            // lone chance sync.
            return index >= 1;
        }
    }
    true
}

/// Parses an MPEG audio frame header at the start of `bytes` and returns the
/// frame's total length in bytes, or `None` if the header is not a valid
/// MPEG-1/2/2.5 Layer I/II/III frame with a usable bitrate and sample rate.
fn mpeg_frame_len(bytes: &[u8]) -> Option<usize> {
    if bytes.len() < 4 {
        return None;
    }
    // 11-bit frame sync.
    if bytes[0] != 0xFF || bytes[1] & 0xE0 != 0xE0 {
        return None;
    }

    let version = match (bytes[1] >> 3) & 0x3 {
        0 => Mpeg::V25,
        2 => Mpeg::V2,
        3 => Mpeg::V1,
        _ => return None, // reserved
    };
    let layer = match (bytes[1] >> 1) & 0x3 {
        1 => Layer::Three,
        2 => Layer::Two,
        3 => Layer::One,
        _ => return None, // reserved
    };

    let bitrate_index = (bytes[2] >> 4) & 0xF;
    let sample_rate_index = (bytes[2] >> 2) & 0x3;
    let padding = usize::from(bytes[2] & 0x2 != 0);

    let bitrate = bitrate_bps(version, layer, bitrate_index)?;
    let sample_rate = sample_rate_hz(version, sample_rate_index)?;

    let frame_len = match layer {
        Layer::One => (12 * bitrate / sample_rate + padding) * 4,
        _ => samples_per_frame(version, layer) / 8 * bitrate / sample_rate + padding,
    };

    (frame_len >= 4).then_some(frame_len)
}

#[derive(Clone, Copy)]
enum Mpeg {
    V1,
    V2,
    V25,
}

#[derive(Clone, Copy)]
enum Layer {
    One,
    Two,
    Three,
}

fn samples_per_frame(version: Mpeg, layer: Layer) -> usize {
    match (version, layer) {
        (_, Layer::One) => 384,
        (_, Layer::Two) => 1152,
        (Mpeg::V1, Layer::Three) => 1152,
        (Mpeg::V2 | Mpeg::V25, Layer::Three) => 576,
    }
}

/// Bitrate in bits per second for the given version/layer/index, or `None`
/// for the "free" (0) and "bad" (15) indices, which carry no usable rate.
fn bitrate_bps(version: Mpeg, layer: Layer, index: u8) -> Option<usize> {
    // Tables in kbps, indexed 1..=14 (0 = free, 15 = bad are rejected below).
    const V1_L1: [usize; 15] = [
        0, 32, 64, 96, 128, 160, 192, 224, 256, 288, 320, 352, 384, 416, 448,
    ];
    const V1_L2: [usize; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 384,
    ];
    const V1_L3: [usize; 15] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
    ];
    const V2_L1: [usize; 15] = [
        0, 32, 48, 56, 64, 80, 96, 112, 128, 144, 160, 176, 192, 224, 256,
    ];
    const V2_L23: [usize; 15] = [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];

    if index == 0 || index >= 15 {
        return None;
    }
    let table = match (version, layer) {
        (Mpeg::V1, Layer::One) => V1_L1,
        (Mpeg::V1, Layer::Two) => V1_L2,
        (Mpeg::V1, Layer::Three) => V1_L3,
        (Mpeg::V2 | Mpeg::V25, Layer::One) => V2_L1,
        (Mpeg::V2 | Mpeg::V25, Layer::Two | Layer::Three) => V2_L23,
    };
    Some(table[index as usize] * 1000)
}

/// Sample rate in Hz for the given version and index, or `None` for the
/// reserved index 3.
fn sample_rate_hz(version: Mpeg, index: u8) -> Option<usize> {
    let table = match version {
        Mpeg::V1 => [44100, 48000, 32000],
        Mpeg::V2 => [22050, 24000, 16000],
        Mpeg::V25 => [11025, 12000, 8000],
    };
    table.get(index as usize).copied()
}

#[cfg(test)]
#[path = "mpeg_frame_tests.rs"]
mod tests;
