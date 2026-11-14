// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The one exclusive library task that may run at a time (#275).
//!
//! Scans, imports, library organization, duplicate consolidation and CD
//! imports each rewrite the library, so they never overlap. The slot holds
//! the running one's bookkeeping in one place: its kind, the persistent
//! notification that reports it, and its cancellation token when it can be
//! cancelled. The workers themselves are spawned and joined elsewhere; the
//! slot claims nothing about their lifetime.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult, BackgroundTaskStatus, NotificationId,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LibraryTaskKind {
    Scan,
    Import,
    Organize,
    DuplicateConsolidation,
    CdImport,
}

#[derive(Debug)]
struct RunningLibraryTask {
    kind: LibraryTaskKind,
    notification_id: NotificationId,
    /// `None` for a task that cannot stop part-way.
    cancellation: Option<Arc<AtomicBool>>,
}

#[derive(Debug, Default)]
pub(crate) struct LibraryTaskSlot {
    running: Option<RunningLibraryTask>,
}

impl LibraryTaskSlot {
    pub(crate) fn is_occupied(&self) -> bool {
        self.running.is_some()
    }

    /// Hand the slot to a task of `kind`, reported by `notification_id`,
    /// unless another task holds it. `cancellation` is the token the worker
    /// polls, `None` for a task that cannot stop part-way.
    pub(crate) fn occupy(
        &mut self,
        kind: LibraryTaskKind,
        notification_id: NotificationId,
        cancellation: Option<Arc<AtomicBool>>,
    ) -> ApplicationRuntimeResult<()> {
        if self.running.is_some() {
            return Err(ApplicationRuntimeError::BackgroundTaskRunning);
        }
        self.running = Some(RunningLibraryTask {
            kind,
            notification_id,
            cancellation,
        });
        Ok(())
    }

    /// The notification reporting the running task, if it is of `kind`.
    pub(crate) fn notification(&self, kind: LibraryTaskKind) -> Option<NotificationId> {
        self.running
            .as_ref()
            .filter(|running| running.kind == kind)
            .map(|running| running.notification_id)
    }

    /// Free the slot held by the task of `kind`, returning its notification
    /// for the caller to dismiss. A finish for a task that is not running
    /// changes nothing and returns `None`.
    pub(crate) fn finish(&mut self, kind: LibraryTaskKind) -> Option<NotificationId> {
        let matches = self
            .running
            .as_ref()
            .is_some_and(|running| running.kind == kind);
        if !matches {
            return None;
        }
        self.running.take().map(|running| running.notification_id)
    }

    /// Ask the running task to stop, if it can. The slot stays occupied
    /// until its result arrives: work completed before the request still
    /// has to be applied.
    pub(crate) fn request_cancellation(&self) {
        if let Some(token) = self
            .running
            .as_ref()
            .and_then(|running| running.cancellation.as_ref())
        {
            token.store(true, Ordering::SeqCst);
        }
    }

    /// [`Self::request_cancellation`], only while a task of `kind` runs.
    pub(crate) fn request_cancellation_of(&self, kind: LibraryTaskKind) {
        if self.running_kind() == Some(kind) {
            self.request_cancellation();
        }
    }

    pub(crate) fn cancellation_requested(&self) -> bool {
        self.running
            .as_ref()
            .and_then(|running| running.cancellation.as_ref())
            .is_some_and(|token| token.load(Ordering::SeqCst))
    }

    pub(crate) fn status(&self) -> BackgroundTaskStatus {
        match self.running_kind() {
            None => BackgroundTaskStatus::Idle,
            Some(LibraryTaskKind::Scan) => BackgroundTaskStatus::LibraryScanRunning,
            Some(LibraryTaskKind::Import) => BackgroundTaskStatus::LibraryImportRunning,
            Some(LibraryTaskKind::Organize) => BackgroundTaskStatus::LibraryConsolidationRunning,
            Some(LibraryTaskKind::DuplicateConsolidation) => {
                BackgroundTaskStatus::DuplicateConsolidationRunning
            }
            Some(LibraryTaskKind::CdImport) => BackgroundTaskStatus::CdImportRunning,
        }
    }

    fn running_kind(&self) -> Option<LibraryTaskKind> {
        self.running.as_ref().map(|running| running.kind)
    }
}

#[cfg(test)]
#[path = "library_task_tests.rs"]
mod tests;
