// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Repacking Sustain's neutral waveform tiers into Pioneer's on-drive
//! waveform encodings.
//!
//! Sustain produces two tiers ([`WaveformSegments`]): a fixed
//! 400-segment preview and a 150-segment-per-second detail tier, each
//! segment carrying a peak amplitude plus low/mid/high band energies
//! (all `u8`). Pioneer wants five encodings:
//!
//! - **PWAV** — 400-byte monochrome preview, `whiteness:3 | height:5`.
//! - **PWV2** — 100-byte tiny preview, 4-bit height.
//! - **PWV3** — monochrome detail, one `whiteness:3 | height:5` byte per
//!   entry at 150 entries/second. Drives needle search + jogwheel.
//! - **PWV4** — 1200-entry colour preview, 6 bytes per entry
//!   (`height,colour` for low/mid/high bands), 8-bit heights.
//! - **PWV5** — colour detail, a 2-byte `rgb + height` pack per entry at
//!   150 entries/second.
//!
//! The amplitudes are already normalized to the track peak by the DSP
//! pass, so there is no resampling of audio here — only down/up-sampling
//! of the segment arrays and bit-packing.

use sustain_domain::{WaveformSegment, WaveformSegments};

/// Bytes-per-second of the Pioneer detail waveforms. Matches the
/// resolution Sustain's detail tier is produced at.
pub const DETAIL_ENTRIES_PER_SECOND: u16 = 150;

const PWAV_COLUMNS: usize = 400;
const PWV2_COLUMNS: usize = 100;
const PWV4_COLUMNS: usize = 1200;

/// All five Pioneer waveform encodings for one track, ready to drop
/// into the ANLZ sections.
pub struct PioneerWaveforms {
    pub pwav: Vec<u8>,
    pub pwv2: Vec<u8>,
    pub pwv3: Vec<u8>,
    pub pwv4: Vec<u8>,
    pub pwv5: Vec<u8>,
    /// Entry count of the detail encodings (PWV3 == PWV5).
    pub detail_entries: u32,
}

/// Build all five encodings. The detail tier's own timeline fixes the
/// detail entry count. `duration_ms` is only a fallback when analysis
/// data is absent and a silent waveform must still cover the track.
pub fn build(
    preview: &WaveformSegments,
    detail: &WaveformSegments,
    duration_ms: u32,
) -> PioneerWaveforms {
    let detail_entries = detail_entry_count(detail, duration_ms);

    let preview_400 = resample(&preview.segments, PWAV_COLUMNS);
    let preview_100 = resample(&preview.segments, PWV2_COLUMNS);
    let detail_n = resample(&detail.segments, detail_entries);
    let preview_1200 = resample(&detail.segments, PWV4_COLUMNS);

    PioneerWaveforms {
        pwav: preview_400
            .iter()
            .map(|s| encode_mono(s.amplitude, 5))
            .collect(),
        pwv2: preview_100.iter().map(|s| scale(s.amplitude, 15)).collect(),
        pwv3: detail_n
            .iter()
            .map(|s| encode_mono(s.amplitude, 7))
            .collect(),
        pwv4: encode_pwv4(&preview_1200),
        pwv5: encode_pwv5(&detail_n),
        detail_entries: detail_entries as u32,
    }
}

fn detail_entry_count(detail: &WaveformSegments, fallback_duration_ms: u32) -> usize {
    let duration_ms = if detail.segments.is_empty() {
        f64::from(fallback_duration_ms)
    } else {
        f64::from(detail.segment_duration_ms) * detail.segments.len() as f64
    };
    ((duration_ms / 1_000.0) * f64::from(DETAIL_ENTRIES_PER_SECOND))
        .round()
        .max(1.0) as usize
}

/// Bucket-resample a segment array to exactly `target` entries. Each
/// output bucket takes the peak amplitude and mean band energies of the
/// source segments it spans. An empty source yields silent segments.
fn resample(source: &[WaveformSegment], target: usize) -> Vec<WaveformSegment> {
    if target == 0 {
        return Vec::new();
    }
    if source.is_empty() {
        return vec![WaveformSegment::silent(); target];
    }

    let len = source.len();
    let mut out = Vec::with_capacity(target);
    for i in 0..target {
        let start = i * len / target;
        let end = (((i + 1) * len) / target).max(start + 1).min(len);
        let span = &source[start..end];
        let count = span.len() as u32;

        let mut amplitude = 0u8;
        let mut low = 0u32;
        let mut mid = 0u32;
        let mut high = 0u32;
        for seg in span {
            amplitude = amplitude.max(seg.amplitude);
            low += seg.low_band as u32;
            mid += seg.mid_band as u32;
            high += seg.high_band as u32;
        }
        out.push(WaveformSegment {
            amplitude,
            low_band: (low / count) as u8,
            mid_band: (mid / count) as u8,
            high_band: (high / count) as u8,
        });
    }
    out
}

/// Linearly scale a 0–255 value into 0–`max`.
fn scale(value: u8, max: u8) -> u8 {
    ((value as u16 * max as u16) / 255) as u8
}

/// Pack a monochrome entry: `whiteness:3 | height:5` (height 0–31).
fn encode_mono(amplitude: u8, whiteness: u8) -> u8 {
    let height = scale(amplitude, 31);
    ((whiteness & 0x07) << 5) | (height & 0x1F)
}

/// PWV4: 6 bytes per entry — `[low_h, low_c, mid_h, mid_c, high_h,
/// high_c]`. Heights are 8-bit (0–127); colours stay within the ranges
/// rekordbox uses (low bright `0xE0–0xFF`, mid `0x01–0x30`, high
/// `0x01–0x20`) and are driven by the band energies.
fn encode_pwv4(segments: &[WaveformSegment]) -> Vec<u8> {
    let mut out = Vec::with_capacity(segments.len() * 6);
    for seg in segments {
        out.push(scale(seg.low_band, 127));
        out.push(0xE0 + (seg.low_band >> 3)); // 0xE0..=0xFF
        out.push(scale(seg.mid_band, 127));
        out.push(0x01 + (seg.mid_band >> 3)); // 0x01..=0x20
        out.push(scale(seg.high_band, 127));
        out.push(0x01 + (seg.high_band >> 4)); // 0x01..=0x10
    }
    out
}

/// PWV5: 2 bytes per entry packing a 3-bit RGB colour and a 5-bit
/// height (per `rekordbox_anlz.ksy`). Colour comes from the relative
/// band balance, so low-heavy content reads warm and high-heavy content
/// reads cool.
fn encode_pwv5(segments: &[WaveformSegment]) -> Vec<u8> {
    let mut out = Vec::with_capacity(segments.len() * 2);
    for seg in segments {
        let height = scale(seg.amplitude, 31);
        let (red, green, blue) = band_colour(seg);
        // byte0: blue[0:2] in bits 7-5, height in bits 4-0.
        // byte1: red in 7-5, green in 4-2, blue[3:4] in 1-0.
        let byte0 = ((blue & 0x07) << 5) | (height & 0x1F);
        let byte1 = ((red & 0x07) << 5) | ((green & 0x07) << 2) | ((blue >> 3) & 0x03);
        out.push(byte0);
        out.push(byte1);
    }
    out
}

/// Map a segment's band energies to a 3-bit-per-channel RGB colour.
/// Red tracks the low band, green the mid, blue the high.
fn band_colour(seg: &WaveformSegment) -> (u8, u8, u8) {
    (
        scale(seg.low_band, 7),
        scale(seg.mid_band, 7),
        scale(seg.high_band, 7),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(amp: u8) -> WaveformSegment {
        WaveformSegment {
            amplitude: amp,
            low_band: amp,
            mid_band: amp / 2,
            high_band: amp / 4,
        }
    }

    fn tier(count: usize, amp: u8) -> WaveformSegments {
        WaveformSegments {
            segment_duration_ms: 1_000.0 / f32::from(DETAIL_ENTRIES_PER_SECOND),
            segments: vec![seg(amp); count],
        }
    }

    #[test]
    fn encodings_have_expected_lengths() {
        let preview = tier(400, 200);
        let detail = tier(900, 200); // 6 s worth at 150/s
        let wf = build(&preview, &detail, 6000);
        assert_eq!(wf.pwav.len(), 400);
        assert_eq!(wf.pwv2.len(), 100);
        assert_eq!(wf.detail_entries, 900);
        assert_eq!(wf.pwv3.len(), 900);
        assert_eq!(wf.pwv5.len(), 1800);
        assert_eq!(wf.pwv4.len(), 1200 * 6);
    }

    #[test]
    fn empty_waveforms_resample_to_silence() {
        let empty = WaveformSegments {
            segment_duration_ms: 0.0,
            segments: Vec::new(),
        };
        let wf = build(&empty, &empty, 2000);
        assert_eq!(wf.pwav.len(), 400);
        assert!(wf.pwav.iter().all(|&b| b == encode_mono(0, 5)));
        assert_eq!(wf.pwv3.len(), 300); // 2 s * 150
    }

    #[test]
    fn analyzed_detail_timeline_wins_over_metadata_duration() {
        let preview = tier(400, 200);
        let detail = WaveformSegments {
            segment_duration_ms: 1_000.0 / f32::from(DETAIL_ENTRIES_PER_SECOND),
            segments: vec![seg(200); 900],
        };
        let wf = build(&preview, &detail, 8000);
        assert_eq!(wf.detail_entries, 900);
        assert_eq!(wf.pwv3.len(), 900);
    }

    #[test]
    fn mono_encoding_packs_height_and_whiteness() {
        assert_eq!(encode_mono(255, 7), 0xFF);
        assert_eq!(encode_mono(0, 5), 0xA0);
    }

    #[test]
    fn resample_preserves_peak_amplitude() {
        let mut segs = vec![seg(10); 8];
        segs[3].amplitude = 250;
        let out = resample(&segs, 2);
        // The loud segment must survive into its bucket.
        assert!(out.iter().any(|s| s.amplitude == 250));
    }
}
