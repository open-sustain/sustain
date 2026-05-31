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

/// The inputs a rebuild needs: the library snapshot and the per-track
/// acoustic features. Both are acquired by the worker, never on the GTK
/// thread (#93).
pub struct RebuildInputs {
    pub tracks: Vec<Track>,
    pub acoustics: Vec<(TrackId, AcousticFeatures)>,
}

/// Worker-run preparation: clones/loads the library snapshot and the
/// acoustics. Runs on the rebuild worker thread so the O(library-size)
/// clone and the `load_all_acoustics` SQLite sweep stay off the GTK
/// main loop. Returns `None` to abort the rebuild without publishing —
/// e.g. when the store read fails, so a transient error leaves the
/// existing index untouched rather than replacing it with an empty one.
pub type RebuildPreparation = Box<dyn FnOnce() -> Option<RebuildInputs> + Send>;

pub struct SmartShuffleScheduler {
    result_sender: async_channel::Sender<SmartShuffleRebuildResult>,
    result_receiver: async_channel::Receiver<SmartShuffleRebuildResult>,
    is_rebuilding: Arc<AtomicBool>,
    /// Set whenever a rebuild is requested while one is already in
    /// flight. The runtime consumes it after applying a result and
    /// re-runs, so a coalesced request never leaves the index stale
    /// against a newer library version. Only ever set here and cleared
    /// by the single-threaded runtime, so there is no lost-update race.
    rerun_requested: Arc<AtomicBool>,
}

impl SmartShuffleScheduler {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            result_sender: tx,
            result_receiver: rx,
            is_rebuilding: Arc::new(AtomicBool::new(false)),
            rerun_requested: Arc::new(AtomicBool::new(false)),
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

    /// Consume the "a rebuild was requested while one was in flight"
    /// flag. The runtime calls this after applying a result; a `true`
    /// means the library may have advanced during the rebuild, so it
    /// re-runs to index the newer state.
    pub fn take_rerun_requested(&self) -> bool {
        self.rerun_requested.swap(false, Ordering::AcqRel)
    }

    /// Spawn an index rebuild on a dedicated background thread.
    ///
    /// `prepare` acquires the library snapshot and acoustics **on the
    /// worker** — the GTK caller only hands over a cheap closure, so the
    /// O(library-size) clone and the `load_all_acoustics` SQLite sweep
    /// never run on the main loop (#93). `built_at` is captured by the
    /// caller (the runtime's clock) so the worker never reads wall-clock
    /// time directly.
    ///
    /// Returns `true` when a worker was spawned. Returns `false` when a
    /// previous rebuild is still in flight; the request is *not* dropped
    /// — [`Self::take_rerun_requested`] then reports that a re-run is due
    /// so the index cannot be left stale against a newer library.
    pub fn request_rebuild(&self, prepare: RebuildPreparation, built_at: SystemTime) -> bool {
        // `compare_exchange` rather than `swap` so a running request
        // leaves the flag set and we do not bounce it.
        if self
            .is_rebuilding
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A rebuild is already running. Remember that the library may
            // have advanced so the runtime re-runs once it completes,
            // rather than dropping this request and leaving stale state.
            self.rerun_requested.store(true, Ordering::Release);
            return false;
        }
        let sender = self.result_sender.clone();
        let flag = self.is_rebuilding.clone();
        let built_at_unix = built_at
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        std::thread::spawn(move || {
            // Snapshot + acoustics acquisition happen here, off the GTK
            // thread. The busy flag stays set through preparation *and*
            // the build so concurrent requests coalesce the whole time;
            // it is released just before the result is published (or, on
            // an aborted preparation, at the end). A `None` means
            // preparation failed (e.g. a store read error): publish
            // nothing so the existing index survives untouched.
            match prepare() {
                Some(RebuildInputs { tracks, acoustics }) => {
                    let index = SmartShuffleIndex::build(&tracks, &acoustics, built_at_unix);
                    flag.store(false, Ordering::Release);
                    // `send_blocking` cannot meaningfully fail on an
                    // unbounded channel whose receiver is owned by the
                    // runtime; drop the error so the worker exits cleanly
                    // at shutdown.
                    let _ = sender.send_blocking(SmartShuffleRebuildResult { index, built_at });
                }
                None => {
                    flag.store(false, Ordering::Release);
                }
            }
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
    use std::sync::mpsc;
    use std::time::{Duration, Instant, SystemTime};

    use sustain_domain::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::{RebuildInputs, RebuildPreparation, SmartShuffleScheduler};

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

    /// Preparation closure returning a fixed snapshot immediately.
    fn ready(tracks: Vec<Track>) -> RebuildPreparation {
        Box::new(move || {
            Some(RebuildInputs {
                tracks,
                acoustics: Vec::new(),
            })
        })
    }

    #[test]
    fn rebuild_delivers_an_index() {
        let scheduler = SmartShuffleScheduler::new();
        let tracks = vec![track(1, "Rock"), track(2, "Shoegaze")];
        assert!(scheduler.request_rebuild(ready(tracks), SystemTime::UNIX_EPOCH));
        let result = scheduler
            .result_receiver()
            .recv_blocking()
            .expect("rebuild result");
        assert_eq!(result.index.indexed_track_count(), 2);
    }

    #[test]
    fn preparation_runs_off_the_calling_thread() {
        // The snapshot clone and acoustics sweep must happen on the
        // worker, not block the (in production, GTK) caller (#93).
        let scheduler = SmartShuffleScheduler::new();
        let prepare: RebuildPreparation = Box::new(|| {
            std::thread::sleep(Duration::from_millis(300));
            Some(RebuildInputs {
                tracks: vec![track(1, "Rock")],
                acoustics: Vec::new(),
            })
        });

        let start = Instant::now();
        assert!(scheduler.request_rebuild(prepare, SystemTime::UNIX_EPOCH));
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "request_rebuild must return before preparation runs, not block on it"
        );

        let result = scheduler
            .result_receiver()
            .recv_blocking()
            .expect("rebuild result lands once preparation completes");
        assert_eq!(result.index.indexed_track_count(), 1);
    }

    #[test]
    fn concurrent_request_is_coalesced_into_a_rerun() {
        // A request that arrives while a rebuild is in flight must not be
        // dropped: it is recorded so the runtime re-runs and the index
        // never lags a newer library version.
        let scheduler = SmartShuffleScheduler::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel::<()>();
        let prepare: RebuildPreparation = Box::new(move || {
            started_tx.send(()).expect("report barrier");
            resume_rx.recv().expect("resume preparation");
            Some(RebuildInputs {
                tracks: vec![track(1, "Rock")],
                acoustics: Vec::new(),
            })
        });

        assert!(scheduler.request_rebuild(prepare, SystemTime::UNIX_EPOCH));
        started_rx
            .recv_timeout(Duration::from_secs(2))
            .expect("first rebuild reached its barrier");

        // The first rebuild is deterministically in flight now.
        assert!(scheduler.is_rebuilding());
        assert!(
            !scheduler.request_rebuild(ready(vec![track(2, "Jazz")]), SystemTime::UNIX_EPOCH),
            "a concurrent request is refused a fresh worker"
        );
        assert!(
            scheduler.take_rerun_requested(),
            "the refused request is recorded as a pending re-run"
        );

        resume_tx.send(()).expect("release first rebuild");
        let result = scheduler
            .result_receiver()
            .recv_blocking()
            .expect("first rebuild result");
        assert_eq!(result.index.indexed_track_count(), 1);
    }

    #[test]
    fn aborted_preparation_publishes_nothing_and_clears_busy() {
        // A `None` from preparation (e.g. a store read error) must leave
        // the existing index in place: no result is published and the
        // scheduler returns to idle.
        let scheduler = SmartShuffleScheduler::new();
        let prepare: RebuildPreparation = Box::new(|| None);
        assert!(scheduler.request_rebuild(prepare, SystemTime::UNIX_EPOCH));

        // The worker releases the busy flag without publishing. Wait for
        // it to settle, then confirm nothing was sent.
        let receiver = scheduler.result_receiver();
        let deadline = Instant::now() + Duration::from_secs(1);
        while scheduler.is_rebuilding() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!scheduler.is_rebuilding(), "busy flag released after abort");
        assert!(
            receiver.try_recv().is_err(),
            "aborted preparation must publish nothing"
        );
    }
}
