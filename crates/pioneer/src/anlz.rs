// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! ANLZ analysis-file writer (`ANLZ0000.DAT` / `ANLZ0000.EXT`).
//!
//! ANLZ files are a sequence of tagged big-endian sections behind a
//! `PMAI` header. The `.DAT` carries the preview waveforms, beatgrid
//! and a placeholder VBR index; the `.EXT` carries the detail
//! waveforms and (empty) extended cue lists. The `.EXT` is the file the
//! XDJ-XZ actually requires for needle/jogwheel display.
//!
//! All section layouts and the constant header values are reproduced
//! from the hardware-validated reference exporter; only the waveform
//! payloads come from Sustain's own analysis.

use crate::model::AnlzInput;
use crate::waveform::{self, DETAIL_ENTRIES_PER_SECOND};

const PMAI_HEADER_SIZE: u32 = 28;
const PPTH_HEADER_SIZE: u32 = 16;
const PVBR_TOTAL_SIZE: u32 = 1620;
const PVBR_HEADER_SIZE: u32 = 16;
const PQTZ_HEADER_SIZE: u32 = 24;
const PQTZ_BEAT_ENTRY_SIZE: u32 = 8;
const PWAV_HEADER_SIZE: u32 = 20;
const PWV2_HEADER_SIZE: u32 = 20;
const PWV3_HEADER_SIZE: u32 = 24;
const PWV4_HEADER_SIZE: u32 = 24;
const PWV5_HEADER_SIZE: u32 = 24;
const PCOB_HEADER_SIZE: u32 = 24;
const PCO2_SIZE: u32 = 20;
const PWV4_ENTRY_COUNT: u32 = 1200;
const PWV4_ENTRY_SIZE: u32 = 6;

/// Encode an audio path as UTF-16 big-endian with a trailing NUL, the
/// form Pioneer stores in the `PPTH` section.
fn encode_path_utf16_be(path: &str) -> Vec<u8> {
    let mut out = Vec::new();
    for unit in path.encode_utf16() {
        out.extend_from_slice(&unit.to_be_bytes());
    }
    out.extend_from_slice(&0u16.to_be_bytes());
    out
}

fn pmai(buf: &mut Vec<u8>, total_len: u32) {
    buf.extend_from_slice(b"PMAI");
    buf.extend_from_slice(&PMAI_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&total_len.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    buf.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
}

fn ppth(buf: &mut Vec<u8>, path_utf16: &[u8]) {
    buf.extend_from_slice(b"PPTH");
    buf.extend_from_slice(&PPTH_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&(PPTH_HEADER_SIZE + path_utf16.len() as u32).to_be_bytes());
    buf.extend_from_slice(&(path_utf16.len() as u32).to_be_bytes());
    buf.extend_from_slice(path_utf16);
}

/// A `PCOB` cue-list section. `is_hot` distinguishes the hot-cue list
/// (entry_count 1) from the memory-cue list (entry_count 0); both are
/// otherwise empty.
fn pcob(buf: &mut Vec<u8>, is_hot: bool) {
    buf.extend_from_slice(b"PCOB");
    buf.extend_from_slice(&PCOB_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&PCOB_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&(if is_hot { 1u32 } else { 0u32 }).to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0xFFFF_FFFFu32.to_be_bytes());
}

fn pco2(buf: &mut Vec<u8>, is_hot: bool) {
    buf.extend_from_slice(b"PCO2");
    buf.extend_from_slice(&PCO2_SIZE.to_be_bytes());
    buf.extend_from_slice(&PCO2_SIZE.to_be_bytes());
    buf.extend_from_slice(&(if is_hot { 1u32 } else { 0u32 }).to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
}

/// One beatgrid entry: position-in-bar (1–4), tempo (BPM×100), and the
/// beat's time in milliseconds.
struct Beat {
    number: u16,
    tempo: u16,
    time_ms: u32,
}

/// Lay a constant-tempo grid across the track. Returns no beats for
/// very short clips (< 4 s), matching rekordbox, which leaves
/// sound-effect-length files ungridded.
fn beats(bpm: Option<f32>, duration_ms: u32) -> Vec<Beat> {
    let Some(bpm) = bpm.filter(|b| *b > 0.0) else {
        return Vec::new();
    };
    let Some(tempo) = crate::tempo_centibpm(bpm) else {
        return Vec::new();
    };
    if duration_ms < 4000 {
        return Vec::new();
    }
    let interval = 60_000.0 / bpm;
    let mut out = Vec::new();
    let mut time = 0.0f32;
    let mut number = 1u16;
    while (time as u32) < duration_ms {
        out.push(Beat {
            number,
            tempo,
            time_ms: time as u32,
        });
        time += interval;
        number = if number >= 4 { 1 } else { number + 1 };
    }
    out
}

/// Serialize the `.DAT` file bytes for a track.
pub fn dat_bytes(input: &AnlzInput) -> Vec<u8> {
    let wf = waveform::build(
        input.waveform_preview,
        input.waveform_detail,
        input.duration_ms,
    );
    let path_utf16 = encode_path_utf16_be(input.device_audio_path);
    let grid = beats(input.bpm, input.duration_ms);

    let ppth_len = PPTH_HEADER_SIZE + path_utf16.len() as u32;
    let pqtz_len = PQTZ_HEADER_SIZE + grid.len() as u32 * PQTZ_BEAT_ENTRY_SIZE;
    let pwav_len = PWAV_HEADER_SIZE + wf.pwav.len() as u32;
    let pwv2_len = PWV2_HEADER_SIZE + wf.pwv2.len() as u32;
    let total = PMAI_HEADER_SIZE
        + ppth_len
        + PVBR_TOTAL_SIZE
        + pqtz_len
        + pwav_len
        + pwv2_len
        + PCOB_HEADER_SIZE * 2;

    let mut buf = Vec::with_capacity(total as usize);
    pmai(&mut buf, total);
    ppth(&mut buf, &path_utf16);

    // PVBR — placeholder VBR seek index (all zeros after the header).
    buf.extend_from_slice(b"PVBR");
    buf.extend_from_slice(&PVBR_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&PVBR_TOTAL_SIZE.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend(std::iter::repeat_n(
        0u8,
        (PVBR_TOTAL_SIZE - PVBR_HEADER_SIZE) as usize,
    ));

    // PQTZ — beatgrid.
    buf.extend_from_slice(b"PQTZ");
    buf.extend_from_slice(&PQTZ_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pqtz_len.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    buf.extend_from_slice(&0x0008_0000u32.to_be_bytes());
    buf.extend_from_slice(&(grid.len() as u32).to_be_bytes());
    for beat in &grid {
        buf.extend_from_slice(&beat.number.to_be_bytes());
        buf.extend_from_slice(&beat.tempo.to_be_bytes());
        buf.extend_from_slice(&beat.time_ms.to_be_bytes());
    }

    // PWAV — monochrome preview (400 bytes).
    buf.extend_from_slice(b"PWAV");
    buf.extend_from_slice(&PWAV_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pwav_len.to_be_bytes());
    buf.extend_from_slice(&(wf.pwav.len() as u32).to_be_bytes());
    buf.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    buf.extend_from_slice(&wf.pwav);

    // PWV2 — tiny preview (100 bytes).
    buf.extend_from_slice(b"PWV2");
    buf.extend_from_slice(&PWV2_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pwv2_len.to_be_bytes());
    buf.extend_from_slice(&(wf.pwv2.len() as u32).to_be_bytes());
    buf.extend_from_slice(&0x0001_0000u32.to_be_bytes());
    buf.extend_from_slice(&wf.pwv2);

    pcob(&mut buf, true);
    pcob(&mut buf, false);

    debug_assert_eq!(buf.len() as u32, total);
    buf
}

/// Serialize the `.EXT` file bytes for a track.
pub fn ext_bytes(input: &AnlzInput) -> Vec<u8> {
    let wf = waveform::build(
        input.waveform_preview,
        input.waveform_detail,
        input.duration_ms,
    );
    let path_utf16 = encode_path_utf16_be(input.device_audio_path);

    let ppth_len = PPTH_HEADER_SIZE + path_utf16.len() as u32;
    let pwv3_len = PWV3_HEADER_SIZE + wf.pwv3.len() as u32;
    let pwv5_len = PWV5_HEADER_SIZE + wf.pwv5.len() as u32;
    let pwv4_len = PWV4_HEADER_SIZE + PWV4_ENTRY_COUNT * PWV4_ENTRY_SIZE;
    let total = PMAI_HEADER_SIZE
        + ppth_len
        + pwv3_len
        + PCOB_HEADER_SIZE * 2
        + PCO2_SIZE * 2
        + pwv5_len
        + pwv4_len;

    let mut buf = Vec::with_capacity(total as usize);
    pmai(&mut buf, total);
    ppth(&mut buf, &path_utf16);

    // PWV3 — monochrome detail.
    buf.extend_from_slice(b"PWV3");
    buf.extend_from_slice(&PWV3_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pwv3_len.to_be_bytes());
    buf.extend_from_slice(&1u32.to_be_bytes());
    buf.extend_from_slice(&wf.detail_entries.to_be_bytes());
    buf.extend_from_slice(&DETAIL_ENTRIES_PER_SECOND.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes());
    buf.extend_from_slice(&wf.pwv3);

    pcob(&mut buf, true);
    pcob(&mut buf, false);
    pco2(&mut buf, true);
    pco2(&mut buf, false);

    // PWV5 — colour detail (2 bytes per entry).
    buf.extend_from_slice(b"PWV5");
    buf.extend_from_slice(&PWV5_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pwv5_len.to_be_bytes());
    buf.extend_from_slice(&2u32.to_be_bytes());
    buf.extend_from_slice(&wf.detail_entries.to_be_bytes());
    buf.extend_from_slice(&DETAIL_ENTRIES_PER_SECOND.to_be_bytes());
    buf.extend_from_slice(&0x0305u16.to_be_bytes());
    buf.extend_from_slice(&wf.pwv5);

    // PWV4 — colour preview (1200 × 6 bytes).
    buf.extend_from_slice(b"PWV4");
    buf.extend_from_slice(&PWV4_HEADER_SIZE.to_be_bytes());
    buf.extend_from_slice(&pwv4_len.to_be_bytes());
    buf.extend_from_slice(&PWV4_ENTRY_SIZE.to_be_bytes());
    buf.extend_from_slice(&PWV4_ENTRY_COUNT.to_be_bytes());
    buf.extend_from_slice(&0u32.to_be_bytes());
    // The detail-derived PWV4 always has 1200 entries; guard anyway.
    let expected = (PWV4_ENTRY_COUNT * PWV4_ENTRY_SIZE) as usize;
    if wf.pwv4.len() == expected {
        buf.extend_from_slice(&wf.pwv4);
    } else {
        buf.extend(std::iter::repeat_n(0u8, expected));
    }

    debug_assert_eq!(buf.len() as u32, total);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use sustain_domain::{WaveformSegment, WaveformSegments};

    fn tier(count: usize) -> WaveformSegments {
        WaveformSegments {
            segment_duration_ms: 10.0,
            segments: vec![
                WaveformSegment {
                    amplitude: 128,
                    low_band: 200,
                    mid_band: 100,
                    high_band: 50
                };
                count
            ],
        }
    }

    fn input<'a>(preview: &'a WaveformSegments, detail: &'a WaveformSegments) -> AnlzInput<'a> {
        AnlzInput {
            device_audio_path: "/Contents/Artist/Album/01 Title.mp3",
            bpm: Some(128.0),
            duration_ms: 180_000,
            waveform_preview: preview,
            waveform_detail: detail,
        }
    }

    #[test]
    fn dat_starts_with_pmai_and_self_reports_length() {
        let preview = tier(400);
        let detail = tier(27_000);
        let dat = dat_bytes(&input(&preview, &detail));
        assert_eq!(&dat[0..4], b"PMAI");
        let reported = u32::from_be_bytes([dat[8], dat[9], dat[10], dat[11]]);
        assert_eq!(reported as usize, dat.len());
    }

    #[test]
    fn ext_self_reports_length_and_has_detail_sections() {
        let preview = tier(400);
        let detail = tier(27_000);
        let ext = ext_bytes(&input(&preview, &detail));
        assert_eq!(&ext[0..4], b"PMAI");
        let reported = u32::from_be_bytes([ext[8], ext[9], ext[10], ext[11]]);
        assert_eq!(reported as usize, ext.len());
        // PWV3 and PWV4 magic must both appear.
        assert!(ext.windows(4).any(|w| w == b"PWV3"));
        assert!(ext.windows(4).any(|w| w == b"PWV4"));
    }

    #[test]
    fn short_tracks_have_no_beatgrid() {
        let preview = tier(400);
        let detail = tier(300);
        let mut inp = input(&preview, &detail);
        inp.duration_ms = 2000;
        let dat = dat_bytes(&inp);
        // PQTZ num_beats field is the 3rd u32 of the PQTZ body; find it.
        let pos = dat
            .windows(4)
            .position(|w| w == b"PQTZ")
            .expect("PQTZ present");
        let num_beats =
            u32::from_be_bytes([dat[pos + 20], dat[pos + 21], dat[pos + 22], dat[pos + 23]]);
        assert_eq!(num_beats, 0);
    }
}
