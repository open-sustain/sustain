// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Paced background driver for network-bound retrievals.
//!
//! Mirrors [`crate::analysis_scheduler::AnalysisScheduler`] in shape
//! but targets remote work: tag enrichment via MusicBrainz, artwork
//! lookups via Cover Art Archive, and lyric pulls from LRClib. The
//! scheduler is intentionally conservative: capabilities are
//! missing-only (a track that already has embedded artwork or stored
//! lyrics is not contacted, tag fills never overwrite an existing
//! value), every completed attempt is stamped through
//! `track_online_status` so we do not re-fetch on every cycle, and
//! per-track pacing keeps the host polite even with the strict
//! per-host rate limits the HTTP client already enforces.
//!
//! Rate-limit handling: when any per-track attempt comes back with
//! [`sustain_metadata_remote::RemoteError::RateLimited`], the
//! capability that hit the limit is left un-stamped and the worker
//! stops the current batch instead of running the remaining tracks
//! straight into the same wall. It waits the cool-down out on its
//! command channel — so a shutdown or a settings change is still heard
//! at once — and then starts the next batch, which picks the track up
//! again. Explicit items the batch had not finished go back to the
//! front of their queue.
//!
//! Lifecycle, command channel, and shutdown semantics are identical
//! to the analysis scheduler; see its docs for the longer rationale.
//!
//! ## Work sources
//!
//! The worker multiplexes two sources of work, mirroring the
//! analysis scheduler:
//!
//! 1. **Background sweep** — driven by `LibraryStore::tracks_needing_online`
//!    with capabilities derived from the global `OnlineSettings`.
//! 2. **Explicit queue** — populated by
//!    `OnlineScheduler::request_explicit_run` for per-playlist
//!    user-initiated runs. Each entry carries its own capability
//!    mask, independent of the global settings, so a user can fetch
//!    lyrics for a single playlist while keeping the global lyrics
//!    toggle off.
//!
//! The explicit queue is drained first on every refill, then any
//! remaining slack is filled from the background query. An explicit
//! run that arrives mid-batch ends the batch's background part, so it
//! waits for the track in progress only, as in the analysis scheduler.

use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::{
    FieldChange, MetadataChange, OnlineSettings, SyncedLyrics, Track, TrackId,
    normalize_search_text,
};
use sustain_library_store::{LibraryStore, OnlineCapabilities, OnlineContext};
use sustain_metadata_remote::{
    FetchedArtwork, FetchedLyrics, GenreCandidate, RemoteError, RemoteMetadataService, TrackMatch,
    TrackMatchRelease, TrackMatchSource, TrackQuery,
};

use crate::artwork_fetcher::query_from_metadata;
use crate::metadata_writer::MetadataWriteHandle;

/// How long the worker sleeps between two consecutive tracks. The
/// HTTP client's per-host rate limiter already prevents bursting
/// against any one provider; this extra pause holds the *cross*-host
/// rate down to something modest so background work does not saturate
/// the user's uplink during normal browsing.
const INTER_TRACK_PAUSE: Duration = Duration::from_millis(250);

/// How many tracks to fetch from the store per
/// `tracks_needing_online` query.
const BATCH_SIZE: usize = 16;

/// Short tag stored alongside synced lyrics so a future diagnostic UI
/// can answer "where did this come from?" without consulting logs.
const LRCLIB_SOURCE_TAG: &str = "lrclib";

/// Sink for progress updates emitted by the worker. The runtime wraps
/// this in an `async_channel` send so notifications surface on the
/// GTK main loop without the worker touching widgets directly.
pub type ProgressSink = Arc<dyn Fn(SchedulerProgress) + Send + Sync>;

/// Sink invoked once per track after the worker has mutated the
/// library store in a way the in-memory `library_tracks` copy needs
/// to see (a lyrics column update, a non-destructive tag fill). The
/// runtime wraps this in an `async_channel` send so the UI shell can
/// refresh that row on the main loop. Stays a no-op when no sink is
/// installed.
pub type TrackUpdatedSink = Arc<dyn Fn(TrackId) + Send + Sync>;

/// Wall-clock source recorded into `track_online_status.*_attempted_at_unix`.
pub type UnixClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Per-track progress signal. Same shape as the analysis scheduler so
/// the UI surface can use a shared widget treatment.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerProgress {
    Tick {
        completed: u32,
        failed: u32,
        remaining: u32,
    },
    /// The run is over, and this is what it came to.
    Idle(OnlineRunTally),
    /// The attempt stamp (`record_online_attempt`) was rejected by
    /// SQLite. Without it the track stays eligible in
    /// `tracks_needing_online` and would be re-fetched over the network
    /// every batch. The worker stops and waits for an explicit wake; this
    /// event surfaces the failure instead of reporting a clean pass that
    /// will silently repeat. `detail` carries the store error.
    PersistenceError { detail: String },
}

/// What a run of online retrieval came to, counting each track once, by
/// the most telling of what its lookups came to: a track given artwork
/// and no lyrics was updated.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OnlineRunTally {
    /// Tracks given online data.
    pub updated: u32,
    /// Tracks the providers were asked about and had nothing new for.
    pub not_found: u32,
    /// Tracks that already held everything asked for: no provider was
    /// asked about them.
    pub already_complete: u32,
    /// Tracks a rate limit cut short that the run ended before finishing.
    pub pending: u32,
    /// Tracks that could not be checked.
    pub failed: u32,
}

impl OnlineRunTally {
    /// Tracks checked to the end, whatever they came to but a failure.
    pub const fn checked(&self) -> u32 {
        self.updated
            .saturating_add(self.not_found)
            .saturating_add(self.already_complete)
    }

    pub const fn is_empty(&self) -> bool {
        self.checked() == 0 && self.pending == 0 && self.failed == 0
    }

    const fn counter(&mut self, outcome: TrackOutcome) -> &mut u32 {
        match outcome {
            TrackOutcome::AlreadyComplete => &mut self.already_complete,
            TrackOutcome::NotFound => &mut self.not_found,
            TrackOutcome::Pending => &mut self.pending,
            TrackOutcome::Failed => &mut self.failed,
            TrackOutcome::Updated => &mut self.updated,
        }
    }
}

/// What a track's capabilities came to, ordered from the least to the
/// most telling: a track is reported by the most telling result among
/// its capabilities.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum TrackOutcome {
    AlreadyComplete,
    NotFound,
    /// Still owed capabilities a rate limit cut short. Never what a
    /// capability comes to: it stands for those not yet run.
    Pending,
    Failed,
    Updated,
}

/// What this run's lookups of one track have come to so far.
#[derive(Clone, Copy, Debug, Default)]
struct TrackRecord {
    /// What its completed capabilities came to.
    outcome: Option<TrackOutcome>,
    /// The capabilities a rate limit cut off that have not run since.
    owed: OnlineCapabilities,
}

impl TrackRecord {
    /// How the track is reported: a track still owed capabilities is
    /// pending unless what it completed says more.
    fn reported(self) -> Option<TrackOutcome> {
        if !self.owed.is_empty() {
            self.outcome.max(Some(TrackOutcome::Pending))
        } else {
            self.outcome
        }
    }
}

/// Bundle of dependencies the scheduler captures at start-up.
pub(crate) struct OnlineSchedulerConfig {
    pub remote_service: Arc<dyn RemoteMetadataService>,
    /// Cloneable handle to the runtime's [`crate::metadata_writer::MetadataWriter`]
    /// actor. Every file-tag write the online scheduler performs is
    /// routed through it so UI rating clicks and background tag fills
    /// can never collide on the same file.
    pub tag_writer: MetadataWriteHandle,
    pub library_store: Arc<dyn LibraryStore>,
    pub progress: ProgressSink,
    /// Optional sink fired after each persisted track mutation so the
    /// runtime can refresh its in-memory `library_tracks` copy. `None`
    /// when the embedder does not care about live UI refreshes (tests,
    /// headless deployments).
    pub track_updated: Option<TrackUpdatedSink>,
    pub clock: UnixClockFn,
    pub initial_settings: OnlineSettings,
    pub library_path: Option<PathBuf>,
    pub provider_version: u32,
}

#[derive(Clone, Debug)]
enum SchedulerCommand {
    SettingsChanged(OnlineSettings),
    LibraryPathChanged(Option<PathBuf>),
    /// "Look for new work" — the set of tracks needing online work
    /// may have grown (see `ApplicationRuntime::wake_background_sweeps`).
    Wake,
    /// User-initiated batch: process every track in `track_ids`
    /// with the given `capabilities`, independent of the global
    /// `OnlineSettings`. The worker enqueues them into the explicit
    /// queue, drained ahead of the background sweep.
    ExplicitRun {
        track_ids: Vec<TrackId>,
        capabilities: OnlineCapabilities,
    },
    Shutdown,
}

/// Per-track entry queued for processing. Each entry carries its own
/// capability mask so the worker can mix the background sweep
/// (capabilities derived from `OnlineSettings`) and the explicit
/// user-initiated queue (capabilities chosen by the right-click menu
/// item) through the same processing path. `is_explicit` distinguishes
/// the two for live-settings handling and for diagnostic logging.
#[derive(Clone, Copy)]
struct PendingItem {
    track_id: TrackId,
    capabilities: OnlineCapabilities,
    is_explicit: bool,
}

impl PendingItem {
    /// The same request, less the capabilities already attempted.
    const fn without(self, attempted: OnlineCapabilities) -> Self {
        Self {
            capabilities: self.capabilities.without(attempted),
            ..self
        }
    }
}

pub(crate) struct OnlineScheduler {
    sender: Sender<SchedulerCommand>,
    handle: Option<JoinHandle<()>>,
}

impl OnlineScheduler {
    pub(crate) fn start(config: OnlineSchedulerConfig) -> Self {
        let (sender, receiver) = mpsc::channel::<SchedulerCommand>();
        let handle = thread::Builder::new()
            .name("sustain-online-scheduler".to_owned())
            .spawn(move || worker_loop(receiver, config))
            .expect("spawn online scheduler thread");
        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub fn update_settings(&self, settings: OnlineSettings) {
        let _ = self
            .sender
            .send(SchedulerCommand::SettingsChanged(settings));
    }

    pub fn set_library_path(&self, path: Option<PathBuf>) {
        let _ = self.sender.send(SchedulerCommand::LibraryPathChanged(path));
    }

    pub fn wake(&self) {
        let _ = self.sender.send(SchedulerCommand::Wake);
    }

    /// Enqueue a user-initiated batch for online retrieval with the
    /// given capability mask. The batch is processed ahead of the
    /// background sweep; capabilities here are independent of the
    /// global `OnlineSettings`, so the caller can fetch lyrics on a
    /// single playlist while the global lyrics toggle is off.
    pub fn request_explicit_run(&self, track_ids: Vec<TrackId>, capabilities: OnlineCapabilities) {
        if track_ids.is_empty() || capabilities.is_empty() {
            return;
        }
        let _ = self.sender.send(SchedulerCommand::ExplicitRun {
            track_ids,
            capabilities,
        });
    }

    /// Send Shutdown, drop the sender, and join the worker. Blocks
    /// until the worker finishes the in-flight track (if any) and
    /// returns from its loop.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(SchedulerCommand::Shutdown);
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for OnlineScheduler {
    fn drop(&mut self) {
        // Best-effort cleanup if `shutdown` was not called. Mirror the
        // analysis scheduler's discipline: do not join from Drop, since
        // Drop may run on the GTK main thread.
        let _ = self.sender.send(SchedulerCommand::Shutdown);
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
    }
}

fn worker_loop(receiver: mpsc::Receiver<SchedulerCommand>, config: OnlineSchedulerConfig) {
    let OnlineSchedulerConfig {
        remote_service,
        tag_writer,
        library_store,
        progress,
        track_updated,
        clock,
        initial_settings,
        library_path,
        provider_version,
    } = config;

    let mut state = WorkerState::new(initial_settings, library_path);

    'outer: loop {
        match drain_commands(&receiver, &mut state) {
            DrainOutcome::Shutdown => return,
            DrainOutcome::Continue => {}
        }

        if let Some(unrecorded) = state.unrecorded_attempt {
            if let Err(error) = library_store.record_online_attempt(
                unrecorded.track_id,
                unrecorded.attempted,
                unrecorded.context,
            ) {
                (progress)(SchedulerProgress::PersistenceError {
                    detail: format!("{error:?}"),
                });
                match receiver.recv() {
                    Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                    Ok(command) => apply_command(command, &mut state),
                }
                continue;
            }
            state.unrecorded_attempt = None;
            state.settle(
                unrecorded.track_id,
                unrecorded.outcome,
                unrecorded.attempted,
                unrecorded.owed,
            );
        }

        let bg_capabilities = effective_capabilities(&state.settings);
        let has_explicit_work = !state.explicit_queue.is_empty();
        let has_background_work = !bg_capabilities.is_empty();
        if state.library_path.is_none() || (!has_explicit_work && !has_background_work) {
            state.end_run(progress.as_ref());
            match receiver.recv() {
                Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                Ok(command) => apply_command(command, &mut state),
            }
            continue;
        }

        // Build the next batch: explicit queue first, then any
        // remaining slack filled from the background sweep.
        let mut batch: Vec<PendingItem> = Vec::new();
        while batch.len() < BATCH_SIZE {
            match state.explicit_queue.pop_front() {
                Some(item) => batch.push(item),
                None => break,
            }
        }
        if batch.len() < BATCH_SIZE && has_background_work {
            let room = BATCH_SIZE.saturating_sub(batch.len());
            match library_store.tracks_needing_online(bg_capabilities, provider_version, room) {
                Ok(ids) => {
                    // A track the user asked for is already in the batch
                    // with its explicit mask; it is not run twice.
                    let explicit_ids = batch.iter().map(|item| item.track_id).collect::<Vec<_>>();
                    for id in ids.into_iter().filter(|id| !explicit_ids.contains(id)) {
                        batch.push(PendingItem {
                            track_id: id,
                            capabilities: bg_capabilities,
                            is_explicit: false,
                        });
                    }
                }
                Err(_) => {
                    // Store error: block on the command channel so
                    // we do not hot-loop against a broken database.
                    // The explicit items already taken keep their place.
                    requeue_explicit_items(&mut state.explicit_queue, batch.into_iter());
                    match receiver.recv() {
                        Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                        Ok(command) => apply_command(command, &mut state),
                    }
                    continue;
                }
            }
        }

        if batch.is_empty() {
            state.end_run(progress.as_ref());
            match receiver.recv() {
                Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                Ok(command) => apply_command(command, &mut state),
            }
            continue;
        }

        let library_path = match state.library_path.as_ref() {
            Some(path) => path.clone(),
            None => continue,
        };

        // The "remaining" figure in the progress ticks is a UI hint.
        // Count it once per batch instead of re-running the
        // needing-online scan after every processed track; stamped
        // background items are decremented locally and any intra-batch
        // drift is corrected when the next batch counts again.
        let mut remaining_background = library_store
            .tracks_needing_online(
                effective_capabilities(&state.settings),
                provider_version,
                BATCH_SIZE.saturating_mul(64),
            )
            .map(|ids| ids.len() as u32)
            .unwrap_or(0);

        // Library-genre bias list for tag enrichment, resolved at most
        // once per batch (see `attempt_tags`).
        let mut genre_bias: Option<LibraryGenres> = None;

        let mut rate_limited_for = None;
        let mut batch = batch.into_iter();
        while let Some(item) = batch.next() {
            // Re-check between tracks so a toggle in Preferences stops
            // the loop within at most one track's worth of work for
            // background items. Explicit items always keep going —
            // the user explicitly asked for them.
            if let Some(command) = receiver.try_iter().next() {
                if matches!(command, SchedulerCommand::Shutdown) {
                    return;
                }
                apply_command(command, &mut state);
            }
            // The batch runs its explicit items first, so only background
            // items are left behind here; unstamped, they come back with
            // the next sweep.
            if !item.is_explicit && !state.explicit_queue.is_empty() {
                break;
            }

            // Resolve dispatch capabilities: explicit items keep what
            // the user submitted; background items snap to the live
            // settings so a mid-batch toggle takes effect within one
            // track.
            let dispatch_caps = if item.is_explicit {
                item.capabilities
            } else {
                effective_capabilities(&state.settings)
            };
            if dispatch_caps.is_empty() {
                continue;
            }

            let track = match library_store.track(item.track_id) {
                Ok(Some(track)) => track,
                // Removed from the library since it was queued.
                Ok(None) => continue,
                Err(error) => {
                    // The row could not be read: surface it and wait for
                    // a wake rather than sweeping the same track again.
                    // Explicit work keeps its place in the queue.
                    (progress)(SchedulerProgress::PersistenceError {
                        detail: format!("{error:?}"),
                    });
                    requeue_explicit_items(
                        &mut state.explicit_queue,
                        std::iter::once(item).chain(batch.by_ref()),
                    );
                    match receiver.recv() {
                        Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                        Ok(command) => apply_command(command, &mut state),
                    }
                    continue 'outer;
                }
            };
            let absolute_path = track.location.absolute_path(&library_path);
            let dispatch_settings = OnlineSettings {
                artwork: dispatch_caps.artwork,
                tags: dispatch_caps.tags,
                lyrics: dispatch_caps.lyrics,
            };
            let report = process_track(
                &track,
                &absolute_path,
                &dispatch_settings,
                remote_service.as_ref(),
                &tag_writer,
                library_store.as_ref(),
                &mut genre_bias,
            );

            if report.changed_track
                && let Some(notify) = track_updated.as_deref()
            {
                notify(item.track_id);
            }

            // Only stamp capabilities that actually completed — a
            // rate-limited attempt did not get to talk to the server,
            // so leaving it un-stamped means a later batch picks it
            // up again, after the cool-down.
            if !report.attempted.is_empty() {
                let context = OnlineContext {
                    provider_version,
                    now_unix: (clock)(),
                };
                if let Err(error) =
                    library_store.record_online_attempt(item.track_id, report.attempted, context)
                {
                    // The attempt stamp was rejected by SQLite. Without
                    // it this track stays eligible in
                    // `tracks_needing_online` and would be re-fetched
                    // over the network every batch. Surface the failure
                    // and wait for an explicit wake rather than
                    // hot-looping — the same bounded response the
                    // store-query error path above uses. The stamp is
                    // retried first after the wake, so the provider is
                    // not asked again, and the rest of the batch's
                    // explicit work keeps its place in the queue. The
                    // track is not counted as completed/failed; the
                    // PersistenceError is the authoritative signal.
                    (progress)(SchedulerProgress::PersistenceError {
                        detail: format!("{error:?}"),
                    });
                    state.unrecorded_attempt = Some(UnrecordedAttempt {
                        track_id: item.track_id,
                        attempted: report.attempted,
                        context,
                        outcome: report.outcome,
                        owed: report.owed,
                    });
                    // A rate-limited track is itself unfinished: the
                    // capabilities the limit cut off are still owed.
                    let unfinished =
                        (!report.is_finished()).then(|| item.without(report.attempted));
                    requeue_explicit_items(
                        &mut state.explicit_queue,
                        unfinished.into_iter().chain(batch.by_ref()),
                    );
                    match receiver.recv() {
                        Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                        Ok(command) => apply_command(command, &mut state),
                    }
                    continue 'outer;
                }
                // The stamp just removed a background item from the
                // needing-online set the batch-start count measured.
                // Explicit items may or may not have been in that set,
                // so they leave the estimate untouched.
                if !item.is_explicit {
                    remaining_background = remaining_background.saturating_sub(1);
                }
            }

            state.settle(item.track_id, report.outcome, report.attempted, report.owed);
            if let Some(cool_down) = report.rate_limited_for {
                // Stop the batch entirely on a rate-limit signal: the
                // HTTP client refuses every request to the held host
                // until the cool-down has passed, so the remaining
                // tracks would only be refused too. The explicit items
                // left, this one first, go back where they came from.
                requeue_explicit_items(
                    &mut state.explicit_queue,
                    std::iter::once(item.without(report.attempted)).chain(batch),
                );
                rate_limited_for = Some(cool_down);
                break;
            }

            let explicit_in_batch = batch.as_slice().iter().filter(|item| item.is_explicit);
            let remaining = remaining_background
                .saturating_add(state.explicit_queue.len() as u32)
                .saturating_add(explicit_in_batch.count() as u32);
            (progress)(SchedulerProgress::Tick {
                completed: state.passes_checked,
                failed: state.passes_failed,
                remaining,
            });

            thread::sleep(INTER_TRACK_PAUSE);
        }

        if let Some(cool_down) = rate_limited_for {
            // Wait the cool-down out where commands are still heard. A
            // command that arrives first is applied and the loop goes
            // round: a request to the held host is then refused with
            // what is left of the cool-down, and the wait resumes.
            match receiver.recv_timeout(cool_down) {
                Ok(SchedulerCommand::Shutdown) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return;
                }
                Ok(command) => apply_command(command, &mut state),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
    }
}

/// Per-track output of [`process_track`]. A rate limit stops the track's
/// capabilities where it hits: the ones before it completed and are
/// stamped and reported, and the rest are owed.
struct ProcessReport {
    /// What the completed capabilities came to; `None` when the rate
    /// limit cut off the first of them.
    outcome: Option<TrackOutcome>,
    /// The capabilities that completed, stamped into
    /// `track_online_status`. A rate-limited one is left out so the track
    /// stays eligible after the cool-down.
    attempted: OnlineCapabilities,
    /// The cool-down a provider asked for (HTTP 429/503); the HTTP client
    /// holds the host until then, so the worker also stops its batch.
    rate_limited_for: Option<Duration>,
    /// The capabilities dispatched that the rate limit cut off.
    owed: OnlineCapabilities,
    /// At least one capability committed new data for the track, whatever
    /// the overall outcome: a later capability can still fail or be rate
    /// limited.
    changed_track: bool,
}

impl ProcessReport {
    const fn new() -> Self {
        Self {
            outcome: None,
            attempted: OnlineCapabilities::none(),
            rate_limited_for: None,
            owed: OnlineCapabilities::none(),
            changed_track: false,
        }
    }

    /// Records one capability's attempt, returning whether it completed
    /// and is to be stamped.
    fn record(&mut self, attempt: AttemptOutcome) -> bool {
        let outcome = match attempt {
            AttemptOutcome::Succeeded => {
                self.changed_track = true;
                TrackOutcome::Updated
            }
            AttemptOutcome::NoMatch => TrackOutcome::NotFound,
            AttemptOutcome::AlreadyComplete => TrackOutcome::AlreadyComplete,
            // What a capability committed before failing stays in the
            // library: the track was updated.
            AttemptOutcome::Failed {
                changed_track: true,
            } => {
                self.changed_track = true;
                TrackOutcome::Updated
            }
            AttemptOutcome::Failed {
                changed_track: false,
            } => TrackOutcome::Failed,
            AttemptOutcome::RateLimited(cool_down) => {
                self.rate_limited_for = Some(cool_down);
                return false;
            }
        };
        self.outcome = self.outcome.max(Some(outcome));
        true
    }

    /// Whether every capability dispatched ran to completion.
    const fn is_finished(&self) -> bool {
        self.rate_limited_for.is_none()
    }
}

fn process_track(
    track: &Track,
    absolute_path: &Path,
    settings: &OnlineSettings,
    remote_service: &dyn RemoteMetadataService,
    tag_writer: &MetadataWriteHandle,
    library_store: &dyn LibraryStore,
    genre_bias: &mut Option<LibraryGenres>,
) -> ProcessReport {
    let query = query_from_metadata(&track.metadata);
    let mut report = ProcessReport::new();

    // Tag enrichment runs first because the matched MusicBrainz
    // recording lets the subsequent artwork attempt walk releases
    // directly instead of re-identifying. We keep our own
    // `Option<TrackMatch>` so the matched result is reused, not
    // refetched.
    let mut cached_match: Option<TrackMatch> = None;

    if settings.tags {
        report.attempted.tags = report.record(attempt_tags(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            library_store,
            &mut cached_match,
            genre_bias,
        ));
    }

    if settings.artwork && report.is_finished() {
        report.attempted.artwork = report.record(attempt_artwork(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            cached_match.as_ref(),
        ));
    }

    if settings.lyrics && report.is_finished() {
        report.attempted.lyrics = report.record(attempt_lyrics(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            library_store,
        ));
    }

    if !report.is_finished() {
        let dispatched = OnlineCapabilities {
            artwork: settings.artwork,
            tags: settings.tags,
            lyrics: settings.lyrics,
        };
        report.owed = dispatched.without(report.attempted);
    }
    report
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Succeeded,
    NoMatch,
    /// The track already held everything the capability fills, so no
    /// provider was asked.
    AlreadyComplete,
    /// The capability failed. A failure partway through can still have
    /// committed some of its data, which the library view must learn of.
    Failed {
        changed_track: bool,
    },
    RateLimited(Duration),
}

/// Convert a remote-side error into the right attempt outcome.
/// `RateLimited` is handled distinctly so the scheduler can stop the
/// batch; every other error is a generic failure.
fn attempt_outcome_for_remote_error(error: &RemoteError) -> AttemptOutcome {
    match error {
        RemoteError::RateLimited { cool_down } => AttemptOutcome::RateLimited(*cool_down),
        _ => AttemptOutcome::Failed {
            changed_track: false,
        },
    }
}

fn attempt_artwork(
    track: &Track,
    absolute_path: &Path,
    query: &TrackQuery,
    remote_service: &dyn RemoteMetadataService,
    tag_writer: &MetadataWriteHandle,
    cached_match: Option<&TrackMatch>,
) -> AttemptOutcome {
    // Missing-only guard, enforced per-track so it holds for *both*
    // work sources. The background sweep's `tracks_needing_online`
    // query already excludes embedded-artwork rows, but the manual
    // (force) retrieval path deliberately bypasses that query — so the
    // guard must live here too, or a manual "Retrieve → Artwork" on a
    // track that already carries a cover would overwrite it. We trust
    // the scanner-maintained `has_embedded_artwork` flag rather than
    // re-probing the file at attempt time; `None` (never scanned) is
    // treated as "no embedded art" so the track stays eligible,
    // matching the SQL's `COALESCE(has_embedded_artwork, 0) = 0`.
    if track.has_embedded_artwork == Some(true) {
        return AttemptOutcome::AlreadyComplete;
    }

    let fetched: Option<FetchedArtwork> = match remote_service.fetch_artwork(query, cached_match) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Sustain: artwork fetch failed: {error}");
            return attempt_outcome_for_remote_error(&error);
        }
    };
    let Some(artwork) = fetched else {
        return AttemptOutcome::NoMatch;
    };
    if let Err(error) = validate_encoded_artwork(&artwork.bytes) {
        eprintln!(
            "Sustain: artwork fetch returned rejected bytes for {}: {error}",
            absolute_path.display()
        );
        return AttemptOutcome::Failed {
            changed_track: false,
        };
    }
    match tag_writer.enqueue_retrieved_artwork(track.id, &artwork.bytes) {
        Ok(true) => AttemptOutcome::Succeeded,
        // The user chose artwork while the fetch was in flight.
        Ok(false) => AttemptOutcome::NoMatch,
        Err(error) => {
            eprintln!(
                "Sustain: artwork mirror enqueue failed for {}: {error:?}",
                absolute_path.display()
            );
            AttemptOutcome::Failed {
                changed_track: false,
            }
        }
    }
}

fn attempt_lyrics(
    track: &Track,
    absolute_path: &Path,
    query: &TrackQuery,
    remote_service: &dyn RemoteMetadataService,
    tag_writer: &MetadataWriteHandle,
    library_store: &dyn LibraryStore,
) -> AttemptOutcome {
    let has_plain = track
        .metadata
        .lyrics
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty());
    // An unreadable synced-lyrics row is not an absent one: guessing absence
    // would contact the provider for a track that may already be complete.
    let has_synced = match library_store.load_synced_lyrics(track.id) {
        Ok(stored) => stored.is_some(),
        Err(error) => {
            eprintln!(
                "Sustain: reading synced lyrics failed for {}: {error:?}",
                absolute_path.display()
            );
            return AttemptOutcome::Failed {
                changed_track: false,
            };
        }
    };

    if has_plain && has_synced {
        return AttemptOutcome::AlreadyComplete;
    }

    let fetched: Option<FetchedLyrics> = match remote_service.fetch_lyrics(query) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("Sustain: lyrics fetch failed: {error}");
            return attempt_outcome_for_remote_error(&error);
        }
    };
    let Some(fetched) = fetched else {
        return AttemptOutcome::NoMatch;
    };

    let mut wrote_anything = false;
    if !has_plain
        && let Some(plain) = fetched.plain
        && !plain.trim().is_empty()
    {
        let change = MetadataChange {
            lyrics: FieldChange::Set(plain.clone()),
            ..MetadataChange::default()
        };
        // Commit SQLite authority and durable mirror intent together. A user
        // edit that landed while the fetch was in flight wins the missing-only
        // fill; in that case there is nothing new to mirror.
        match tag_writer.fill_missing_metadata(track.id, &change) {
            Ok(changed) => wrote_anything |= changed,
            Err(error) => {
                eprintln!(
                    "Sustain: persist of lyrics column failed for {}: {error:?}",
                    absolute_path.display()
                );
                return AttemptOutcome::Failed {
                    changed_track: false,
                };
            }
        }
    }

    if !has_synced
        && let Some(synced_lrc) = fetched.synced_lrc.as_deref()
        && let Some(parsed) = SyncedLyrics::parse_lrc(synced_lrc)
    {
        if let Err(error) = library_store.record_synced_lyrics(track.id, &parsed, LRCLIB_SOURCE_TAG)
        {
            eprintln!("Sustain: synced lyrics persist failed: {error:?}");
            return AttemptOutcome::Failed {
                changed_track: wrote_anything,
            };
        }
        wrote_anything = true;
    }

    if wrote_anything {
        AttemptOutcome::Succeeded
    } else {
        AttemptOutcome::NoMatch
    }
}

struct WorkerState {
    settings: OnlineSettings,
    library_path: Option<PathBuf>,
    /// The tracks this run has looked up, each counted once in `tally`
    /// by everything its passes came to, however many it took: a manual
    /// request and a background sweep can both reach one track, and a
    /// rate limit can cut a pass short.
    tracks: HashMap<TrackId, TrackRecord>,
    tally: OnlineRunTally,
    /// Passes that ran every capability they dispatched, and how many of
    /// them failed: the live progress, which only grows, where a track's
    /// place in `tally` moves as its passes add up.
    passes_checked: u32,
    passes_failed: u32,
    /// User-initiated work, drained ahead of the background sweep.
    /// Items here keep their own capability mask, independent of the
    /// global `OnlineSettings`.
    explicit_queue: VecDeque<PendingItem>,
    /// A completed attempt whose stamp the store rejected, retried before
    /// any new work so the provider is not asked again.
    unrecorded_attempt: Option<UnrecordedAttempt>,
}

impl WorkerState {
    fn new(settings: OnlineSettings, library_path: Option<PathBuf>) -> Self {
        Self {
            settings,
            library_path,
            tracks: HashMap::new(),
            tally: OnlineRunTally::default(),
            passes_checked: 0,
            passes_failed: 0,
            explicit_queue: VecDeque::new(),
            unrecorded_attempt: None,
        }
    }

    /// Reports the run's totals and starts the next run's from zero.
    fn end_run(&mut self, progress: &(dyn Fn(SchedulerProgress) + Send + Sync)) {
        self.tracks.clear();
        self.passes_checked = 0;
        self.passes_failed = 0;
        progress(SchedulerProgress::Idle(std::mem::take(&mut self.tally)));
    }

    /// Takes a track's stamped pass into account, moving the track in the
    /// tally to what all its passes have come to. `attempted` are the
    /// capabilities the pass completed and `owed` those a rate limit cut
    /// off.
    fn settle(
        &mut self,
        track_id: TrackId,
        outcome: Option<TrackOutcome>,
        attempted: OnlineCapabilities,
        owed: OnlineCapabilities,
    ) {
        if owed.is_empty() {
            match outcome {
                Some(TrackOutcome::Failed) => {
                    self.passes_failed = self.passes_failed.saturating_add(1);
                }
                Some(_) => self.passes_checked = self.passes_checked.saturating_add(1),
                None => {}
            }
        }
        let record = self.tracks.entry(track_id).or_default();
        if let Some(previous) = record.reported() {
            let counter = self.tally.counter(previous);
            *counter = counter.saturating_sub(1);
        }
        record.outcome = record.outcome.max(outcome);
        record.owed = record.owed.without(attempted).union(owed);
        if let Some(reported) = record.reported() {
            let counter = self.tally.counter(reported);
            *counter = counter.saturating_add(1);
        }
    }
}

/// A track the provider answered for whose attempt stamp the store
/// rejected. Its outcome is taken into account once the stamp is
/// recorded.
#[derive(Clone, Copy, Debug)]
struct UnrecordedAttempt {
    track_id: TrackId,
    attempted: OnlineCapabilities,
    /// When the provider was asked, not when the stamp finally lands.
    context: OnlineContext,
    outcome: Option<TrackOutcome>,
    /// The capabilities dispatched that a rate limit cut off.
    owed: OnlineCapabilities,
}

enum DrainOutcome {
    Continue,
    Shutdown,
}

fn drain_commands(
    receiver: &mpsc::Receiver<SchedulerCommand>,
    state: &mut WorkerState,
) -> DrainOutcome {
    while let Ok(command) = receiver.try_recv() {
        if matches!(command, SchedulerCommand::Shutdown) {
            return DrainOutcome::Shutdown;
        }
        apply_command(command, state);
    }
    DrainOutcome::Continue
}

/// Puts the explicit items of an interrupted batch back at the front of the
/// queue, in their order. A track queued again meanwhile is merged into
/// its returning item; an item with nothing left to attempt is done.
/// Background items are dropped: unstamped, they come back with the next
/// sweep.
fn requeue_explicit_items(
    explicit_queue: &mut VecDeque<PendingItem>,
    items: impl Iterator<Item = PendingItem>,
) {
    let returning = items
        .filter(|item| item.is_explicit && !item.capabilities.is_empty())
        .collect::<Vec<_>>();
    for mut item in returning.into_iter().rev() {
        if let Some(position) = explicit_queue
            .iter()
            .position(|queued| queued.track_id == item.track_id)
            && let Some(queued) = explicit_queue.remove(position)
        {
            item.capabilities = item.capabilities.union(queued.capabilities);
        }
        explicit_queue.push_front(item);
    }
}

/// Queues an explicit request. A track already queued is not queued twice
/// (a double-clicking user does not queue the same playlist twice); it
/// takes on the newly requested capabilities instead, so asking for lyrics
/// after artwork gets both.
fn queue_explicit_request(
    explicit_queue: &mut VecDeque<PendingItem>,
    track_ids: Vec<TrackId>,
    capabilities: OnlineCapabilities,
) {
    let mut positions: std::collections::HashMap<TrackId, usize> = explicit_queue
        .iter()
        .enumerate()
        .map(|(position, item)| (item.track_id, position))
        .collect();
    for track_id in track_ids {
        match positions.get(&track_id) {
            Some(&position) => {
                let queued = &mut explicit_queue[position];
                queued.capabilities = queued.capabilities.union(capabilities);
            }
            None => {
                positions.insert(track_id, explicit_queue.len());
                explicit_queue.push_back(PendingItem {
                    track_id,
                    capabilities,
                    is_explicit: true,
                });
            }
        }
    }
}

fn apply_command(command: SchedulerCommand, state: &mut WorkerState) {
    match command {
        SchedulerCommand::SettingsChanged(settings) => {
            state.settings = settings;
        }
        SchedulerCommand::LibraryPathChanged(path) => {
            state.library_path = path;
        }
        SchedulerCommand::ExplicitRun {
            track_ids,
            capabilities,
        } => {
            if capabilities.is_empty() {
                return;
            }
            queue_explicit_request(&mut state.explicit_queue, track_ids, capabilities);
        }
        SchedulerCommand::Wake | SchedulerCommand::Shutdown => {
            // Shutdown is handled at the caller; Wake has no side
            // effect beyond returning control to the loop top.
        }
    }
}

/// Project the user's `OnlineSettings` into `OnlineCapabilities` for
/// the storage layer. Every capability the scheduler actually
/// attempts must be reflected here so the attempt-stamping side stays
/// in sync with the work side.
fn effective_capabilities(settings: &OnlineSettings) -> OnlineCapabilities {
    OnlineCapabilities {
        artwork: settings.artwork,
        tags: settings.tags,
        lyrics: settings.lyrics,
    }
}

/// Non-destructive tag enrichment: identify the track through
/// MusicBrainz and fill in metadata fields that are currently empty.
/// Never overwrites existing data — per the persistence policy in
/// AGENTS.md, the library wins, and Sustain itself never re-imports
/// from external sources. Successful identifications are cached into
/// `cached_match` so the artwork attempt that runs next does not
/// need to re-identify the same track.
///
/// Field-by-field policy:
///
/// * **title / artist** — recording-level facts; safe to fill from
///   any match.
/// * **year** — taken from the recording's first-release-date, not
///   from any particular release. Compilations and reissues all
///   share the same first-release year, which is what users mean by
///   "year" of a song.
/// * **genre** — picked from the recording's community-voted genre
///   tags with a library-aware bias: if any candidate is already a
///   genre the user has in their library, that candidate wins over a
///   higher-voted one not yet present, and the library's existing
///   spelling is preserved. Otherwise the top-voted candidate wins.
///   This stops the enrichment path from sprawling the library into
///   dozens of near-duplicate genres ("electronica" vs "house" when
///   the library already has "House").
/// * **album** — release-level. Filled from MusicBrainz's first
///   release only when the user has no album value yet (the same
///   recording appears on many releases; "first" is a guess).
/// * **track_number / track_total / disc_number** — release-specific
///   *and* the most damaging to get wrong (same recording can be
///   #3/12 on one album and #1/4 on another). Only filled when the
///   user already has an album value that matches one of the
///   matched releases' titles — otherwise the values are skipped
///   entirely. Leaving them empty is strictly better than writing
///   a guess.
#[allow(clippy::too_many_arguments)]
fn attempt_tags(
    track: &Track,
    absolute_path: &Path,
    query: &TrackQuery,
    remote_service: &dyn RemoteMetadataService,
    tag_writer: &MetadataWriteHandle,
    library_store: &dyn LibraryStore,
    cached_match: &mut Option<TrackMatch>,
    genre_bias: &mut Option<LibraryGenres>,
) -> AttemptOutcome {
    // Gate-shaped check: don't talk to the network if there is
    // nothing we are allowed to fill. The positional fields require
    // an existing album to align against, so they only count toward
    // "we have work to do" when the album is already set.
    let need_title = track.metadata.title.is_none();
    let need_artist = track.metadata.artist.is_none();
    let need_album = track.metadata.album.is_none();
    let need_year = track.metadata.year.is_none();
    let need_genre = track
        .metadata
        .genre
        .as_deref()
        .is_none_or(|value| value.trim().is_empty());
    let need_positional = track.metadata.album.is_some()
        && (track.metadata.track_number.is_none()
            || track.metadata.track_total.is_none()
            || track.metadata.disc_number.is_none());
    if !(need_title || need_artist || need_album || need_year || need_genre || need_positional) {
        return AttemptOutcome::AlreadyComplete;
    }

    let matched = match remote_service.identify_track(query) {
        Ok(Some(value)) => value,
        Ok(None) => return AttemptOutcome::NoMatch,
        Err(error) => {
            eprintln!("Sustain: track identification failed: {error}");
            return attempt_outcome_for_remote_error(&error);
        }
    };

    // The lookup-only fields (year, genre) are absent from a text-search
    // match until enriched; promote the match only when one is actually
    // needed (issue #44). AcoustID matches already carry these fields from
    // identification, so they never need the second lookup.
    let matched =
        if (need_year || need_genre) && matched.source == TrackMatchSource::MusicBrainzTags {
            match remote_service.enrich_match(&matched) {
                Ok(enriched) => enriched,
                Err(error) => {
                    eprintln!("Sustain: track enrichment failed: {error}");
                    return attempt_outcome_for_remote_error(&error);
                }
            }
        } else {
            matched
        };

    let mut change = MetadataChange::default();

    if need_title
        && let Some(value) = matched.title.as_deref()
        && !value.trim().is_empty()
    {
        change.title = FieldChange::Set(value.to_owned());
    }
    if need_artist
        && let Some(value) = matched.artist.as_deref()
        && !value.trim().is_empty()
    {
        change.artist = FieldChange::Set(value.to_owned());
    }
    if need_year && let Some(year) = matched.first_release_year {
        change.year = FieldChange::Set(year);
    }
    if need_genre {
        // Library-aware genre selection. The bias list is resolved at
        // most once per worker batch through the `genre_bias` memo —
        // dozens of genre-needing tracks share one genre_track_counts()
        // query, and a list one batch stale is harmless. A failed query
        // degrades gracefully to "no library bias" for the rest of the
        // batch: the worker still gets to pick a genre based on votes
        // alone, rather than silently skipping the whole track.
        let library_genres = genre_bias.get_or_insert_with(|| {
            LibraryGenres::of(library_store.genre_track_counts().unwrap_or_default())
        });
        if let Some(name) = select_genre(&matched.genres, library_genres) {
            change.genre = FieldChange::Set(name);
        }
    }
    if need_album
        && let Some(release) = matched.releases.first()
        && let Some(title) = release.title.as_deref()
        && !title.trim().is_empty()
    {
        change.album = FieldChange::Set(title.to_owned());
    }
    if need_positional
        && let Some(existing_album) = track.metadata.album.as_deref()
        && let Some(release) = find_release_matching_album(&matched.releases, existing_album)
    {
        if track.metadata.track_number.is_none()
            && let Some(value) = release.track_number
        {
            change.track_number = FieldChange::Set(value);
        }
        if track.metadata.track_total.is_none()
            && let Some(value) = release.track_total
        {
            change.track_total = FieldChange::Set(value);
        }
        if track.metadata.disc_number.is_none()
            && let Some(value) = release.disc_number
        {
            change.disc_number = FieldChange::Set(value);
        }
    }

    *cached_match = Some(matched);

    if matches!(change.title, FieldChange::Unchanged)
        && matches!(change.artist, FieldChange::Unchanged)
        && matches!(change.album, FieldChange::Unchanged)
        && matches!(change.year, FieldChange::Unchanged)
        && matches!(change.genre, FieldChange::Unchanged)
        && matches!(change.track_number, FieldChange::Unchanged)
        && matches!(change.track_total, FieldChange::Unchanged)
        && matches!(change.disc_number, FieldChange::Unchanged)
    {
        // Identification succeeded but every field it could fill was
        // already present (or the match carried no data for it).
        // No write, but still "attempted" — the SQL stamp keeps us
        // from re-trying.
        return AttemptOutcome::NoMatch;
    }

    // Commit SQLite authority and durable mirror intent together. A user may
    // have edited the row while identification was in flight; those values
    // win the missing-only fill and the worker mirrors the resulting latest
    // canonical row.
    match tag_writer.fill_missing_metadata(track.id, &change) {
        Ok(true) => AttemptOutcome::Succeeded,
        Ok(false) => AttemptOutcome::NoMatch,
        Err(error) => {
            eprintln!(
                "Sustain: tag enrichment persist failed for {}: {error:?}",
                absolute_path.display()
            );
            AttemptOutcome::Failed {
                changed_track: false,
            }
        }
    }
}

/// Choose the best single genre to write back, given the recording's
/// candidate list (sorted by community vote count, descending) and
/// the set of genre values the user already has in their library.
///
/// A candidate that already exists in the library wins outright over
/// higher-voted candidates that don't, because the alternative is
/// genre sprawl: silently adding a near-duplicate ("Electronica") to
/// a library that already organizes around "House" means the user's
/// existing genre-based smart playlists stop catching new arrivals.
/// When multiple candidates are in the library, the one with the
/// highest vote count among them wins (the `matched_genres` list is
/// sorted descending, so the first hit wins by iteration order). The
/// library's existing spelling is preserved, whatever the case and
/// accents of the candidate, so one genre keeps one spelling.
///
/// Falls back to the top-voted candidate if none of them are in the
/// library yet — better to seed the library with a community-curated
/// genre than to leave the field blank forever.
fn select_genre(
    matched_genres: &[GenreCandidate],
    library_genres: &LibraryGenres,
) -> Option<String> {
    matched_genres
        .iter()
        .find_map(|candidate| library_genres.spelling_of(&candidate.name))
        .map(str::to_owned)
        .or_else(|| {
            matched_genres
                .first()
                .map(|candidate| candidate.name.clone())
        })
}

/// The genres the library already holds, each under the spelling an
/// online candidate adopts.
///
/// A candidate matches a library genre without regard to case, accents
/// or outer whitespace. Where the library itself spells such a genre
/// several ways ("House", "house"), the candidate adopts the spelling
/// most tracks carry, the first in byte order when several are carried
/// equally, so enrichment converges on one spelling instead of feeding
/// a minority one.
#[derive(Debug, Default)]
struct LibraryGenres {
    spelling_by_normalized: std::collections::HashMap<String, String>,
}

impl LibraryGenres {
    /// `track_counts` pairs each genre tag with the number of tracks
    /// that carry it, as `LibraryStore::genre_track_counts` reports them.
    fn of(track_counts: Vec<(String, u64)>) -> Self {
        // One spelling under different outer whitespace is one spelling,
        // its tracks counted together.
        let mut count_by_spelling = std::collections::HashMap::<&str, u64>::new();
        for (genre, count) in &track_counts {
            *count_by_spelling.entry(genre.trim()).or_insert(0) += count;
        }

        let mut spelling_by_normalized = std::collections::HashMap::<String, (&str, u64)>::new();
        for candidate in count_by_spelling {
            spelling_by_normalized
                .entry(normalize_search_text(candidate.0))
                .and_modify(|best| {
                    let more_used = candidate.1 > best.1;
                    let as_used_and_first = candidate.1 == best.1 && candidate.0 < best.0;
                    if more_used || as_used_and_first {
                        *best = candidate;
                    }
                })
                .or_insert(candidate);
        }

        Self {
            spelling_by_normalized: spelling_by_normalized
                .into_iter()
                .map(|(normalized, (spelling, _))| (normalized, spelling.to_owned()))
                .collect(),
        }
    }

    fn spelling_of(&self, genre: &str) -> Option<&str> {
        self.spelling_by_normalized
            .get(&normalize_search_text(genre))
            .map(String::as_str)
    }
}

/// Find the release in `releases` whose title matches `album`
/// (without regard to case, accents or outer whitespace). Returns `None` when no match
/// exists; callers treat that as "we can't trust the positional
/// fields on any of these releases".
fn find_release_matching_album<'a>(
    releases: &'a [TrackMatchRelease],
    album: &str,
) -> Option<&'a TrackMatchRelease> {
    let needle = normalize_search_text(album);
    if needle.is_empty() {
        return None;
    }
    releases.iter().find(|release| {
        release
            .title
            .as_deref()
            .map(|title| normalize_search_text(title) == needle)
            .unwrap_or(false)
    })
}

#[cfg(test)]
#[path = "online_scheduler_tests.rs"]
mod tests;
