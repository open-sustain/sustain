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
//! capability that hit the limit is left un-stamped (so the next
//! batch picks it up after the HTTP client's per-host cool-down) and
//! the worker stops the current batch instead of running the
//! remaining tracks straight into the same wall.
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
//! remaining slack is filled from the background query.

use std::{
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::{FieldChange, MetadataChange, OnlineSettings, SyncedLyrics, Track, TrackId};
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
    Idle {
        completed: u32,
        failed: u32,
    },
    /// The attempt stamp (`record_online_attempt`) was rejected by
    /// SQLite. Without it the track stays eligible in
    /// `tracks_needing_online` and would be re-fetched over the network
    /// every batch. The worker stops and waits for an explicit wake; this
    /// event surfaces the failure instead of reporting a clean pass that
    /// will silently repeat. `detail` carries the store error.
    PersistenceError {
        detail: String,
    },
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
    /// "Look for new work" — the library has grown or the user
    /// manually requested a re-run.
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

    let mut state = WorkerState {
        settings: initial_settings,
        library_path,
        completed: 0,
        failed: 0,
        explicit_queue: VecDeque::new(),
    };

    'outer: loop {
        match drain_commands(&receiver, &mut state) {
            DrainOutcome::Shutdown => return,
            DrainOutcome::Continue => {}
        }

        let bg_capabilities = effective_capabilities(&state.settings);
        let has_explicit_work = !state.explicit_queue.is_empty();
        let has_background_work = !bg_capabilities.is_empty();
        if state.library_path.is_none() || (!has_explicit_work && !has_background_work) {
            (progress)(SchedulerProgress::Idle {
                completed: state.completed,
                failed: state.failed,
            });
            state.completed = 0;
            state.failed = 0;
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
                    for id in ids {
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
                    match receiver.recv() {
                        Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                        Ok(command) => apply_command(command, &mut state),
                    }
                    continue;
                }
            }
        }

        if batch.is_empty() {
            (progress)(SchedulerProgress::Idle {
                completed: state.completed,
                failed: state.failed,
            });
            state.completed = 0;
            state.failed = 0;
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

        for item in batch {
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

            let Ok(Some(track)) = library_store.track(item.track_id) else {
                continue;
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
            );

            if matches!(report.outcome, ProcessOutcome::Succeeded)
                && let Some(notify) = track_updated.as_deref()
            {
                notify(item.track_id);
            }

            // Only stamp capabilities that actually completed — a
            // rate-limited attempt did not get to talk to the server,
            // so leaving it un-stamped means the next batch picks it
            // up again (after the HTTP client's per-host cool-down).
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
                    // store-query error path above uses. The track is
                    // not counted as completed/failed; the
                    // PersistenceError is the authoritative signal.
                    (progress)(SchedulerProgress::PersistenceError {
                        detail: format!("{error:?}"),
                    });
                    match receiver.recv() {
                        Ok(SchedulerCommand::Shutdown) | Err(_) => return,
                        Ok(command) => apply_command(command, &mut state),
                    }
                    continue 'outer;
                }
            }

            match report.outcome {
                ProcessOutcome::Succeeded | ProcessOutcome::NoMatch => {
                    state.completed = state.completed.saturating_add(1);
                }
                ProcessOutcome::Failed | ProcessOutcome::RateLimited => {
                    state.failed = state.failed.saturating_add(1);
                }
            }

            let remaining = library_store
                .tracks_needing_online(
                    effective_capabilities(&state.settings),
                    provider_version,
                    BATCH_SIZE.saturating_mul(64),
                )
                .map(|ids| ids.len() as u32)
                .unwrap_or(0)
                .saturating_add(state.explicit_queue.len() as u32);
            (progress)(SchedulerProgress::Tick {
                completed: state.completed,
                failed: state.failed,
                remaining,
            });

            if matches!(report.outcome, ProcessOutcome::RateLimited) {
                // Stop the batch entirely on a rate-limit signal. The
                // HTTP client has already pushed the host's cool-down
                // forward, so even if we kept iterating we'd just sit
                // in `respect_rate_limit` for the same duration; this
                // way the worker drops back to the outer recv() and
                // resumes on the next nudge (library scan, settings
                // change, manual wake) without the cool-down also
                // blocking unrelated work.
                break;
            }

            thread::sleep(INTER_TRACK_PAUSE);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessOutcome {
    /// Provider returned data and the persist path succeeded for at
    /// least one capability requested.
    Succeeded,
    /// Every requested capability ran to completion and produced no
    /// new data. Still counted as a successful pass — the attempt
    /// timestamps for the capabilities we tried are stamped.
    NoMatch,
    /// A network or provider error occurred for at least one
    /// capability. Counted as failed for the UI summary; the
    /// capabilities that *did* complete are still stamped, the
    /// failing ones are stamped as well so we do not hammer a
    /// misbehaving provider every cycle.
    Failed,
    /// The server explicitly asked us to back off (HTTP 429/503).
    /// The HTTP client has already pushed the host's cool-down
    /// forward. The capabilities that hit the rate limit are *not*
    /// reported as attempted so the track stays eligible on the
    /// next pass; the worker also stops the current batch.
    RateLimited,
}

/// Per-track output of [`process_track`]: the overall outcome (used
/// for accounting and for the batch-break decision) plus the exact
/// set of capabilities that actually completed (used to decide what
/// to stamp into `track_online_status`). Anything that was rate-
/// limited is intentionally absent from `attempted` so the track
/// remains eligible for the next batch.
struct ProcessReport {
    outcome: ProcessOutcome,
    attempted: OnlineCapabilities,
}

fn process_track(
    track: &Track,
    absolute_path: &Path,
    settings: &OnlineSettings,
    remote_service: &dyn RemoteMetadataService,
    tag_writer: &MetadataWriteHandle,
    library_store: &dyn LibraryStore,
) -> ProcessReport {
    let query = query_from_metadata(&track.metadata);
    let mut any_success = false;
    let mut any_failure = false;
    let mut any_rate_limited = false;
    let mut attempted = OnlineCapabilities::none();

    // Tag enrichment runs first because the matched MusicBrainz
    // recording lets the subsequent artwork attempt walk releases
    // directly instead of re-identifying. We keep our own
    // `Option<TrackMatch>` so the matched result is reused, not
    // refetched.
    let mut cached_match: Option<TrackMatch> = None;

    if settings.tags {
        match attempt_tags(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            library_store,
            &mut cached_match,
        ) {
            AttemptOutcome::Succeeded => {
                any_success = true;
                attempted.tags = true;
            }
            AttemptOutcome::NoMatch => {
                attempted.tags = true;
            }
            AttemptOutcome::Failed => {
                any_failure = true;
                attempted.tags = true;
            }
            AttemptOutcome::RateLimited => {
                any_rate_limited = true;
            }
        }
    }

    if settings.artwork && !any_rate_limited {
        match attempt_artwork(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            cached_match.as_ref(),
        ) {
            AttemptOutcome::Succeeded => {
                any_success = true;
                attempted.artwork = true;
            }
            AttemptOutcome::NoMatch => {
                attempted.artwork = true;
            }
            AttemptOutcome::Failed => {
                any_failure = true;
                attempted.artwork = true;
            }
            AttemptOutcome::RateLimited => {
                any_rate_limited = true;
            }
        }
    }

    if settings.lyrics && !any_rate_limited {
        match attempt_lyrics(
            track,
            absolute_path,
            &query,
            remote_service,
            tag_writer,
            library_store,
        ) {
            AttemptOutcome::Succeeded => {
                any_success = true;
                attempted.lyrics = true;
            }
            AttemptOutcome::NoMatch => {
                attempted.lyrics = true;
            }
            AttemptOutcome::Failed => {
                any_failure = true;
                attempted.lyrics = true;
            }
            AttemptOutcome::RateLimited => {
                any_rate_limited = true;
            }
        }
    }

    let outcome = if any_rate_limited {
        ProcessOutcome::RateLimited
    } else if any_success {
        ProcessOutcome::Succeeded
    } else if any_failure {
        ProcessOutcome::Failed
    } else {
        ProcessOutcome::NoMatch
    };
    ProcessReport { outcome, attempted }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AttemptOutcome {
    Succeeded,
    NoMatch,
    Failed,
    RateLimited,
}

/// Convert a remote-side error into the right attempt outcome.
/// `RateLimited` is handled distinctly so the scheduler can stop the
/// batch; every other error is a generic failure.
fn attempt_outcome_for_remote_error(error: &RemoteError) -> AttemptOutcome {
    if matches!(error, RemoteError::RateLimited { .. }) {
        AttemptOutcome::RateLimited
    } else {
        AttemptOutcome::Failed
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
        return AttemptOutcome::NoMatch;
    }

    let fetched: Option<FetchedArtwork> = match cached_match {
        Some(track_match) => match remote_service.fetch_artwork_for_match(track_match) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("Sustain: artwork fetch failed: {error}");
                return attempt_outcome_for_remote_error(&error);
            }
        },
        None => match remote_service.fetch_artwork(query) {
            Ok(value) => value,
            Err(error) => {
                eprintln!("Sustain: artwork fetch failed: {error}");
                return attempt_outcome_for_remote_error(&error);
            }
        },
    };
    let Some(artwork) = fetched else {
        return AttemptOutcome::NoMatch;
    };
    if let Err(error) = validate_encoded_artwork(&artwork.bytes) {
        eprintln!(
            "Sustain: artwork fetch returned rejected bytes for {}: {error}",
            absolute_path.display()
        );
        return AttemptOutcome::Failed;
    }
    if !tag_writer.enqueue_artwork(track.id, Some(artwork.bytes)) {
        eprintln!(
            "Sustain: artwork mirror enqueue failed for {}",
            absolute_path.display()
        );
        return AttemptOutcome::Failed;
    }
    AttemptOutcome::Succeeded
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
    let has_synced = library_store
        .load_synced_lyrics(track.id)
        .map(|stored| stored.is_some())
        .unwrap_or(false);

    if has_plain && has_synced {
        return AttemptOutcome::NoMatch;
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
                return AttemptOutcome::Failed;
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
            return AttemptOutcome::Failed;
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
    completed: u32,
    failed: u32,
    /// User-initiated work, drained ahead of the background sweep.
    /// Items here keep their own capability mask, independent of the
    /// global `OnlineSettings`.
    explicit_queue: VecDeque<PendingItem>,
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
            // Dedup against the existing queue so a double-clicking
            // user does not queue the same playlist twice.
            let already_queued: std::collections::HashSet<TrackId> = state
                .explicit_queue
                .iter()
                .map(|item| item.track_id)
                .collect();
            for track_id in track_ids {
                if already_queued.contains(&track_id) {
                    continue;
                }
                state.explicit_queue.push_back(PendingItem {
                    track_id,
                    capabilities,
                    is_explicit: true,
                });
            }
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
        return AttemptOutcome::NoMatch;
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
        // Library-aware genre selection. A failed distinct_genres()
        // query degrades gracefully to "no library bias": the worker
        // still gets to pick a genre based on votes alone, rather
        // than silently skipping the whole track.
        let library_genres = library_store.distinct_genres().unwrap_or_default();
        if let Some(name) = select_genre(&matched.genres, &library_genres) {
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
            AttemptOutcome::Failed
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
/// library's existing spelling is preserved so casing stays
/// consistent across the library.
///
/// Falls back to the top-voted candidate if none of them are in the
/// library yet — better to seed the library with a community-curated
/// genre than to leave the field blank forever.
fn select_genre(matched_genres: &[GenreCandidate], library_genres: &[String]) -> Option<String> {
    let library_by_normalized: std::collections::HashMap<String, &String> = library_genres
        .iter()
        .map(|name| (normalize_genre(name), name))
        .collect();
    for candidate in matched_genres {
        if let Some(library_spelling) = library_by_normalized.get(&normalize_genre(&candidate.name))
        {
            return Some((*library_spelling).clone());
        }
    }
    matched_genres
        .first()
        .map(|candidate| candidate.name.clone())
}

fn normalize_genre(value: &str) -> String {
    value.trim().to_lowercase()
}

/// Find the release in `releases` whose title matches `album`
/// (case- and whitespace-normalized). Returns `None` when no match
/// exists; callers treat that as "we can't trust the positional
/// fields on any of these releases".
fn find_release_matching_album<'a>(
    releases: &'a [TrackMatchRelease],
    album: &str,
) -> Option<&'a TrackMatchRelease> {
    let needle = normalize_album(album);
    if needle.is_empty() {
        return None;
    }
    releases.iter().find(|release| {
        release
            .title
            .as_deref()
            .map(|title| normalize_album(title) == needle)
            .unwrap_or(false)
    })
}

fn normalize_album(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
#[path = "online_scheduler_tests.rs"]
mod tests;
