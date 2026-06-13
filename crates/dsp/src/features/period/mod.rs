// SPDX-License-Identifier: MIT OR Apache-2.0
//! Period estimation (BPM detection).
//!
//! Tempogram-based BPM detection: a novelty curve (SuperFlux + energy flux +
//! HFC) drives both an FFT tempogram and an autocorrelation tempogram, whose
//! peaks are scored with tempo-folding to resolve metrical-level ambiguity.
//!
//! Trimmed for Sustain to the tempogram path reached from
//! [`tempogram::estimate_bpm_tempogram`]. The upstream legacy onset-list
//! estimators (`estimate_bpm`, autocorrelation/comb-filter/candidate-filter,
//! peak-picking) and the multi-resolution escalation belong to the
//! `analyze_audio` orchestration Sustain does not call and are not vendored.

pub(crate) mod novelty;
pub(crate) mod tempogram;
pub(crate) mod tempogram_autocorr;
pub(crate) mod tempogram_fft;

/// Final BPM estimate with method agreement
#[derive(Debug, Clone)]
pub struct BpmEstimate {
    /// BPM estimate
    pub bpm: f32,

    /// Confidence score
    pub confidence: f32,

    /// Number of methods that agree
    pub method_agreement: u32,
}
