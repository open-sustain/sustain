// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Background scheduler for device syncs.
//!
//! A sync copies (potentially many gigabytes of) audio to an external
//! drive, so it runs on a dedicated worker thread, never the GTK main
//! loop. The scheduler owns that worker until the runtime applies its
//! identified completion. Keeping the run active through completion
//! application prevents a late event from one run mutating the
//! notification or manifest state of a newer run.

use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::thread::{self, JoinHandle};

use sustain_device_sync::{
    PreparedSyncRequest, SourceSnapshot, SyncOutcome, SyncProgress, SyncRequest, engine,
    resolve_source_fingerprint,
};
use sustain_domain::SyncDeviceId;
use sustain_library_store::LibraryStore;

/// Monotonically increasing identity for one accepted device-sync run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DeviceSyncRunId(u64);

/// An event published by the sync worker.
#[derive(Debug)]
pub enum DeviceSyncEvent {
    Progress {
        run_id: DeviceSyncRunId,
        progress: SyncProgress,
    },
    Finished {
        run_id: DeviceSyncRunId,
        completion: DeviceSyncCompletion,
    },
}

/// The final result of a sync run. The error is stringified because the
/// engine's `SyncError` carries non-`Clone` sources; the runtime only
/// needs the message for its notification.
#[derive(Debug)]
pub struct DeviceSyncCompletion {
    pub device_id: SyncDeviceId,
    pub result: Result<SyncOutcome, String>,
}

/// Typed result of asking the scheduler to start a run.
#[derive(Debug, Eq, PartialEq)]
pub enum DeviceSyncStartOutcome {
    Started(DeviceSyncRunId),
    AlreadyRunning,
    SpawnFailed(String),
}

type SyncTask = Box<
    dyn FnOnce(&mut dyn FnMut(SyncProgress), &dyn Fn() -> bool) -> Result<SyncOutcome, String>
        + Send,
>;

struct ActiveRun {
    id: DeviceSyncRunId,
    worker: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct SchedulerState {
    next_run_id: u64,
    active: Option<ActiveRun>,
}

pub struct DeviceSyncScheduler {
    event_sender: async_channel::Sender<DeviceSyncEvent>,
    event_receiver: async_channel::Receiver<DeviceSyncEvent>,
    state: Mutex<SchedulerState>,
    cancel: Arc<AtomicBool>,
}

impl DeviceSyncScheduler {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            event_sender: tx,
            event_receiver: rx,
            state: Mutex::new(SchedulerState::default()),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Event channel the UI shell drains on the main loop.
    pub fn event_receiver(&self) -> async_channel::Receiver<DeviceSyncEvent> {
        self.event_receiver.clone()
    }

    pub fn is_syncing(&self) -> bool {
        self.state
            .lock()
            .expect("device-sync scheduler state poisoned")
            .active
            .is_some()
    }

    pub fn is_active_run(&self, run_id: DeviceSyncRunId) -> bool {
        self.state
            .lock()
            .expect("device-sync scheduler state poisoned")
            .active
            .as_ref()
            .is_some_and(|active| active.id == run_id)
    }

    /// True once cancellation has been requested for the in-flight sync
    /// but the worker has not yet wound down.
    pub fn is_cancelling(&self) -> bool {
        self.is_syncing() && self.cancel.load(Ordering::Acquire)
    }

    /// Ask the in-flight sync to stop at the next file boundary.
    pub fn request_cancellation(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// Spawn a sync on a background thread. The run remains active after
    /// the worker publishes `Finished`; the runtime must acknowledge that
    /// identified completion after applying it.
    pub fn start(
        &self,
        device_id: SyncDeviceId,
        request: SyncRequest,
        library_store: Arc<dyn LibraryStore>,
    ) -> DeviceSyncStartOutcome {
        self.start_task(
            device_id,
            Box::new(move |progress, cancel| {
                prepare_sync_request(request, library_store.as_ref())
                    .and_then(|request| engine::sync(&request, progress, cancel))
                    .map_err(|error| error.to_string())
            }),
        )
    }

    fn start_task(&self, device_id: SyncDeviceId, task: SyncTask) -> DeviceSyncStartOutcome {
        let mut state = self
            .state
            .lock()
            .expect("device-sync scheduler state poisoned");
        if state.active.is_some() {
            return DeviceSyncStartOutcome::AlreadyRunning;
        }

        state.next_run_id = state
            .next_run_id
            .checked_add(1)
            .expect("device-sync run id space exhausted");
        let run_id = DeviceSyncRunId(state.next_run_id);
        state.active = Some(ActiveRun {
            id: run_id,
            worker: None,
        });
        self.cancel.store(false, Ordering::Release);

        let sender = self.event_sender.clone();
        let cancel = self.cancel.clone();
        let spawn_result = thread::Builder::new()
            .name("sustain-device-sync".to_owned())
            .spawn(move || {
                let progress_sender = sender.clone();
                let mut on_progress = |progress: SyncProgress| {
                    let _ = progress_sender
                        .send_blocking(DeviceSyncEvent::Progress { run_id, progress });
                };
                let cancelled = || cancel.load(Ordering::Acquire);
                let result = task(&mut on_progress, &cancelled);
                let _ = sender.send_blocking(DeviceSyncEvent::Finished {
                    run_id,
                    completion: DeviceSyncCompletion { device_id, result },
                });
            });

        match spawn_result {
            Ok(worker) => {
                state
                    .active
                    .as_mut()
                    .expect("accepted device-sync run remains active")
                    .worker = Some(worker);
                DeviceSyncStartOutcome::Started(run_id)
            }
            Err(error) => {
                state.active = None;
                DeviceSyncStartOutcome::SpawnFailed(error.to_string())
            }
        }
    }

    /// Release one finished run after the runtime has applied its final
    /// event. Returns `false` for stale completions.
    pub fn acknowledge_completion(&self, run_id: DeviceSyncRunId) -> bool {
        let mut state = self
            .state
            .lock()
            .expect("device-sync scheduler state poisoned");
        if state.active.as_ref().map(|active| active.id) != Some(run_id) {
            return false;
        }
        if let Some(worker) = state
            .active
            .as_mut()
            .and_then(|active| active.worker.take())
        {
            let _ = worker.join();
        }
        state.active = None;
        true
    }

    /// Cancel and join an in-flight worker before runtime teardown returns.
    /// Any queued events are obsolete after the active run is cleared.
    pub fn shutdown(&self) {
        self.request_cancellation();
        let mut state = self
            .state
            .lock()
            .expect("device-sync scheduler state poisoned");
        if let Some(worker) = state
            .active
            .as_mut()
            .and_then(|active| active.worker.take())
        {
            let _ = worker.join();
        }
        state.active = None;
        while self.event_receiver.try_recv().is_ok() {}
    }

    #[cfg(test)]
    pub(crate) fn start_test_task(
        &self,
        device_id: SyncDeviceId,
        task: impl FnOnce(
            &mut dyn FnMut(SyncProgress),
            &dyn Fn() -> bool,
        ) -> Result<SyncOutcome, String>
        + Send
        + 'static,
    ) -> DeviceSyncStartOutcome {
        self.start_task(device_id, Box::new(task))
    }
}

fn prepare_sync_request(
    mut request: SyncRequest,
    library_store: &dyn LibraryStore,
) -> Result<PreparedSyncRequest, sustain_device_sync::SyncError> {
    for track in &mut request.tracks {
        let cached = library_store
            .source_fingerprint(track.track_id)
            .map_err(|error| sustain_device_sync::SyncError::Preparation(format!("{error:?}")))?;
        let fingerprint = resolve_source_fingerprint(&track.source_path, cached.as_ref())
            .map_err(|error| sustain_device_sync::SyncError::io(&track.source_path, error))?;
        library_store
            .save_source_fingerprint(track.track_id, &fingerprint)
            .map_err(|error| sustain_device_sync::SyncError::Preparation(format!("{error:?}")))?;
        track.source = SourceSnapshot::resolved(fingerprint);
    }
    PreparedSyncRequest::new(request)
}

impl Default for DeviceSyncScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DeviceSyncScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    };
    use std::time::Duration;

    use sustain_device_sync::{SyncOutcome, SyncProgress, SyncStage};
    use sustain_domain::SyncDeviceId;

    use super::{DeviceSyncEvent, DeviceSyncScheduler, DeviceSyncStartOutcome};

    fn device_id() -> SyncDeviceId {
        SyncDeviceId::new("device-id").expect("device id")
    }

    #[test]
    fn idle_scheduler_never_reports_cancelling() {
        let scheduler = DeviceSyncScheduler::new();
        assert!(!scheduler.is_syncing());
        assert!(!scheduler.is_cancelling());
        scheduler.request_cancellation();
        assert!(!scheduler.is_cancelling());
    }

    #[test]
    fn completion_keeps_the_scheduler_busy_until_acknowledged() {
        let scheduler = DeviceSyncScheduler::new();
        let first =
            scheduler.start_test_task(device_id(), |_progress, _cancel| Ok(SyncOutcome::default()));
        assert!(matches!(first, DeviceSyncStartOutcome::Started(_)));
        let DeviceSyncStartOutcome::Started(first_id) = first else {
            return;
        };
        let event = scheduler
            .event_receiver()
            .recv_blocking()
            .expect("completion event");
        assert!(matches!(
            event,
            DeviceSyncEvent::Finished {
                run_id,
                completion: _
            } if run_id == first_id
        ));
        assert!(scheduler.is_syncing());
        assert_eq!(
            scheduler.start_test_task(device_id(), |_progress, _cancel| {
                Ok(SyncOutcome::default())
            }),
            DeviceSyncStartOutcome::AlreadyRunning
        );
        assert!(scheduler.acknowledge_completion(first_id));
        assert!(!scheduler.is_syncing());
    }

    #[test]
    fn progress_and_completion_carry_the_same_run_id() {
        let scheduler = DeviceSyncScheduler::new();
        let started = scheduler.start_test_task(device_id(), |progress, _cancel| {
            progress(SyncProgress {
                stage: SyncStage::Copying,
                completed: 1,
                total: 1,
            });
            Ok(SyncOutcome::default())
        });
        assert!(matches!(started, DeviceSyncStartOutcome::Started(_)));
        let DeviceSyncStartOutcome::Started(run_id) = started else {
            return;
        };
        assert!(matches!(
            scheduler.event_receiver().recv_blocking(),
            Ok(DeviceSyncEvent::Progress {
                run_id: event_run_id,
                progress: _
            }) if event_run_id == run_id
        ));
        assert!(matches!(
            scheduler.event_receiver().recv_blocking(),
            Ok(DeviceSyncEvent::Finished {
                run_id: event_run_id,
                completion: _
            }) if event_run_id == run_id
        ));
        assert!(scheduler.acknowledge_completion(run_id));
    }

    #[test]
    fn shutdown_requests_cancellation_and_joins_the_worker() {
        let scheduler = DeviceSyncScheduler::new();
        let cancellation_observed = Arc::new(AtomicBool::new(false));
        let observed_for_worker = cancellation_observed.clone();
        let (started_tx, started_rx) = mpsc::channel();
        assert!(matches!(
            scheduler.start_test_task(device_id(), move |_progress, cancel| {
                started_tx.send(()).expect("signal worker start");
                while !cancel() {
                    std::thread::sleep(Duration::from_millis(1));
                }
                observed_for_worker.store(true, Ordering::Release);
                Ok(SyncOutcome::default())
            }),
            DeviceSyncStartOutcome::Started(_)
        ));
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("worker started");

        scheduler.shutdown();

        assert!(cancellation_observed.load(Ordering::Acquire));
        assert!(!scheduler.is_syncing());
        assert!(scheduler.event_receiver().try_recv().is_err());
    }
}
