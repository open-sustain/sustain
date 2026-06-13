// SPDX-License-Identifier: MIT OR Apache-2.0
//! Audio preprocessing modules.
//!
//! Trimmed for Sustain to [`normalization`], whose ITU-R BS.1770-4 path the
//! acoustics pass uses to measure integrated/short-term loudness. The upstream
//! `silence` trimming and `channel_mixer` modules belong to the `analyze_audio`
//! preprocessing chain Sustain does not use and are not vendored.

pub(crate) mod normalization;
