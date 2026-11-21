// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;

fn body(s: &str) -> String {
    s.to_owned()
}

#[test]
fn ephemeral_pushes_append_to_the_tail() {
    let mut center = NotificationCenter::new();
    let first = center.push_ephemeral(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("Scan complete: 1 track"),
    );
    let second = center.push_ephemeral(
        NotificationCategory::ArtworkFetch,
        NotificationSeverity::Info,
        body("Artwork updated."),
    );

    let queue: Vec<_> = center.ephemeral_queue().iter().map(|n| n.id).collect();
    assert_eq!(queue, vec![first, second]);
}

#[test]
fn same_category_ephemeral_replaces_in_queue_without_preempting_head() {
    let mut center = NotificationCenter::new();
    let head = center.push_ephemeral(
        NotificationCategory::LibraryImport,
        NotificationSeverity::Info,
        body("Imported 1 track"),
    );
    let stale = center.push_ephemeral(
        NotificationCategory::LibraryImport,
        NotificationSeverity::Info,
        body("Imported 5 tracks"),
    );
    let _ = stale;
    let fresh = center.push_ephemeral(
        NotificationCategory::LibraryImport,
        NotificationSeverity::Info,
        body("Imported 12 tracks"),
    );

    assert_eq!(center.ephemeral_queue().len(), 2);
    let head_now = center.current_ephemeral().expect("head present");
    assert_eq!(head_now.id, head);
    assert_eq!(head_now.body, "Imported 1 track");
    let tail_now = center.ephemeral_queue().back().expect("tail present");
    assert_eq!(tail_now.id, fresh);
    assert_eq!(tail_now.body, "Imported 12 tracks");
}

#[test]
fn unrelated_categories_are_not_coalesced() {
    let mut center = NotificationCenter::new();
    center.push_ephemeral(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("Scan complete"),
    );
    center.push_ephemeral(
        NotificationCategory::ArtworkFetch,
        NotificationSeverity::Info,
        body("Artwork updated."),
    );
    center.push_ephemeral(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("Scan complete again"),
    );

    let bodies: Vec<_> = center
        .ephemeral_queue()
        .iter()
        .map(|n| n.body.clone())
        .collect();
    // Head (LibraryScan) is untouched. The second push in the
    // LibraryScan category replaces the queued (non-head) entry —
    // but here there is no queued LibraryScan entry, so it appends.
    assert_eq!(
        bodies,
        vec![
            "Scan complete".to_owned(),
            "Artwork updated.".to_owned(),
            "Scan complete again".to_owned(),
        ]
    );
}

#[test]
fn dismiss_removes_persistent_by_id() {
    let mut center = NotificationCenter::new();
    let id = center.push_persistent(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("Scanning library..."),
        true,
    );
    assert!(center.current_persistent().is_some());
    center.dismiss(id);
    assert!(center.current_persistent().is_none());
}

#[test]
fn newer_persistent_displaces_older_until_dismissed() {
    let mut center = NotificationCenter::new();
    let scan = center.push_persistent(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("Scanning library..."),
        true,
    );
    let fetch = center.push_persistent(
        NotificationCategory::ArtworkFetch,
        NotificationSeverity::Info,
        body("Fetching artwork..."),
        false,
    );

    assert_eq!(
        center.current_persistent().map(|n| n.id),
        Some(fetch),
        "back of stack wins"
    );

    center.dismiss(fetch);
    assert_eq!(
        center.current_persistent().map(|n| n.id),
        Some(scan),
        "scan returns to surface once fetch dismisses",
    );
}

#[test]
fn expiring_the_displayed_ephemeral_pops_the_head() {
    let mut center = NotificationCenter::new();
    let first = center.push_ephemeral(
        NotificationCategory::LibraryScan,
        NotificationSeverity::Info,
        body("First"),
    );
    let second = center.push_ephemeral(
        NotificationCategory::ArtworkFetch,
        NotificationSeverity::Info,
        body("Second"),
    );

    let expired = center.expire_ephemeral(first).expect("had a head");
    assert_eq!(expired.id, first);
    assert_eq!(center.current_ephemeral().map(|n| n.id), Some(second));
}

/// A notification dismissed while its timer ran expires nothing when
/// the timer fires: the one shown in its place keeps its own time.
#[test]
fn expiring_a_dismissed_ephemeral_leaves_its_successor() {
    let mut center = NotificationCenter::new();
    let first = center.push_ephemeral(
        NotificationCategory::DeviceSyncPlanning,
        NotificationSeverity::Warning,
        body("First"),
    );
    let second = center.push_ephemeral(
        NotificationCategory::DeviceSync,
        NotificationSeverity::Info,
        body("Second"),
    );
    center.dismiss(first);

    assert!(center.expire_ephemeral(first).is_none());
    assert_eq!(center.current_ephemeral().map(|n| n.id), Some(second));
}

#[test]
fn hard_cap_drops_newcomers_rather_than_evicting_unexpired_entries() {
    // Replace-by-category keeps the queue tiny under normal use,
    // so the cap is unreachable through the regular push path.
    // The cap is defence in depth against pathological producers;
    // we prove its behavior by force-stuffing the queue past
    // coalescing.
    let mut center = NotificationCenter::new();
    for index in 0..NOTIFICATION_QUEUE_HARD_CAP {
        center.__test_force_push_ephemeral(NotificationCategory::Command, format!("msg {index}"));
    }
    assert_eq!(center.ephemeral_queue().len(), NOTIFICATION_QUEUE_HARD_CAP);

    let _ = center.push_ephemeral(
        NotificationCategory::ArtworkFetch,
        NotificationSeverity::Info,
        body("overflow"),
    );
    assert_eq!(center.ephemeral_queue().len(), NOTIFICATION_QUEUE_HARD_CAP);
    assert!(
        !center
            .ephemeral_queue()
            .iter()
            .any(|n| n.body == "overflow"),
        "newest is dropped when the cap is hit",
    );
}
