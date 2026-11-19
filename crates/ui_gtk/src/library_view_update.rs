// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! What the views must redo for a batch of committed library changes (#272).
//!
//! The runtime reports which fields of which tracks changed, and which
//! playlists' entries; which of those each view depends on is the view's
//! knowledge, kept here beside the views rather than in the runtime or in
//! whoever made the change. A track that joined or left the library
//! changed in every field.

use std::collections::BTreeSet;

use sustain_app_runtime::{LibraryChanges, PlaylistId, TrackFields, TrackId};

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
    .union(TrackFields::DISC_TOTAL)
    .union(TrackFields::TRACK_NUMBER)
    .union(TrackFields::TRACK_TOTAL)
    .union(TrackFields::DURATION)
    .union(TrackFields::LOCATION);

/// The fields that decide which tracks are duplicate candidates. Audio
/// analysis decides it too: it verifies candidates or tells them apart,
/// and losing it (a replaced recording) puts apart tracks back together.
const DUPLICATE_IDENTITY: TrackFields = TrackFields::TITLE
    .union(TrackFields::ARTIST)
    .union(TrackFields::ALBUM)
    .union(TrackFields::DURATION);

/// The fields the Statistics page aggregates. Its play and rating
/// rankings are by genre, and its distributions are of genres, bitrates,
/// release years and the dates tracks were added.
const STATISTICS_PAGE: TrackFields = TrackFields::GENRE
    .union(TrackFields::AUDIO_FORMAT)
    .union(TrackFields::YEAR)
    .union(TrackFields::RATING)
    .union(TrackFields::STATISTICS);

/// The view work one batch of changes calls for. The track tables bring
/// the rows of every touched track up to date with the runtime: in place
/// when the track is still in the library, gone when it is not, and in
/// when it is listed but not shown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct LibraryViewUpdate {
    pub(crate) track_ids: Vec<TrackId>,
    pub(crate) albums: AlbumsUpdate,
    pub(crate) duplicates: DuplicatesUpdate,
    /// Whether the Statistics page aggregates anything that changed.
    pub(crate) statistics: bool,
    /// The regular playlists whose entries changed.
    pub(crate) playlist_entries: BTreeSet<PlaylistId>,
    /// The tracks whose persisted audio analysis changed: their
    /// waveform, beat grid or acoustic features.
    pub(crate) analysed: BTreeSet<TrackId>,
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

impl LibraryViewUpdate {
    pub(crate) fn for_changes(changes: &LibraryChanges) -> Self {
        let mut fields = TrackFields::NONE;
        let mut album_tracks = Vec::new();
        let mut analysed = BTreeSet::new();
        for (track_id, change) in &changes.tracks {
            fields |= change.fields;
            if change.fields.intersects(ALBUM_TRACK) {
                album_tracks.push(*track_id);
            }
            if change.analysis {
                analysed.insert(*track_id);
            }
        }
        let albums = if fields.intersects(ALBUM_GROUPING) {
            AlbumsUpdate::Regroup
        } else if album_tracks.is_empty() {
            AlbumsUpdate::Unaffected
        } else {
            AlbumsUpdate::Patch(album_tracks)
        };
        let duplicates = if !analysed.is_empty() || fields.intersects(DUPLICATE_IDENTITY) {
            DuplicatesUpdate::Regroup
        } else {
            DuplicatesUpdate::Rows
        };
        Self {
            track_ids: changes.tracks.keys().copied().collect(),
            albums,
            duplicates,
            statistics: fields.intersects(STATISTICS_PAGE),
            playlist_entries: changes.playlist_entries.clone(),
            analysed,
        }
    }

    /// The artwork embedded in these tracks' files changed, which no
    /// library field records.
    pub(crate) fn for_artwork(track_ids: &[TrackId]) -> Self {
        Self {
            track_ids: track_ids.to_vec(),
            albums: AlbumsUpdate::Artwork(track_ids.to_vec()),
            duplicates: DuplicatesUpdate::Rows,
            statistics: false,
            playlist_entries: BTreeSet::new(),
            analysed: BTreeSet::new(),
        }
    }
}

#[cfg(test)]
#[path = "library_view_update_tests.rs"]
mod tests;
