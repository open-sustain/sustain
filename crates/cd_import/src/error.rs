// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The error surface returned by the CD backend.
//!
//! Kept small and provider-agnostic: the runtime maps these into its own
//! notifications, so finer GStreamer detail belongs in the message string,
//! not in extra variants the caller would have to branch on.

use std::fmt;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CdImportError {
    /// A required GStreamer element (named) is not installed. Surfaced
    /// before an import can start so the user gets a precise message.
    MissingGstElement(String),
    /// GStreamer could not initialize at all.
    GstInitFailed,
    /// The encode pipeline could not be constructed or linked.
    PipelineBuildFailed,
    /// The extraction/encode pipeline reported an error (message attached).
    EncodeFailed(String),
    /// The disc in the drive no longer matches the one the import started
    /// against (the user swapped discs).
    DiscChanged,
    /// The caller requested cancellation; extraction stopped at a track
    /// boundary or mid-track.
    Cancelled,
}

impl fmt::Display for CdImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingGstElement(name) => {
                write!(f, "required GStreamer element `{name}` is not installed")
            }
            Self::GstInitFailed => f.write_str("GStreamer could not be initialized"),
            Self::PipelineBuildFailed => {
                f.write_str("the CD extraction pipeline could not be built")
            }
            Self::EncodeFailed(detail) => write!(f, "CD track extraction failed: {detail}"),
            Self::DiscChanged => f.write_str("the disc in the drive changed during import"),
            Self::Cancelled => f.write_str("CD import was cancelled"),
        }
    }
}

impl std::error::Error for CdImportError {}

pub type CdImportResult<T> = Result<T, CdImportError>;
