// SPDX-License-Identifier: MIT OR Apache-2.0
// Vendored from stratum-dsp (https://github.com/HLLMR/stratum-dsp), via the
// open-sustain fork at commit 5f4b4160441725cdd8a6e4355b6908d41b0451db. See
// PROVENANCE.md and LICENSE-MIT / LICENSE-APACHE in this crate. This file
// (the crate root) is Sustain-authored glue that curates the public surface;
// the modules below are the vendored DSP, trimmed to the reachable subset,
// plus clearly-marked Sustain-authored DSP extensions (currently the
// median-filter HPSS in `features/chroma/hpss.rs`). New files in this crate
// stay `MIT OR Apache-2.0` to match the crate's license, NOT the workspace's
// GPL — see PROVENANCE.md.

//! Minimal audio-analysis DSP for Sustain.
//!
//! This crate began as a **vendored, trimmed** copy of the parts of
//! `stratum-dsp` that `sustain-analysis` calls, and now also carries a small
//! set of **clearly-marked Sustain-authored DSP extensions** (re-derived from
//! the literature, not vendored). It is pure DSP: it takes mono `f32` samples
//! (or a precomputed magnitude spectrogram) and returns tempo, key, onsets,
//! and loudness. It has **no** I/O, decoding, storage, or UI dependencies; its
//! only external crates are `rustfft` (FFT) and `log`.
//!
//! The public surface is deliberately narrow — exactly the primitives the
//! analyzer composes, no compute-everything orchestration and no genre/profile
//! layer:
//!
//! * [`compute_stft`] — Hann-windowed magnitude STFT, the spectral front-end
//!   tempo and key both build on (each at its own frame size).
//! * [`estimate_bpm_tempogram`] — tempogram BPM (returns [`BpmEstimate`]).
//! * [`emphasize_harmonic`] — *Sustain-authored* median-filter HPSS that
//!   attenuates percussive energy in a spectrogram before chroma.
//! * [`extract_chroma_from_spectrogram_with_options`] + [`detect_key`] +
//!   [`KeyTemplates`] — chroma → Krumhansl-Kessler template key detection
//!   (returns [`KeyDetectionResult`] over [`Key`]). Key selection is
//!   deterministic (a total-order sort with a fixed tiebreak).
//! * [`detect_spectral_flux_onsets`] — spectral-flux onset frames.
//! * [`normalize`] (+ [`NormalizationConfig`], [`NormalizationMethod`],
//!   [`LoudnessMetadata`]) — ITU-R BS.1770-4 loudness measurement/normalization.
//!
//! All fallible entry points return [`AnalysisError`].
//!
//! # Example
//!
//! ```no_run
//! use sustain_dsp::{
//!     compute_stft, estimate_bpm_tempogram, extract_chroma_from_spectrogram_with_options,
//!     detect_key, KeyTemplates,
//! };
//!
//! let samples: Vec<f32> = vec![0.0; 44_100 * 30]; // mono, normalized
//! let stft = compute_stft(&samples, 2048, 512)?;
//! let bpm = estimate_bpm_tempogram(&stft, 44_100, 512, 70.0, 170.0, 1.0)?;
//! let chroma = extract_chroma_from_spectrogram_with_options(&stft, 44_100, 2048, true, 0.5)?;
//! let key = detect_key(&chroma, &KeyTemplates::new())?;
//! println!("{:.1} BPM, key {:?}", bpm.bpm, key.key);
//! # Ok::<(), sustain_dsp::AnalysisError>(())
//! ```

#![forbid(unsafe_code)]

mod analysis;
mod error;
mod features;
mod preprocessing;

pub use analysis::result::Key;
pub use error::AnalysisError;
pub use features::chroma::extractor::{
    compute_stft, extract_chroma_from_spectrogram_with_options, CHROMA_FMAX_HZ,
};
pub use features::chroma::hpss::emphasize_harmonic;
pub use features::key::detector::detect_key;
pub use features::key::templates::KeyTemplates;
pub use features::key::KeyDetectionResult;
pub use features::onset::spectral_flux::detect_spectral_flux_onsets;
pub use features::period::tempogram::estimate_bpm_tempogram;
pub use features::period::BpmEstimate;
pub use preprocessing::normalization::{
    normalize, LoudnessMetadata, NormalizationConfig, NormalizationMethod,
};
