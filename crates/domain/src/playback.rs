// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::Duration;

use crate::TrackId;

mod options;
mod queue;
mod shuffle;
mod source;
mod volume;

pub use options::{PlaybackOptions, RepeatMode, ShuffleMode};
pub use queue::{
    LazyPickContext, PlaybackQueue, PlaybackQueueEntry, PlaybackQueueEntryKind,
    PlaybackQueueRequest, PlaybackQueueSource,
};
pub use source::TrackPlaybackSource;
pub use volume::VolumePercent;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackCommand {
    /// Start playback at `track_id` and set the play queue from `queue`.
    /// The queue request is part of the command — not derived inside the
    /// runtime — so the caller (UI / MPRIS / test) decides what context
    /// the activation runs in. Activating a track from the Songs view
    /// passes [`PlaybackQueueRequest::Library`]; activating from a
    /// playlist passes [`PlaybackQueueRequest::Explicit`] with the
    /// playlist's track ids so auto-advance stays within the playlist.
    PlayTrack {
        track_id: TrackId,
        queue: PlaybackQueueRequest,
    },
    /// Start playback at an already-upcoming queue entry without rebuilding
    /// the queue. Dispatched by double-clicking a row in the queue popover so
    /// the existing curated region and source continuation remain intact.
    /// No-op when `track_id` is no longer upcoming by the time the command is
    /// handled.
    PlayQueueTrack(TrackId),
    PlayPreviousTrack,
    /// Auto-advance to the next track. Used by the GStreamer EOS callback
    /// when the current track ends naturally. NOT a user-initiated skip;
    /// does not affect skip statistics.
    PlayNextTrack,
    /// User-initiated skip of the currently playing track. Counts as a
    /// skip (increments `skip_count`, sets `last_skipped_at`) when the
    /// play threshold has not yet been reached, then advances to the
    /// next track. Dispatched by the titlebar Next button and any other
    /// surface where the user is explicitly choosing to abandon the
    /// current track in favor of the next one (e.g. media-key Next).
    SkipCurrentTrack,
    /// Insert explicitly requested tracks at the head of the curated Up
    /// Next region, immediately after the current track.
    EnqueueNext(Vec<TrackId>),
    /// Append explicitly requested tracks to the tail of the curated Up
    /// Next region, before the source playthrough continuation.
    EnqueueLast(Vec<TrackId>),
    /// Remove a single curated Up Next track without disturbing source
    /// continuation or the shuffle seed. Dispatched by the queue popover's
    /// per-track evict control. No-op for read-only continuation rows.
    RemoveFromQueue(TrackId),
    /// Reorder one curated Up Next track immediately before or after
    /// another. Dispatched by the queue popover's drag-to-reorder. No-op
    /// for read-only continuation rows.
    ReorderQueue {
        track_id: TrackId,
        target_track_id: TrackId,
        place_after: bool,
    },
    /// Re-derive the play queue from `request` while keeping the currently
    /// playing track and transport state untouched. Dispatched when the
    /// browsing context that defined the queue widens — e.g. the search
    /// filter is cleared after the user played a track from a narrow
    /// result set — so auto-advance continues through the full view
    /// instead of stopping at the end of the now-stale filtered queue
    /// (#78). No-op when nothing is playing, when the widened queue would
    /// not contain the playing track (the browsing context changed out
    /// from under it), or when the resolved track pool is unchanged.
    RepopulateQueue(PlaybackQueueRequest),
    /// Advance to the next mode in the shuffle cycle
    /// (`Off → Pure → Smart → Off`). The transport's shuffle button
    /// dispatches this on every click.
    CycleShuffleMode,
    /// Explicitly set the shuffle mode without relying on the caller's
    /// view of the current option state. Used by source-specific Play /
    /// Shuffle controls that always want a definite outcome (e.g. the
    /// Album header's "Shuffle" button always wants Pure).
    SetShuffleMode(ShuffleMode),
    ToggleRepeat,
    Pause,
    Resume,
    TogglePlayPause,
    Stop,
    Seek(Duration),
    SetVolume(VolumePercent),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum PlaybackState {
    #[default]
    Stopped,
    Loading {
        track_id: TrackId,
    },
    Playing {
        track_id: TrackId,
        position: Duration,
    },
    Paused {
        track_id: TrackId,
        position: Duration,
    },
}
