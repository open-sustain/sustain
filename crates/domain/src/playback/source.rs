// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::PathBuf;

use super::PlayableId;

/// What the playback backend is asked to play: the playable, and where
/// its audio is read from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackSource {
    pub track_id: PlayableId,
    pub location: PlaybackLocation,
}

/// Where a playable's audio is read from.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackLocation {
    /// An audio file of the library.
    File(PathBuf),
    /// A track of the audio CD in the optical drive at `device`.
    AudioCd { device: PathBuf, track_number: u32 },
}

impl PlaybackSource {
    /// A library file.
    pub fn file(track_id: PlayableId, path: impl Into<PathBuf>) -> Self {
        Self {
            track_id,
            location: PlaybackLocation::File(path.into()),
        }
    }
}
