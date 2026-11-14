// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;
use crate::WaveformSegment;

const RATE: usize = DETAIL_SEGMENTS_PER_SECOND as usize;

/// Deterministic pseudo-random source, so every test sees the same audio.
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0 >> 33
    }

    fn below(&mut self, bound: u64) -> u64 {
        self.next() % bound
    }
}

/// `seconds` of music-like detail segments: notes of random length and
/// timbre over a steady beat, so the envelope has shape at every scale.
fn music(seed: u64, seconds: f64) -> Vec<WaveformSegment> {
    let mut noise = Noise(seed);
    let total = (seconds * RATE as f64) as usize;
    let mut segments = Vec::with_capacity(total);
    while segments.len() < total {
        let length = 15 + noise.below(75) as usize;
        let level = 60 + noise.below(160) as u32;
        let bands = [
            noise.below(255) as u32,
            noise.below(255) as u32,
            noise.below(255) as u32,
        ];
        for step in 0..length {
            let beat = if (segments.len() / 75) % 2 == 0 {
                40
            } else {
                0
            };
            let decay = (length - step) as u32 * 100 / length as u32;
            let amplitude = (level * (50 + decay) / 150 + beat).min(255);
            segments.push(segment(
                amplitude,
                bands[0] * amplitude / 255,
                bands[1] * amplitude / 255,
                bands[2] * amplitude / 255,
            ));
        }
    }
    segments.truncate(total);
    segments
}

fn segment(amplitude: u32, low: u32, mid: u32, high: u32) -> WaveformSegment {
    let byte = |value: u32| value.min(255) as u8;
    WaveformSegment {
        amplitude: byte(amplitude),
        low_band: byte(low),
        mid_band: byte(mid),
        high_band: byte(high),
    }
}

fn silence(seconds: f64) -> Vec<WaveformSegment> {
    vec![WaveformSegment::silent(); (seconds * RATE as f64) as usize]
}

fn profile(segments: Vec<WaveformSegment>) -> RecordingProfile {
    RecordingProfile::from_detail(&WaveformSegments {
        segment_duration_ms: 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32,
        segments,
    })
    .expect("long enough to judge")
}

fn identity(left: Vec<WaveformSegment>, right: Vec<WaveformSegment>) -> RecordingIdentity {
    let (left, right) = (profile(left), profile(right));
    let forward = compare_recordings(&left, &right);
    assert_eq!(
        forward,
        compare_recordings(&right, &left),
        "the verdict does not depend on argument order"
    );
    forward
}

fn concat(parts: &[&[WaveformSegment]]) -> Vec<WaveformSegment> {
    parts.iter().flat_map(|part| part.iter().copied()).collect()
}

/// The same audio played `ratio` times slower (a different clock).
fn resampled(segments: &[WaveformSegment], ratio: f64) -> Vec<WaveformSegment> {
    let length = (segments.len() as f64 * ratio) as usize;
    (0..length)
        .map(|index| segments[((index as f64 / ratio) as usize).min(segments.len() - 1)])
        .collect()
}

/// Small codec-like disturbances on every value.
fn jittered(segments: &[WaveformSegment], seed: u64) -> Vec<WaveformSegment> {
    let mut noise = Noise(seed);
    segments
        .iter()
        .map(|segment| {
            let mut wobble =
                |value: u8| (i32::from(value) + noise.below(7) as i32 - 3).clamp(0, 255) as u32;
            self::segment(
                wobble(segment.amplitude),
                wobble(segment.low_band),
                wobble(segment.mid_band),
                wobble(segment.high_band),
            )
        })
        .collect()
}

#[test]
fn identical_recordings_are_the_same() {
    let track = music(1, 200.0);
    assert_eq!(identity(track.clone(), track), RecordingIdentity::Same);
}

#[test]
fn encoder_padding_and_trailing_silence_are_ignored() {
    let track = music(2, 200.0);
    // 1.37 s is deliberately not a whole number of envelope bins.
    let padded = concat(&[&silence(1.37), &track, &silence(2.5)]);
    assert_eq!(identity(track, padded), RecordingIdentity::Same);
}

#[test]
fn a_reencoded_copy_is_the_same() {
    let track = music(3, 240.0);
    let reencoded = jittered(&track, 99);
    assert_eq!(identity(track, reencoded), RecordingIdentity::Same);
}

#[test]
fn a_small_clock_difference_is_the_same_recording() {
    let track = music(4, 300.0);
    let slower = resampled(&track, 1.002);
    assert_eq!(identity(track, slower), RecordingIdentity::Same);
}

#[test]
fn repeated_phrases_do_not_derail_the_alignment() {
    let phrase = music(5, 8.0);
    let mut track = Vec::new();
    for variation in 0..25 {
        track.extend_from_slice(&phrase);
        // Every few bars differ a little, as in a real loop-based track.
        if variation % 5 == 4 {
            track.extend_from_slice(&music(50 + variation, 2.0));
        }
    }
    let padded = concat(&[&silence(0.81), &jittered(&track, 7)]);
    assert_eq!(identity(track, padded), RecordingIdentity::Same);
}

#[test]
fn a_different_performance_is_different() {
    assert_eq!(
        identity(music(6, 240.0), music(7, 250.0)),
        RecordingIdentity::Different
    );
}

#[test]
fn an_extended_mix_around_the_same_audio_is_a_different_edit() {
    let radio_edit = music(8, 200.0);
    let extended = concat(&[&music(9, 45.0), &radio_edit, &music(10, 30.0)]);
    assert_eq!(identity(radio_edit, extended), RecordingIdentity::Different);
}

#[test]
fn an_internal_cut_is_not_the_same_recording() {
    let track = music(11, 240.0);
    let cut = concat(&[&track[..120 * RATE], &track[150 * RATE..]]);
    assert_ne!(identity(track, cut), RecordingIdentity::Same);
}

#[test]
fn a_replaced_section_is_not_the_same_recording() {
    let track = music(12, 240.0);
    let changed = concat(&[&track[..100 * RATE], &music(13, 12.0), &track[112 * RATE..]]);
    assert_ne!(identity(track, changed), RecordingIdentity::Same);
}

#[test]
fn an_early_fade_out_is_not_the_same_recording() {
    let track = music(14, 240.0);
    let shortened = track[..215 * RATE].to_vec();
    assert_ne!(identity(track, shortened), RecordingIdentity::Same);
}

#[test]
fn too_short_or_silent_waveforms_are_not_judged() {
    let detail = |segments| WaveformSegments {
        segment_duration_ms: 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32,
        segments,
    };
    assert!(RecordingProfile::from_detail(&detail(music(15, 20.0))).is_none());
    assert!(RecordingProfile::from_detail(&detail(silence(200.0))).is_none());
    // Twenty seconds of sound inside long silence is still too short.
    assert!(
        RecordingProfile::from_detail(&detail(concat(&[
            &silence(100.0),
            &music(16, 20.0),
            &silence(100.0)
        ])))
        .is_none()
    );
}

#[test]
fn a_waveform_at_another_resolution_is_not_judged() {
    assert!(
        RecordingProfile::from_detail(&WaveformSegments {
            segment_duration_ms: 10.0,
            segments: music(17, 200.0),
        })
        .is_none()
    );
}
