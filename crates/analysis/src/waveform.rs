// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Waveform segmenter. Produces two tiers from one decode pass:
//!   * **Detail** — `DETAIL_SEGMENTS_PER_SECOND` segments per second
//!     of audio. Time-resolution; suitable for the active-track
//!     scrubber and for re-encoding into hardware-specific formats
//!     downstream (the Pioneer export's PWV3/PWV5 waveforms).
//!   * **Preview** — `PREVIEW_SEGMENT_COUNT` segments, fixed
//!     regardless of track length. Suitable for a thumbnail / pre-roll
//!     waveform; derived by bucketing detail segments so we do not
//!     stream the audio twice.
//!
//! Each segment is `WaveformSegment` (4 bytes): peak amplitude plus
//! RMS energy in three frequency bands. Amplitudes are normalized to
//! the track's loudest segment, so the loudest part of any track sits
//! at 255 and quieter material scales below it — visually consistent
//! across tracks regardless of their absolute peak level. Each band energy
//! is normalized the same way to its own loudest segment, so a faint band
//! keeps the resolution its shape needs; the tier's `BandScale` records
//! how the band peaks compare, the balance a segment's colour is read
//! from.

use sustain_domain::{
    BandScale, DETAIL_SEGMENTS_PER_SECOND, PREVIEW_SEGMENT_COUNT, WaveformSegment, WaveformSegments,
};

use crate::bands::ThreeBandSplitter;

/// Both waveform tiers (preview + detail) produced from one decode
/// pass. Public so that [`crate::Analyzer::waveform`] can hand the
/// renderer / persistence layer the pair without exposing the
/// intermediate segmenter accumulators.
pub struct WaveformTiers {
    pub preview: WaveformSegments,
    pub detail: WaveformSegments,
}

/// Pure builder: input is one mono stream + sample rate, output is
/// both tiers. The full-decode entry point used to live in this
/// module; the Analyzer now owns decoding via its capped/full caches
/// and calls `build_tiers` directly, so the I/O wrapper is gone.
pub(crate) fn build_tiers(samples: &[f32], sample_rate: u32) -> WaveformTiers {
    if samples.is_empty() || sample_rate == 0 {
        return empty_tiers(sample_rate);
    }

    let detail = build_detail(samples, sample_rate);
    let preview = build_preview(&detail);

    WaveformTiers { preview, detail }
}

fn empty_tiers(sample_rate: u32) -> WaveformTiers {
    let rate = sample_rate.max(1) as f32;
    WaveformTiers {
        preview: WaveformSegments::empty(),
        detail: WaveformSegments {
            segment_duration_ms: 1_000.0 / rate,
            ..WaveformSegments::empty()
        },
    }
}

fn build_detail(samples: &[f32], sample_rate: u32) -> WaveformSegments {
    // Pioneer detail waveforms use one entry per half-frame: exactly
    // 150 entries per second. Rational boundaries preserve that
    // timeline for sample rates that are not divisible by 150.
    let detail_rate = DETAIL_SEGMENTS_PER_SECOND as usize;
    let sample_rate_usize = sample_rate as usize;
    let scaled_sample_count = samples
        .len()
        .checked_mul(detail_rate)
        .expect("decoded audio is too large to segment");
    let segment_count = scaled_sample_count.div_ceil(sample_rate_usize);

    // Pre-compute the peak across every segment so we can normalize in
    // a single pass below. The IIR splitter is single-pass and
    // stateful, so we collect raw accumulators first and only
    // quantize to u8 at the end.
    let mut accum: Vec<RawSegment> = Vec::with_capacity(segment_count);

    let mut splitter = ThreeBandSplitter::new(sample_rate);

    for segment_index in 0..segment_count {
        let start = segment_index * sample_rate_usize / detail_rate;
        let end = ((segment_index + 1) * sample_rate_usize / detail_rate).min(samples.len());
        let chunk = &samples[start..end];

        let mut peak = 0.0_f32;
        let mut low_sq = 0.0_f64;
        let mut mid_sq = 0.0_f64;
        let mut high_sq = 0.0_f64;

        for &sample in chunk {
            let abs = sample.abs();
            if abs > peak {
                peak = abs;
            }
            let (l, m, h) = splitter.process(sample);
            low_sq += l * l;
            mid_sq += m * m;
            high_sq += h * h;
        }

        let n = chunk.len().max(1) as f64;
        accum.push(RawSegment {
            peak,
            low_rms: (low_sq / n).sqrt(),
            mid_rms: (mid_sq / n).sqrt(),
            high_rms: (high_sq / n).sqrt(),
        });
    }

    let track_peak = accum
        .iter()
        .map(|seg| seg.peak)
        .fold(0.0_f32, f32::max)
        .max(f32::EPSILON);
    // Each band is scaled to its own loudest segment. A band without
    // energy takes the least positive peak, so it reads zero, and the
    // scale relates the peaks the bytes were scaled to.
    let band_peak = |band: fn(&RawSegment) -> f64| {
        accum
            .iter()
            .map(band)
            .fold(0.0_f64, f64::max)
            .max(f64::MIN_POSITIVE)
    };
    let low_peak = band_peak(|seg| seg.low_rms);
    let mid_peak = band_peak(|seg| seg.mid_rms);
    let high_peak = band_peak(|seg| seg.high_rms);

    let segments = accum
        .into_iter()
        .map(|seg| WaveformSegment {
            amplitude: quantize(seg.peak / track_peak),
            low_band: quantize((seg.low_rms / low_peak) as f32),
            mid_band: quantize((seg.mid_rms / mid_peak) as f32),
            high_band: quantize((seg.high_rms / high_peak) as f32),
        })
        .collect();

    WaveformSegments {
        segment_duration_ms: 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32,
        segments,
        band_scale: band_scale(low_peak, mid_peak, high_peak),
    }
}

/// How the bands' loudest segments compare: each band peak relative to
/// the strongest.
fn band_scale(low_peak: f64, mid_peak: f64, high_peak: f64) -> BandScale {
    let strongest = low_peak.max(mid_peak).max(high_peak);
    BandScale {
        low: (low_peak / strongest) as f32,
        mid: (mid_peak / strongest) as f32,
        high: (high_peak / strongest) as f32,
    }
}

fn build_preview(detail: &WaveformSegments) -> WaveformSegments {
    if detail.segments.is_empty() {
        return WaveformSegments::empty();
    }

    // If the detail tier is already shorter than the preview budget
    // (very short tracks), just copy. We do not pad with silence —
    // the renderer treats segment count as the actual track length.
    if detail.segments.len() <= PREVIEW_SEGMENT_COUNT {
        return WaveformSegments {
            segment_duration_ms: detail.segment_duration_ms,
            segments: detail.segments.clone(),
            band_scale: detail.band_scale,
        };
    }

    let detail_len = detail.segments.len();
    // Bucket boundaries in detail-segment indices. Index `i` covers
    // detail segments `[i * detail_len / PREVIEW .. (i+1) * detail_len / PREVIEW)`.
    // This is the standard "bucket k items into n groups" split and
    // gives buckets that differ in size by at most one.
    let segments = (0..PREVIEW_SEGMENT_COUNT)
        .map(|i| {
            let start = i * detail_len / PREVIEW_SEGMENT_COUNT;
            let end = ((i + 1) * detail_len / PREVIEW_SEGMENT_COUNT).max(start + 1);
            let slice = &detail.segments[start..end];
            bucket_segments(slice)
        })
        .collect();

    WaveformSegments {
        segment_duration_ms: detail.segment_duration_ms * detail_len as f32
            / PREVIEW_SEGMENT_COUNT as f32,
        segments,
        band_scale: detail.band_scale,
    }
}

/// Reduce a slice of detail segments into one preview segment: peak
/// amplitude is the max across the bucket (loudest moment dominates
/// the visual), band energies are averaged (RMS-of-RMS would require
/// the underlying squared values which we no longer have at this
/// stage; mean is a fine perceptual proxy).
fn bucket_segments(slice: &[WaveformSegment]) -> WaveformSegment {
    if slice.is_empty() {
        return WaveformSegment::silent();
    }
    let mut max_amp = 0_u8;
    let mut sum_low = 0_u32;
    let mut sum_mid = 0_u32;
    let mut sum_high = 0_u32;
    for seg in slice {
        if seg.amplitude > max_amp {
            max_amp = seg.amplitude;
        }
        sum_low += seg.low_band as u32;
        sum_mid += seg.mid_band as u32;
        sum_high += seg.high_band as u32;
    }
    let n = slice.len() as u32;
    WaveformSegment {
        amplitude: max_amp,
        low_band: (sum_low / n) as u8,
        mid_band: (sum_mid / n) as u8,
        high_band: (sum_high / n) as u8,
    }
}

#[inline]
fn quantize(normalized: f32) -> u8 {
    let scaled = (normalized.clamp(0.0, 1.0) * 255.0).round();
    scaled as u8
}

/// Raw per-segment accumulator carried between the chunk loop and the
/// normalization pass. Held in `f32`/`f64` so we do not lose precision
/// before knowing the track-wide peak.
struct RawSegment {
    peak: f32,
    low_rms: f64,
    mid_rms: f64,
    high_rms: f64,
}

#[cfg(test)]
mod tests {
    use super::{DETAIL_SEGMENTS_PER_SECOND, PREVIEW_SEGMENT_COUNT, WaveformSegment, build_tiers};
    use std::f32::consts::TAU;

    const SAMPLE_RATE: u32 = 44_100;

    fn sine(freq: f32, duration_secs: f32, amplitude: f32) -> Vec<f32> {
        let count = (duration_secs * SAMPLE_RATE as f32) as usize;
        (0..count)
            .map(|i| amplitude * (TAU * freq * i as f32 / SAMPLE_RATE as f32).sin())
            .collect()
    }

    #[test]
    fn empty_input_produces_empty_segments() {
        let tiers = build_tiers(&[], SAMPLE_RATE);
        assert!(tiers.preview.segments.is_empty());
        assert!(tiers.detail.segments.is_empty());
    }

    #[test]
    fn detail_segment_count_matches_track_length() {
        let samples = sine(440.0, 2.0, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        let expected = samples
            .len()
            .checked_mul(DETAIL_SEGMENTS_PER_SECOND as usize)
            .expect("test samples fit")
            .div_ceil(SAMPLE_RATE as usize);
        assert_eq!(tiers.detail.segments.len(), expected);
        assert!(
            (tiers.detail.segment_duration_ms - 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32).abs()
                < 1e-3
        );
    }

    #[test]
    fn detail_timeline_is_exact_for_non_divisible_sample_rates() {
        let sample_rate = 32_000;
        let samples = vec![0.0; sample_rate as usize * 2];
        let tiers = build_tiers(&samples, sample_rate);
        assert_eq!(tiers.detail.segments.len(), 300);
        assert!(
            (tiers.detail.segment_duration_ms - 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32).abs()
                < 1e-3
        );
    }

    #[test]
    fn long_track_preview_is_exactly_fixed_width() {
        // 30 s of audio yields ~4500 detail segments — well over the
        // PREVIEW_SEGMENT_COUNT budget — so the preview tier must
        // bucket down to exactly that many segments.
        let samples = sine(440.0, 30.0, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        assert_eq!(tiers.preview.segments.len(), PREVIEW_SEGMENT_COUNT);
    }

    #[test]
    fn short_track_preview_falls_back_to_detail_segments() {
        // 0.5 s of audio yields ~75 detail segments — below the
        // PREVIEW_SEGMENT_COUNT budget — so the preview tier should
        // mirror the detail tier instead of fabricating padding.
        let samples = sine(440.0, 0.5, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        assert_eq!(tiers.preview.segments.len(), tiers.detail.segments.len());
        assert!(tiers.preview.segments.len() < PREVIEW_SEGMENT_COUNT);
    }

    #[test]
    fn loudest_segment_normalizes_to_full_scale() {
        // Steady sine at 0.5 amplitude — every detail segment has the
        // same peak — so the normalized amplitude for every segment
        // should saturate to 255 (each segment IS the track peak).
        let samples = sine(440.0, 2.0, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        let any_at_full = tiers.detail.segments.iter().any(|seg| seg.amplitude >= 250);
        assert!(any_at_full, "expected at least one segment near 255");
    }

    #[test]
    fn low_tone_dominates_low_band_after_normalization() {
        // A bass tone's low band outweighs its high band once the bands
        // are compared on their shared scale. (The segment is chosen
        // after the IIR filters have reached steady state.)
        let samples = sine(80.0, 2.0, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        // Skip the first 10% of segments — IIR transient.
        let start = tiers.detail.segments.len() / 10;
        let [low, _, high] = tiers
            .detail
            .band_scale
            .balanced(&tiers.detail.segments[start + 50]);
        assert!(
            low > high,
            "low band {low} should dominate high band {high} on 80 Hz tone"
        );
    }

    #[test]
    fn a_tone_lights_its_own_band_alone() {
        // A 1 kHz tone is mid-band: its low and high bands carry only
        // faint leakage, however their own bytes are scaled.
        let samples = sine(1_000.0, 2.0, 0.5);
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        let [low, mid, high] = tiers
            .detail
            .band_scale
            .balanced(&tiers.detail.segments[tiers.detail.segments.len() / 2]);

        assert!(mid >= 0.98, "mid band {mid}");
        assert!(low < 0.25, "low band {low}");
        assert!(high < 0.25, "high band {high}");
        assert_eq!(tiers.preview.band_scale, tiers.detail.band_scale);
    }

    #[test]
    fn a_faint_band_keeps_the_shape_of_its_energy() {
        // A loud bass under a faint high tone that swells and fades: the
        // high band's own bytes still trace the swell, which recording
        // identity compares, though on the shared scale it barely shows.
        let samples: Vec<f32> = (0..SAMPLE_RATE as usize * 4)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                let swell = 0.5 - 0.5 * (TAU * 0.5 * t).cos();
                0.8 * (TAU * 150.0 * t).sin() + 0.0008 * swell * (TAU * 6_000.0 * t).sin()
            })
            .collect();
        let tiers = build_tiers(&samples, SAMPLE_RATE);
        let settled = &tiers.detail.segments[30..];
        let highest = settled.iter().map(|seg| seg.high_band).max().unwrap_or(0);
        let lowest = settled.iter().map(|seg| seg.high_band).min().unwrap_or(0);

        assert!(
            highest - lowest >= 20,
            "high band spans {lowest}..{highest}"
        );
        assert!(tiers.detail.band_scale.high < 0.01);
    }

    #[test]
    fn a_whisper_keeps_the_balance_of_its_bands() {
        // The same mix at a level far below one float epsilon: its bands
        // compare as they do at full level.
        let mix = |gain: f32| -> Vec<f32> {
            sine(80.0, 2.0, 0.5)
                .iter()
                .zip(sine(1_000.0, 2.0, 0.25))
                .map(|(low, mid)| gain * (low + mid))
                .collect()
        };
        let balance = |samples: &[f32]| {
            let tiers = build_tiers(samples, SAMPLE_RATE);
            tiers
                .detail
                .band_scale
                .balanced(&tiers.detail.segments[tiers.detail.segments.len() / 2])
        };
        let loud = balance(&mix(1.0));
        let whisper = balance(&mix(1e-12));

        for (loud, whisper) in loud.iter().zip(whisper) {
            assert!(
                (loud - whisper).abs() < 0.01,
                "{loud:?} against {whisper:?}"
            );
        }
    }

    #[test]
    fn samples_far_beyond_full_scale_still_scale_the_bands() {
        let mut samples = sine(1_000.0, 1.0, 0.5);
        samples.extend([f32::MAX, f32::MAX]);
        let scale = build_tiers(&samples, SAMPLE_RATE).detail.band_scale;

        for band in [scale.low, scale.mid, scale.high] {
            assert!((0.0..=1.0).contains(&band), "{scale:?}");
        }
    }

    #[test]
    fn silent_segment_helper_is_all_zero() {
        let s = WaveformSegment::silent();
        assert_eq!(s.amplitude, 0);
        assert_eq!(s.low_band, 0);
        assert_eq!(s.mid_band, 0);
        assert_eq!(s.high_band, 0);
    }
}
