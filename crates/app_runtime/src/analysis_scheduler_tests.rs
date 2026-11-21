// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    fs::File,
    io::Write,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU32, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, Instant},
};

use sustain_analysis::{AnalysisError, AnalysisOptions, TrackAnalysis};
use sustain_domain::{
    AnalysisSettings, BackgroundResourceUsage, Bpm, BpmRange, MusicalKey, PREVIEW_SEGMENT_COUNT,
    Track, TrackLocation, TrackRelativePath, WaveformSegment, WaveformSegments,
};
use sustain_library_store::{
    AnalysisCapabilities, AnalysisDue, AnalyzerVersions, LibraryStore, SqliteLibraryStore, TrackId,
};
use tempfile::TempDir;

use super::{
    AnalysisScheduler, AnalysisSchedulerConfig, AnalyzerFn, ProgressSink, SchedulerProgress,
    UnixClockFn,
};

/// The analyzer versions the scheduler tests run under.
const VERSIONS: AnalyzerVersions = AnalyzerVersions {
    bpm: 1,
    key: 1,
    audio: 1,
};

/// Place an empty file inside `library_root` at `relative` so the
/// scheduler's "resolve to absolute path" step succeeds. The stub
/// analyzer does not actually read the file — it returns a canned
/// `TrackAnalysis` — so the contents do not matter.
fn touch_in(library_root: &std::path::Path, relative: &str) -> Track {
    let absolute = library_root.join(relative);
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    File::create(&absolute)
        .and_then(|mut f| f.write_all(b""))
        .expect("create file");
    let relative_path = TrackRelativePath::new(relative).expect("valid relative path");
    Track {
        id: TrackId::new(1).expect("non-zero"),
        location: TrackLocation::available(relative_path),
        metadata: Default::default(),
        rating: Default::default(),
        statistics: Default::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

fn fixed_clock(value: i64) -> UnixClockFn {
    Arc::new(move || value)
}

fn ok_analyzer() -> AnalyzerFn {
    Arc::new(|_path, _caps, _opts, _duration| {
        Ok(TrackAnalysis {
            bpm: Bpm::from_whole(120),
            key: Some(MusicalKey::CMajor),
            beat_grid: None,
            waveform_preview: WaveformSegments {
                segment_duration_ms: 25.0,
                segments: vec![WaveformSegment::silent(); PREVIEW_SEGMENT_COUNT],
                band_scale: sustain_domain::BandScale::EVEN,
            },
            waveform_detail: WaveformSegments {
                segment_duration_ms: 6.67,
                segments: vec![WaveformSegment::silent(); 512],
                band_scale: sustain_domain::BandScale::EVEN,
            },
            acoustics: None,
        })
    })
}

fn err_analyzer() -> AnalyzerFn {
    Arc::new(|_path, _caps, _opts, _duration| {
        Err(AnalysisError::TooShort {
            path: "stub".into(),
            samples: 0,
        })
    })
}

/// Channel-backed progress sink: every `SchedulerProgress` the
/// supervisor emits is pushed onto a queue the test can drain.
fn capturing_sink() -> (ProgressSink, std_mpsc::Receiver<SchedulerProgress>) {
    let (tx, rx) = std_mpsc::channel();
    let sink: ProgressSink = Arc::new(move |progress| {
        // Send failure means the receiver has been dropped — the
        // test is over; nothing useful to do.
        let _ = tx.send(progress);
    });
    (sink, rx)
}

/// Wait up to `timeout` for a progress event matching `predicate`,
/// returning the matching event. Fails the test on timeout.
fn wait_for(
    rx: &std_mpsc::Receiver<SchedulerProgress>,
    timeout: Duration,
    predicate: impl Fn(&SchedulerProgress) -> bool,
) -> SchedulerProgress {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let progress = rx
            .recv_timeout(remaining)
            .expect("scheduler progress within timeout");
        if predicate(&progress) {
            return progress;
        }
    }
}

/// The tracks of a needing-analysis result, for tests that only check
/// which tracks are due.
fn due_ids(due: Vec<AnalysisDue>) -> Vec<TrackId> {
    due.into_iter().map(|due| due.track_id).collect()
}

/// Explicit work asking for `capabilities` on every track in `track_ids`.
fn explicit_work(track_ids: Vec<TrackId>, capabilities: AnalysisCapabilities) -> Vec<AnalysisDue> {
    track_ids
        .into_iter()
        .map(|track_id| AnalysisDue {
            track_id,
            capabilities,
        })
        .collect()
}

fn capability_count(capabilities: AnalysisCapabilities) -> u32 {
    u32::from(capabilities.bpm) + u32::from(capabilities.key) + u32::from(capabilities.audio)
}

fn deterministic_config(
    analyzer: AnalyzerFn,
    progress: ProgressSink,
    clock: UnixClockFn,
    library_store: Arc<dyn LibraryStore>,
    settings: AnalysisSettings,
    library_path: Option<std::path::PathBuf>,
) -> AnalysisSchedulerConfig {
    AnalysisSchedulerConfig {
        analyzer,
        progress,
        track_updated: None,
        clock,
        library_store,
        initial_settings: settings,
        // Tests run with a single worker so progress events arrive
        // in a predictable order regardless of host core count.
        initial_resource_usage: BackgroundResourceUsage::Innocuous,
        processor_count: 1,
        library_path,
        analyzer_versions: VERSIONS,
    }
}

#[test]
fn scheduler_processes_pending_tracks_with_settings_enabled() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track.clone()).expect("save track");

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1_700_000_000),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });
    if let SchedulerProgress::Tick {
        completed, failed, ..
    } = tick
    {
        assert_eq!(completed, 1);
        assert_eq!(failed, 0);
    }

    // Track should now be ineligible for re-analysis at the same
    // analyzer versions.
    let still_pending = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), VERSIONS, 100)
        .map(due_ids)
        .expect("query");
    assert!(still_pending.is_empty(), "track should be marked attempted");

    scheduler.shutdown();
}

#[test]
fn scheduler_records_failures_without_clobbering_run() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "broken.flac");

    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track.clone()).expect("save");

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        err_analyzer(),
        sink,
        fixed_clock(2_000),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { failed: 1, .. })
    });
    if let SchedulerProgress::Tick {
        completed, failed, ..
    } = tick
    {
        assert_eq!(completed, 0);
        assert_eq!(failed, 1);
    }

    // The failure attempt should be stamped so the track does not
    // re-queue at the same analyzer versions.
    let pending = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            VERSIONS,
            10,
        )
        .map(due_ids)
        .expect("query");
    assert!(pending.is_empty());

    scheduler.shutdown();
}

/// A decoder or DSP bug that panics on one file must cost that file its
/// attempt, not the worker its thread: the track is stamped as failed,
/// the run reaches its end, and the tracks after it are still analysed.
#[test]
fn an_analyzer_panic_fails_its_track_and_spares_the_worker() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    // More tracks than any preset has workers: every worker meets a
    // panic, and the healthy track still has one left to run on.
    let poisoned = 40;
    for id in 1..=poisoned + 1 {
        let name = if id <= poisoned {
            format!("poisoned-{id}.flac")
        } else {
            "healthy.flac".to_owned()
        };
        let mut track = touch_in(&library_root, &name);
        track.id = TrackId::new(id).expect("non-zero");
        store.save_track(track).expect("save");
    }

    let healthy = ok_analyzer();
    let analyzer: AnalyzerFn = Arc::new(move |path, capabilities, options, duration| {
        let poisoned = path
            .file_name()
            .is_some_and(|name| name.to_string_lossy().starts_with("poisoned"));
        assert!(!poisoned, "decoder bug on {}", path.display());
        healthy(path, capabilities, options, duration)
    });
    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(2_000),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let idle = wait_for(&rx, Duration::from_secs(10), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });
    assert_eq!(
        idle,
        SchedulerProgress::Idle {
            completed: 1,
            failed: poisoned as u32,
            audio_recorded: false,
        }
    );
    let pending = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            VERSIONS,
            100,
        )
        .map(due_ids)
        .expect("query");
    assert!(pending.is_empty(), "every attempt is stamped");

    scheduler.shutdown();
}

#[test]
fn scheduler_progress_denominator_does_not_slide_when_pool_grows_mid_run() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    for (id, relative) in [(1, "first.flac"), (2, "second.flac")] {
        let mut track = touch_in(&library_root, relative);
        track.id = TrackId::new(id).expect("track id");
        store.save_track(track).expect("save track");
    }
    let analyzer: AnalyzerFn = Arc::new(|path, caps, options, duration| {
        std::thread::sleep(Duration::from_millis(80));
        ok_analyzer()(path, caps, options, duration)
    });
    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root.clone()),
    ));

    let first = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(
            progress,
            SchedulerProgress::Tick {
                completed: 1,
                total: 2,
                ..
            }
        )
    });
    assert!(matches!(first, SchedulerProgress::Tick { total: 2, .. }));

    let mut added = touch_in(&library_root, "third.flac");
    added.id = TrackId::new(3).expect("track id");
    store.save_track(added).expect("save added track");

    let second = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 2, .. })
    });
    assert!(
        matches!(second, SchedulerProgress::Tick { total: 2, .. }),
        "the denominator is a run-start snapshot, not a live pool size"
    );

    scheduler.shutdown();
}

#[test]
fn rejected_record_analysis_halts_without_false_completion() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    faulty.save_track(track).expect("save");
    faulty.set_fail_record_analysis(true);

    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1_700_000_000),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    // The rejected write must surface as a PersistenceError, never as a
    // completed Tick — the DSP result was lost, not landed.
    let event = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::PersistenceError { .. })
    });
    assert!(matches!(event, SchedulerProgress::PersistenceError { .. }));

    // Bounded retry: the scheduler halts rather than re-running DSP
    // against a store that keeps rejecting the write. Give it time to
    // hot-loop if it were going to; it must not.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        faulty.record_analysis_calls(),
        1,
        "scheduler must not retry a rejected write in a hot loop"
    );
    let pending = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), VERSIONS, 10)
        .map(due_ids)
        .expect("query");
    assert_eq!(pending.len(), 1, "the unpersisted track is still pending");

    // Once the store recovers, an explicit wake resumes the sweep and
    // the track persists for real.
    faulty.set_fail_record_analysis(false);
    scheduler.wake();
    let tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });
    assert!(matches!(tick, SchedulerProgress::Tick { completed: 1, .. }));
    let pending = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), VERSIONS, 10)
        .map(due_ids)
        .expect("query");
    assert!(pending.is_empty(), "track persisted after recovery");

    scheduler.shutdown();
}

#[test]
fn rejected_failure_stamp_halts_the_sweep() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "broken.flac");

    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    faulty.save_track(track).expect("save");
    faulty.set_fail_attempt_failure(true);

    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        err_analyzer(),
        sink,
        fixed_clock(2_000),
        store,
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let event = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::PersistenceError { .. })
    });
    assert!(matches!(event, SchedulerProgress::PersistenceError { .. }));

    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(
        faulty.attempt_failure_calls(),
        1,
        "a rejected failure stamp must not hot-loop"
    );

    scheduler.shutdown();
}

#[test]
fn scheduler_idles_when_settings_disabled() {
    let temp = TempDir::new().expect("temp dir");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let track = touch_in(temp.path(), "alpha.flac");
    store.save_track(track).expect("save");

    // Count how often the analyzer is called — must be zero with
    // every capability off.
    let calls = Arc::new(AtomicU32::new(0));
    let counted = calls.clone();
    let analyzer: AnalyzerFn = Arc::new(move |_path, _caps, _opts, _duration| {
        counted.fetch_add(1, Ordering::SeqCst);
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(0),
        store,
        AnalysisSettings::default(),
        Some(temp.path().to_path_buf()),
    ));

    // First emission must be Idle (no work).
    let first = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first progress");
    assert!(matches!(first, SchedulerProgress::Idle { .. }));

    // Give the supervisor a moment; it must remain idle.
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(calls.load(Ordering::SeqCst), 0);

    scheduler.shutdown();
}

#[test]
fn settings_change_resumes_a_blocked_scheduler() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track).expect("save");

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings::default(),
        Some(library_root),
    ));

    // Initial Idle.
    let initial = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("initial progress");
    assert!(matches!(initial, SchedulerProgress::Idle { .. }));

    // Flip audio on; the supervisor must wake, process, and
    // emit a Tick.
    scheduler.update_settings(AnalysisSettings {
        bpm: false,
        key: false,
        audio: true,
        bpm_range: BpmRange::default(),
    });

    let tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });
    assert!(matches!(tick, SchedulerProgress::Tick { .. }));

    scheduler.shutdown();
}

#[test]
fn toggling_capabilities_off_stops_the_running_scheduler() {
    // When the user un-toggles a background capability the
    // scheduler must stop analyzing additional tracks. The
    // in-flight DSP completes naturally (we do not cancel
    // mid-track) so this asserts the bounded behavior: no more
    // than a small number of tracks get analyzed after the
    // toggle, even though many were pending.
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    for i in 0..32 {
        let relative = format!("track_{i:02}.flac");
        let mut track = touch_in(&library_root, &relative);
        track.id = TrackId::new(i + 1).expect("non-zero");
        store.save_track(track).expect("save");
    }

    // Analyzer that artificially extends per-track work so the
    // test has a chance to send the toggle-off before the
    // scheduler drains the queue.
    let analyzer: AnalyzerFn = Arc::new(|_path, _caps, _opts, _duration| {
        std::thread::sleep(Duration::from_millis(80));
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings {
            bpm: false,
            key: false,
            audio: true,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    // Let a couple of ticks land — proves the scheduler is
    // genuinely analyzing — then toggle audio off.
    let _first_tick = wait_for(
        &rx,
        Duration::from_secs(2),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 1),
    );
    scheduler.update_settings(AnalysisSettings::default());

    // Wait for Idle to confirm the supervisor observed the toggle.
    let _idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });

    // Snapshot how many tracks are still pending; if the
    // scheduler really stopped, a generous additional wait should
    // not move the count.
    let still_pending_before = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            VERSIONS,
            100,
        )
        .map(due_ids)
        .expect("query")
        .len();
    std::thread::sleep(Duration::from_millis(400));
    let still_pending_after = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            VERSIONS,
            100,
        )
        .map(due_ids)
        .expect("query")
        .len();
    assert_eq!(
        still_pending_before, still_pending_after,
        "scheduler must stop processing after capabilities go to zero"
    );
    assert!(
        still_pending_after > 0,
        "test fixture should leave un-analysed tracks; saw {still_pending_after}",
    );

    scheduler.shutdown();
}

#[test]
fn shutdown_returns_after_join() {
    // Smoke test: a scheduler that never gets work should still
    // shut down promptly. Regression guard against the supervisor
    // blocking on recv past the shutdown nudge.
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let (sink, _rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(0),
        store,
        AnalysisSettings::default(),
        None,
    ));
    let start = Instant::now();
    scheduler.shutdown();
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn track_updated_sink_fires_after_successful_analysis() {
    assert_track_updated_after(ok_analyzer());
}

/// A failed attempt replaces the track's earlier outcome, so its row is
/// reloaded too: a waveform it no longer has must stop showing.
#[test]
fn track_updated_sink_fires_after_failed_analysis() {
    assert_track_updated_after(err_analyzer());
}

fn assert_track_updated_after(analyzer: AnalyzerFn) {
    use std::sync::mpsc as std_mpsc;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track.clone()).expect("save track");

    let (notify_tx, notify_rx) = std_mpsc::channel::<sustain_domain::TrackId>();
    let track_updated: super::TrackUpdatedSink = Arc::new(move |id| {
        let _ = notify_tx.send(id);
    });

    let (sink, _rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(AnalysisSchedulerConfig {
        analyzer,
        progress: sink,
        track_updated: Some(track_updated),
        clock: fixed_clock(1),
        library_store: store,
        initial_settings: AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
            bpm_range: BpmRange::default(),
        },
        initial_resource_usage: BackgroundResourceUsage::Innocuous,
        processor_count: 1,
        library_path: Some(library_root),
        analyzer_versions: VERSIONS,
    });

    let observed = notify_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("track_updated sink fires once analysis completes");
    assert_eq!(observed, track.id);

    scheduler.shutdown();
}

#[test]
fn resource_usage_change_respawns_pool_without_losing_work() {
    // Pool teardown + respawn must not drop pending tracks: after
    // the resource-usage flip, the scheduler should continue
    // draining the queue under the new preset.
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    for i in 0..8 {
        let relative = format!("track_{i:02}.flac");
        let mut track = touch_in(&library_root, &relative);
        track.id = TrackId::new(i + 1).expect("non-zero");
        store.save_track(track).expect("save");
    }

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    // Let the first track land so we know the pool is alive.
    let _first = wait_for(
        &rx,
        Duration::from_secs(2),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 1),
    );

    scheduler.update_resource_usage(BackgroundResourceUsage::Aggressive);

    // After respawn the remaining tracks should still drain.
    let _idle = wait_for(&rx, Duration::from_secs(5), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });

    let still_pending = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), VERSIONS, 100)
        .map(due_ids)
        .expect("query");
    assert!(
        still_pending.is_empty(),
        "respawn must not drop pending tracks; {} still pending",
        still_pending.len(),
    );

    scheduler.shutdown();
}

#[test]
fn analyzer_receives_the_live_capability_mask() {
    // The scheduler must hand the analyzer the exact capability
    // set the user has on right now — not the full mask. This is
    // the integration end of "capability-gated `Analyzer`
    // returning None for the bands the caller did not ask for":
    // production wires the analyzer to only call the band methods
    // that the mask permits, so confirming the mask makes it
    // through guards against the scheduler quietly defaulting to
    // `AnalysisCapabilities::all()`.
    use std::sync::Mutex;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track.clone()).expect("save track");

    let observed: Arc<Mutex<Option<AnalysisCapabilities>>> = Arc::new(Mutex::new(None));
    let observed_for_closure = observed.clone();
    let analyzer: AnalyzerFn = Arc::new(move |_path, caps, _opts, _duration| {
        if let Ok(mut guard) = observed_for_closure.lock() {
            *guard = Some(caps);
        }
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store,
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    let seen = observed
        .lock()
        .expect("lock")
        .take()
        .expect("analyzer was called");
    assert!(seen.bpm, "bpm must be set");
    assert!(!seen.key, "key must be cleared — user toggled it off");
    assert!(!seen.audio, "audio must be cleared — user toggled it off");

    scheduler.shutdown();
}

#[test]
fn streaming_dispatch_keeps_workers_fed_across_batch_boundary() {
    // Regression guard for the idle gap the old supervisor left
    // between batches: it waited for `in_flight == 0` before querying
    // the store again, so one slow track held every other worker idle
    // until it finished. Two workers run here; the first track to
    // start is held until a track beyond the first batch has started.
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));

    let track_count = (super::BATCH_SIZE as i64) * 2;
    for i in 0..track_count {
        let relative = format!("track_{i:03}.flac");
        let mut track = touch_in(&library_root, &relative);
        track.id = TrackId::new(i + 1).expect("non-zero");
        store.save_track(track).expect("save");
    }

    let calls = Arc::new(AtomicU32::new(0));
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let release_rx = Arc::new(Mutex::new(release_rx));
    let analyzer: AnalyzerFn = {
        let calls = calls.clone();
        Arc::new(move |_path, _caps, _opts, _duration| {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                let _ = release_rx
                    .lock()
                    .expect("release lock")
                    .recv_timeout(Duration::from_secs(10));
            }
            ok_analyzer()(
                std::path::Path::new("ignored"),
                AnalysisCapabilities::all(),
                AnalysisOptions::default(),
                None,
            )
        })
    };

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(AnalysisSchedulerConfig {
        // `Aggressive` keeps two of four processors as headroom, which
        // leaves exactly two workers.
        initial_resource_usage: BackgroundResourceUsage::Aggressive,
        processor_count: 4,
        ..deterministic_config(
            analyzer,
            sink,
            fixed_clock(1),
            store.clone(),
            AnalysisSettings {
                bpm: true,
                key: true,
                audio: true,
                bpm_range: BpmRange::default(),
            },
            Some(library_root),
        )
    });

    let deadline = Instant::now() + Duration::from_secs(5);
    while calls.load(Ordering::SeqCst) <= super::BATCH_SIZE as u32 && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    let started_while_held = calls.load(Ordering::SeqCst);
    release_tx.send(()).expect("release the held track");

    let _ = wait_for(
        &rx,
        Duration::from_secs(10),
        |progress| matches!(progress, SchedulerProgress::Idle { completed, .. } if i64::from(*completed) == track_count),
    );
    scheduler.shutdown();

    assert!(
        started_while_held > super::BATCH_SIZE as u32,
        "the free worker must start the next batch while one track is held; \
         only {started_while_held} tracks started",
    );
}

#[test]
fn capability_count_is_a_small_helper() {
    // Just exercises the helper so refactors don't silently break
    // it; the public API itself doesn't expose this.
    assert_eq!(
        capability_count(AnalysisCapabilities {
            bpm: true,
            key: true,
            audio: false
        }),
        2
    );
    assert_eq!(capability_count(AnalysisCapabilities::all()), 3);
    assert_eq!(capability_count(AnalysisCapabilities::none()), 0);
}

#[test]
fn explicit_run_processes_tracks_with_global_settings_off() {
    // The per-playlist right-click menu lets the user run, say,
    // audio analysis on a single playlist while the global
    // audio toggle stays off. The scheduler must accept and
    // process the explicit batch even though
    // `tracks_needing_analysis` would return nothing for the
    // empty settings mask.
    use std::sync::Mutex;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let mut track_a = touch_in(&library_root, "a.flac");
    track_a.id = TrackId::new(11).expect("non-zero");
    let mut track_b = touch_in(&library_root, "b.flac");
    track_b.id = TrackId::new(22).expect("non-zero");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track_a.clone()).expect("save");
    store.save_track(track_b.clone()).expect("save");

    let observed: Arc<Mutex<Vec<AnalysisCapabilities>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_closure = observed.clone();
    let analyzer: AnalyzerFn = Arc::new(move |_path, caps, _opts, _duration| {
        observed_for_closure.lock().expect("lock").push(caps);
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings {
            bpm: false,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    // Without the explicit request the scheduler should be idle
    // (no capabilities, nothing to do). Submit the per-playlist
    // batch and expect both tracks to be processed.
    scheduler.request_explicit_run(explicit_work(
        vec![track_a.id, track_b.id],
        AnalysisCapabilities {
            bpm: false,
            key: false,
            audio: true,
        },
    ));

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 2, .. })
    });

    let seen = observed.lock().expect("lock").clone();
    assert_eq!(seen.len(), 2, "both tracks must be processed");
    for caps in seen {
        assert!(!caps.bpm, "explicit caps must control the mask");
        assert!(!caps.key);
        assert!(caps.audio);
    }

    scheduler.shutdown();
}

#[test]
fn explicit_run_drains_ahead_of_background_sweep() {
    // The explicit queue is meant to feel "do this now". Even
    // when the background sweep has tracks lined up, the
    // explicit batch must preempt any background work that has
    // not already reached the worker pool.

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));

    // More background tracks than the one-worker pool can have in
    // flight. That leaves undispatched pending work for the explicit
    // request to preempt.
    for index in 0..8_i64 {
        let relative = format!("bg_{index:02}.flac");
        let mut track = touch_in(&library_root, &relative);
        track.id = TrackId::new(index + 1).expect("non-zero");
        store.save_track(track).expect("save background track");
    }
    // One explicit track the user clicked on.
    let mut explicit_track = touch_in(&library_root, "explicit.flac");
    explicit_track.id = TrackId::new(99).expect("non-zero");
    store
        .save_track(explicit_track.clone())
        .expect("save explicit");

    let order: Arc<Mutex<Vec<TrackId>>> = Arc::new(Mutex::new(Vec::new()));
    let first_background_started = Arc::new((Mutex::new(false), Condvar::new()));
    let release_first_background = Arc::new((Mutex::new(false), Condvar::new()));
    let blocked_once = Arc::new(AtomicBool::new(false));
    let order_for_closure = order.clone();
    let first_background_started_for_closure = first_background_started.clone();
    let release_first_background_for_closure = release_first_background.clone();
    let blocked_once_for_closure = blocked_once.clone();
    let analyzer: AnalyzerFn = Arc::new(move |path, _caps, _opts, _duration| {
        // Recover the TrackId from the path stem so the test can
        // assert on processing order without needing a richer
        // analyzer hook.
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default();
        let id_value: i64 = if stem == "explicit" {
            99
        } else {
            stem.trim_start_matches("bg_")
                .parse::<i64>()
                .map(|n| n + 1)
                .unwrap_or(0)
        };
        if let Some(id) = TrackId::new(id_value) {
            order_for_closure.lock().expect("lock").push(id);
        }
        if stem.starts_with("bg_") && !blocked_once_for_closure.swap(true, Ordering::SeqCst) {
            let (started_lock, started_cvar) = &*first_background_started_for_closure;
            *started_lock.lock().expect("lock started") = true;
            started_cvar.notify_one();

            let (release_lock, release_cvar) = &*release_first_background_for_closure;
            let mut released = release_lock.lock().expect("lock release");
            while !*released {
                released = release_cvar.wait(released).expect("wait release");
            }
        }
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store,
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    let (started_lock, started_cvar) = &*first_background_started;
    let mut started = started_lock.lock().expect("lock started");
    while !*started {
        let wait_result = started_cvar
            .wait_timeout(started, Duration::from_secs(2))
            .expect("wait for first background");
        started = wait_result.0;
        assert!(
            !wait_result.1.timed_out(),
            "background worker did not start"
        );
    }

    // The first background track is in-flight and the pool may have
    // accepted its bounded queue slack. Submit the explicit run; the
    // supervisor must put it ahead of any background tracks that are
    // only pending in the supervisor.
    scheduler.request_explicit_run(explicit_work(
        vec![explicit_track.id],
        AnalysisCapabilities {
            bpm: false,
            key: true,
            audio: false,
        },
    ));

    let (release_lock, release_cvar) = &*release_first_background;
    *release_lock.lock().expect("lock release") = true;
    release_cvar.notify_one();

    // Wait until everything is done.
    let _tick = wait_for(
        &rx,
        Duration::from_secs(5),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 9),
    );

    let seen = order.lock().expect("lock").clone();
    let explicit_position = seen
        .iter()
        .position(|id| *id == explicit_track.id)
        .expect("explicit track was processed");
    // A background track may slip ahead if it already reached the
    // worker pool before the explicit command arrived. With the
    // one-worker test pool and its one-worker-plus-one-slack queue,
    // at most three tracks can be ahead: one running and up to two
    // buffered. The remaining pending background tracks must be
    // preempted.
    assert!(
        explicit_position <= 3,
        "explicit run must preempt pending background work, saw position {explicit_position} of {}",
        seen.len()
    );

    scheduler.shutdown();
}

#[test]
fn explicit_run_arriving_mid_dispatch_goes_first() {
    // The supervisor takes in commands between two dispatches. An
    // explicit run it takes in there replaces the background work in
    // `pending`, and must itself be what is dispatched next.
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    for index in 0..4_i64 {
        let mut track = touch_in(&library_root, &format!("bg_{index:02}.flac"));
        track.id = TrackId::new(index + 1).expect("non-zero");
        faulty.save_track(track).expect("save background track");
    }
    let mut explicit_track = touch_in(&library_root, "explicit.flac");
    explicit_track.id = TrackId::new(99).expect("non-zero");
    faulty
        .save_track(explicit_track.clone())
        .expect("save explicit");

    // The first sweep query counts the run's total; the second fills
    // `pending`, and is held until the explicit run has been sent, so
    // the supervisor takes it in once it starts dispatching that batch.
    let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let sweeps = AtomicU32::new(0);
    faulty.run_in_analysis_sweeps(move || {
        if sweeps.fetch_add(1, Ordering::SeqCst) == 1 {
            entered_tx.send(()).expect("test waits for the sweep");
            release_rx
                .lock()
                .expect("lock release")
                .recv()
                .expect("test releases the sweep");
        }
        Ok(())
    });

    let analyzed: Arc<Mutex<Vec<(String, AnalysisCapabilities)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let analyzed_for_closure = analyzed.clone();
    let analyzer: AnalyzerFn = Arc::new(move |path, caps, opts, duration| {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_owned();
        analyzed_for_closure
            .lock()
            .expect("lock")
            .push((stem, caps));
        ok_analyzer()(path, caps, opts, duration)
    });

    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store,
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));

    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("supervisor fills its pending batch");
    let key_only = AnalysisCapabilities {
        bpm: false,
        key: true,
        audio: false,
    };
    scheduler.request_explicit_run(explicit_work(vec![explicit_track.id], key_only));
    release_tx.send(()).expect("sweep waits for its release");

    let _tick = wait_for(
        &rx,
        Duration::from_secs(5),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 1),
    );
    let first = analyzed.lock().expect("lock").first().cloned();
    assert_eq!(first, Some(("explicit".to_owned(), key_only)));

    scheduler.shutdown();
}

#[test]
fn configured_bpm_range_reaches_the_analyzer() {
    // The BPM detection range is a DSP tunable resolved from the
    // analysis settings and baked into each worker at spawn. A
    // non-preset window in the settings must arrive at the analyzer
    // verbatim — widened from whole BPM to the f32 the DSP expects —
    // proving the settings → AnalysisOptions wiring.
    use std::sync::Mutex;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "tempo.flac");

    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track).expect("save track");

    let observed: Arc<Mutex<Vec<AnalysisOptions>>> = Arc::new(Mutex::new(Vec::new()));
    let observed_for_closure = observed.clone();
    let analyzer: AnalyzerFn = Arc::new(move |_path, _caps, opts, _duration| {
        observed_for_closure.lock().expect("lock").push(opts);
        ok_analyzer()(
            std::path::Path::new("ignored"),
            AnalysisCapabilities::all(),
            AnalysisOptions::default(),
            None,
        )
    });

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::new(60, 180),
        },
        Some(library_root),
    ));

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    let seen = observed.lock().expect("lock").clone();
    assert_eq!(seen.len(), 1, "the one track must be analyzed once");
    assert_eq!(seen[0].min_bpm, 60.0);
    assert_eq!(seen[0].max_bpm, 180.0);

    scheduler.shutdown();
}

fn idle_supervisor_state() -> super::SupervisorState {
    super::SupervisorState {
        settings: AnalysisSettings::default(),
        resource_usage: BackgroundResourceUsage::default(),
        library_path: None,
        completed: 0,
        failed: 0,
        audio_recorded: false,
        total: None,
        explicit_queue: std::collections::VecDeque::new(),
        pending_persistence_error: None,
    }
}

const BPM_ONLY: AnalysisCapabilities = AnalysisCapabilities {
    bpm: true,
    key: false,
    audio: false,
};
const KEY_ONLY: AnalysisCapabilities = AnalysisCapabilities {
    bpm: false,
    key: true,
    audio: false,
};
const BPM_AND_KEY: AnalysisCapabilities = AnalysisCapabilities {
    bpm: true,
    key: true,
    audio: false,
};

#[test]
fn a_second_capability_requested_for_a_queued_track_joins_the_first() {
    let track_id = TrackId::new(1).expect("valid track id");
    let mut state = idle_supervisor_state();
    let mut pool = None;

    for capabilities in [BPM_ONLY, KEY_ONLY] {
        super::apply_command(
            super::SchedulerCommand::ExplicitRun {
                work: explicit_work(vec![track_id], capabilities),
            },
            &mut state,
            &mut pool,
        );
    }

    assert_eq!(state.explicit_queue.len(), 1);
    assert_eq!(state.explicit_queue[0].capabilities, BPM_AND_KEY);
}

#[test]
fn promoting_a_capability_for_an_already_pending_track_joins_the_pending_item() {
    let track_id = TrackId::new(1).expect("valid track id");
    let mut state = idle_supervisor_state();
    state.explicit_queue.push_back(super::PendingItem {
        track_id,
        capabilities: KEY_ONLY,
        is_explicit: true,
    });
    let mut pending = std::collections::VecDeque::from([super::PendingItem {
        track_id,
        capabilities: BPM_ONLY,
        is_explicit: true,
    }]);

    super::drain_ready_explicit_work(&mut state, &mut pending, &super::InFlight::default());

    assert!(state.explicit_queue.is_empty());
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].capabilities, BPM_AND_KEY);
}

#[test]
fn a_full_batch_still_takes_a_new_capability_for_a_track_it_holds() {
    let mut state = idle_supervisor_state();
    let mut pending = (1..=super::BATCH_SIZE as i64)
        .map(|id| super::PendingItem {
            track_id: TrackId::new(id).expect("valid track id"),
            capabilities: BPM_ONLY,
            is_explicit: true,
        })
        .collect::<std::collections::VecDeque<_>>();
    let held = pending[0].track_id;
    state.explicit_queue.push_back(super::PendingItem {
        track_id: held,
        capabilities: KEY_ONLY,
        is_explicit: true,
    });

    super::drain_ready_explicit_work(&mut state, &mut pending, &super::InFlight::default());

    assert!(state.explicit_queue.is_empty());
    assert_eq!(pending.len(), super::BATCH_SIZE);
    assert_eq!(pending[0].capabilities, BPM_AND_KEY);
}

#[test]
fn returning_explicit_work_merges_with_a_newer_request_for_the_same_track() {
    let track_id = TrackId::new(1).expect("valid track id");
    let mut queue = std::collections::VecDeque::from([super::PendingItem {
        track_id,
        capabilities: KEY_ONLY,
        is_explicit: true,
    }]);
    let returning = [super::PendingItem {
        track_id,
        capabilities: BPM_ONLY,
        is_explicit: true,
    }];

    super::requeue_explicit_items(&mut queue, returning.into_iter());

    assert_eq!(queue.len(), 1);
    assert_eq!(queue[0].capabilities, BPM_AND_KEY);
}

fn settings_off() -> AnalysisSettings {
    AnalysisSettings {
        bpm: false,
        key: false,
        audio: false,
        bpm_range: BpmRange::default(),
    }
}

#[test]
fn explicit_work_whose_result_was_rejected_runs_again_on_wake() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let track_id = track.id;

    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    faulty.save_track(track).expect("save");
    faulty.set_fail_record_analysis(true);

    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    // Background analysis is off: only the explicit request can bring
    // the track back after the store recovers.
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1_700_000_000),
        store.clone(),
        settings_off(),
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(vec![track_id], BPM_ONLY));
    wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::PersistenceError { .. })
    });

    faulty.set_fail_record_analysis(false);
    scheduler.wake();
    let idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });
    scheduler.shutdown();

    assert!(matches!(
        idle,
        SchedulerProgress::Idle {
            completed: 1,
            failed: 0,
            audio_recorded: false,
        }
    ));
    assert_eq!(faulty.record_analysis_calls(), 2);
    let still_due = store
        .tracks_needing_analysis(BPM_ONLY, VERSIONS, 10)
        .map(due_ids)
        .expect("query");
    assert!(still_due.is_empty(), "the explicit BPM run landed");
}

/// A failed audio attempt still replaces what the track measured before,
/// so the run reports it: Smart Shuffle rebuilds on that report.
#[test]
fn a_failed_explicit_audio_run_reports_the_audio_it_recorded() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let track_id = track.id;
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track).expect("save");

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        err_analyzer(),
        sink,
        fixed_clock(1_700_000_000),
        store,
        settings_off(),
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(
        vec![track_id],
        AnalysisCapabilities {
            bpm: false,
            key: false,
            audio: true,
        },
    ));
    // The run's Idle, not the empty one the scheduler may report at
    // start-up before the request arrives.
    let idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { failed: 1, .. })
    });
    scheduler.shutdown();

    assert_eq!(
        idle,
        SchedulerProgress::Idle {
            completed: 0,
            failed: 1,
            audio_recorded: true,
        }
    );
}

#[test]
fn an_explicit_run_counts_its_first_batch_in_the_total() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let track_count = super::BATCH_SIZE as i64 + 5;
    let mut track_ids = Vec::new();
    for i in 0..track_count {
        let mut track = touch_in(&library_root, &format!("track_{i:03}.flac"));
        track.id = TrackId::new(i + 1).expect("non-zero");
        track_ids.push(track.id);
        store.save_track(track).expect("save");
    }

    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        ok_analyzer(),
        sink,
        fixed_clock(1),
        store,
        settings_off(),
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(track_ids, BPM_ONLY));
    let first_tick = wait_for(&rx, Duration::from_secs(5), |progress| {
        matches!(progress, SchedulerProgress::Tick { .. })
    });
    scheduler.shutdown();

    let SchedulerProgress::Tick { total, .. } = first_tick else {
        unreachable!("wait_for matched a Tick");
    };
    assert_eq!(i64::from(total), track_count);
}

#[test]
fn a_key_version_bump_runs_only_the_key_pass() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track.clone()).expect("save");
    let analysis = ok_analyzer()(
        std::path::Path::new("ignored"),
        AnalysisCapabilities::all(),
        AnalysisOptions::default(),
        None,
    )
    .expect("stub analysis");
    store
        .record_analysis(
            track.id,
            &analysis,
            AnalysisCapabilities::all(),
            sustain_library_store::AnalysisContext {
                versions: VERSIONS,
                now_unix: 1,
            },
        )
        .expect("analyse everything at the current versions");

    let requested = Arc::new(Mutex::new(Vec::new()));
    let analyzer: AnalyzerFn = {
        let requested = requested.clone();
        Arc::new(move |path, capabilities, options, duration| {
            requested.lock().expect("lock").push(capabilities);
            ok_analyzer()(path, capabilities, options, duration)
        })
    };
    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(AnalysisSchedulerConfig {
        analyzer_versions: AnalyzerVersions { key: 2, ..VERSIONS },
        ..deterministic_config(
            analyzer,
            sink,
            fixed_clock(2),
            store,
            AnalysisSettings {
                bpm: true,
                key: true,
                audio: true,
                bpm_range: BpmRange::default(),
            },
            Some(library_root),
        )
    });
    wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { completed: 1, .. })
    });
    scheduler.shutdown();

    assert_eq!(*requested.lock().expect("lock"), vec![KEY_ONLY]);
}

#[test]
fn writes_rejected_while_the_sweep_stops_do_not_stop_the_next_wake() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    let mut track_ids = Vec::new();
    for index in 0..2_i64 {
        let mut track = touch_in(&library_root, &format!("track_{index}.flac"));
        track.id = TrackId::new(index + 1).expect("non-zero");
        track_ids.push(track.id);
        faulty.save_track(track).expect("save");
    }
    faulty.set_fail_record_analysis(true);

    // The first analysis is held until the second track has been handed
    // to the pool, so the second write is rejected while the supervisor
    // joins the pool to report the first.
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let release_rx = Mutex::new(Some(release_rx));
    let analyzer: AnalyzerFn = Arc::new(move |path, caps, opts, duration| {
        let held = release_rx.lock().expect("lock release").take();
        if let Some(release) = held {
            release.recv().expect("test releases the first analysis");
        }
        ok_analyzer()(path, caps, opts, duration)
    });
    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store,
        settings_off(),
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(track_ids, BPM_ONLY));
    std::thread::sleep(Duration::from_millis(200));
    release_tx.send(()).expect("analysis waits for its release");
    wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::PersistenceError { .. })
    });
    assert_eq!(faulty.record_analysis_calls(), 2);

    faulty.set_fail_record_analysis(false);
    scheduler.wake();
    let after_wake = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(
            progress,
            SchedulerProgress::Idle { .. } | SchedulerProgress::PersistenceError { .. }
        )
    });
    scheduler.shutdown();

    assert!(matches!(
        after_wake,
        SchedulerProgress::Idle {
            completed: 2,
            failed: 0,
            audio_recorded: false,
        }
    ));
}

#[test]
fn a_settings_change_before_the_first_dispatch_fetches_the_batch_again() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    let analysis = ok_analyzer()(
        std::path::Path::new("ignored"),
        AnalysisCapabilities::all(),
        AnalysisOptions::default(),
        None,
    )
    .expect("stub analysis");
    let context = sustain_library_store::AnalysisContext {
        versions: VERSIONS,
        now_unix: 1,
    };
    // A full batch due only for BPM, then one track due only for Key.
    let bpm_due_count = super::BATCH_SIZE as i64;
    let mut key_due = None;
    for index in 0..=bpm_due_count {
        let mut track = touch_in(&library_root, &format!("track_{index:03}.flac"));
        track.id = TrackId::new(index + 1).expect("non-zero");
        let done = if index < bpm_due_count {
            KEY_ONLY
        } else {
            key_due = Some(track.id);
            BPM_ONLY
        };
        faulty.save_track(track.clone()).expect("save");
        faulty
            .record_analysis(track.id, &analysis, done, context)
            .expect("record the pass already done");
    }

    // The first sweep counts the run's total; the second fills `pending`
    // and is held until BPM has been turned off.
    let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let release_rx = Mutex::new(release_rx);
    let sweeps = AtomicU32::new(0);
    faulty.run_in_analysis_sweeps(move || {
        if sweeps.fetch_add(1, Ordering::SeqCst) == 1 {
            entered_tx.send(()).expect("test waits for the sweep");
            release_rx
                .lock()
                .expect("lock release")
                .recv()
                .expect("test releases the sweep");
        }
        Ok(())
    });
    let analyzed = Arc::new(Mutex::new(Vec::new()));
    let analyzer: AnalyzerFn = {
        let analyzed = analyzed.clone();
        Arc::new(move |path, caps, opts, duration| {
            analyzed.lock().expect("lock").push(caps);
            ok_analyzer()(path, caps, opts, duration)
        })
    };
    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let key_only_settings = AnalysisSettings {
        bpm: false,
        key: true,
        audio: false,
        bpm_range: BpmRange::default(),
    };
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(2),
        store.clone(),
        AnalysisSettings {
            bpm: true,
            ..key_only_settings
        },
        Some(library_root),
    ));
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("supervisor fills its pending batch");
    scheduler.update_settings(key_only_settings);
    release_tx.send(()).expect("sweep waits for its release");

    wait_for(&rx, Duration::from_secs(5), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });
    scheduler.shutdown();

    assert_eq!(*analyzed.lock().expect("lock"), vec![KEY_ONLY]);
    let still_due = store
        .tracks_needing_analysis(KEY_ONLY, VERSIONS, 10)
        .map(due_ids)
        .expect("query");
    assert!(
        !still_due.contains(&key_due.expect("key-due track saved")),
        "the Key-due track was analysed"
    );
}

#[test]
fn a_second_request_for_a_running_track_does_not_repeat_its_pass() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");
    let track_id = track.id;
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    store.save_track(track).expect("save");

    // The first analysis is held until the same request has been made
    // again, as a user re-requesting a track still being analysed would.
    let (entered_tx, entered_rx) = std_mpsc::channel::<()>();
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let hold = Mutex::new(Some((entered_tx, release_rx)));
    let calls = Arc::new(AtomicU32::new(0));
    let analyzer: AnalyzerFn = {
        let calls = calls.clone();
        Arc::new(move |path, caps, opts, duration| {
            calls.fetch_add(1, Ordering::SeqCst);
            let held = hold.lock().expect("lock hold").take();
            if let Some((entered, release)) = held {
                entered.send(()).expect("test waits for the analysis");
                release.recv().expect("test releases the analysis");
            }
            ok_analyzer()(path, caps, opts, duration)
        })
    };
    let (sink, rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(1),
        store,
        settings_off(),
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(vec![track_id], BPM_ONLY));
    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the first request is being analysed");
    scheduler.request_explicit_run(explicit_work(vec![track_id], BPM_ONLY));
    release_tx.send(()).expect("analysis waits for its release");

    // The worker may have gone idle once, on nothing, before the first
    // request reached it: the Idle awaited is the one after the run.
    let idle = wait_for(
        &rx,
        Duration::from_secs(2),
        |progress| matches!(progress, SchedulerProgress::Idle { completed, failed, .. } if completed + failed > 0),
    );
    scheduler.shutdown();

    assert!(matches!(
        idle,
        SchedulerProgress::Idle {
            completed: 1,
            failed: 0,
            audio_recorded: false,
        }
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn a_sweep_failure_that_stops_the_run_settles_its_rejected_writes_first() {
    use crate::test_store::FaultyStore;
    use sustain_library_store::StoreError;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let inner: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store"));
    let faulty = Arc::new(FaultyStore::new(inner));
    let analysis = ok_analyzer()(
        std::path::Path::new("ignored"),
        AnalysisCapabilities::all(),
        AnalysisOptions::default(),
        None,
    )
    .expect("stub analysis");
    // The explicit track needs only Key; the background track only BPM.
    let mut explicit_track = touch_in(&library_root, "explicit.flac");
    explicit_track.id = TrackId::new(1).expect("non-zero");
    let mut background_track = touch_in(&library_root, "background.flac");
    background_track.id = TrackId::new(2).expect("non-zero");
    faulty.save_track(explicit_track.clone()).expect("save");
    faulty.save_track(background_track).expect("save");
    faulty
        .record_analysis(
            explicit_track.id,
            &analysis,
            BPM_ONLY,
            sustain_library_store::AnalysisContext {
                versions: VERSIONS,
                now_unix: 1,
            },
        )
        .expect("record the BPM pass already done");
    faulty.set_fail_record_analysis(true);

    // The first analysis is held until the third sweep, which runs once
    // both tracks are in the pool, and fails: the run stops on the sweep
    // failure while the held write is still to be rejected.
    let (release_tx, release_rx) = std_mpsc::channel::<()>();
    let release_rx = Mutex::new(Some(release_rx));
    let analyzer: AnalyzerFn = Arc::new(move |path, caps, opts, duration| {
        let held = release_rx.lock().expect("lock release").take();
        if let Some(release) = held {
            release
                .recv()
                .expect("the failing sweep releases the analysis");
        }
        ok_analyzer()(path, caps, opts, duration)
    });
    let release_tx = Mutex::new(Some(release_tx));
    let sweeps = AtomicU32::new(0);
    let sweeps_fail = Arc::new(AtomicBool::new(true));
    let sweeps_fail_in_hook = sweeps_fail.clone();
    faulty.run_in_analysis_sweeps(move || {
        if sweeps.fetch_add(1, Ordering::SeqCst) < 2 || !sweeps_fail_in_hook.load(Ordering::SeqCst)
        {
            return Ok(());
        }
        let held = release_tx.lock().expect("lock release").take();
        if let Some(release) = held {
            release
                .send(())
                .expect("the analysis waits for its release");
        }
        Err(StoreError::StoreUnavailable)
    });

    let (sink, rx) = capturing_sink();
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let scheduler = AnalysisScheduler::start(deterministic_config(
        analyzer,
        sink,
        fixed_clock(2),
        store,
        AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
            bpm_range: BpmRange::default(),
        },
        Some(library_root),
    ));
    scheduler.request_explicit_run(explicit_work(vec![explicit_track.id], KEY_ONLY));
    wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::PersistenceError { .. })
    });

    faulty.set_fail_record_analysis(false);
    sweeps_fail.store(false, Ordering::SeqCst);
    scheduler.wake();
    let after_wake = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(
            progress,
            SchedulerProgress::Idle { .. } | SchedulerProgress::PersistenceError { .. }
        )
    });
    scheduler.shutdown();

    assert!(
        matches!(after_wake, SchedulerProgress::Idle { .. }),
        "the wake after recovery runs the work instead of reporting the old rejection"
    );
}
