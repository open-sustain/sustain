// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Paced multi-worker driver for `sustain_analysis::analyze`.
//!
//! Architecture: one supervisor thread and N worker threads. The
//! supervisor owns the command channel, the shared work queue, and
//! the lifecycle of the worker pool. Workers each apply nice +
//! ionice priorities to themselves on entry, pull `WorkItem`s off
//! the shared queue, run the DSP, write results back through the
//! library store, and report progress back to the supervisor.
//!
//! ```text
//!   ApplicationRuntime -- SchedulerCommand --> [supervisor]
//!                                                |
//!                                                | WorkItem  (mpmc)
//!                                                v
//!                                            [worker 0] [worker 1] ... [worker N-1]
//!                                                |
//!                                                | WorkOutcome (mpsc)
//!                                                v
//!                                            [supervisor]
//!                                                |
//!                                                | SchedulerProgress
//!                                                v
//!                                            ProgressSink
//! ```
//!
//! The worker count is read from
//! [`crate::priority::resolve_worker_count`] for the current
//! [`BackgroundResourceUsage`] preset. A user changing the
//! Preferences slider while the scheduler is running triggers a
//! teardown-and-respawn of the worker pool with the new count + new
//! priorities — the work queue between supervisor and workers is
//! drained first so no track is processed twice with stale settings.
//!
//! `INTER_TRACK_PAUSE` is intentionally short (25 ms). With every
//! worker running under +10 / IO best-effort-7 (`Balanced`) or +19 /
//! IO idle (`Innocuous`), the kernel scheduler is the real
//! throttling mechanism; the pause only exists to keep the worker
//! loop from spinning when the store has nothing to dispense.
//!
//! ## Work sources
//!
//! The supervisor multiplexes two sources of work:
//!
//! 1. **Background sweep** — driven by `tracks_needing_analysis` with
//!    capabilities derived from the global `AnalysisSettings`. Empty
//!    when every flag is off.
//! 2. **Explicit queue** — populated by
//!    `AnalysisScheduler::request_explicit_run` for per-playlist
//!    user-initiated runs (the right-click menu items). Each entry
//!    carries its own capability mask, independent of the global
//!    settings, so a user can ask for audio analysis on a single
//!    playlist while keeping audio analysis globally off.
//!
//! The explicit queue is drained first on every refill, then any
//! remaining buffer slack is filled from the background query. Items
//! that overlap (same track present in both queues with different
//! capabilities) are not merged — they dispatch as two separate
//! passes against the file, which is wasteful but correct.

use std::{
    collections::{HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sustain_analysis::{AnalysisError, AnalysisOptions, TrackAnalysis};
use sustain_domain::{AnalysisSettings, BackgroundResourceUsage, TrackId};
use sustain_library_store::{AnalysisCapabilities, AnalysisContext, LibraryStore};

use crate::priority::{self, IoPriorityClass, NiceLevel};

/// How long each worker sleeps between two consecutive analyses.
/// Short — the heavy lifting (CPU/IO throttling) happens via the
/// nice and ionice priorities the worker applies to itself; the
/// pause only exists to keep the loop from yelling at the store
/// every microsecond when there is nothing to do.
const INTER_TRACK_PAUSE: Duration = Duration::from_millis(25);

/// How many tracks to fetch from the store per
/// `tracks_needing_analysis` query. Small enough that capability
/// changes propagate within a few tracks, large enough that we are
/// not querying the database between every track even on a fast
/// pool of workers.
const BATCH_SIZE: usize = 16;

/// The DSP function the worker calls per track. Production composes a
/// [`sustain_analysis::Analyzer`] and only calls the band methods for
/// capabilities that are currently active, so a track scheduled with
/// `bpm: true, key: false, audio: false` does **not** pay for the
/// chroma extraction or full-track decode. The `Option<Duration>` is the
/// library's stored track length, passed so the analyzer can centre its
/// analysis windows and classify the track as normal vs. long without a
/// preliminary probe. Tests substitute a closure that returns canned
/// `TrackAnalysis` values and may ignore the capability mask and
/// duration.
pub type AnalyzerFn = Arc<
    dyn Fn(
            &Path,
            AnalysisCapabilities,
            AnalysisOptions,
            Option<Duration>,
        ) -> Result<TrackAnalysis, AnalysisError>
        + Send
        + Sync,
>;

/// Sink for progress updates emitted by the supervisor thread. The
/// runtime wraps this in an `async_channel` send so notifications
/// surface on the GTK main loop without the supervisor touching
/// widgets directly.
pub type ProgressSink = Arc<dyn Fn(SchedulerProgress) + Send + Sync>;

/// Sink invoked once per track after a successful `record_analysis`
/// landed BPM/key/audio data into the store. The runtime wraps
/// this in an `async_channel` send so the UI shell refreshes the
/// matching row on the main loop. Stays a no-op when no sink is
/// installed (tests, headless deployments).
pub type TrackUpdatedSink = Arc<dyn Fn(TrackId) + Send + Sync>;

/// Source for the current wall-clock unix timestamp recorded into the
/// `track_analysis.*_attempted_at_unix` columns. Injected so tests can
/// run with a deterministic clock; production passes a closure backed
/// by `SystemTime::now()`.
pub type UnixClockFn = Arc<dyn Fn() -> i64 + Send + Sync>;

/// Per-track progress signal. The widget aggregates these into the
/// notification text without caring about the underlying ids.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SchedulerProgress {
    /// At least one track has been processed since the last summary.
    /// `completed` and `failed` are batch-running totals; `total` is a
    /// snapshot taken once when the run starts so the notification denominator
    /// does not slide as the scheduler refills its streaming queue.
    Tick {
        completed: u32,
        failed: u32,
        total: u32,
    },
    /// The pool has caught up: either the queue is empty or every
    /// requested capability is currently disabled. UI side: dismiss
    /// the persistent "analysing" notification, emit an ephemeral
    /// summary if `completed + failed > 0`.
    Idle { completed: u32, failed: u32 },
    /// A durable store write was rejected by SQLite — either the
    /// analysis result or the failed-attempt stamp. The completed DSP
    /// work was *not* persisted, so the track would resurface in
    /// `tracks_needing_analysis` and be re-analysed forever. The
    /// supervisor stops the sweep and waits for an explicit wake; this
    /// event lets the UI surface the failure instead of silently
    /// reporting progress that never landed. `detail` carries the store
    /// error for diagnosis.
    PersistenceError { detail: String },
}

/// Bundle of dependencies the scheduler captures at start-up. Grouped
/// so the supervisor signature stays manageable and so tests can
/// build a config struct once and reuse it across cases.
pub struct AnalysisSchedulerConfig {
    pub analyzer: AnalyzerFn,
    pub progress: ProgressSink,
    /// Optional refresh signal: after every successful analysis pass
    /// the worker pushes the touched `TrackId` so the runtime can
    /// reload that row from the store into its in-memory copy. `None`
    /// when the embedder does not care about live UI refreshes.
    pub track_updated: Option<TrackUpdatedSink>,
    pub clock: UnixClockFn,
    pub library_store: Arc<dyn LibraryStore>,
    pub initial_settings: AnalysisSettings,
    pub initial_resource_usage: BackgroundResourceUsage,
    pub library_path: Option<PathBuf>,
    pub analyzer_version: u32,
    pub analysis_options: AnalysisOptions,
}

#[derive(Clone, Debug)]
enum SchedulerCommand {
    SettingsChanged(AnalysisSettings),
    ResourceUsageChanged(BackgroundResourceUsage),
    LibraryPathChanged(Option<PathBuf>),
    /// "Look for new work" — the store may have grown (library scan
    /// added tracks, or the user manually requested re-analysis).
    Wake,
    /// User-initiated batch: process every track in `track_ids` with
    /// the given `capabilities`, independent of the global
    /// `AnalysisSettings`. The supervisor enqueues them into the
    /// explicit queue, which is drained ahead of the background
    /// sweep.
    ExplicitRun {
        track_ids: Vec<TrackId>,
        capabilities: AnalysisCapabilities,
    },
    Shutdown,
}

pub struct AnalysisScheduler {
    sender: Sender<SchedulerCommand>,
    handle: Option<JoinHandle<()>>,
}

impl AnalysisScheduler {
    pub fn start(config: AnalysisSchedulerConfig) -> Self {
        let (sender, receiver) = mpsc::channel::<SchedulerCommand>();
        let handle = thread::Builder::new()
            .name("sustain-analysis-supervisor".to_owned())
            .spawn(move || supervisor_loop(receiver, config))
            .expect("spawn analysis scheduler supervisor thread");
        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub fn update_settings(&self, settings: AnalysisSettings) {
        // Channel send only fails on a closed channel, which only
        // happens after `shutdown` — by which point the runtime should
        // not be issuing more updates. Drop silently in that case.
        let _ = self
            .sender
            .send(SchedulerCommand::SettingsChanged(settings));
    }

    /// Tell the supervisor that the resource-usage preset changed.
    /// The supervisor will drain the in-flight work queue, tear down
    /// the worker pool, and spin up a fresh pool sized + niced for
    /// the new preset.
    pub fn update_resource_usage(&self, usage: BackgroundResourceUsage) {
        let _ = self
            .sender
            .send(SchedulerCommand::ResourceUsageChanged(usage));
    }

    pub fn set_library_path(&self, path: Option<PathBuf>) {
        let _ = self.sender.send(SchedulerCommand::LibraryPathChanged(path));
    }

    pub fn wake(&self) {
        let _ = self.sender.send(SchedulerCommand::Wake);
    }

    /// Enqueue a user-initiated batch of tracks for analysis with the
    /// given capability mask. The batch is processed ahead of the
    /// background sweep; capabilities here are independent of the
    /// global `AnalysisSettings`, so the caller can request audio
    /// analysis on a single playlist while the global audio toggle
    /// is off.
    ///
    /// Duplicate `TrackId`s already in flight or already queued (either
    /// explicit or background) are filtered at the supervisor; submitting
    /// the same list twice in quick succession is wasteful but safe.
    pub fn request_explicit_run(
        &self,
        track_ids: Vec<TrackId>,
        capabilities: AnalysisCapabilities,
    ) {
        if track_ids.is_empty() || capabilities.is_empty() {
            return;
        }
        let _ = self.sender.send(SchedulerCommand::ExplicitRun {
            track_ids,
            capabilities,
        });
    }

    /// Send Shutdown, drop the sender, and join the supervisor. Blocks
    /// until the supervisor finishes draining the queue and returns
    /// from its loop. In-flight DSP completes naturally — we do not
    /// cancel mid-track.
    pub fn shutdown(mut self) {
        let _ = self.sender.send(SchedulerCommand::Shutdown);
        // Drop the live sender so even if Shutdown was lost (impossible
        // with mpsc, but defensive) the supervisor's `recv` returns Err.
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for AnalysisScheduler {
    fn drop(&mut self) {
        // Best-effort cleanup if `shutdown` was not called. Don't
        // block on join in Drop — Drop may run on the GTK main thread.
        let _ = self.sender.send(SchedulerCommand::Shutdown);
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
    }
}

/// Per-track work unit handed to workers via the shared MPMC queue.
struct WorkItem {
    track_id: TrackId,
    absolute_path: PathBuf,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
    /// The library's stored track length, forwarded to the analyzer so
    /// it can centre its analysis windows without a preliminary probe.
    duration: Option<Duration>,
}

/// Per-track entry queued for dispatch. Each entry carries its own
/// capability mask so the supervisor can mix two work sources — the
/// background `tracks_needing_analysis` sweep (capabilities derived
/// from `AnalysisSettings`) and the explicit user-initiated queue
/// (capabilities chosen by the per-playlist right-click menu item) —
/// through the same dispatch path.
///
/// `is_explicit` distinguishes the two at dispatch time: background
/// items snap to the *current* settings (so a user toggling a
/// capability off mid-batch takes effect within one batch), while
/// explicit items use the capabilities the user submitted with the
/// right-click, regardless of any settings change since then.
#[derive(Clone, Copy)]
struct PendingItem {
    track_id: TrackId,
    capabilities: AnalysisCapabilities,
    is_explicit: bool,
}

/// Per-track outcome workers report back to the supervisor. The
/// `track_id` is the same id the supervisor dispatched; the
/// supervisor needs it to remove the entry from its in-flight set
/// (the set is what stops a track from being re-dispatched while a
/// worker is still chewing on it).
struct WorkOutcome {
    track_id: TrackId,
    kind: OutcomeKind,
}

/// What a worker actually accomplished for one track. A background task
/// is not complete until its durable store mutation succeeds, so the
/// distinction between "analysed and persisted" and "store rejected the
/// write" is reported faithfully rather than collapsed into a bool.
enum OutcomeKind {
    /// DSP succeeded and `record_analysis` persisted the result.
    Analyzed,
    /// DSP failed and the failed-attempt stamp persisted, so the track
    /// will not be retried until its analyzer version advances.
    AnalysisFailed,
    /// A store write was rejected by SQLite (the analysis result or the
    /// failed-attempt stamp). Carries the rendered store error for the
    /// notification surface.
    PersistenceFailed(String),
}

/// Shared dependencies every worker captures so it can run the DSP
/// + persist + report progress without going back to the supervisor.
struct WorkerCtx {
    work_rx: async_channel::Receiver<WorkItem>,
    outcome_tx: mpsc::Sender<WorkOutcome>,
    library_store: Arc<dyn LibraryStore>,
    analyzer: AnalyzerFn,
    track_updated: Option<TrackUpdatedSink>,
    analysis_options: AnalysisOptions,
    priority_pair: (NiceLevel, IoPriorityClass),
    /// Whether this worker should pin itself to the machine's efficiency
    /// cores on entry (true for the polite presets on a hybrid CPU; a
    /// no-op elsewhere).
    prefer_efficiency_cores: bool,
}

/// Handles to the running pool, kept by the supervisor for clean
/// teardown across resource-usage changes.
struct WorkerPool {
    work_tx: async_channel::Sender<WorkItem>,
    handles: Vec<JoinHandle<()>>,
}

impl WorkerPool {
    fn spawn(
        worker_count: usize,
        usage: BackgroundResourceUsage,
        library_store: Arc<dyn LibraryStore>,
        analyzer: AnalyzerFn,
        track_updated: Option<TrackUpdatedSink>,
        analysis_options: AnalysisOptions,
        outcome_tx: mpsc::Sender<WorkOutcome>,
    ) -> Self {
        // Bounded queue: keep it small so a teardown-and-respawn does
        // not have a huge backlog of pre-dispatched work to drain. One
        // item per worker plus a one-item slack is plenty — the
        // supervisor's per-track dispatch loop refills as workers free
        // up.
        let queue_capacity = worker_count.max(1).saturating_add(1);
        let (work_tx, work_rx) = async_channel::bounded::<WorkItem>(queue_capacity);
        let priority_pair = priority::priority_for(usage);
        let prefer_efficiency_cores = priority::prefers_efficiency_cores(usage);
        let mut handles = Vec::with_capacity(worker_count);
        for index in 0..worker_count {
            let ctx = WorkerCtx {
                work_rx: work_rx.clone(),
                outcome_tx: outcome_tx.clone(),
                library_store: library_store.clone(),
                analyzer: analyzer.clone(),
                track_updated: track_updated.clone(),
                analysis_options,
                priority_pair,
                prefer_efficiency_cores,
            };
            let handle = thread::Builder::new()
                .name(format!("sustain-analysis-worker-{index}"))
                .spawn(move || worker_loop(ctx))
                .expect("spawn analysis worker thread");
            handles.push(handle);
        }
        Self { work_tx, handles }
    }

    /// Close the queue (signals workers to exit once it drains) and
    /// join them. Blocks until every worker has returned.
    fn shutdown(self) {
        // Closing the sender side prevents the supervisor from
        // accidentally pushing more work, and once the receivers drain
        // they observe a closed channel and exit.
        self.work_tx.close();
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

fn worker_loop(ctx: WorkerCtx) {
    // Apply the resource-usage preset to *this* thread. A failure here
    // is non-fatal: the worker still runs at default priority, which
    // is the same fall-back the kernel produces if we did not call at
    // all. We do not log because the scheduler runs on every launch
    // and a sandbox without the right caps would spam the journal.
    let _ = priority::apply_to_current_thread(ctx.priority_pair.0, ctx.priority_pair.1);

    // On a hybrid CPU, the polite presets also pin to the efficiency
    // cores so background analysis stays off the performance cores
    // playback and the UI want. Best-effort and a no-op on non-hybrid
    // (incl. AMD) machines; failure leaves the thread on default
    // affinity, so it is ignored for the same reason as the priority
    // calls above.
    if ctx.prefer_efficiency_cores {
        let _ = priority::pin_current_thread_to_efficiency_cores_best_effort();
    }

    loop {
        let item = match ctx.work_rx.recv_blocking() {
            Ok(item) => item,
            // Sender closed: pool is being torn down. Exit cleanly.
            Err(_) => return,
        };

        let kind = match (ctx.analyzer)(
            &item.absolute_path,
            item.capabilities,
            ctx.analysis_options,
            item.duration,
        ) {
            Ok(analysis) => match ctx.library_store.record_analysis(
                item.track_id,
                &analysis,
                item.capabilities,
                item.context,
            ) {
                // Only after the result is durable do we report it as
                // analysed and ask the UI to reload the row — a row the
                // store rejected must not look refreshed or completed.
                Ok(()) => {
                    if let Some(notify) = ctx.track_updated.as_deref() {
                        notify(item.track_id);
                    }
                    OutcomeKind::Analyzed
                }
                Err(error) => OutcomeKind::PersistenceFailed(format!("{error:?}")),
            },
            Err(_) => match ctx.library_store.record_analysis_attempt_failure(
                item.track_id,
                item.capabilities,
                item.context,
            ) {
                Ok(()) => OutcomeKind::AnalysisFailed,
                Err(error) => OutcomeKind::PersistenceFailed(format!("{error:?}")),
            },
        };
        let outcome = WorkOutcome {
            track_id: item.track_id,
            kind,
        };

        // Outcome channel send only fails on a closed receiver, which
        // happens during shutdown; nothing useful to do in that case.
        let _ = ctx.outcome_tx.send(outcome);

        thread::sleep(INTER_TRACK_PAUSE);
    }
}

struct SupervisorState {
    settings: AnalysisSettings,
    resource_usage: BackgroundResourceUsage,
    library_path: Option<PathBuf>,
    completed: u32,
    failed: u32,
    /// Stable denominator for the active run. Reset only after `Idle`.
    total: Option<u32>,
    /// User-initiated work, drained ahead of the background sweep.
    /// Items here keep their own capability mask, independent of the
    /// global `AnalysisSettings`.
    explicit_queue: VecDeque<PendingItem>,
    /// Set when a worker reports a rejected store write. The supervisor
    /// loop drains this at its top: it tears the pool down, surfaces a
    /// [`SchedulerProgress::PersistenceError`], and waits for an
    /// explicit wake — never re-running expensive DSP against a store
    /// that will reject the result again.
    pending_persistence_error: Option<String>,
}

fn supervisor_loop(receiver: mpsc::Receiver<SchedulerCommand>, config: AnalysisSchedulerConfig) {
    let AnalysisSchedulerConfig {
        analyzer,
        progress,
        track_updated,
        clock,
        library_store,
        initial_settings,
        initial_resource_usage,
        library_path,
        analyzer_version,
        analysis_options,
    } = config;

    let mut state = SupervisorState {
        settings: initial_settings,
        resource_usage: initial_resource_usage,
        library_path,
        completed: 0,
        failed: 0,
        total: None,
        explicit_queue: VecDeque::new(),
        pending_persistence_error: None,
    };

    let (outcome_tx, outcome_rx) = mpsc::channel::<WorkOutcome>();
    let mut pool: Option<WorkerPool> = None;
    // Streaming dispatcher state.
    //
    // `in_flight` holds the ids of every track the supervisor has
    // handed to the worker pool but not yet seen an outcome for.
    // Those tracks are also still reported by
    // `tracks_needing_analysis` (their `record_analysis` hasn't
    // landed yet), so the set doubles as the dedup filter against
    // the store's query results.
    //
    // `pending` holds ids pulled from the store but not yet
    // shipped to a worker. The dispatch loop drains it into the
    // bounded async-channel via `try_send`, which means the
    // supervisor never blocks waiting for queue space — once the
    // channel is full, it stays at the head of `pending` and the
    // outer loop re-enters via the outcome wait below.
    let mut in_flight: HashSet<TrackId> = HashSet::new();
    let mut pending: VecDeque<PendingItem> = VecDeque::new();

    loop {
        // 1. Drain commands. A resource-usage flip tears down the
        //    pool here; we detect that by comparing the
        //    before/after Option and clear the streaming
        //    bookkeeping so the next iteration starts fresh.
        let pool_was_alive = pool.is_some();
        match drain_commands(&receiver, &mut state, &mut pool) {
            DrainOutcome::Shutdown => {
                if let Some(p) = pool.take() {
                    p.shutdown();
                }
                drain_outcomes_blocking_until_quiet(&outcome_rx, &mut in_flight);
                return;
            }
            DrainOutcome::Continue => {}
        }
        if pool_was_alive && pool.is_none() {
            // A command (resource-usage flip) tore the pool down.
            // The old pool's last-second outcomes may still be in
            // transit; we discard the in-flight set so the new
            // pool starts clean, and `apply_outcome`'s
            // `HashSet::remove` is defensive for the stragglers.
            //
            // Background items in `pending` can be dropped — the
            // next refill's `tracks_needing_analysis` query will
            // surface them again. Explicit items must be preserved,
            // so we move them to the front of `explicit_queue` (in
            // original order) before clearing `pending`. Losing
            // user-initiated work on a settings flip would be a
            // silent surprise.
            in_flight.clear();
            let preserved: Vec<PendingItem> = pending
                .iter()
                .filter(|item| item.is_explicit)
                .copied()
                .collect();
            for item in preserved.into_iter().rev() {
                state.explicit_queue.push_front(item);
            }
            pending.clear();
        }

        // 1b. A worker reported a rejected store write. Persisting is
        //     the gating step for "work complete": a rejected write
        //     means the DSP result is lost and the track will resurface
        //     in `tracks_needing_analysis`. Re-running expensive
        //     analysis against a store that keeps rejecting it would be
        //     an unbounded hot loop, so stop the sweep and wait for an
        //     explicit wake — the same bounded response the store-query
        //     error path below uses. `pending`/`explicit_queue` are left
        //     intact so a later wake re-dispatches that work.
        if let Some(detail) = state.pending_persistence_error.take() {
            if let Some(p) = pool.take() {
                p.shutdown();
            }
            // The join above guarantees no more outcomes; drop any that
            // queued behind the failure so a later wake starts clean.
            while outcome_rx.try_recv().is_ok() {}
            in_flight.clear();
            (progress)(SchedulerProgress::PersistenceError { detail });
            if !block_for_next_command(&receiver, &mut state, &mut pool) {
                return;
            }
            continue;
        }

        let bg_capabilities = capabilities_from(&state.settings);
        let has_explicit_work = !state.explicit_queue.is_empty();
        let library_path = match state.library_path.clone() {
            Some(path)
                if !bg_capabilities.is_empty() || has_explicit_work || !pending.is_empty() =>
            {
                path
            }
            _ => {
                // No library path, or both work sources empty. Drain
                // outcomes so running totals stay honest, then either
                // go idle (nothing in flight) or wait for the tail.
                drain_outcomes_nonblocking(&outcome_rx, &mut state, &mut in_flight, &progress);
                if in_flight.is_empty() {
                    pending.clear();
                    if let Some(p) = pool.take() {
                        p.shutdown();
                    }
                    emit_idle(&progress, &mut state);
                    if !block_for_next_command(&receiver, &mut state, &mut pool) {
                        if let Some(p) = pool.take() {
                            p.shutdown();
                        }
                        drain_outcomes_blocking_until_quiet(&outcome_rx, &mut in_flight);
                        return;
                    }
                } else {
                    wait_for_outcome_with_timeout(
                        &outcome_rx,
                        &mut state,
                        &mut in_flight,
                        &progress,
                    );
                }
                continue;
            }
        };

        if state.total.is_none() {
            match snapshot_run_total(&state, &library_store, bg_capabilities, analyzer_version) {
                Ok(total) => state.total = Some(total),
                Err(_) => {
                    if let Some(p) = pool.take() {
                        p.shutdown();
                    }
                    if !block_for_next_command(&receiver, &mut state, &mut pool) {
                        return;
                    }
                    continue;
                }
            }
        }

        // 2. Make sure a pool is alive and sized to the current preset.
        if pool.is_none() {
            pool = Some(spawn_pool(
                state.resource_usage,
                &library_store,
                &analyzer,
                &track_updated,
                analysis_options,
                &outcome_tx,
            ));
        }
        let work_tx = pool
            .as_ref()
            .expect("pool was just ensured")
            .work_tx
            .clone();

        // 3. Refill `pending` from the two work sources when the
        //    buffer empties. Explicit (user-initiated) work goes
        //    first; any remaining slack is filled from the store's
        //    background `tracks_needing_analysis` query.
        //
        //    The query happens on the cadence of "buffer empty"
        //    rather than "in-flight zero", so workers stay fed
        //    while the previous batch's long tail finishes —
        //    that's the fix for the multi-second idle gaps
        //    between batches that the old "wait for in_flight==0"
        //    gate produced.
        if pending.is_empty() {
            // 3a. Explicit queue first.
            while pending.len() < BATCH_SIZE {
                match state.explicit_queue.pop_front() {
                    Some(item) => {
                        if !in_flight.contains(&item.track_id) {
                            pending.push_back(item);
                        }
                    }
                    None => break,
                }
            }

            // 3b. Background sweep fills the remainder.
            if pending.len() < BATCH_SIZE && !bg_capabilities.is_empty() {
                let room = BATCH_SIZE.saturating_sub(pending.len());
                let limit = room.saturating_add(in_flight.len());
                match library_store.tracks_needing_analysis(
                    bg_capabilities,
                    analyzer_version,
                    limit,
                ) {
                    Ok(fresh) => {
                        for id in fresh {
                            if !in_flight.contains(&id) {
                                pending.push_back(PendingItem {
                                    track_id: id,
                                    capabilities: bg_capabilities,
                                    is_explicit: false,
                                });
                            }
                        }
                    }
                    Err(_) => {
                        // A store error here would be alarming; tear
                        // the pool down and wait for an explicit
                        // nudge so we do not hot-loop on a broken
                        // database.
                        if let Some(p) = pool.take() {
                            p.shutdown();
                        }
                        if !block_for_next_command(&receiver, &mut state, &mut pool) {
                            return;
                        }
                        continue;
                    }
                }
            }

            if pending.is_empty() && in_flight.is_empty() && state.explicit_queue.is_empty() {
                // Truly caught up: every source drained.
                if let Some(p) = pool.take() {
                    p.shutdown();
                }
                emit_idle(&progress, &mut state);
                if !block_for_next_command(&receiver, &mut state, &mut pool) {
                    return;
                }
                continue;
            }
        }

        // 4. Dispatch from `pending` until either the channel is
        //    full (workers saturated) or `pending` is empty.
        //    `try_send` keeps the supervisor responsive to
        //    commands; anything that didn't fit stays at the head
        //    of `pending` for the next iteration. Each item
        //    dispatches with its own capability mask — background
        //    items use the live settings, explicit items use
        //    whatever the right-click submitted.
        while let Some(&item) = pending.front() {
            // Re-check commands between dispatch attempts so a
            // capability toggle or resource-usage flip mid-batch
            // applies promptly. `command_drain_step` returns false
            // on Shutdown.
            if !command_drain_step(&receiver, &mut state, &mut pool) {
                if let Some(p) = pool.take() {
                    p.shutdown();
                }
                drain_outcomes_blocking_until_quiet(&outcome_rx, &mut in_flight);
                return;
            }
            if pool.is_none() {
                // Resource-usage flip tore the pool down between
                // the work_tx clone above and this iteration.
                break;
            }

            // Resolve capabilities: explicit items keep what the user
            // submitted; background items snap to the live settings so
            // a mid-batch toggle takes effect within one batch.
            let dispatch_caps = if item.is_explicit {
                item.capabilities
            } else {
                capabilities_from(&state.settings)
            };
            if dispatch_caps.is_empty() {
                // Background item whose capability has since been
                // toggled off. Drop it; the next refill will reflect
                // the new settings.
                pending.pop_front();
                continue;
            }

            let Ok(Some(track)) = library_store.track(item.track_id) else {
                // Track vanished (concurrent delete or DB error).
                pending.pop_front();
                continue;
            };
            let absolute_path = track.location.absolute_path(&library_path);
            let context = AnalysisContext {
                analyzer_version,
                now_unix: (clock)(),
            };
            let work_item = WorkItem {
                track_id: item.track_id,
                absolute_path,
                capabilities: dispatch_caps,
                context,
                duration: track.metadata.duration,
            };
            match work_tx.try_send(work_item) {
                Ok(()) => {
                    pending.pop_front();
                    in_flight.insert(item.track_id);
                }
                Err(async_channel::TrySendError::Full(_)) => {
                    // Workers saturated; the outcome wait below
                    // will free space when one finishes.
                    break;
                }
                Err(async_channel::TrySendError::Closed(_)) => {
                    // Pool was torn down. Next iteration respawns.
                    break;
                }
            }
        }

        // 5. Wait for an outcome (short timeout for command
        //    responsiveness). Outcomes are the dispatcher's clock:
        //    each one frees a queue slot, so the next iteration
        //    can dispatch more from `pending`.
        if in_flight.is_empty() && pending.is_empty() && state.explicit_queue.is_empty() {
            // Nothing to wait on and nothing to dispatch — block
            // on the command channel so we don't busy-loop.
            if !block_for_next_command(&receiver, &mut state, &mut pool) {
                return;
            }
        } else {
            wait_for_outcome_with_timeout(&outcome_rx, &mut state, &mut in_flight, &progress);
        }
    }
}

/// Wait up to a short timeout for one outcome, fold it into the
/// running totals, and emit a Tick. The timeout keeps the supervisor
/// responsive to commands even when workers are mid-DSP.
fn wait_for_outcome_with_timeout(
    outcome_rx: &mpsc::Receiver<WorkOutcome>,
    state: &mut SupervisorState,
    in_flight: &mut HashSet<TrackId>,
    progress: &ProgressSink,
) {
    match outcome_rx.recv_timeout(Duration::from_millis(50)) {
        Ok(outcome) => {
            apply_outcome(outcome, state, in_flight, progress);
        }
        Err(mpsc::RecvTimeoutError::Timeout) | Err(mpsc::RecvTimeoutError::Disconnected) => {}
    }
}

/// Block on the outcome channel until every in-flight item has been
/// acknowledged. Called during shutdown so the pool's `join` does not
/// race against in-flight tracks that haven't reported back yet.
fn drain_outcomes_blocking_until_quiet(
    outcome_rx: &mpsc::Receiver<WorkOutcome>,
    in_flight: &mut HashSet<TrackId>,
) {
    while !in_flight.is_empty() {
        match outcome_rx.recv_timeout(Duration::from_secs(2)) {
            Ok(outcome) => {
                in_flight.remove(&outcome.track_id);
            }
            Err(_) => {
                // Either disconnect or timeout — either way the
                // workers won't be sending more outcomes. Give up
                // tracking; the pool join below will block on the
                // worker handles anyway.
                break;
            }
        }
    }
}

fn apply_outcome(
    outcome: WorkOutcome,
    state: &mut SupervisorState,
    in_flight: &mut HashSet<TrackId>,
    progress: &ProgressSink,
) {
    let WorkOutcome { track_id, kind } = outcome;
    match kind {
        OutcomeKind::PersistenceFailed(detail) => {
            // Record the failure (keeping the first of several) and
            // leave `track_id` in `in_flight` so the supervisor never
            // treats the pool as quiescent and blocks before the loop
            // top surfaces the error and tears the pool down. No Tick:
            // a rejected write is not progress.
            if state.pending_persistence_error.is_none() {
                state.pending_persistence_error = Some(detail);
            }
            return;
        }
        OutcomeKind::Analyzed => {
            state.completed = state.completed.saturating_add(1);
        }
        OutcomeKind::AnalysisFailed => {
            state.failed = state.failed.saturating_add(1);
        }
    }
    // The HashSet::remove is harmless when the id isn't there — which
    // happens after a resource-usage flip drops the in-flight set
    // (the old pool's last few outcomes can still trickle in).
    in_flight.remove(&track_id);
    let processed = state.completed.saturating_add(state.failed);
    let total = state.total.unwrap_or(processed).max(processed);
    (progress)(SchedulerProgress::Tick {
        completed: state.completed,
        failed: state.failed,
        total,
    });
}

/// Pop every queued outcome without blocking, fold it into the running
/// totals, and emit a Tick per outcome so the UI's running count moves
/// in real time. Cheap because the outcomes are small structs.
fn drain_outcomes_nonblocking(
    outcome_rx: &mpsc::Receiver<WorkOutcome>,
    state: &mut SupervisorState,
    in_flight: &mut HashSet<TrackId>,
    progress: &ProgressSink,
) {
    while let Ok(outcome) = outcome_rx.try_recv() {
        apply_outcome(outcome, state, in_flight, progress);
    }
}

fn emit_idle(progress: &ProgressSink, state: &mut SupervisorState) {
    (progress)(SchedulerProgress::Idle {
        completed: state.completed,
        failed: state.failed,
    });
    state.completed = 0;
    state.failed = 0;
    state.total = None;
}

fn snapshot_run_total(
    state: &SupervisorState,
    library_store: &Arc<dyn LibraryStore>,
    capabilities: AnalysisCapabilities,
    analyzer_version: u32,
) -> sustain_library_store::StoreResult<u32> {
    let mut track_ids: HashSet<TrackId> = state
        .explicit_queue
        .iter()
        .map(|item| item.track_id)
        .collect();
    if !capabilities.is_empty() {
        track_ids.extend(library_store.tracks_needing_analysis(
            capabilities,
            analyzer_version,
            i64::MAX as usize,
        )?);
    }
    Ok(u32::try_from(track_ids.len()).unwrap_or(u32::MAX))
}

/// Wait on the next command, then apply it. Returns `false` if
/// shutdown was requested.
fn block_for_next_command(
    receiver: &mpsc::Receiver<SchedulerCommand>,
    state: &mut SupervisorState,
    pool: &mut Option<WorkerPool>,
) -> bool {
    match receiver.recv() {
        Ok(SchedulerCommand::Shutdown) | Err(_) => false,
        Ok(command) => {
            apply_command(command, state, pool);
            true
        }
    }
}

/// Drain a single pending command (non-blocking). Returns false on
/// shutdown (the supervisor must return to the caller immediately).
fn command_drain_step(
    receiver: &mpsc::Receiver<SchedulerCommand>,
    state: &mut SupervisorState,
    pool: &mut Option<WorkerPool>,
) -> bool {
    if let Some(command) = receiver.try_iter().next() {
        if matches!(command, SchedulerCommand::Shutdown) {
            if let Some(p) = pool.take() {
                p.shutdown();
            }
            return false;
        }
        apply_command(command, state, pool);
    }
    true
}

enum DrainOutcome {
    Continue,
    Shutdown,
}

fn drain_commands(
    receiver: &mpsc::Receiver<SchedulerCommand>,
    state: &mut SupervisorState,
    pool: &mut Option<WorkerPool>,
) -> DrainOutcome {
    while let Ok(command) = receiver.try_recv() {
        if matches!(command, SchedulerCommand::Shutdown) {
            return DrainOutcome::Shutdown;
        }
        apply_command(command, state, pool);
    }
    DrainOutcome::Continue
}

fn apply_command(
    command: SchedulerCommand,
    state: &mut SupervisorState,
    pool: &mut Option<WorkerPool>,
) {
    match command {
        SchedulerCommand::SettingsChanged(settings) => {
            state.settings = settings;
        }
        SchedulerCommand::ResourceUsageChanged(usage) => {
            if state.resource_usage != usage {
                state.resource_usage = usage;
                // Force a respawn at the new size + new priority. The
                // dispatch loop will recreate the pool on next iteration.
                if let Some(p) = pool.take() {
                    p.shutdown();
                }
            }
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
            // Dedup against everything we already know about: the
            // explicit queue itself (user double-clicked) and any
            // background-sourced entries we have already lined up.
            // Items already in flight are filtered at refill time
            // (the `in_flight` HashSet check), so we don't see them
            // here.
            let already_queued: HashSet<TrackId> = state
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

fn capabilities_from(settings: &AnalysisSettings) -> AnalysisCapabilities {
    AnalysisCapabilities {
        bpm: settings.bpm,
        key: settings.key,
        audio: settings.audio,
    }
}

fn spawn_pool(
    usage: BackgroundResourceUsage,
    library_store: &Arc<dyn LibraryStore>,
    analyzer: &AnalyzerFn,
    track_updated: &Option<TrackUpdatedSink>,
    analysis_options: AnalysisOptions,
    outcome_tx: &mpsc::Sender<WorkOutcome>,
) -> WorkerPool {
    let worker_count = priority::resolve_worker_count(usage);
    WorkerPool::spawn(
        worker_count,
        usage,
        library_store.clone(),
        analyzer.clone(),
        track_updated.clone(),
        analysis_options,
        outcome_tx.clone(),
    )
}

#[cfg(test)]
#[path = "analysis_scheduler_tests.rs"]
mod tests;
