// SPDX-License-Identifier: MIT OR Apache-2.0
//! Feature extraction modules.
//!
//! Trimmed for Sustain to the four feature families the analyzer uses: the
//! chroma front-end + extraction, key detection, spectral-flux onsets, and
//! tempogram period estimation. The upstream `beat_tracking` module is not
//! vendored.

pub(crate) mod chroma;
pub(crate) mod key;
pub(crate) mod onset;
pub(crate) mod period;
