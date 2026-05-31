// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Background scheduler for device syncs.
//!
//! A sync copies (potentially many gigabytes of) audio to an external
//! drive, so it runs on a dedicated worker thread, never the GTK main
//! loop. The worker streams [`SyncProgress`] events and a final
//! completion through an `async_channel` the UI shell drains on idle and
//! feeds back into the runtime. Only one sync runs at a time;
//! cancellation is cooperative via a shared flag the engine polls
//! between files. Mirrors [`crate::smart_shuffle_scheduler`]; the index
//! is owned by the runtime, the device manifest is persisted by the
//! runtime on completion — the scheduler is purely "run it off-thread".

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use sustain_device_sync::{
    PreparedSyncRequest, SourceSnapshot, SyncOutcome, SyncProgress, SyncRequest, engine,
    resolve_source_fingerprint,
};
use sustain_domain::SyncDeviceId;
use sustain_library_store::LibraryStore;

/// An event published by the sync worker.
#[derive(Debug)]
pub enum DeviceSyncEvent {
    Progress(SyncProgress),
    Finished(DeviceSyncCompletion),
}

/// The final result of a sync run. The error is stringified because the
/// engine's `SyncError` carries non-`Clone` sources; the runtime only
/// needs the message for its notification.
#[derive(Debug)]
pub struct DeviceSyncCompletion {
    pub device_id: SyncDeviceId,
    pub result: Result<SyncOutcome, String>,
}

pub struct DeviceSyncScheduler {
    event_sender: async_channel::Sender<DeviceSyncEvent>,
    event_receiver: async_channel::Receiver<DeviceSyncEvent>,
    is_syncing: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
}

impl DeviceSyncScheduler {
    pub fn new() -> Self {
        let (tx, rx) = async_channel::unbounded();
        Self {
            event_sender: tx,
            event_receiver: rx,
            is_syncing: Arc::new(AtomicBool::new(false)),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }

    /// Event channel the UI shell drains on the main loop.
    pub fn event_receiver(&self) -> async_channel::Receiver<DeviceSyncEvent> {
        self.event_receiver.clone()
    }

    pub fn is_syncing(&self) -> bool {
        self.is_syncing.load(Ordering::Acquire)
    }

    /// True once cancellation has been requested for the in-flight sync
    /// but the worker has not yet wound down. The engine polls the cancel
    /// flag only at file boundaries, so there is a window — potentially a
    /// whole multi-gigabyte file long — between the click and the sync
    /// actually stopping; the status lane reads this to show
    /// "Cancelling..." during it so the user sees their click landed.
    /// Gated on `is_syncing` so a cancel flag left set from a previous run
    /// (it is only reset when the next sync starts) never reports a phantom
    /// cancelling state while idle.
    pub fn is_cancelling(&self) -> bool {
        self.is_syncing.load(Ordering::Acquire) && self.cancel.load(Ordering::Acquire)
    }

    /// Ask the in-flight sync to stop at the next file boundary.
    pub fn request_cancellation(&self) {
        self.cancel.store(true, Ordering::Release);
    }

    /// Spawn a sync on a background thread. Returns `false` when a sync
    /// is already running (the request is dropped — only one device
    /// syncs at a time).
    pub fn start(
        &self,
        device_id: SyncDeviceId,
        request: SyncRequest,
        library_store: Arc<dyn LibraryStore>,
    ) -> bool {
        if self
            .is_syncing
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return false;
        }
        self.cancel.store(false, Ordering::Release);
        let sender = self.event_sender.clone();
        let flag = self.is_syncing.clone();
        let cancel = self.cancel.clone();
        std::thread::spawn(move || {
            let progress_sender = sender.clone();
            let mut on_progress = |progress: SyncProgress| {
                let _ = progress_sender.send_blocking(DeviceSyncEvent::Progress(progress));
            };
            let cancelled = || cancel.load(Ordering::Acquire);
            let result = prepare_sync_request(request, library_store.as_ref())
                .and_then(|request| engine::sync(&request, &mut on_progress, &cancelled))
                .map_err(|e| e.to_string());
            flag.store(false, Ordering::Release);
            let _ = sender.send_blocking(DeviceSyncEvent::Finished(DeviceSyncCompletion {
                device_id,
                result,
            }));
        });
        true
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idle_scheduler_never_reports_cancelling() {
        let scheduler = DeviceSyncScheduler::new();
        assert!(!scheduler.is_syncing());
        assert!(!scheduler.is_cancelling());
        // A cancel request while no sync is running must not produce a
        // phantom "Cancelling..." state: the flag stays set until the next
        // sync starts, and `is_cancelling` gates on `is_syncing` precisely
        // so a stale flag cannot leak into the idle status lane.
        scheduler.request_cancellation();
        assert!(!scheduler.is_cancelling());
    }
}
