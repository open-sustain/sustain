// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Neutral analysis value types shared by every layer that touches
//! analysis data: the DSP crate (`sustain_analysis`) produces these,
//! the storage layer (`sustain_library_store`) persists them, and the
//! UI renderer consumes them. Lives in the domain layer so the
//! storage layer does not have to pull in symphonia / stratum-dsp
//! merely to know what an analysis result looks like.
//!
//! Each waveform segment is exactly 4 bytes, so a vector of segments
//! can be serialized to/from a SQLite BLOB by raw-bytes copy without
//! any framing — the byte length divided by four is the segment
//! count. Endianness is irrelevant because every field is a single
//! `u8`.

use crate::{AcousticFeatures, BeatGrid, Bpm, MusicalKey};

/// Fixed segment count for the preview waveform. Matches Pioneer's
/// PWAV column count (so a future export crate does not have to
/// resample) and is a comfortable thumbnail width in the GTK UI.
pub const PREVIEW_SEGMENT_COUNT: usize = 400;

/// Detail-waveform time resolution. Matches Pioneer's PWV3/PWV5
/// entries-per-second figure so a future export crate can serialize
/// our detail segments directly. Also covers any sensible UI zoom on
/// a desktop scrubber.
pub const DETAIL_SEGMENTS_PER_SECOND: u32 = 150;

/// A single waveform segment: peak amplitude plus per-band RMS
/// energies, all quantized to `u8`. 4 bytes total. Each band is
/// quantized against its own loudest segment, so a faint band keeps the
/// resolution its shape needs; its tier's [`BandScale`] relates the
/// bands to each other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WaveformSegment {
    /// Peak amplitude during the segment window, normalized to the
    /// loudest segment in the track (0 = silence, 255 = track peak).
    pub amplitude: u8,
    /// RMS energy in the low band (≤ ~250 Hz), 0–255, relative to the
    /// band's loudest segment in the track.
    pub low_band: u8,
    /// RMS energy in the mid band (~250 Hz – 4 kHz), 0–255, relative to
    /// the band's loudest segment in the track.
    pub mid_band: u8,
    /// RMS energy in the high band (≥ ~4 kHz), 0–255, relative to the
    /// band's loudest segment in the track.
    pub high_band: u8,
}

impl WaveformSegment {
    /// All-zero segment. Helper for the silent-track / empty-bucket
    /// fallback in renderers and segmenters.
    pub const fn silent() -> Self {
        Self {
            amplitude: 0,
            low_band: 0,
            mid_band: 0,
            high_band: 0,
        }
    }
}

/// How the band energies of a tier relate to each other. Each band's
/// bytes are scaled to its own loudest segment; its scale is that
/// segment's energy relative to the loudest segment of the track's
/// strongest band. The strongest band's scale is 1, a band with no energy
/// at all has 0. Every scale lies in 0–1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandScale {
    /// Scale of the low band.
    pub low: f32,
    /// Scale of the mid band.
    pub mid: f32,
    /// Scale of the high band.
    pub high: f32,
}

impl BandScale {
    /// Bands that peak equally loud: their bytes already compare with
    /// each other as they are.
    pub const EVEN: Self = Self {
        low: 1.0,
        mid: 1.0,
        high: 1.0,
    };

    /// `segment`'s low, mid and high band energies on one scale shared by
    /// the three bands, each in 0–1: the balance between them, which the
    /// segment's colour is read from.
    pub fn balanced(self, segment: &WaveformSegment) -> [f32; 3] {
        let level = |byte: u8, scale: f32| f32::from(byte) / 255.0 * scale;
        [
            level(segment.low_band, self.low),
            level(segment.mid_band, self.mid),
            level(segment.high_band, self.high),
        ]
    }
}

/// A waveform tier (preview or detail). The renderer multiplies its
/// horizontal pixel position by `segment_duration_ms` to map a screen
/// column back to a time offset within the track.
#[derive(Clone, Debug, PartialEq)]
pub struct WaveformSegments {
    /// How many milliseconds of audio each segment covers.
    pub segment_duration_ms: f32,
    /// The segments themselves, in time order.
    pub segments: Vec<WaveformSegment>,
    /// How the segments' band energies relate to each other.
    pub band_scale: BandScale,
}

impl WaveformSegments {
    /// A tier without segments: the waveform of no audio.
    pub const fn empty() -> Self {
        Self {
            segment_duration_ms: 0.0,
            segments: Vec::new(),
            band_scale: BandScale::EVEN,
        }
    }
}

/// Result of analyzing a single track. Produced by `sustain_analysis`,
/// consumed by `sustain_library_store` (for persistence) and the UI
/// renderer (for display). The struct is just a bag of values — no
/// behavior — so it lives in the domain layer next to the field
/// types it composes.
#[derive(Clone, Debug, PartialEq)]
pub struct TrackAnalysis {
    /// Detected tempo. `None` if BPM detection was disabled, the file
    /// was too short, or the DSP engine could not produce a confident
    /// value. When a beat grid was found this is the grid's tempo.
    pub bpm: Option<Bpm>,
    /// Detected musical key. `None` if key detection was disabled or
    /// the DSP engine returned a value that did not match a known key.
    pub key: Option<MusicalKey>,
    /// The track's beats. `None` if BPM detection was disabled or the
    /// track has no steady beat to lay a constant grid on.
    pub beat_grid: Option<BeatGrid>,
    /// Fixed-resolution overview ([`PREVIEW_SEGMENT_COUNT`] segments).
    pub waveform_preview: WaveformSegments,
    /// Time-resolution detail ([`DETAIL_SEGMENTS_PER_SECOND`]
    /// segments per second of audio).
    pub waveform_detail: WaveformSegments,
    /// Perceptual acoustic features for Smart Shuffle (loudness, onset
    /// density, timbre, …). `None` when the acoustics capability was
    /// not requested or the DSP pass could not produce them.
    pub acoustics: Option<AcousticFeatures>,
}
