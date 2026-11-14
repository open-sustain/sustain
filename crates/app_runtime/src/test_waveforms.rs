// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Stored waveforms for tests of recording evidence.

use sustain_domain::{
    DETAIL_SEGMENTS_PER_SECOND, PREVIEW_SEGMENT_COUNT, TrackAnalysis, TrackId, WaveformSegment,
    WaveformSegments,
};
use sustain_library_store::{AnalysisCapabilities, AnalysisContext, LibraryStore};

/// `seconds` of music-like detail segments: notes of pseudo-random length
/// and timbre, so the envelope has shape at every scale.
pub(crate) fn music(seed: u64, seconds: usize) -> Vec<WaveformSegment> {
    let mut state = seed;
    let mut below = |bound: u64| {
        state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (state >> 33) % bound
    };
    let total = seconds * DETAIL_SEGMENTS_PER_SECOND as usize;
    let mut segments = Vec::with_capacity(total);
    while segments.len() < total {
        let length = 15 + below(75);
        let level = 60 + below(160);
        let bands = [below(255), below(255), below(255)];
        for step in 0..length {
            let amplitude = level * (50 + (length - step) * 100 / length) / 150;
            let band = |value: u64| (value * amplitude / 255) as u8;
            segments.push(WaveformSegment {
                amplitude: amplitude as u8,
                low_band: band(bands[0]),
                mid_band: band(bands[1]),
                high_band: band(bands[2]),
            });
        }
    }
    segments.truncate(total);
    segments
}

/// Record `audio` as the analysed detail waveform of `track_id`.
pub(crate) fn analyse(
    store: &dyn LibraryStore,
    track_id: TrackId,
    audio: Vec<WaveformSegment>,
    now: i64,
) {
    let analysis = TrackAnalysis {
        bpm: None,
        key: None,
        beat_grid: None,
        waveform_preview: WaveformSegments {
            segment_duration_ms: 25.0,
            segments: vec![WaveformSegment::silent(); PREVIEW_SEGMENT_COUNT],
        },
        waveform_detail: WaveformSegments {
            segment_duration_ms: 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32,
            segments: audio,
        },
        acoustics: None,
    };
    store
        .record_analysis(
            track_id,
            &analysis,
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            AnalysisContext {
                now_unix: now,
                versions: sustain_analysis::ANALYZER_VERSIONS,
            },
        )
        .expect("record analysis");
}
