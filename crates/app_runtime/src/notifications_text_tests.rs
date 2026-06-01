// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;

#[test]
fn scan_outcome_lists_the_changes_not_the_library_total() {
    let summary = LibraryScanSummary {
        added_tracks: 3,
        updated_tracks: 1,
        missing_tracks: 2,
        // Unchanged files are the no-op baseline and must not appear.
        unchanged_tracks: 9_995,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&summary),
        "Scan complete: 3 added, 1 updated, 2 missing."
    );
}

#[test]
fn scan_outcome_reports_no_changes_when_nothing_changed() {
    let summary = LibraryScanSummary {
        unchanged_tracks: 10_000,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&summary),
        "Scan complete: no changes."
    );
}

#[test]
fn scan_outcome_after_cancellation_reports_partial_changes() {
    let with_changes = LibraryScanSummary {
        added_tracks: 5,
        cancelled: true,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&with_changes),
        "Scan stopped: 5 added."
    );

    let nothing_done = LibraryScanSummary {
        cancelled: true,
        ..LibraryScanSummary::default()
    };
    assert_eq!(library_scan_outcome_text(&nothing_done), "Scan stopped.");
}

#[test]
fn scan_outcome_partial_lists_failures_and_notes_the_skip() {
    let summary = LibraryScanSummary {
        added_tracks: 2,
        failed_files: 1,
        missing_reconciliation_skipped: true,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&summary),
        "Scan partial: 2 added, 1 failed; missing-file reconciliation skipped."
    );

    let no_changes = LibraryScanSummary {
        missing_reconciliation_skipped: true,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&no_changes),
        "Scan partial: missing-file reconciliation skipped."
    );
}

#[test]
fn library_organization_outcome_reports_empty_folder_cleanup_failure() {
    let summary = LibraryConsolidationSummary {
        moved_tracks: 1,
        empty_directory_cleanup_failed: true,
        ..LibraryConsolidationSummary::default()
    };

    assert_eq!(
        library_consolidation_outcome_text(&summary),
        "Library organized: 1 moved, 0 already organized, 0 missing. Some empty folders could not be removed."
    );
}

#[test]
fn runtime_error_text_maps_metadata_write_failed() {
    assert_eq!(
        runtime_error_text(&ApplicationRuntimeError::MetadataWriteFailed),
        "The track metadata could not be updated."
    );
}

#[test]
fn analysis_progress_always_reads_as_completed_over_total() {
    assert_eq!(
        analysis_background_running_text(3, 7),
        "Analyzing tracks (3/10)..."
    );
    // Regression for #74: a tick whose live "remaining" count has
    // reached zero must still read as N/N, not "(N tracks done)".
    assert_eq!(
        analysis_background_running_text(10, 0),
        "Analyzing tracks (10/10)..."
    );
}

#[test]
fn online_progress_always_reads_as_completed_over_total() {
    assert_eq!(
        online_background_running_text(2, 5),
        "Retrieving online data (2/7)..."
    );
    assert_eq!(
        online_background_running_text(7, 0),
        "Retrieving online data (7/7)..."
    );
}

#[test]
fn device_sync_preparation_progress_is_user_visible() {
    assert_eq!(
        device_sync_progress_text(sustain_device_sync::SyncProgress {
            stage: sustain_device_sync::SyncStage::Preparing,
            completed: 4,
            total: 10,
        }),
        "Preparing tracks (4/10)…"
    );
}
