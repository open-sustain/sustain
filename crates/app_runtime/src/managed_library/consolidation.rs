// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Library consolidation: relocating already-imported tracks to the canonical
//! managed layout. Planning is pure (`plan_library_consolidation`,
//! `plan_managed_track_retarget`); the runner journals its intent, performs
//! no-overwrite moves, and persists `is_missing` corrections it discovers.

use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

use sustain_domain::{
    ManagedTrackPathInput, ManagedTrackPathPlanner, Track, TrackAvailability, TrackId,
    TrackLocation, TrackRelativePath,
};

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult, LibraryConsolidationResult,
    LibraryConsolidationSummary, LibraryConsolidationTask,
};

use super::file_ops::{
    FileIdentity, move_file_without_copy_or_overwrite_matching_identity, rollback_file_move,
};
use super::journal::{
    recover_library_consolidation_journal, remove_consolidation_journal_if_present,
    write_consolidation_journal,
};

pub fn run_library_consolidation_task(
    task: LibraryConsolidationTask,
) -> ApplicationRuntimeResult<LibraryConsolidationResult> {
    let context = LibraryConsolidationContext {
        settings: task.settings,
        existing_tracks: task.existing_tracks,
        library_store: task.library_store,
        cancellation_requested: task.cancellation_requested,
    };

    context.consolidate_library()
}

struct LibraryConsolidationContext {
    settings: sustain_domain::UserSettings,
    existing_tracks: Vec<Track>,
    library_store: std::sync::Arc<dyn sustain_library_store::LibraryStore>,
    cancellation_requested: Arc<AtomicBool>,
}

impl LibraryConsolidationContext {
    fn consolidate_library(self) -> ApplicationRuntimeResult<LibraryConsolidationResult> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();

        recover_library_consolidation_journal(&library_path, self.library_store.as_ref())?;

        let plan = plan_library_consolidation(&library_path, &self.existing_tracks)?;

        // Persist any `is_missing` flag corrections discovered during
        // planning before touching any files on disk: the flag flip
        // is durable even if a later move fails, and the result we
        // return always carries the corrected tracks so the runtime's
        // in-memory copy matches SQLite. Done in one narrow transaction
        // to keep the cost bounded on a 10k library without overwriting
        // unrelated columns from the planning snapshot.
        if !plan.missing_track_updates.is_empty() {
            let updates = plan
                .missing_track_updates
                .iter()
                .map(|track| (track.id, track.location.clone()))
                .collect::<Vec<_>>();
            self.library_store
                .update_track_locations(&updates)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }

        if plan.moves.is_empty() {
            remove_consolidation_journal_if_present(&library_path)?;
            return Ok(LibraryConsolidationResult {
                tracks: plan.missing_track_updates,
                summary: LibraryConsolidationSummary {
                    planned_tracks: 0,
                    moved_tracks: 0,
                    already_organized_tracks: plan.already_organized_tracks,
                    missing_tracks: plan.missing_tracks,
                    cancelled: self.cancellation_requested.load(Ordering::SeqCst),
                },
            });
        }

        write_consolidation_journal(&library_path, &plan.moves)?;

        let mut updated_tracks = plan.missing_track_updates;
        let mut moved_tracks = 0;
        let mut cancelled = false;

        for planned_move in &plan.moves {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                cancelled = true;
                break;
            }

            if move_file_without_copy_or_overwrite_matching_identity(
                &planned_move.source_path,
                &planned_move.destination_path,
                planned_move.source_identity,
            )
            .is_err()
            {
                return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
            }

            let updated_track = planned_move.updated_track.clone();
            if self
                .library_store
                .update_track_location(updated_track.id, &updated_track.location)
                .is_err()
            {
                rollback_file_move(&planned_move.source_path, &planned_move.destination_path).ok();
                return Err(ApplicationRuntimeError::LibraryStoreFailed);
            }

            updated_tracks.push(updated_track);
            moved_tracks += 1;
        }

        // Protocol ordering:
        // 1. the journal file was fsynced, renamed, and root-directory synced;
        // 2. each move linked + synced its destination directory, unlinked +
        //    synced its source directory, then committed its precise row path;
        // 3. checkpoint SQLite before removing + root-directory syncing the
        //    journal. Until this barrier succeeds, restart recovery owns the
        //    reconciliation of every entry.
        self.library_store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        remove_consolidation_journal_if_present(&library_path)?;

        Ok(LibraryConsolidationResult {
            tracks: updated_tracks,
            summary: LibraryConsolidationSummary {
                planned_tracks: plan.moves.len(),
                moved_tracks,
                already_organized_tracks: plan.already_organized_tracks,
                missing_tracks: plan.missing_tracks,
                cancelled,
            },
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct LibraryConsolidationPlan {
    moves: Vec<PlannedLibraryConsolidationMove>,
    already_organized_tracks: usize,
    /// Total number of tracks whose source file was missing or
    /// non-regular at plan time — surfaced in the
    /// [`LibraryConsolidationSummary`] so the user sees a stable
    /// count of orphaned rows regardless of whether the SQLite flag
    /// was already correct.
    missing_tracks: usize,
    /// Subset of the missing tracks whose persisted `is_missing` flag
    /// was still `false` at plan time. The runner flips and persists
    /// these in a single transaction so subsequent reads of SQLite
    /// see the corrected availability, and the per-row warning icon
    /// in the table lights up.
    missing_track_updates: Vec<Track>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct PlannedLibraryConsolidationMove {
    pub(super) track_id: TrackId,
    pub(super) source_path: PathBuf,
    pub(super) destination_path: PathBuf,
    pub(super) source_identity: FileIdentity,
    pub(super) source_relative_path: TrackRelativePath,
    pub(super) destination_relative_path: TrackRelativePath,
    pub(super) updated_track: Track,
}

fn plan_library_consolidation(
    library_path: &Path,
    existing_tracks: &[Track],
) -> ApplicationRuntimeResult<LibraryConsolidationPlan> {
    let planner = ManagedTrackPathPlanner::default();
    let mut occupied_paths = existing_tracks
        .iter()
        .map(|track| track.location.relative_path.clone())
        .collect::<BTreeSet<_>>();
    let mut moves = Vec::new();
    let mut already_organized_tracks = 0;
    let mut missing_tracks = 0;
    let mut missing_track_updates: Vec<Track> = Vec::new();
    let mut record_missing_track = |track: &Track| {
        missing_tracks += 1;
        // Only push an update when the persisted flag is actually
        // wrong; an already-missing row needs no rewrite. The runner
        // commits the whole batch in a single transaction so the
        // table's missing-file indicator lights up on the very next
        // refresh.
        if !track.location.is_missing() {
            let mut updated = track.clone();
            updated.location = updated
                .location
                .with_availability(TrackAvailability::Missing);
            missing_track_updates.push(updated);
        }
    };

    for track in existing_tracks {
        let source_relative_path = track.location.relative_path.clone();
        let source_path = track.location.absolute_path(library_path);
        let Ok(source_metadata) = fs::symlink_metadata(&source_path) else {
            record_missing_track(track);
            continue;
        };
        if !source_metadata.file_type().is_file() {
            record_missing_track(track);
            continue;
        }

        occupied_paths.remove(&source_relative_path);
        let plan = plan_consolidation_destination(
            &planner,
            &mut occupied_paths,
            library_path,
            &source_path,
            &track.metadata,
            &source_relative_path,
        )?;
        occupied_paths.insert(source_relative_path.clone());

        if plan.relative_path == source_relative_path {
            already_organized_tracks += 1;
            continue;
        }

        let destination_path = library_path.join(plan.relative_path.as_path());
        let mut updated_track = track.clone();
        updated_track.location = TrackLocation::available(plan.relative_path.clone());

        moves.push(PlannedLibraryConsolidationMove {
            track_id: track.id,
            source_path,
            destination_path,
            source_identity: FileIdentity {
                device: source_metadata.dev(),
                inode: source_metadata.ino(),
            },
            source_relative_path,
            destination_relative_path: plan.relative_path,
            updated_track,
        });
    }

    Ok(LibraryConsolidationPlan {
        moves,
        already_organized_tracks,
        missing_tracks,
        missing_track_updates,
    })
}

pub(super) fn plan_managed_track_retarget(
    library_path: &Path,
    existing_tracks: &[Track],
    track: Track,
) -> ApplicationRuntimeResult<Option<PlannedLibraryConsolidationMove>> {
    let source_relative_path = track.location.relative_path.clone();
    let source_path = track.location.absolute_path(library_path);
    let source_metadata = fs::symlink_metadata(&source_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
    if !source_metadata.file_type().is_file() {
        return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
    }

    let planner = ManagedTrackPathPlanner::default();
    let mut occupied_paths = existing_tracks
        .iter()
        .filter(|existing_track| existing_track.id != track.id)
        .map(|existing_track| existing_track.location.relative_path.clone())
        .collect::<BTreeSet<_>>();

    let plan = plan_consolidation_destination(
        &planner,
        &mut occupied_paths,
        library_path,
        &source_path,
        &track.metadata,
        &source_relative_path,
    )?;
    if plan.relative_path == source_relative_path {
        return Ok(None);
    }

    let destination_path = library_path.join(plan.relative_path.as_path());
    let mut updated_track = track;
    updated_track.location = TrackLocation::available(plan.relative_path.clone());

    Ok(Some(PlannedLibraryConsolidationMove {
        track_id: updated_track.id,
        source_path,
        destination_path,
        source_identity: FileIdentity {
            device: source_metadata.dev(),
            inode: source_metadata.ino(),
        },
        source_relative_path,
        destination_relative_path: plan.relative_path,
        updated_track,
    }))
}

fn plan_consolidation_destination(
    planner: &ManagedTrackPathPlanner,
    occupied_paths: &mut BTreeSet<TrackRelativePath>,
    library_path: &Path,
    source_path: &Path,
    metadata: &sustain_domain::TrackMetadata,
    current_relative_path: &TrackRelativePath,
) -> ApplicationRuntimeResult<sustain_domain::ManagedTrackPathPlan> {
    for _attempt in 0..10_000 {
        let plan = planner
            .plan(
                ManagedTrackPathInput {
                    metadata,
                    source_path,
                },
                occupied_paths,
            )
            .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;
        if &plan.relative_path == current_relative_path {
            occupied_paths.insert(plan.relative_path.clone());
            return Ok(plan);
        }
        if library_path.join(plan.relative_path.as_path()).exists() {
            occupied_paths.insert(plan.relative_path);
            continue;
        }
        occupied_paths.insert(plan.relative_path.clone());
        return Ok(plan);
    }

    Err(ApplicationRuntimeError::LibraryConsolidationFailed)
}
