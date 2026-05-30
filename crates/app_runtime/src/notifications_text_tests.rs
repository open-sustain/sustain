// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;

#[test]
fn scan_outcome_mentions_missing_and_failed_counts() {
    let summary = LibraryScanSummary {
        scanned_tracks: 10,
        missing_tracks: 2,
        failed_files: 1,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&summary),
        "Scan complete: 10 tracks, 2 missing, 1 failed"
    );
}

#[test]
fn scan_outcome_reports_partial_count_after_cancellation() {
    let summary = LibraryScanSummary {
        scanned_tracks: 42,
        cancelled: true,
        ..LibraryScanSummary::default()
    };
    assert_eq!(
        library_scan_outcome_text(&summary),
        "Scan stopped: 42 tracks indexed."
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
