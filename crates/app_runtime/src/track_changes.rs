// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The committed track changes the views have not taken yet (#272).
//!
//! Every write the runtime adopts into its in-memory library — a command,
//! a background result read back from the library store, a wholesale
//! reload — records what it changed here, computed from the track before
//! and after. Views take the changes in one batch on the main loop and
//! decide for themselves what each change means to them, so whoever made
//! a change never has to know which views show it.
//!
//! Changes to one track merge until they are taken. The outbox wakes its
//! reader only when it stops being empty: the reader takes everything
//! pending, so a change recorded while it applies a batch wakes it again
//! for the next one.

use std::collections::BTreeMap;

use crate::{TrackFields, TrackId};

/// What committed writes changed about one track.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrackChange {
    /// The track's fields that differ from the last version views took.
    pub fields: TrackFields,
    /// Whether the track's persisted audio analysis changed: its waveform,
    /// beat grid or acoustic features, none of which a `Track` holds.
    pub analysis: bool,
}

impl TrackChange {
    pub fn is_empty(self) -> bool {
        self.fields.is_empty() && !self.analysis
    }

    fn merge(&mut self, other: Self) {
        self.fields |= other.fields;
        self.analysis |= other.analysis;
    }
}

/// Pending changes, one merged entry per track, in track-id order.
pub type TrackChanges = BTreeMap<TrackId, TrackChange>;

/// Fired when committed track changes start waiting to be taken with
/// [`crate::ApplicationRuntime::take_track_changes`]. The runtime is
/// mid-borrow when it fires: the observer must defer the take to the main
/// loop rather than re-enter the runtime.
pub type TrackChangesObserver = Box<dyn Fn()>;

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

#[derive(Default)]
pub(crate) struct TrackChangeOutbox {
    pending: TrackChanges,
}

impl TrackChangeOutbox {
    /// Merge `change` into the track's pending entry. Returns whether the
    /// outbox stopped being empty, the moment to wake its reader; an empty
    /// change records nothing.
    pub(crate) fn record(&mut self, track_id: TrackId, change: TrackChange) -> bool {
        if change.is_empty() {
            return false;
        }
        let was_empty = self.pending.is_empty();
        self.pending.entry(track_id).or_default().merge(change);
        was_empty
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub(crate) fn take(&mut self) -> TrackChanges {
        std::mem::take(&mut self.pending)
    }
}

#[cfg(test)]
#[path = "track_changes_tests.rs"]
mod tests;
