// SPDX-License-Identifier: MIT OR Apache-2.0
//! Onset detection modules.
//!
//! Trimmed for Sustain to [`spectral_flux`], the only onset detector the
//! acoustics pass uses (for onset density). The upstream energy-flux, HFC,
//! HPSS, consensus, thresholding detectors and the `OnsetCandidate` type belong
//! to the `analyze_audio` onset-consensus path Sustain does not use and are not
//! vendored.

pub(crate) mod spectral_flux;
