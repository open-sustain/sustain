// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::Duration;

use sustain_app_runtime::PlayableId;

/// Trimmed snapshot of the track currently being played, used to derive
/// the MPRIS `Metadata` dictionary. Keeping a dedicated type — rather than
/// passing a full `Track` — avoids leaking unrelated domain concerns
/// (ratings, statistics, file locations) into the desktop-integration
/// surface, and lets the translation be unit-tested without constructing
/// a library store.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NowPlayingMetadata {
    pub track_id: Option<PlayableId>,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_artist: Option<String>,
    pub genre: Option<String>,
    pub track_number: Option<u32>,
    pub disc_number: Option<u32>,
    pub duration: Option<Duration>,
}
