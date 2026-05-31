// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::{Arc, atomic::AtomicBool},
    time::SystemTime,
};

use sustain_domain::{
    PlayStatistics, Track, TrackAvailability, TrackId, TrackLocation, TrackRelativePath,
};
use sustain_library_store::LibraryStore;
use sustain_metadata::{LibraryScan, LibraryScanner, ScanFingerprint, ScannedTrack};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, LibraryScanResult,
    LibraryScanSummary, LibraryScanTask, NotificationCategory, NotificationSeverity,
    file_presence::{FilePresence, probe_file_presence},
    notifications,
};

impl ApplicationRuntime {
    pub(super) fn scan_library(
        &mut self,
        library_path: std::path::PathBuf,
    ) -> ApplicationRuntimeResult<()> {
        let task = self.prepare_library_scan(library_path)?;
        match run_library_scan_task(task) {
            Ok(result) => {
                self.apply_library_scan_result(result);
                Ok(())
            }
            Err(error) => {
                self.fail_library_scan(error.clone());
                Err(error)
            }
        }
    }

    pub fn prepare_library_scan(
        &mut self,
        library_path: std::path::PathBuf,
    ) -> ApplicationRuntimeResult<LibraryScanTask> {
        self.ensure_no_conflicting_library_mutation()?;

        self.last_scan_library_path = Some(library_path.clone());
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        let cancellation_requested = Arc::new(AtomicBool::new(false));
        self.library_scan_cancellation = Some(cancellation_requested.clone());
        self.background_task_status = crate::BackgroundTaskStatus::LibraryScanRunning;
        let notification_id = self.push_persistent_notification(
            NotificationCategory::LibraryScan,
            NotificationSeverity::Info,
            notifications::library_scan_running_text(),
            true,
        );
        self.library_scan_notification_id = Some(notification_id);

        Ok(LibraryScanTask {
            library_path,
            existing_tracks: self.library_tracks.clone(),
            library_store,
            metadata_service,
            cancellation_requested,
        })
    }

    pub fn apply_library_scan_result(&mut self, result: LibraryScanResult) {
        let summary = result.summary;
        self.last_scan_summary = Some(summary.clone());
        self.library_tracks = result.tracks;
        self.rebuild_search_index();
        self.refresh_playback_queue_track_ids();
        self.library_scan_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_scan_notification_id.take() {
            self.dismiss_notification(id);
        }
        let severity = if summary.missing_reconciliation_skipped {
            NotificationSeverity::Warning
        } else {
            NotificationSeverity::Info
        };
        self.push_ephemeral_notification(
            NotificationCategory::LibraryScan,
            severity,
            notifications::library_scan_outcome_text(&summary),
        );
        // New tracks may have landed; nudge the analysis scheduler so
        // it polls for fresh work without waiting for the next idle
        // cycle. No-op when no scheduler has been started.
        if let Some(scheduler) = self.analysis_scheduler() {
            scheduler.wake();
        }
        if let Some(scheduler) = self.online_scheduler() {
            scheduler.wake();
        }
        // A scan is one of the events that genuinely changes the Smart
        // Shuffle index (new tracks, new genres). Rebuild on the
        // background worker — it is milliseconds of work and the
        // scheduler coalesces re-entrant requests, so this is cheap even
        // when the scan added nothing.
        self.request_smart_shuffle_rebuild();
    }

    pub fn fail_library_scan(&mut self, error: ApplicationRuntimeError) {
        self.library_scan_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.library_scan_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.push_ephemeral_notification(
            NotificationCategory::LibraryScan,
            NotificationSeverity::Error,
            notifications::runtime_error_text(&error).to_owned(),
        );
    }
}

pub fn run_library_scan_task(task: LibraryScanTask) -> ApplicationRuntimeResult<LibraryScanResult> {
    let known_fingerprints = existing_scan_fingerprints(&task.existing_tracks);
    let scan = LibraryScanner::new(task.metadata_service.as_ref())
        .scan(
            &task.library_path,
            task.cancellation_requested.as_ref(),
            &known_fingerprints,
        )
        .map_err(|_| ApplicationRuntimeError::LibraryScanFailed)?;
    let result = reconcile_library_scan(&task.library_path, task.existing_tracks, scan)?;

    // Even on a cancelled scan, persist whatever was indexed before
    // the abort. The work has already been paid for and re-doing it
    // on the next run would punish the user for cancelling. Existing rows
    // merge scanner-owned observations only; then reload SQLite truth so
    // playback or user edits committed during the scan survive in memory.
    task.library_store
        .reconcile_scanned_tracks(&result.tracks)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let tracks = task
        .library_store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;

    Ok(LibraryScanResult {
        tracks,
        summary: result.summary,
    })
}

fn reconcile_library_scan(
    library_path: &Path,
    existing_tracks: Vec<Track>,
    scan: LibraryScan,
) -> ApplicationRuntimeResult<LibraryScanResult> {
    reconcile_library_scan_with_probe(library_path, existing_tracks, scan, probe_file_presence)
}

pub(crate) fn reconcile_library_scan_with_probe(
    library_path: &Path,
    existing_tracks: Vec<Track>,
    scan: LibraryScan,
    probe: impl Fn(&Path) -> FilePresence,
) -> ApplicationRuntimeResult<LibraryScanResult> {
    let skipped_unsupported_files = scan.skipped_unsupported_files;
    let failed_files = scan.failures.len();
    let cancelled = scan.cancelled;
    let complete_for_missing_reconciliation =
        scan.complete_for_missing_reconciliation && !cancelled && scan.failures.is_empty();
    let scanned_tracks = scan.tracks;
    let unchanged_paths = scan.unchanged;
    let mut tracks_by_path = tracks_by_path(existing_tracks.clone());
    let mut scanned_paths = BTreeSet::new();
    let mut tracks = Vec::new();
    let mut next_track_id = next_track_id(&existing_tracks)?;
    let mut added_tracks = 0;
    let mut updated_tracks = 0;

    for scanned_track in scanned_tracks {
        scanned_paths.insert(scanned_track.relative_path.clone());
        let existing_track = tracks_by_path.remove(&scanned_track.relative_path);
        if existing_track.is_some() {
            updated_tracks += 1;
        } else {
            added_tracks += 1;
        }
        let track = track_from_scanned_track(scanned_track, existing_track, &mut next_track_id)?;
        tracks.push(track);
    }

    // Files the scanner skipped because their size + mtime fingerprint was
    // unchanged. Keep the existing row exactly as the library had it, but
    // force availability back to Available (the file is confirmed present
    // this pass) and record the path as seen so the missing-file
    // reconciliation below never treats it as gone (#71).
    let mut unchanged_tracks = 0;
    for relative_path in unchanged_paths {
        let Some(mut track) = tracks_by_path.remove(&relative_path) else {
            continue;
        };
        track.location.availability = TrackAvailability::Available;
        scanned_paths.insert(relative_path);
        tracks.push(track);
        unchanged_tracks += 1;
    }

    let unvisited_tracks = existing_tracks
        .into_iter()
        .filter(|track| !scanned_paths.contains(&track.location.relative_path))
        .collect::<Vec<_>>();
    let probed_presence = complete_for_missing_reconciliation.then(|| {
        unvisited_tracks
            .iter()
            .map(|track| probe(&track.location.absolute_path(library_path)))
            .collect::<Vec<_>>()
    });
    let missing_reconciliation_skipped = probed_presence
        .as_ref()
        .is_none_or(|presence| presence.contains(&FilePresence::ProbeFailed));

    let mut missing_tracks = 0;
    if missing_reconciliation_skipped {
        // Missing reconciliation is intentionally all-or-nothing. If even one
        // unvisited path cannot be answered reliably, preserve every unvisited
        // row exactly as it was so scan order cannot decide which rows become
        // Missing after a transient filesystem failure.
        tracks.extend(unvisited_tracks);
    } else if let Some(probed_presence) = probed_presence {
        for (track, presence) in unvisited_tracks.into_iter().zip(probed_presence) {
            let track = track_with_presence(track, presence);
            if track.location.is_missing() {
                missing_tracks += 1;
            }
            tracks.push(track);
        }
    }

    tracks.sort_by_key(|track| track.id);

    Ok(LibraryScanResult {
        summary: LibraryScanSummary {
            added_tracks,
            updated_tracks,
            unchanged_tracks,
            missing_tracks,
            skipped_unsupported_files,
            failed_files,
            missing_reconciliation_skipped,
            cancelled,
        },
        tracks,
    })
}

fn tracks_by_path(tracks: Vec<Track>) -> BTreeMap<TrackRelativePath, Track> {
    tracks
        .into_iter()
        .map(|track| (track.location.relative_path.clone(), track))
        .collect()
}

/// Build the size + mtime fingerprints the scanner uses to skip
/// re-parsing unchanged files (#71). Only tracks that carry both a stored
/// size and a stored mtime contribute an entry; anything else (no scan has
/// fingerprinted it yet) is simply absent from the map and gets parsed.
fn existing_scan_fingerprints(tracks: &[Track]) -> BTreeMap<TrackRelativePath, ScanFingerprint> {
    tracks
        .iter()
        .filter_map(|track| {
            let fingerprint =
                ScanFingerprint::new(track.file_size_bytes?, track.file_modified_at?)?;
            Some((track.location.relative_path.clone(), fingerprint))
        })
        .collect()
}

// Reconciles a freshly scanned file with whatever the library already
// knows. Per the persistence policy in AGENTS.md, SQLite wins over file
// tags for every value tied to an already-imported track: ratings,
// play statistics, and every tag-derived metadata field. Audio-stream
// properties (duration, bitrate, sample rate, channels) and the file
// size are refreshed from the scan because they describe the bytes on
// disk, not the user-managed library. For a brand-new file (no
// existing row) the scanned values seed the initial state.
fn track_from_scanned_track(
    scanned_track: ScannedTrack,
    existing_track: Option<Track>,
    next_track_id: &mut i64,
) -> ApplicationRuntimeResult<Track> {
    match existing_track {
        Some(mut track) => {
            track
                .metadata
                .refresh_audio_stream_properties_from(&scanned_track.metadata);
            track.location = TrackLocation::available(scanned_track.relative_path);
            track.file_size_bytes = scanned_track.file_size_bytes;
            // The file was re-parsed because its fingerprint differed
            // (or it had none); record the fresh mtime so the next scan
            // can skip it (#71).
            track.file_modified_at = scanned_track.file_modified_at;
            // Refresh the artwork-presence bit on every scan: the
            // user may have embedded a cover externally (e.g. via
            // another tagger) since the last pass, and the online
            // scheduler must stop offering to fetch a cover for a
            // file that now has one.
            track.has_embedded_artwork = Some(scanned_track.has_embedded_artwork);
            Ok(track)
        }
        None => {
            let Some(track_id) = TrackId::new(*next_track_id) else {
                return Err(ApplicationRuntimeError::LibraryStoreFailed);
            };
            *next_track_id += 1;
            Ok(Track {
                id: track_id,
                location: TrackLocation::available(scanned_track.relative_path),
                metadata: scanned_track.metadata,
                rating: scanned_track.rating,
                statistics: PlayStatistics {
                    date_added_at: Some(SystemTime::now()),
                    ..PlayStatistics::default()
                },
                file_size_bytes: scanned_track.file_size_bytes,
                has_embedded_artwork: Some(scanned_track.has_embedded_artwork),
                file_modified_at: scanned_track.file_modified_at,
            })
        }
    }
}

fn track_with_presence(track: Track, presence: FilePresence) -> Track {
    let Track {
        id,
        location,
        metadata,
        rating,
        statistics,
        file_size_bytes,
        has_embedded_artwork,
        file_modified_at,
    } = track;
    let availability = match presence {
        FilePresence::Present => TrackAvailability::Available,
        FilePresence::Absent => TrackAvailability::Missing,
        FilePresence::ProbeFailed => {
            unreachable!("probe failures are excluded before availability reconciliation")
        }
    };

    Track {
        id,
        location: location.with_availability(availability),
        metadata,
        rating,
        statistics,
        file_size_bytes,
        has_embedded_artwork,
        file_modified_at,
    }
}

pub(super) fn load_library_tracks(
    library_store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<Vec<Track>> {
    // Trust the persisted availability flag. Per the iTunes-like model,
    // post-scan disappearance is detected lazily — when a mutation or
    // playback start touches the file and fails — rather than by polling
    // every track at startup.
    library_store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
}

pub(super) fn next_track_id(existing_tracks: &[Track]) -> ApplicationRuntimeResult<i64> {
    let next_id = existing_tracks
        .iter()
        .map(|track| track.id.get())
        .max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;

    if TrackId::new(next_id).is_some() {
        Ok(next_id)
    } else {
        Err(ApplicationRuntimeError::LibraryStoreFailed)
    }
}
