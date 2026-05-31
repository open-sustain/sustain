// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Coalescing background scheduler for removable-device sync plans.
//!
//! The GTK caller submits a cheap closure. One owned worker performs the
//! SQLite reads, source-file stats, removable-filesystem probes, and plan
//! diff. While it runs, repeated toggles replace one pending slot with the
//! newest generation instead of building an unbounded queue of obsolete
//! plans.

use std::path::PathBuf;
use std::sync::{
    Arc, Condvar, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
};
use std::thread::{self, JoinHandle};

use sustain_device_sync::{DeviceCapacity, SyncPlan};
use sustain_domain::SyncDeviceId;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DevicePlanGeneration(u64);

impl DevicePlanGeneration {
    pub(crate) const fn new(value: u64) -> Self {
        Self(value)
    }

    const fn value(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceMountIdentity {
    pub device_id: SyncDeviceId,
    pub mount_path: PathBuf,
    pub volume_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DevicePlanSnapshot {
    pub plan: Option<SyncPlan>,
    pub capacity: DeviceCapacity,
}

#[derive(Debug)]
pub struct DevicePlanResult {
    pub generation: DevicePlanGeneration,
    pub mount: DeviceMountIdentity,
    pub result: Result<DevicePlanSnapshot, String>,
}

pub(crate) type DevicePlanTask =
    Box<dyn FnOnce(&dyn Fn() -> bool) -> Option<Result<DevicePlanSnapshot, String>> + Send>;

struct DevicePlanJob {
    generation: DevicePlanGeneration,
    mount: DeviceMountIdentity,
    task: DevicePlanTask,
}

#[derive(Default)]
struct WorkerState {
    pending: Option<DevicePlanJob>,
}

struct SharedState {
    worker: Mutex<WorkerState>,
    wake: Condvar,
    shutdown: AtomicBool,
    latest_generation: AtomicU64,
}

pub struct DevicePlanScheduler {
    result_receiver: async_channel::Receiver<DevicePlanResult>,
    shared: Arc<SharedState>,
    worker: Mutex<Option<JoinHandle<()>>>,
    spawn_error: Option<String>,
}

impl DevicePlanScheduler {
    pub fn new() -> Self {
        let (result_sender, result_receiver) = async_channel::unbounded();
        let shared = Arc::new(SharedState {
            worker: Mutex::new(WorkerState::default()),
            wake: Condvar::new(),
            shutdown: AtomicBool::new(false),
            latest_generation: AtomicU64::new(0),
        });
        let shared_for_worker = shared.clone();
        let spawn = thread::Builder::new()
            .name("sustain-device-plan".to_owned())
            .spawn(move || worker_loop(&shared_for_worker, &result_sender));
        let (worker, spawn_error) = match spawn {
            Ok(worker) => (Some(worker), None),
            Err(error) => (None, Some(error.to_string())),
        };
        Self {
            result_receiver,
            shared,
            worker: Mutex::new(worker),
            spawn_error,
        }
    }

    pub fn result_receiver(&self) -> async_channel::Receiver<DevicePlanResult> {
        self.result_receiver.clone()
    }

    pub(crate) fn request_plan(
        &self,
        generation: DevicePlanGeneration,
        mount: DeviceMountIdentity,
        task: DevicePlanTask,
    ) -> Result<(), String> {
        if let Some(error) = &self.spawn_error {
            return Err(error.clone());
        }
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err("device-plan scheduler is shut down".to_owned());
        }
        self.shared
            .latest_generation
            .store(generation.value(), Ordering::Release);
        let mut state = self
            .shared
            .worker
            .lock()
            .expect("device-plan worker state poisoned");
        if self.shared.shutdown.load(Ordering::Acquire) {
            return Err("device-plan scheduler is shut down".to_owned());
        }
        state.pending = Some(DevicePlanJob {
            generation,
            mount,
            task,
        });
        self.shared.wake.notify_one();
        Ok(())
    }

    /// Invalidate the running generation and discard any pending obsolete
    /// request, for example when the displayed device disconnects.
    pub(crate) fn cancel_before(&self, generation: DevicePlanGeneration) {
        self.shared
            .latest_generation
            .store(generation.value(), Ordering::Release);
        self.shared
            .worker
            .lock()
            .expect("device-plan worker state poisoned")
            .pending = None;
    }

    pub fn shutdown(&self) {
        self.shared.shutdown.store(true, Ordering::Release);
        self.shared
            .worker
            .lock()
            .expect("device-plan worker state poisoned")
            .pending = None;
        self.shared.wake.notify_one();
        if let Some(worker) = self
            .worker
            .lock()
            .expect("device-plan join handle poisoned")
            .take()
        {
            let _ = worker.join();
        }
        while self.result_receiver.try_recv().is_ok() {}
    }
}

fn worker_loop(shared: &SharedState, result_sender: &async_channel::Sender<DevicePlanResult>) {
    loop {
        let job = {
            let mut state = shared
                .worker
                .lock()
                .expect("device-plan worker state poisoned");
            while state.pending.is_none() && !shared.shutdown.load(Ordering::Acquire) {
                state = shared
                    .wake
                    .wait(state)
                    .expect("device-plan worker state poisoned");
            }
            if shared.shutdown.load(Ordering::Acquire) {
                return;
            }
            state.pending.take().expect("pending plan job exists")
        };

        let cancelled = || {
            shared.shutdown.load(Ordering::Acquire)
                || shared.latest_generation.load(Ordering::Acquire) != job.generation.value()
        };
        if let Some(result) = (job.task)(&cancelled)
            && !cancelled()
        {
            let _ = result_sender.send_blocking(DevicePlanResult {
                generation: job.generation,
                mount: job.mount,
                result,
            });
        }
    }
}

impl Default for DevicePlanScheduler {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DevicePlanScheduler {
    fn drop(&mut self) {
        self.shutdown();
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use sustain_device_sync::{DeviceCapacity, SyncPlan};
    use sustain_domain::SyncDeviceId;

    use super::{
        DeviceMountIdentity, DevicePlanGeneration, DevicePlanScheduler, DevicePlanSnapshot,
    };

    fn mount() -> DeviceMountIdentity {
        DeviceMountIdentity {
            device_id: SyncDeviceId::new("device-id").expect("device id"),
            mount_path: "/mnt/device".into(),
            volume_id: Some("volume".to_owned()),
        }
    }

    fn snapshot(bytes_total: u64) -> DevicePlanSnapshot {
        DevicePlanSnapshot {
            plan: Some(SyncPlan {
                bytes_total,
                ..SyncPlan::default()
            }),
            capacity: DeviceCapacity::default(),
        }
    }

    #[test]
    fn request_returns_before_slow_plan_finishes() {
        let scheduler = DevicePlanScheduler::new();
        let started = Instant::now();
        scheduler
            .request_plan(
                DevicePlanGeneration::new(1),
                mount(),
                Box::new(|_| {
                    std::thread::sleep(Duration::from_millis(200));
                    Some(Ok(snapshot(1)))
                }),
            )
            .expect("queue plan");
        assert!(started.elapsed() < Duration::from_millis(100));
        scheduler
            .result_receiver()
            .recv_blocking()
            .expect("plan result");
    }

    #[test]
    fn rapid_requests_keep_only_the_latest_pending_generation() {
        let scheduler = DevicePlanScheduler::new();
        let (started_tx, started_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        scheduler
            .request_plan(
                DevicePlanGeneration::new(1),
                mount(),
                Box::new(move |_| {
                    started_tx.send(()).expect("report plan start");
                    resume_rx.recv().expect("resume first plan");
                    Some(Ok(snapshot(1)))
                }),
            )
            .expect("queue first plan");
        started_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("first plan starts");

        scheduler
            .request_plan(
                DevicePlanGeneration::new(2),
                mount(),
                Box::new(|_| Some(Ok(snapshot(2)))),
            )
            .expect("queue second plan");
        scheduler
            .request_plan(
                DevicePlanGeneration::new(3),
                mount(),
                Box::new(|_| Some(Ok(snapshot(3)))),
            )
            .expect("replace second plan");
        resume_tx.send(()).expect("resume first plan");

        let result = scheduler
            .result_receiver()
            .recv_blocking()
            .expect("latest plan result");
        assert_eq!(result.generation, DevicePlanGeneration::new(3));
        assert_eq!(
            result.result.expect("successful plan").plan,
            Some(SyncPlan {
                bytes_total: 3,
                ..SyncPlan::default()
            })
        );
        std::thread::sleep(Duration::from_millis(50));
        assert!(scheduler.result_receiver().try_recv().is_err());
    }

    #[test]
    fn invalidation_cancels_pending_request() {
        let scheduler = DevicePlanScheduler::new();
        scheduler
            .request_plan(
                DevicePlanGeneration::new(1),
                mount(),
                Box::new(|cancelled| {
                    while !cancelled() {
                        std::thread::sleep(Duration::from_millis(1));
                    }
                    None
                }),
            )
            .expect("queue first plan");
        scheduler.cancel_before(DevicePlanGeneration::new(2));
        std::thread::sleep(Duration::from_millis(50));
        assert!(scheduler.result_receiver().try_recv().is_err());
    }
}
