// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

pub use sustain_app_runtime::{PlayableId, PlaybackState, TrackId};

mod metadata;
mod mpris;

pub use metadata::NowPlayingMetadata;
pub use mpris::{MprisCommand, MprisPlaybackSink, MprisService, MprisStartConfig};

pub type DesktopResult<T> = Result<T, DesktopError>;

#[derive(Debug)]
pub enum DesktopError {
    /// The session bus connection could not be established or the well-known
    /// MPRIS bus name was already taken (e.g. another Sustain instance is
    /// already running and holds it). When this surfaces, the application
    /// should continue without desktop integration; in-window controls and
    /// keyboard shortcuts still work, only the media-key bridge is missing.
    BusConnectionFailed(zbus::Error),
    /// The dedicated MPRIS worker thread could not be spawned. Treat the
    /// same as `BusConnectionFailed`: log and continue without integration.
    ThreadSpawnFailed(std::io::Error),
}

impl std::fmt::Display for DesktopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BusConnectionFailed(error) => {
                write!(f, "MPRIS bus connection failed: {error}")
            }
            Self::ThreadSpawnFailed(error) => {
                write!(f, "MPRIS worker thread spawn failed: {error}")
            }
        }
    }
}

impl std::error::Error for DesktopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BusConnectionFailed(error) => Some(error),
            Self::ThreadSpawnFailed(error) => Some(error),
        }
    }
}
