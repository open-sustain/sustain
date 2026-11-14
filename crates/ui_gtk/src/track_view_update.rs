// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! What the views must redo for a batch of committed track changes (#272).
//!
//! The runtime reports which fields of which tracks changed; which of
//! those fields each view depends on is the view's knowledge, kept here
//! beside the views rather than in the runtime or in whoever made the
//! change.

use sustain_app_runtime::{TrackChanges, TrackFields, TrackId};

/// The fields that decide which album a track belongs to, or its tile's
/// subtitle. A change to one regroups the Albums view.
const ALBUM_GROUPING: TrackFields = TrackFields::ARTIST
    .union(TrackFields::ALBUM)
    .union(TrackFields::ALBUM_ARTIST)
    .union(TrackFields::YEAR)
    .union(TrackFields::COMPILATION);

/// The fields an album shows of each of its tracks, outside the grouping.
/// The location gives the file its artwork is read from, whether the
/// track is missing, and the title of a track that has none.
const ALBUM_TRACK: TrackFields = TrackFields::TITLE
    .union(TrackFields::DISC_NUMBER)
    .union(TrackFields::TRACK_NUMBER)
    .union(TrackFields::DURATION)
    .union(TrackFields::LOCATION);

/// The fields that decide which tracks are duplicate candidates. Audio
/// analysis decides it too: it verifies candidates or tells them apart,
/// and losing it (a replaced recording) puts apart tracks back together.
const DUPLICATE_IDENTITY: TrackFields = TrackFields::TITLE
    .union(TrackFields::ARTIST)
    .union(TrackFields::ALBUM)
    .union(TrackFields::DURATION);

/// The view work one batch of changes calls for. Rows of every changed
/// track are always repainted in place in the track tables.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct TrackViewUpdate {
    pub(crate) track_ids: Vec<TrackId>,
    pub(crate) albums: AlbumsUpdate,
    pub(crate) duplicates: DuplicatesUpdate,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AlbumsUpdate {
    /// Nothing the Albums view shows changed.
    Unaffected,
    /// Patch these tracks inside their albums, keeping the grid and the
    /// expanded album as they are.
    Patch(Vec<TrackId>),
    /// Reload these tracks' artwork.
    Artwork(Vec<TrackId>),
    /// Group the albums again.
    Regroup,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DuplicatesUpdate {
    /// Repaint the changed rows in place.
    Rows,
    /// Group the duplicates again.
    Regroup,
}

impl TrackViewUpdate {
    pub(crate) fn for_changes(changes: &TrackChanges) -> Self {
        let mut fields = TrackFields::NONE;
        let mut album_tracks = Vec::new();
        let mut analysis = false;
        for (track_id, change) in changes {
            fields |= change.fields;
            if change.fields.intersects(ALBUM_TRACK) {
                album_tracks.push(*track_id);
            }
            analysis |= change.analysis;
        }
        let albums = if fields.intersects(ALBUM_GROUPING) {
            AlbumsUpdate::Regroup
        } else if album_tracks.is_empty() {
            AlbumsUpdate::Unaffected
        } else {
            AlbumsUpdate::Patch(album_tracks)
        };
        let duplicates = if analysis || fields.intersects(DUPLICATE_IDENTITY) {
            DuplicatesUpdate::Regroup
        } else {
            DuplicatesUpdate::Rows
        };
        Self {
            track_ids: changes.keys().copied().collect(),
            albums,
            duplicates,
        }
    }

    /// The artwork embedded in these tracks' files changed, which no
    /// library field records.
    pub(crate) fn for_artwork(track_ids: &[TrackId]) -> Self {
        Self {
            track_ids: track_ids.to_vec(),
            albums: AlbumsUpdate::Artwork(track_ids.to_vec()),
            duplicates: DuplicatesUpdate::Rows,
        }
    }
}

#[cfg(test)]
#[path = "track_view_update_tests.rs"]
mod tests;
