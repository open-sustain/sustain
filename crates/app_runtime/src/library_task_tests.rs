// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;
use crate::{NotificationCategory, NotificationCenter, NotificationSeverity};

fn notification(center: &mut NotificationCenter) -> NotificationId {
    center.push_persistent(
        NotificationCategory::LibraryImport,
        NotificationSeverity::Info,
        "Running".to_owned(),
        true,
    )
}

fn token() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

#[test]
fn a_running_task_keeps_the_slot_until_it_finishes() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    let id = notification(&mut center);

    slot.occupy(LibraryTaskKind::Import, id, Some(token()))
        .expect("free slot");
    assert!(slot.is_occupied());
    assert_eq!(slot.status(), BackgroundTaskStatus::LibraryImportRunning);
    assert_eq!(slot.notification(LibraryTaskKind::Import), Some(id));
    assert_eq!(slot.notification(LibraryTaskKind::Scan), None);

    assert_eq!(slot.finish(LibraryTaskKind::Import), Some(id));
    assert!(!slot.is_occupied());
    assert_eq!(slot.status(), BackgroundTaskStatus::Idle);
}

#[test]
fn a_second_task_cannot_take_a_busy_slot() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    let running = notification(&mut center);
    let import = token();
    slot.occupy(LibraryTaskKind::Import, running, Some(import.clone()))
        .expect("free slot");

    let refused = notification(&mut center);
    assert_eq!(
        slot.occupy(LibraryTaskKind::CdImport, refused, Some(token())),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    );

    assert_eq!(slot.status(), BackgroundTaskStatus::LibraryImportRunning);
    assert_eq!(slot.notification(LibraryTaskKind::Import), Some(running));
    slot.request_cancellation();
    assert!(import.load(Ordering::SeqCst), "the running task's token");
}

#[test]
fn finishing_a_task_that_is_not_running_changes_nothing() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    assert_eq!(slot.finish(LibraryTaskKind::Scan), None);

    let id = notification(&mut center);
    slot.occupy(LibraryTaskKind::Import, id, Some(token()))
        .expect("free slot");
    assert_eq!(slot.finish(LibraryTaskKind::CdImport), None);
    assert_eq!(slot.notification(LibraryTaskKind::Import), Some(id));
}

#[test]
fn cancelling_keeps_the_slot_until_the_result_arrives() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    let id = notification(&mut center);
    let scan = token();
    slot.occupy(LibraryTaskKind::Scan, id, Some(scan.clone()))
        .expect("free slot");
    assert!(!slot.cancellation_requested());

    slot.request_cancellation();

    assert!(scan.load(Ordering::SeqCst));
    assert!(slot.cancellation_requested());
    assert_eq!(slot.status(), BackgroundTaskStatus::LibraryScanRunning);
    assert_eq!(slot.finish(LibraryTaskKind::Scan), Some(id));
}

#[test]
fn a_task_that_cannot_stop_ignores_cancellation() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    let id = notification(&mut center);
    slot.occupy(LibraryTaskKind::DuplicateConsolidation, id, None)
        .expect("free slot");

    slot.request_cancellation();

    assert!(!slot.cancellation_requested());
    assert_eq!(
        slot.status(),
        BackgroundTaskStatus::DuplicateConsolidationRunning
    );
}

#[test]
fn cancelling_one_kind_leaves_another_running() {
    let mut center = NotificationCenter::new();
    let mut slot = LibraryTaskSlot::default();
    let id = notification(&mut center);
    let import = token();
    slot.occupy(LibraryTaskKind::Import, id, Some(import.clone()))
        .expect("free slot");

    slot.request_cancellation_of(LibraryTaskKind::Organize);
    assert!(!import.load(Ordering::SeqCst));

    slot.request_cancellation_of(LibraryTaskKind::Import);
    assert!(import.load(Ordering::SeqCst));
}
