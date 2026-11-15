// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The committed library changes the views have not taken yet (#272).
//!
//! Every write the runtime adopts into its in-memory library — a command,
//! a background result read back from the library store, a wholesale
//! reload — records what it touched here: which fields of which tracks
//! changed, and which playlists' entries. Views take the changes in one
//! batch on the main loop and bring what they show of each touched track
//! or playlist up to date with the runtime, so whoever made a change never
//! has to know which views show it.
//!
//! A change says what to look at again, not what the views hold: views
//! also rebuild from the runtime on their own (a search, a view shown
//! again), so a track that joined the library may be on screen already,
//! and one that left may be gone. Each view compares a touched track's
//! current state with what it shows. Changes to one track therefore only
//! ever merge; none cancels another.
//!
//! The outbox wakes its reader only when it stops being empty: the reader
//! takes everything pending, so a change recorded while it applies a
//! batch wakes it again for the next one.

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use crate::{Playlist, PlaylistEntry, PlaylistId, Track, TrackFields, TrackId};

/// What committed writes changed about one track.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrackChange {
    /// The track's fields that differ from a version views may show.
    pub fields: TrackFields,
    /// Whether the track's persisted audio analysis changed: its waveform,
    /// beat grid or acoustic features, none of which a `Track` holds.
    pub analysis: bool,
}

impl TrackChange {
    /// A track that joined or left the library: to a view, everything
    /// about it changed.
    pub const EVERYTHING: Self = Self {
        fields: TrackFields::ALL,
        analysis: true,
    };

    /// A change to `fields` of the track.
    pub const fn fields(fields: TrackFields) -> Self {
        Self {
            fields,
            analysis: false,
        }
    }

    /// A change to the track's persisted audio analysis.
    pub const fn analysis() -> Self {
        Self {
            fields: TrackFields::NONE,
            analysis: true,
        }
    }

    pub fn is_empty(self) -> bool {
        self.fields.is_empty() && !self.analysis
    }

    fn merge(&mut self, other: Self) {
        self.fields |= other.fields;
        self.analysis |= other.analysis;
    }
}

/// Pending track changes, one merged entry per track, in track-id order.
pub type TrackChanges = BTreeMap<TrackId, TrackChange>;

/// Everything committed since the views last took the library's changes.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LibraryChanges {
    pub tracks: TrackChanges,
    /// The regular playlists whose entries changed: tracks joined or left
    /// them, or their order changed.
    pub playlist_entries: BTreeSet<PlaylistId>,
}

impl LibraryChanges {
    pub fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.playlist_entries.is_empty()
    }
}

/// Fired when committed changes start waiting to be taken with
/// [`crate::ApplicationRuntime::take_library_changes`]. The runtime is
/// mid-borrow when it fires: the observer must defer the take to the main
/// loop rather than re-enter the runtime.
pub type LibraryChangesObserver = Box<dyn Fn()>;

/// A committed track, announced by a background worker that changed its
/// persisted state behind the runtime's back. The runtime reads the track
/// back from the library store to adopt the change.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackUpdated {
    pub track_id: TrackId,
    /// Whether the worker changed the track's persisted audio analysis,
    /// which reading the track back does not reveal.
    pub analysis: bool,
}

/// What turned `previous` into `current`, two whole libraries sorted by
/// track id: every track only one of them holds joined or left, and the
/// tracks both hold changed in the fields that differ.
pub(crate) fn library_track_changes(
    previous: &[Track],
    current: &[Track],
) -> Vec<(TrackId, TrackChange)> {
    let mut changes = Vec::new();
    let mut previous = previous.iter().peekable();
    let mut current = current.iter().peekable();
    loop {
        let order = match (previous.peek(), current.peek()) {
            (None, None) => break,
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (Some(old), Some(new)) => old.id.cmp(&new.id),
        };
        let change = match order {
            Ordering::Less => previous.next().map(|old| (old.id, TrackChange::EVERYTHING)),
            Ordering::Greater => current.next().map(|new| (new.id, TrackChange::EVERYTHING)),
            Ordering::Equal => previous
                .next()
                .zip(current.next())
                .map(|(old, new)| (new.id, TrackChange::fields(TrackFields::between(old, new)))),
        };
        changes.extend(change);
    }
    changes
}

/// The playlists whose entries differ between `previous` and `current`.
/// A playlist only one of them holds counts as one whose entries were, or
/// are, none.
pub(crate) fn playlists_with_changed_entries(
    previous: &[Playlist],
    current: &[Playlist],
) -> BTreeSet<PlaylistId> {
    fn entries_by_playlist(playlists: &[Playlist]) -> BTreeMap<PlaylistId, &[PlaylistEntry]> {
        playlists
            .iter()
            .map(|playlist| (playlist.id, playlist.entries.as_slice()))
            .collect()
    }
    let previous = entries_by_playlist(previous);
    let current = entries_by_playlist(current);
    previous
        .keys()
        .chain(current.keys())
        .copied()
        .filter(|playlist_id| {
            previous.get(playlist_id).copied().unwrap_or_default()
                != current.get(playlist_id).copied().unwrap_or_default()
        })
        .collect()
}

#[derive(Default)]
pub(crate) struct LibraryChangeOutbox {
    tracks: TrackChanges,
    playlist_entries: BTreeSet<PlaylistId>,
}

impl LibraryChangeOutbox {
    /// Merge `change` into the track's pending entry. Returns whether the
    /// outbox stopped being empty, the moment to wake its reader; an empty
    /// change records nothing.
    pub(crate) fn record_track(&mut self, track_id: TrackId, change: TrackChange) -> bool {
        if change.is_empty() {
            return false;
        }
        let was_empty = self.is_empty();
        self.tracks.entry(track_id).or_default().merge(change);
        was_empty
    }

    /// Record that the entries of `playlist_id` changed. Returns whether
    /// the outbox stopped being empty.
    pub(crate) fn record_playlist_entries(&mut self, playlist_id: PlaylistId) -> bool {
        let was_empty = self.is_empty();
        self.playlist_entries.insert(playlist_id);
        was_empty
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.tracks.is_empty() && self.playlist_entries.is_empty()
    }

    pub(crate) fn take(&mut self) -> LibraryChanges {
        LibraryChanges {
            tracks: std::mem::take(&mut self.tracks),
            playlist_entries: std::mem::take(&mut self.playlist_entries),
        }
    }
}

#[cfg(test)]
#[path = "library_changes_tests.rs"]
mod tests;
