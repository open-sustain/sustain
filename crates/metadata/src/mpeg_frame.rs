// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Minimal MPEG audio frame parsing, used to locate where the audio
//! stream of an MP3 begins and to tell how long that stream plays.
//!
//! # Where the audio begins
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
//!
//! # How long the audio plays
//!
//! An MP3 does not state its length. An encoder may record its frame count
//! in a Xing, Info or VBRI header inside the first frame; without one the
//! only way to know is to count the frames. lofty does not count: it divides
//! the stream's size by the *first* frame's bitrate, which is right for a
//! constant bitrate and arbitrarily wrong for a variable one (a 5-minute
//! track that opens on a 32 kbps frame reads as 37 minutes).
//! [`audio_extent`] takes the declared count when there is one and counts
//! the frames otherwise — unless a few short samples across the stream show
//! its bitrate is constant. lofty's estimate then stands, and the file is
//! not read through: it is right but for an encoder that never pads its
//! frames, which runs a fraction of a percent short of the bitrate it
//! declares, and counting would read the whole file for that.

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
    time::Duration,
};

/// How many consecutive frames must chain before an offset is accepted as the
/// real start of the audio stream. One valid header can occur by chance
/// inside junk; several in a row at their exact computed lengths effectively
/// cannot.
const REQUIRED_CHAINED_FRAMES: usize = 3;

/// Upper bound on how far past the leading ID3v2 tags we look for the first
/// frame. The audio of a real MP3 begins within at most a few tens of
/// kilobytes of its tags (padding, junk); a file with no frame sync in the
/// mebibyte after them is not something we will rewrite. The bound starts
/// after the tags because a valid tag carrying large artwork can itself
/// exceed it.
const MAX_LEADING_REGION_BYTES: usize = 1024 * 1024;

/// Returns the byte offset of the first MPEG audio frame in `bytes`, or
/// `None` if no chained run of valid frames is found within
/// [`MAX_LEADING_REGION_BYTES`] after the leading ID3v2 tags.
pub(crate) fn first_audio_frame_offset(bytes: &[u8]) -> Option<usize> {
    // Skip any stacked ID3v2 tags up front so their frame contents are not
    // scanned for false syncs. Whatever remains (padding, junk, or audio) is
    // searched frame-by-frame; the chaining check is what actually validates
    // the start, so this skip is an optimisation, not a correctness crutch.
    let scan_start = skip_leading_id3v2_tags(bytes);
    let scan_limit = bytes
        .len()
        .min(scan_start.saturating_add(MAX_LEADING_REGION_BYTES));

    let mut offset = scan_start;
    while offset < scan_limit {
        if frames_chain_from(bytes, offset) {
            return Some(offset);
        }
        offset += 1;
    }
    None
}

/// How much audio [`audio_extent`] reads first, before falling back to the
/// whole leading region.
const INITIAL_AUDIO_WINDOW_BYTES: usize = 64 * 1024;

/// The longest frame [`FrameHeader::parse`] accepts: a padded Layer II
/// frame at 160 kbps and 8 kHz (MPEG-2.5), 144 × 160,000 / 8,000 + 1 bytes.
const MAX_FRAME_LEN: usize = 2881;

/// Below this much audio, counting its frames reads little more than
/// sampling them would.
const SAMPLED_AUDIO_MIN_BYTES: u64 = 4 * INITIAL_AUDIO_WINDOW_BYTES as u64;

/// Where [`audio_extent`] samples the bitrate of a stream that declares no
/// frame count, as fractions of its audio: away from the quiet opening and
/// close a variable bitrate runs low through. Each sample is
/// [`INITIAL_AUDIO_WINDOW_BYTES`] long.
const BITRATE_SAMPLE_POINTS: [(u64, u64); 3] = [(1, 4), (1, 2), (3, 4)];

/// Reads from `file` until `buffer` holds `len` bytes; returns whether the
/// file ended first.
fn read_up_to(file: &mut impl Read, buffer: &mut Vec<u8>, len: usize) -> Option<bool> {
    let missing = len.saturating_sub(buffer.len());
    file.by_ref()
        .take(missing as u64)
        .read_to_end(buffer)
        .ok()?;
    Some(buffer.len() < len)
}

/// How long the MPEG audio of a file plays, and the bitrate it averages
/// when that had to be measured.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct AudioExtent {
    pub(crate) duration: Duration,
    /// The stream's average bitrate, known only when its frames were
    /// counted: a stream that declares its frame count is not read through.
    pub(crate) measured_bitrate_kbps: Option<u32>,
}

/// The extent of the MPEG audio in the file at `path`, or `None` when
/// lofty's estimate from the first frame's bitrate stands: the stream's
/// bitrate is constant, the file cannot be read, or no audio frame is found
/// near its start.
///
/// A file that declares its frame count costs two short reads, and one
/// with a constant bitrate three more; only one whose bitrate varies is
/// read through.
pub(crate) fn audio_extent(path: &Path) -> Option<AudioExtent> {
    audio_extent_of(&mut File::open(path).ok()?)
}

fn audio_extent_of(file: &mut (impl Read + Seek)) -> Option<AudioExtent> {
    let tags_len = leading_id3v2_tags_len(file)?;
    let audio_len = file.seek(SeekFrom::End(0)).ok()?.saturating_sub(tags_len);
    file.seek(SeekFrom::Start(tags_len)).ok()?;
    let mut audio = Vec::new();
    // Real audio starts right after its tags, so a short window almost
    // always holds the first frames — and with them a declared count. An
    // offset found in the window is only trusted when the chains of it and
    // of every earlier candidate fit inside, so that it is the offset the
    // whole leading region would yield.
    let window_is_whole_file = read_up_to(file, &mut audio, INITIAL_AUDIO_WINDOW_BYTES)?;
    let first_frame_offset = match first_audio_frame_offset(&audio).filter(|&offset| {
        window_is_whole_file || offset + REQUIRED_CHAINED_FRAMES * MAX_FRAME_LEN <= audio.len()
    }) {
        Some(offset) => offset,
        None => {
            read_up_to(file, &mut audio, MAX_LEADING_REGION_BYTES)?;
            first_audio_frame_offset(&audio)?
        }
    };
    let first_frame = FrameHeader::parse(&audio[first_frame_offset..])?;
    if let Some(frame_count) = declared_frame_count(&audio[first_frame_offset..], &first_frame) {
        return Some(AudioExtent {
            duration: first_frame.duration_of(frame_count),
            measured_bitrate_kbps: None,
        });
    }

    if audio_len >= SAMPLED_AUDIO_MIN_BYTES
        && bitrate_is_constant(
            file,
            tags_len,
            audio_len,
            &audio[first_frame_offset..],
            &first_frame,
        )?
    {
        return None;
    }

    file.seek(SeekFrom::Start(tags_len + audio.len() as u64))
        .ok()?;
    file.read_to_end(&mut audio).ok()?;
    let counted = count_frames(&audio[first_frame_offset..], &first_frame);
    let duration = first_frame.duration_of(counted.frames);
    Some(AudioExtent {
        duration,
        measured_bitrate_kbps: average_bitrate_kbps(counted.bytes, duration),
    })
}

/// Total on-disk length of the consecutive ID3v2 tags at the start of
/// `file`, read from their headers alone: a tag holding artwork runs to
/// megabytes. `None` when the file cannot be read.
fn leading_id3v2_tags_len(file: &mut (impl Read + Seek)) -> Option<u64> {
    let mut offset = 0;
    loop {
        file.seek(SeekFrom::Start(offset)).ok()?;
        let mut header = [0; ID3V2_HEADER_LEN];
        let mut read = 0;
        while read < header.len() {
            match file.read(&mut header[read..]).ok()? {
                0 => return Some(offset),
                count => read += count,
            }
        }
        match id3v2_declared_len(&header) {
            Some(tag_len) => offset += tag_len as u64,
            None => return Some(offset),
        }
    }
}

/// The frame count a Xing, Info or VBRI header in the stream's first frame
/// declares, if the frame holds one. Such a frame carries no audio and the
/// count leaves it out.
fn declared_frame_count(first_frame: &[u8], header: &FrameHeader) -> Option<u64> {
    const XING_FRAME_COUNT_FLAG: u32 = 0x1;
    /// A VBRI header always sits 32 bytes after the frame header.
    const VBRI_OFFSET: usize = 4 + 32;
    /// Version, delay and quality (2 bytes each), then the byte count (4).
    const VBRI_FRAME_COUNT_OFFSET: usize = 4 + 2 + 2 + 2 + 4;

    let frame = first_frame.get(..header.len)?;
    let count = if let Some(xing) = header
        .xing_offset()
        .and_then(|offset| frame.get(offset..))
        .filter(|xing| xing.starts_with(b"Xing") || xing.starts_with(b"Info"))
    {
        let flags = big_endian_u32(xing.get(4..)?)?;
        if flags & XING_FRAME_COUNT_FLAG == 0 {
            return None;
        }
        big_endian_u32(xing.get(8..)?)?
    } else {
        let vbri = frame.get(VBRI_OFFSET..)?;
        if !vbri.starts_with(b"VBRI") {
            return None;
        }
        big_endian_u32(vbri.get(VBRI_FRAME_COUNT_OFFSET..)?)?
    };
    (count > 0).then_some(u64::from(count))
}

/// Whether the stream `first_frame` starts keeps that frame's bitrate
/// through `opening`, the audio read from its first frame on, and through a
/// sample at each of [`BITRATE_SAMPLE_POINTS`] of the `audio_len` bytes of
/// audio from `audio_start` in `file`. A sample holding anything but such
/// frames, or none at all, does not show a constant bitrate.
fn bitrate_is_constant(
    file: &mut (impl Read + Seek),
    audio_start: u64,
    audio_len: u64,
    opening: &[u8],
    first_frame: &FrameHeader,
) -> Option<bool> {
    if !frames_keep_bitrate(opening, first_frame) {
        return Some(false);
    }
    let mut sample = Vec::with_capacity(INITIAL_AUDIO_WINDOW_BYTES);
    for (numerator, denominator) in BITRATE_SAMPLE_POINTS {
        file.seek(SeekFrom::Start(
            audio_start + audio_len * numerator / denominator,
        ))
        .ok()?;
        sample.clear();
        read_up_to(file, &mut sample, INITIAL_AUDIO_WINDOW_BYTES)?;
        // A sample starts anywhere in a frame; the next frame starts within
        // the longest frame's length.
        let first_whole_frame = (0..MAX_FRAME_LEN.min(sample.len())).find(|&offset| {
            FrameHeader::parse(&sample[offset..])
                .is_some_and(|frame| frame.is_of_the_same_stream_as(first_frame))
                && frames_chain_from(&sample, offset)
        });
        match first_whole_frame {
            Some(offset) if frames_keep_bitrate(&sample[offset..], first_frame) => {}
            _ => return Some(false),
        }
    }
    Some(true)
}

/// Whether `frames`, walked from frame to frame up to its end, are all
/// frames of `first_frame`'s stream at its bitrate. The last frame may run
/// past the end.
fn frames_keep_bitrate(frames: &[u8], first_frame: &FrameHeader) -> bool {
    let mut offset = 0;
    while offset + FRAME_HEADER_LEN <= frames.len() {
        match FrameHeader::parse(&frames[offset..]) {
            Some(frame)
                if frame.is_of_the_same_stream_as(first_frame)
                    && frame.bitrate == first_frame.bitrate =>
            {
                offset += frame.len;
            }
            _ => return false,
        }
    }
    true
}

fn big_endian_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

/// The frames of one stream and the bytes they take up.
struct CountedFrames {
    frames: u64,
    bytes: u64,
}

/// Count the frames of the stream `first_frame` starts, from the start of
/// `audio` to its end.
///
/// A frame counts when it belongs to that stream (same MPEG version, layer
/// and sample rate) and either follows the previous frame directly or
/// starts a chained run of its own. Anything else, such as a tag after the
/// audio or damage in the middle of it, is stepped over a byte at a time.
fn count_frames(audio: &[u8], first_frame: &FrameHeader) -> CountedFrames {
    let mut counted = CountedFrames {
        frames: 0,
        bytes: 0,
    };
    let mut offset = 0;
    let mut follows_a_frame = false;
    while offset < audio.len() {
        match FrameHeader::parse(&audio[offset..]) {
            Some(frame)
                if frame.is_of_the_same_stream_as(first_frame)
                    && (follows_a_frame || frames_chain_from(audio, offset)) =>
            {
                counted.frames += 1;
                counted.bytes += frame.len.min(audio.len() - offset) as u64;
                offset += frame.len;
                follows_a_frame = true;
            }
            _ => {
                offset += 1;
                follows_a_frame = false;
            }
        }
    }
    counted
}

fn average_bitrate_kbps(audio_bytes: u64, duration: Duration) -> Option<u32> {
    let milliseconds = u64::try_from(duration.as_millis()).ok()?;
    if milliseconds == 0 {
        return None;
    }
    // Bits per millisecond are kilobits per second.
    u32::try_from((audio_bytes * 8 + milliseconds / 2) / milliseconds).ok()
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
/// start with a valid-looking ID3v2 header or ends before the tag does: a
/// header claiming more than the data holds has nothing behind it to skip to.
fn id3v2_tag_len(bytes: &[u8]) -> Option<usize> {
    let header = bytes.get(..ID3V2_HEADER_LEN)?.try_into().ok()?;
    let tag_len = id3v2_declared_len(header)?;
    (tag_len <= bytes.len()).then_some(tag_len)
}

const ID3V2_HEADER_LEN: usize = 10;

/// On-disk length an ID3v2 header declares for its tag (the header itself,
/// the declared size and an optional 10-byte footer), or `None` if `header`
/// is not a valid-looking ID3v2 header.
fn id3v2_declared_len(header: &[u8; ID3V2_HEADER_LEN]) -> Option<usize> {
    if &header[0..3] != b"ID3" {
        return None;
    }
    // A real ID3v2 version byte is 2, 3, or 4; 0xFF is reserved and is the
    // signature of an MPEG frame sync, so rejecting it keeps a frame that
    // happens to follow "ID3"-like bytes from being mistaken for a tag.
    if header[3] == 0xFF || header[4] == 0xFF {
        return None;
    }
    let size = synchsafe_u32(&header[6..10])? as usize;
    let has_footer = header[5] & 0x10 != 0;
    Some(ID3V2_HEADER_LEN + size + if has_footer { 10 } else { 0 })
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
    FrameHeader::parse(bytes).map(|header| header.len)
}

const FRAME_HEADER_LEN: usize = 4;

/// What an MPEG audio frame header says about its frame.
#[derive(Clone, Copy)]
struct FrameHeader {
    version: Mpeg,
    layer: Layer,
    /// In bits per second.
    bitrate: usize,
    sample_rate: usize,
    single_channel: bool,
    /// The frame's total length in bytes, header included.
    len: usize,
}

impl FrameHeader {
    /// Parses the frame header at the start of `bytes`, or `None` if it is
    /// not a valid MPEG-1/2/2.5 Layer I/II/III header with a usable bitrate
    /// and sample rate.
    fn parse(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < FRAME_HEADER_LEN {
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
        let single_channel = (bytes[3] >> 6) & 0x3 == 0x3;

        let bitrate = bitrate_bps(version, layer, bitrate_index)?;
        let sample_rate = sample_rate_hz(version, sample_rate_index)?;

        let len = match layer {
            Layer::One => (12 * bitrate / sample_rate + padding) * 4,
            _ => samples_per_frame(version, layer) / 8 * bitrate / sample_rate + padding,
        };

        (len >= FRAME_HEADER_LEN).then_some(Self {
            version,
            layer,
            bitrate,
            sample_rate,
            single_channel,
            len,
        })
    }

    /// Whether `other` can be a frame of the stream this one belongs to:
    /// bitrate, padding and channel mode may vary from frame to frame, these
    /// do not.
    fn is_of_the_same_stream_as(&self, other: &Self) -> bool {
        self.version == other.version
            && self.layer == other.layer
            && self.sample_rate == other.sample_rate
    }

    /// How long `frame_count` frames of this frame's stream play.
    fn duration_of(&self, frame_count: u64) -> Duration {
        let samples = frame_count * samples_per_frame(self.version, self.layer) as u64;
        let sample_rate = self.sample_rate as u64;
        Duration::new(
            samples / sample_rate,
            // Below one second's worth of samples, so within `u32`.
            (samples % sample_rate * 1_000_000_000 / sample_rate) as u32,
        )
    }

    /// Where a Xing or Info header sits in this frame: right after the
    /// Layer III side information, whose size depends on the MPEG version
    /// and the channel count. `None` for the other layers, which have none.
    fn xing_offset(&self) -> Option<usize> {
        let side_information_len = match (self.layer, self.version, self.single_channel) {
            (Layer::One | Layer::Two, _, _) => return None,
            (Layer::Three, Mpeg::V1, false) => 32,
            (Layer::Three, Mpeg::V1, true) | (Layer::Three, Mpeg::V2 | Mpeg::V25, false) => 17,
            (Layer::Three, Mpeg::V2 | Mpeg::V25, true) => 9,
        };
        Some(4 + side_information_len)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mpeg {
    V1,
    V2,
    V25,
}

#[derive(Clone, Copy, PartialEq, Eq)]
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
