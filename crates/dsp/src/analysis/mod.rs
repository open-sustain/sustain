// SPDX-License-Identifier: MIT OR Apache-2.0
//! Analysis result types.
//!
//! Trimmed for Sustain to [`result`], which carries the `Key` enum. The
//! upstream `confidence` and `metadata` modules belong to the `analyze_audio`
//! orchestration Sustain does not use and are not vendored.

pub(crate) mod result;
