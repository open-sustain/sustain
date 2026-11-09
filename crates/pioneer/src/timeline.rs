// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Where a Pioneer player's timeline starts, relative to Sustain's.
//!
//! Sustain measures every time — beats, waveform entries — on the
//! *gapless* timeline: the audio a decoder renders once it has dropped
//! the encoder's delay. Pioneer players drop no MP3 delay. For most files
//! the two agree, but an MP3 that carries a Xing/Info header frame starts
//! later on the player, by an amount set by the encoder that wrote it:
//!
//! * **LAME** (and LAME-compatible encoders): the 529-sample decoder
//!   delay plus the encoder delay recorded in the LAME tag, plus one
//!   frame — the player renders the header frame as a frame of silence.
//! * **FFmpeg** (`Lavf`/`Lavc` in the tag): the decoder and encoder delay,
//!   but no header frame.
//! * **Any other encoder** with a Xing/Info frame: one frame. Sustain's
//!   decoder trims nothing for these encoders, but skips the header frame.
//! * **No Xing/Info frame** (and every format other than MP3): no delay.
//!
//! These rules were measured on an XDJ-XZ export of several thousand
//! tracks, analysed by rekordbox. Rekordbox's detail waveforms were
//! cross-correlated with Sustain's decode of the same files, and the
//! measured delays clustered on the rules within a few samples. The
//! FFmpeg exception is an empirical finding; its cause is not known.
//! FLAC measured no delay. So did AAC in MP4: on 354 such files,
//! rekordbox's waveforms lined up with a FLAC copy of the same recording,
//! so the player drops the encoder's priming as Sustain's decoder does.

use std::fs::File;
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::time::Duration;

/// Samples every MP3 decoder delays its output by (the synthesis
/// filterbank's latency).
const DECODER_DELAY_SAMPLES: u32 = 529;
/// How far past the ID3 tags the first frame is searched for.
const FRAME_SEARCH_BYTES: usize = 64 * 1024;
/// Header frames are far smaller than this.
const HEADER_FRAME_BYTES: usize = 4 * 1024;

/// How far the player's timeline runs behind Sustain's for the audio
/// file at `path`: add it to a time Sustain measured to get the time the
/// player shows for the same sound.
pub fn device_delay(path: &Path) -> io::Result<Duration> {
    let is_mp3 = path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("mp3"));
    if !is_mp3 {
        return Ok(Duration::ZERO);
    }
    mp3_device_delay(&mut BufReader::new(File::open(path)?))
}

/// [`device_delay`] of an MP3 stream. A stream with no recognizable
/// first frame has no delay: there is nothing to tell the two timelines
/// apart.
pub fn mp3_device_delay<R: Read + Seek>(source: &mut R) -> io::Result<Duration> {
    let audio_start = skip_id3v2_tags(source)?;
    source.seek(SeekFrom::Start(audio_start))?;
    let mut head = Vec::with_capacity(FRAME_SEARCH_BYTES + HEADER_FRAME_BYTES);
    source
        .take((FRAME_SEARCH_BYTES + HEADER_FRAME_BYTES) as u64)
        .read_to_end(&mut head)?;
    let Some((offset, frame)) = first_frame(&head) else {
        return Ok(Duration::ZERO);
    };
    let delay_samples = match header_frame_encoder(&head[offset..], frame) {
        None => 0,
        Some(Encoder::Lame { delay }) => DECODER_DELAY_SAMPLES + delay + frame.samples_per_frame(),
        Some(Encoder::Ffmpeg { delay }) => DECODER_DELAY_SAMPLES + delay,
        Some(Encoder::Other) => frame.samples_per_frame(),
    };
    Ok(Duration::from_secs_f64(
        f64::from(delay_samples) / f64::from(frame.sample_rate),
    ))
}

/// Offset of the audio after any leading ID3v2 tags (some files carry
/// several).
fn skip_id3v2_tags<R: Read + Seek>(source: &mut R) -> io::Result<u64> {
    let mut position = 0_u64;
    loop {
        source.seek(SeekFrom::Start(position))?;
        let mut header = [0_u8; 10];
        let read = read_up_to(source, &mut header)?;
        if read < header.len() || &header[..3] != b"ID3" {
            return Ok(position);
        }
        let size = header[6..10]
            .iter()
            .fold(0_u64, |size, &byte| (size << 7) | u64::from(byte & 0x7f));
        let footer = if header[5] & 0x10 != 0 { 10 } else { 0 };
        position += 10 + size + footer;
    }
}

fn read_up_to<R: Read>(source: &mut R, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..])? {
            0 => break,
            read => filled += read,
        }
    }
    Ok(filled)
}

/// The fields of an MPEG audio Layer III frame header that matter here.
#[derive(Clone, Copy, Debug, PartialEq)]
struct FrameHeader {
    /// MPEG-1 (as opposed to MPEG-2 or 2.5).
    mpeg1: bool,
    mono: bool,
    sample_rate: u32,
    /// Frame length in bytes.
    length: usize,
}

impl FrameHeader {
    fn parse(bytes: &[u8]) -> Option<Self> {
        let &[sync, flags, rates, mode, ..] = bytes else {
            return None;
        };
        if sync != 0xff || flags & 0xe0 != 0xe0 {
            return None;
        }
        let version = (flags >> 3) & 0b11;
        let layer = (flags >> 1) & 0b11;
        // Layer III only (bits 01); version 01 is reserved.
        if layer != 0b01 || version == 0b01 {
            return None;
        }
        let mpeg1 = version == 0b11;
        let bitrate_index = usize::from(rates >> 4);
        let rate_index = usize::from((rates >> 2) & 0b11);
        if bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
            return None;
        }
        const MPEG1_KBPS: [u32; 15] = [
            0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320,
        ];
        const MPEG2_KBPS: [u32; 15] =
            [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160];
        let base_rate = [44_100, 48_000, 32_000][rate_index];
        let (kbps, sample_rate) = match version {
            0b11 => (MPEG1_KBPS[bitrate_index], base_rate),
            0b10 => (MPEG2_KBPS[bitrate_index], base_rate / 2),
            _ => (MPEG2_KBPS[bitrate_index], base_rate / 4),
        };
        let padding = u32::from((rates >> 1) & 1);
        let slots = if mpeg1 { 144 } else { 72 };
        Some(Self {
            mpeg1,
            mono: mode >> 6 == 0b11,
            sample_rate,
            length: (slots * kbps * 1_000 / sample_rate + padding) as usize,
        })
    }

    fn samples_per_frame(self) -> u32 {
        if self.mpeg1 { 1_152 } else { 576 }
    }

    /// Bytes from the frame start to the Xing/Info tag: the 4-byte header
    /// and the side information.
    fn side_info_end(self) -> usize {
        4 + match (self.mpeg1, self.mono) {
            (true, true) => 17,
            (true, false) => 32,
            (false, true) => 9,
            (false, false) => 17,
        }
    }
}

/// The first frame in `bytes`: a valid header immediately followed by
/// another valid header, so a stray sync pattern in leftover tag data is
/// not mistaken for audio.
fn first_frame(bytes: &[u8]) -> Option<(usize, FrameHeader)> {
    (0..bytes.len().min(FRAME_SEARCH_BYTES)).find_map(|offset| {
        let frame = FrameHeader::parse(&bytes[offset..])?;
        let next = FrameHeader::parse(bytes.get(offset + frame.length..)?)?;
        (next.sample_rate == frame.sample_rate).then_some((offset, frame))
    })
}

/// Who wrote a header frame, as far as the timeline is concerned.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Encoder {
    /// LAME, with the encoder delay its tag records.
    Lame { delay: u32 },
    /// FFmpeg's encoder, with the encoder delay its LAME-format tag
    /// records.
    Ffmpeg { delay: u32 },
    /// An encoder that writes a Xing/Info frame but no LAME tag.
    Other,
}

/// The encoder of the Xing/Info header frame at the start of `frame_bytes`,
/// or `None` when the frame is audio.
fn header_frame_encoder(frame_bytes: &[u8], frame: FrameHeader) -> Option<Encoder> {
    let tag = frame_bytes.get(frame.side_info_end()..)?;
    if !(tag.starts_with(b"Xing") || tag.starts_with(b"Info")) {
        return None;
    }
    let flags = u32::from_be_bytes(tag.get(4..8)?.try_into().ok()?);
    let optional_fields: [(u32, usize); 4] = [(1, 4), (2, 4), (4, 100), (8, 4)];
    let extension_start = 8 + optional_fields
        .iter()
        .filter(|(flag, _)| flags & flag != 0)
        .map(|(_, len)| len)
        .sum::<usize>();
    let Some(extension) = tag.get(extension_start..extension_start + 24) else {
        return Some(Encoder::Other);
    };
    let delay = (u32::from(extension[21]) << 4) | (u32::from(extension[22]) >> 4);
    Some(match &extension[..4] {
        b"LAME" => Encoder::Lame { delay },
        b"Lavf" | b"Lavc" => Encoder::Ffmpeg { delay },
        _ => Encoder::Other,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;
    use std::time::Duration;

    use super::mp3_device_delay;

    /// An MPEG-1 Layer III, 128 kb/s, 44.1 kHz, joint-stereo header.
    const HEADER_44K: [u8; 4] = [0xff, 0xfb, 0x90, 0x44];
    /// The same at 48 kHz.
    const HEADER_48K: [u8; 4] = [0xff, 0xfb, 0x94, 0x44];

    fn frame_len(header: [u8; 4]) -> usize {
        if header[2] & 0x0c == 0x04 { 384 } else { 417 }
    }

    /// One frame: `header`, then `body` at the Xing tag offset, then zeros.
    fn frame(header: [u8; 4], body: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0_u8; frame_len(header)];
        bytes[..4].copy_from_slice(&header);
        bytes[36..36 + body.len()].copy_from_slice(body);
        bytes
    }

    /// A Xing tag (frame count only) followed by a LAME-format extension
    /// naming `encoder` with `delay` samples of encoder delay.
    fn xing(encoder: Option<&[u8; 9]>, delay: u32) -> Vec<u8> {
        let mut tag = b"Info".to_vec();
        tag.extend_from_slice(&1_u32.to_be_bytes());
        tag.extend_from_slice(&1_000_u32.to_be_bytes());
        if let Some(encoder) = encoder {
            let mut extension = [0_u8; 36];
            extension[..9].copy_from_slice(encoder);
            extension[21] = (delay >> 4) as u8;
            extension[22] = ((delay & 0xf) << 4) as u8;
            tag.extend_from_slice(&extension);
        }
        tag
    }

    fn stream(prefix: &[u8], header: [u8; 4], first_body: &[u8]) -> Vec<u8> {
        let mut bytes = prefix.to_vec();
        bytes.extend(frame(header, first_body));
        for _ in 0..3 {
            bytes.extend(frame(header, &[]));
        }
        bytes
    }

    fn delay_samples(bytes: Vec<u8>, sample_rate: f64) -> f64 {
        let delay = mp3_device_delay(&mut Cursor::new(bytes)).expect("readable");
        (delay.as_secs_f64() * sample_rate).round()
    }

    #[test]
    fn lame_files_start_after_both_delays_and_the_header_frame() {
        let bytes = stream(&[], HEADER_44K, &xing(Some(b"LAME3.100"), 576));
        assert_eq!(delay_samples(bytes, 44_100.0), (529 + 576 + 1152) as f64);
    }

    #[test]
    fn ffmpeg_files_start_after_both_delays_only() {
        let bytes = stream(&[], HEADER_48K, &xing(Some(b"Lavc58.13"), 576));
        assert_eq!(delay_samples(bytes, 48_000.0), (529 + 576) as f64);
    }

    #[test]
    fn other_encoders_start_after_the_header_frame() {
        let bytes = stream(&[], HEADER_44K, &xing(None, 0));
        assert_eq!(delay_samples(bytes, 44_100.0), 1152.0);
        let bytes = stream(&[], HEADER_44K, &xing(Some(b"iTunes v7"), 0));
        assert_eq!(delay_samples(bytes, 44_100.0), 1152.0);
    }

    #[test]
    fn files_without_a_header_frame_have_no_delay() {
        let bytes = stream(&[], HEADER_44K, &[]);
        assert_eq!(
            mp3_device_delay(&mut Cursor::new(bytes)).expect("readable"),
            Duration::ZERO
        );
        assert_eq!(
            mp3_device_delay(&mut Cursor::new(b"fLaC not an mp3".to_vec())).expect("readable"),
            Duration::ZERO
        );
    }

    #[test]
    fn id3_tags_are_skipped_and_stray_syncs_ignored() {
        // Two ID3v2 tags; the second holds bytes that look like a frame
        // sync but are not followed by a second frame.
        let mut prefix = Vec::new();
        for body in [vec![0_u8; 20], vec![0xff, 0xfb, 0x90, 0x44, 0, 0, 0, 0]] {
            prefix.extend_from_slice(b"ID3\x04\x00\x00");
            prefix.extend_from_slice(&[0, 0, 0, body.len() as u8]);
            prefix.extend(body);
        }
        prefix.extend_from_slice(&[0xff, 0xfb, 0x90]);
        let bytes = stream(&prefix, HEADER_44K, &xing(Some(b"LAME3.99r"), 576));
        assert_eq!(delay_samples(bytes, 44_100.0), (529 + 576 + 1152) as f64);
    }
}
