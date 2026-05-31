// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Background scheduler for Smart Shuffle index rebuilds.
//!
//! Rebuilding the index (the genre-token IDF sweep and, later, the
//! robust normalization statistics) is milliseconds of work on a
//! 10 000-track library — but it is real, library-dependent work, and
//! running it on the GTK main loop would still risk a hitch on the
//! larger collections. So it runs on a dedicated worker thread,
//! exactly like the old trainer's shell, with the *meaning* changed:
//! there is no model and no training, only an index recompute.
//!
//! Two trigger paths feed the scheduler:
//!   * Explicit — the "Rebuild index" button in the Shuffle
//!     preferences tab, or the runtime's first enable-Smart-Shuffle
//!     attempt when no index exists yet.
//!   * Interval — a glib timer in the UI shell calls back into the
//!     runtime periodically; the runtime checks elapsed time against
//!     the user-configured cadence and forwards through here when a
//!     rebuild is due.
//!
//! The scheduler does NOT own the index; the runtime owns the
//! in-memory copy and writes the persisted blob into the library
//! store. The scheduler is purely "run the rebuild on a background
//! thread" — and one that coalesces overlapping requests (the worker
//! drops re-entrant requests rather than queuing them, because two
//! back-to-back rebuilds on the same library produce identical
//! indexes).

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{SystemTime, UNIX_EPOCH};

use sustain_domain::{AcousticFeatures, Track, TrackId};
use sustain_smart_shuffle::SmartShuffleIndex;

/// A freshly-rebuilt index published by the worker. The runtime reads
/// these on its result-sink tick and swaps in the new index (and
/// persists its blob). `built_at` is the runtime clock value captured
/// when the rebuild was requested.
#[derive(Debug)]
pub struct SmartShuffleRebuildResult {
    pub index: SmartShuffleIndex,
    pub built_at: SystemTime,
}

pub struct SmartShuffleScheduler {
    result_sender: async_channel::Sender<SmartShuffleRebuildResult>,
    result_receiver: async_channel::Receiver<SmartShuffleRebuildResult>,
    is_rebuilding: Arc<AtomicBool>,
}

impl SmartShuffleScheduler {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            result_sender: tx,
            result_receiver: rx,
            is_rebuilding: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Result channel the UI shell drains on the main loop. The
    /// receiver is cloneable so the shell can hold its own copy
    /// without taking ownership of the scheduler.
    pub fn result_receiver(&self) -> async_channel::Receiver<SmartShuffleRebuildResult> {
        self.result_receiver.clone()
    }

    pub fn is_rebuilding(&self) -> bool {
        self.is_rebuilding.load(Ordering::Acquire)
    }

    /// Spawn an index rebuild on a dedicated background thread.
    /// Returns `false` when a previous rebuild is still in flight —
    /// the request is dropped rather than queued, because two
    /// back-to-back rebuilds on an unchanged library produce identical
    /// indexes. `acoustics` is the store's full set of analysed tracks
    /// (the index ignores entries for unavailable or unknown tracks).
    /// `built_at` is captured by the caller (the runtime's clock) so the
    /// worker thread never reads wall-clock time directly.
    pub fn request_rebuild(
        &self,
        tracks: Vec<Track>,
        acoustics: Vec<(TrackId, AcousticFeatures)>,
        built_at: SystemTime,
    ) -> bool {
        // `compare_exchange` rather than `swap` so a running request
        // leaves the flag set and we do not bounce it.
        if self
            .is_rebuilding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        let sender = self.result_sender.clone();
        let flag = self.is_rebuilding.clone();
        let built_at_unix = built_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        std::thread::spawn(move || {
            let index = SmartShuffleIndex::build(&tracks, &acoustics, built_at_unix);
            flag.store(false, Ordering::Release);
            // `send_blocking` cannot meaningfully fail on an unbounded
            // channel whose receiver is owned by the runtime; drop the
            // error so the worker can exit cleanly at shutdown.
            let _ = sender.send_blocking(SmartShuffleRebuildResult { index, built_at });
        });
        true
    }
}

impl Default for SmartShuffleScheduler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::SmartShuffleScheduler;

    fn track(id: i64, genre: &str) -> Track {
        Track {
            id: TrackId::new(id).expect("valid id"),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("g/{id}.flac")).expect("relative path"),
            ),
            metadata: TrackMetadata {
                genre: Some(genre.to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
        }
    }

    #[test]
    fn scheduler_rejects_concurrent_rebuilds() {
        let scheduler = SmartShuffleScheduler::new();
        let tracks: Vec<Track> = (0..160)
            .map(|index| track(index + 1, if index % 2 == 0 { "Rock" } else { "Jazz" }))
            .collect();

        assert!(scheduler.request_rebuild(tracks.clone(), Vec::new(), SystemTime::UNIX_EPOCH));
        // A second request while the first is in flight should be
        // refused. There is an inherent race between the worker
        // clearing `is_rebuilding` and this assertion; the first
        // result is drained to keep the worker tidy regardless.
        let _ = scheduler.request_rebuild(tracks, Vec::new(), SystemTime::UNIX_EPOCH);
        let _ = scheduler.result_receiver().recv_blocking();
    }

    #[test]
    fn rebuild_delivers_an_index() {
        let scheduler = SmartShuffleScheduler::new();
        let tracks = vec![track(1, "Rock"), track(2, "Shoegaze")];
        assert!(scheduler.request_rebuild(tracks, Vec::new(), SystemTime::UNIX_EPOCH));
        let result = scheduler
            .result_receiver()
            .recv_blocking()
            .expect("rebuild result");
        assert_eq!(result.index.indexed_track_count(), 2);
    }
}
