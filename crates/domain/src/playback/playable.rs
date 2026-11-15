// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use crate::TrackId;

/// Something the player can play: a track of the library, or a track of
/// the audio CD being played without being ripped (#197).
///
/// The play queue, the playback state and the transport speak in
/// playables, so a disc plays through the same transport, Now Playing and
/// media keys as the library. Library-only behaviour — play and skip
/// counts, ratings, Smart Shuffle — reaches a playable through
/// [`Self::library_track_id`], so a disc track never touches it.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum PlayableId {
    /// A library track.
    Library(TrackId),
    /// The track numbered `number` on the audio CD being played.
    AudioCdTrack { number: u32 },
}

impl PlayableId {
    /// The library track this playable is, if it is one.
    pub fn library_track_id(self) -> Option<TrackId> {
        match self {
            Self::Library(track_id) => Some(track_id),
            Self::AudioCdTrack { .. } => None,
        }
    }
}

impl From<TrackId> for PlayableId {
    fn from(track_id: TrackId) -> Self {
        Self::Library(track_id)
    }
}
