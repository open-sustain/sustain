// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    fs::File,
    io::Write,
    sync::{
        Arc,
        atomic::{AtomicU32, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, Instant},
};

use sustain_analysis::{AnalysisError, AnalysisOptions, TrackAnalysis};
use sustain_domain::{
    AnalysisSettings, BackgroundResourceUsage, MusicalKey, PREVIEW_SEGMENT_COUNT, Track,
    TrackLocation, TrackRelativePath, WaveformSegment, WaveformSegments,
};
use sustain_library_store::{AnalysisCapabilities, InMemoryLibraryStore, LibraryStore, TrackId};
use tempfile::TempDir;

use super::{
    AnalysisScheduler, AnalysisSchedulerConfig, AnalyzerFn, ProgressSink, SchedulerProgress,
    UnixClockFn,
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
            bpm: Some(120.0),
            key: Some(MusicalKey::CMajor),
            beatgrid: None,
            waveform_preview: WaveformSegments {
                segment_duration_ms: 25.0,
                segments: vec![WaveformSegment::silent(); PREVIEW_SEGMENT_COUNT],
            },
            waveform_detail: WaveformSegments {
                segment_duration_ms: 6.67,
                segments: vec![WaveformSegment::silent(); 512],
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
        library_path,
        analyzer_version: 1,
        analysis_options: AnalysisOptions::default(),
    }
}

#[test]
fn scheduler_processes_pending_tracks_with_settings_enabled() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    // analyzer_version.
    let still_pending = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 100)
        .expect("query");
    assert!(still_pending.is_empty(), "track should be marked attempted");

    scheduler.shutdown();
}

#[test]
fn scheduler_records_failures_without_clobbering_run() {
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "broken.flac");

    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    // re-queue at the same analyzer_version.
    let pending = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            1,
            10,
        )
        .expect("query");
    assert!(pending.is_empty());

    scheduler.shutdown();
}

#[test]
fn rejected_record_analysis_halts_without_false_completion() {
    use crate::test_store::FaultyStore;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let inner: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
        .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 10)
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
        .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 10)
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

    let inner: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
            1,
            100,
        )
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
            1,
            100,
        )
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    use std::sync::mpsc as std_mpsc;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let track = touch_in(&library_root, "alpha.flac");

    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    store.save_track(track.clone()).expect("save track");

    let (notify_tx, notify_rx) = std_mpsc::channel::<sustain_domain::TrackId>();
    let track_updated: super::TrackUpdatedSink = Arc::new(move |id| {
        let _ = notify_tx.send(id);
    });

    let (sink, _rx) = capturing_sink();
    let scheduler = AnalysisScheduler::start(AnalysisSchedulerConfig {
        analyzer: ok_analyzer(),
        progress: sink,
        track_updated: Some(track_updated),
        clock: fixed_clock(1),
        library_store: store,
        initial_settings: AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
        },
        initial_resource_usage: BackgroundResourceUsage::Innocuous,
        library_path: Some(library_root),
        analyzer_version: 1,
        analysis_options: AnalysisOptions::default(),
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
        .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 100)
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
    // Regression guard for the "6-second idle gap between
    // batches" we saw under the old supervisor: it waited for
    // `in_flight == 0` before re-querying the store, so the
    // tail end of every BATCH_SIZE-sized burst left N-2
    // workers spinning. The streaming dispatcher must keep
    // dispatching across the boundary — concretely, on a pool
    // with capacity > BATCH_SIZE, we should never see all
    // workers go simultaneously idle until the whole library
    // is done.
    //
    // We simulate that by pre-populating BATCH_SIZE*3 tracks
    // and verifying that the analyzer call rate stays smooth
    // through the run, i.e. there is never a window during
    // which `analyzer_call_count` plateaus for more than the
    // per-track slowdown the stub introduces.
    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());

    let track_count = (super::BATCH_SIZE as i64) * 3;
    for i in 0..track_count {
        let relative = format!("track_{i:03}.flac");
        let mut track = touch_in(&library_root, &relative);
        track.id = TrackId::new(i + 1).expect("non-zero");
        store.save_track(track).expect("save");
    }

    // Stub analyzer with a fixed per-track delay. The delay is
    // long enough that several outcomes overlap with the
    // dispatcher's outer loop, which is exactly when the old
    // gated-on-in-flight logic stalled.
    use std::sync::Mutex;
    let call_times: Arc<Mutex<Vec<Instant>>> = Arc::new(Mutex::new(Vec::new()));
    let recorded_times = call_times.clone();
    let analyzer: AnalyzerFn = Arc::new(move |_path, _caps, _opts, _duration| {
        if let Ok(mut guard) = recorded_times.lock() {
            guard.push(Instant::now());
        }
        std::thread::sleep(Duration::from_millis(40));
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
            key: true,
            audio: true,
        },
        Some(library_root),
    ));

    // Drain to Idle so we know every track was processed.
    let _ = wait_for(
        &rx,
        Duration::from_secs(30),
        |progress| matches!(progress, SchedulerProgress::Idle { completed, .. } if *completed as i64 == track_count),
    );
    scheduler.shutdown();

    // Inspect the dispatch cadence: pick the largest gap
    // between consecutive analyzer entries; on the old
    // gated-batched supervisor this gap was the batch tail
    // (≈ inter-track-sleep * inflight_count). On the
    // streaming dispatcher it should be at most one
    // inter-track sleep plus a touch of supervisor overhead.
    // We assert "no gap larger than 5x the per-track sleep",
    // which is a conservative regression bound — the old
    // behavior under a BATCH_SIZE=16 / 1-worker (Innocuous)
    // run produced gaps proportional to the whole batch.
    let times = call_times.lock().expect("lock");
    assert_eq!(
        times.len(),
        track_count as usize,
        "analyzer should be called once per track",
    );
    let mut max_gap = Duration::ZERO;
    for window in times.windows(2) {
        let gap = window[1].duration_since(window[0]);
        if gap > max_gap {
            max_gap = gap;
        }
    }
    let bound = Duration::from_millis(40 * 5);
    assert!(
        max_gap <= bound,
        "dispatch cadence stalled: max gap {:?} exceeded {:?}",
        max_gap,
        bound,
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
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
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
        },
        Some(library_root),
    ));

    // Without the explicit request the scheduler should be idle
    // (no capabilities, nothing to do). Submit the per-playlist
    // batch and expect both tracks to be processed.
    scheduler.request_explicit_run(
        vec![track_a.id, track_b.id],
        AnalysisCapabilities {
            bpm: false,
            key: false,
            audio: true,
        },
    );

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
    // explicit batch must reach the workers first.
    use std::sync::Mutex;

    let temp = TempDir::new().expect("temp dir");
    let library_root = temp.path().to_path_buf();
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());

    // Five background tracks the settings will sweep up.
    for index in 0..5_i64 {
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
    let order_for_closure = order.clone();
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
        },
        Some(library_root),
    ));

    // The five background tracks are already eligible. Submit
    // the explicit run; the supervisor must put it ahead of the
    // queue.
    scheduler.request_explicit_run(
        vec![explicit_track.id],
        AnalysisCapabilities {
            bpm: false,
            key: true,
            audio: false,
        },
    );

    // Wait until everything is done.
    let _tick = wait_for(
        &rx,
        Duration::from_secs(5),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 6),
    );

    let seen = order.lock().expect("lock").clone();
    let explicit_position = seen
        .iter()
        .position(|id| *id == explicit_track.id)
        .expect("explicit track was processed");
    // A background track may slip ahead if it reached the worker
    // queue before the explicit command arrived: that race is
    // bounded by the queue capacity (worker_count + 1). With one
    // worker that's at most one background track.
    assert!(
        explicit_position <= 1,
        "explicit run must reach the workers immediately, saw position {explicit_position} of {}",
        seen.len()
    );

    scheduler.shutdown();
}
