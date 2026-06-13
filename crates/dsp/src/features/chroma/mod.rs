// SPDX-License-Identifier: MIT OR Apache-2.0
//! Chroma modules.
//!
//! Trimmed for Sustain to the STFT front-end + straight chroma extraction in
//! [`extractor`]. The upstream chroma `normalization` (`sharpen_chroma`) and
//! `smoothing` (`smooth_chroma`) helpers are only used by the `analyze_audio`
//! key pipeline Sustain does not call and are not vendored.
//!
//! [`hpss`] and [`hpcp`] are **Sustain-authored** (not vendored): a
//! median-filter harmonic-percussive separation that runs before chroma, and a
//! core Harmonic Pitch Class Profile that the key detector uses in place of the
//! vendored band-summed [`extractor`] path.

pub(crate) mod extractor;
pub(crate) mod hpcp;
pub(crate) mod hpss;
