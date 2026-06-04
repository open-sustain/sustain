// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Library consolidation: relocating already-imported tracks to the canonical
//! managed layout. Planning pins source identities with pre-journal recovery
//! hard links one file at a time and never retains source descriptors in the
//! returned plan; the runner journals its intent, performs no-overwrite moves,
//! and persists `is_missing` corrections it discovers.

use std::{
    collections::BTreeSet,
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

use super::capabilities::ManagedLibraryFilesystemValidator;
use super::file_ops::{
    FileIdentity, RegularFileProbe, move_file_without_copy_or_overwrite_matching_capability,
    open_regular_file, probe_regular_file, prune_empty_ancestor_directories_for_sources,
    rollback_file_move,
};
use super::journal::{
    PreparedConsolidationRecovery, open_consolidation_recovery_source,
    prepare_consolidation_recovery, recover_library_consolidation_journal,
    remove_consolidation_journal_if_present, write_consolidation_journal,
};

pub fn run_library_consolidation_task(
    task: LibraryConsolidationTask,
) -> ApplicationRuntimeResult<LibraryConsolidationResult> {
    let context = LibraryConsolidationContext {
        settings: task.settings,
        existing_tracks: task.existing_tracks,
        library_store: task.library_store,
        managed_library_filesystem_validator: task.managed_library_filesystem_validator,
        cancellation_requested: task.cancellation_requested,
    };

    context.consolidate_library()
}

struct LibraryConsolidationContext {
    settings: sustain_domain::UserSettings,
    existing_tracks: Vec<Track>,
    library_store: std::sync::Arc<dyn sustain_library_store::LibraryStore>,
    managed_library_filesystem_validator: ManagedLibraryFilesystemValidator,
    cancellation_requested: Arc<AtomicBool>,
}

impl LibraryConsolidationContext {
    fn consolidate_library(self) -> ApplicationRuntimeResult<LibraryConsolidationResult> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();

        self.managed_library_filesystem_validator
            .validate(&library_path)
            .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
        recover_library_consolidation_journal(
            &library_path,
            self.library_store.as_ref(),
            &self.managed_library_filesystem_validator,
        )?;

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
                    empty_directory_cleanup_failed: false,
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

            let source = open_consolidation_recovery_source(planned_move)?;
            if move_file_without_copy_or_overwrite_matching_capability(
                &planned_move.source_path,
                &planned_move.destination_path,
                &source,
            )
            .is_err()
            {
                prune_empty_ancestor_directories_for_sources(
                    &library_path,
                    std::slice::from_ref(&planned_move.destination_path),
                );
                return Err(ApplicationRuntimeError::LibraryConsolidationFailed);
            }

            let updated_track = planned_move.updated_track.clone();
            if self
                .library_store
                .update_track_location(updated_track.id, &updated_track.location)
                .is_err()
            {
                rollback_file_move(&planned_move.source_path, &planned_move.destination_path).ok();
                prune_empty_ancestor_directories_for_sources(
                    &library_path,
                    std::slice::from_ref(&planned_move.destination_path),
                );
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
        let moved_source_paths = plan
            .moves
            .iter()
            .take(moved_tracks)
            .map(|planned_move| planned_move.source_path.clone())
            .collect::<Vec<_>>();
        let prune_outcome =
            prune_empty_ancestor_directories_for_sources(&library_path, &moved_source_paths);

        Ok(LibraryConsolidationResult {
            tracks: updated_tracks,
            summary: LibraryConsolidationSummary {
                planned_tracks: plan.moves.len(),
                moved_tracks,
                already_organized_tracks: plan.already_organized_tracks,
                missing_tracks: plan.missing_tracks,
                empty_directory_cleanup_failed: prune_outcome.failed,
                cancelled,
            },
        })
    }
}

#[derive(Clone, Debug)]
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

#[derive(Clone, Debug)]
pub(super) struct PlannedLibraryConsolidationMove {
    pub(super) track_id: TrackId,
    pub(super) source_path: PathBuf,
    pub(super) destination_path: PathBuf,
    /// Identity captured while planning and pinned by the pre-journal recovery
    /// hard link held alive by `prepared_recovery`.
    pub(super) source_identity: FileIdentity,
    pub(super) prepared_recovery: Arc<PreparedConsolidationRecovery>,
    pub(super) source_relative_path: TrackRelativePath,
    pub(super) destination_relative_path: TrackRelativePath,
    pub(super) updated_track: Track,
    pub(super) persistence: JournalTrackPersistence,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum JournalTrackPersistence {
    LocationOnly,
    Relocation,
}

fn plan_library_consolidation(
    library_path: &Path,
    existing_tracks: &[Track],
) -> ApplicationRuntimeResult<LibraryConsolidationPlan> {
    let recovery = prepare_consolidation_recovery(library_path)?;
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
        let source = match probe_regular_file(&source_path) {
            Ok(RegularFileProbe::Present(source)) => source,
            Ok(RegularFileProbe::MissingOrNonRegular) => {
                record_missing_track(track);
                continue;
            }
            Err(_) => return Err(ApplicationRuntimeError::LibraryConsolidationFailed),
        };

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
        let source_identity = recovery.pin_source(track.id, &source)?;

        moves.push(PlannedLibraryConsolidationMove {
            track_id: track.id,
            source_path,
            destination_path,
            source_identity,
            prepared_recovery: recovery.clone(),
            source_relative_path,
            destination_relative_path: plan.relative_path,
            updated_track,
            persistence: JournalTrackPersistence::LocationOnly,
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
    let recovery = prepare_consolidation_recovery(library_path)?;
    let source_relative_path = track.location.relative_path.clone();
    let source_path = track.location.absolute_path(library_path);
    let source = open_regular_file(&source_path)
        .map_err(|_| ApplicationRuntimeError::LibraryConsolidationFailed)?;

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
    let source_identity = recovery.pin_source(updated_track.id, &source)?;

    Ok(Some(PlannedLibraryConsolidationMove {
        track_id: updated_track.id,
        source_path,
        destination_path,
        source_identity,
        prepared_recovery: recovery,
        source_relative_path,
        destination_relative_path: plan.relative_path,
        updated_track,
        persistence: JournalTrackPersistence::LocationOnly,
    }))
}

pub(super) fn plan_managed_missing_track_relocation(
    library_path: &Path,
    existing_tracks: &[Track],
    mut track: Track,
    source_path: &Path,
    source_relative_path: Option<&TrackRelativePath>,
) -> ApplicationRuntimeResult<(Track, Option<PlannedLibraryConsolidationMove>)> {
    let recovery = prepare_consolidation_recovery(library_path)
        .map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;
    let source = open_regular_file(source_path)
        .map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;

    let planner = ManagedTrackPathPlanner::default();
    let mut occupied_paths = existing_tracks
        .iter()
        .filter(|existing_track| existing_track.id != track.id)
        .map(|existing_track| existing_track.location.relative_path.clone())
        .collect::<BTreeSet<_>>();

    let plan = plan_missing_track_destination(
        &planner,
        &mut occupied_paths,
        library_path,
        source_path,
        &track.metadata,
        source_relative_path,
    )?;
    track.location = TrackLocation::available(plan.relative_path.clone());
    let Some(source_relative_path) = source_relative_path else {
        return Ok((track, None));
    };
    if &plan.relative_path == source_relative_path {
        return Ok((track, None));
    }
    let source_identity = recovery
        .pin_source(track.id, &source)
        .map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;

    Ok((
        track.clone(),
        Some(PlannedLibraryConsolidationMove {
            track_id: track.id,
            source_path: source_path.to_path_buf(),
            destination_path: library_path.join(plan.relative_path.as_path()),
            source_identity,
            prepared_recovery: recovery,
            source_relative_path: source_relative_path.clone(),
            destination_relative_path: plan.relative_path,
            updated_track: track,
            persistence: JournalTrackPersistence::Relocation,
        }),
    ))
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

fn plan_missing_track_destination(
    planner: &ManagedTrackPathPlanner,
    occupied_paths: &mut BTreeSet<TrackRelativePath>,
    library_path: &Path,
    source_path: &Path,
    metadata: &sustain_domain::TrackMetadata,
    source_relative_path: Option<&TrackRelativePath>,
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
            .map_err(|_| ApplicationRuntimeError::TrackRelocationFailed)?;
        if source_relative_path == Some(&plan.relative_path)
            || !library_path.join(plan.relative_path.as_path()).exists()
        {
            occupied_paths.insert(plan.relative_path.clone());
            return Ok(plan);
        }
        occupied_paths.insert(plan.relative_path);
    }

    Err(ApplicationRuntimeError::TrackRelocationFailed)
}

#[cfg(test)]
mod tests {
    use std::{fs, os::unix::fs::symlink, path::PathBuf};

    use sustain_domain::{PlayStatistics, Rating, TrackMetadata};

    use super::*;

    #[test]
    fn planning_pins_sources_without_retaining_file_descriptors() {
        let root = tempfile::tempdir().expect("create test root");
        fs::create_dir(root.path().join("loose")).expect("create source directory");
        let tracks = (1..=256)
            .map(|id| {
                let relative_path = format!("loose/{id}.flac");
                fs::write(root.path().join(&relative_path), b"audio").expect("write source");
                track(id, &relative_path)
            })
            .collect::<Vec<_>>();

        let plan = plan_library_consolidation(root.path(), &tracks).expect("plan consolidation");

        assert_eq!(plan.moves.len(), tracks.len());
        assert_eq!(
            open_file_descriptors_beneath(root.path()),
            vec![root.path().join(".sustain-consolidation-recovery")],
            "a live consolidation plan must retain only its shared recovery-directory descriptor"
        );
        assert_eq!(
            fs::read_dir(root.path().join(".sustain-consolidation-recovery"))
                .expect("read recovery directory")
                .count(),
            tracks.len(),
            "every planned move must pin its source with a recovery hard link"
        );
        drop(plan);
        assert!(!root.path().join(".sustain-consolidation-recovery").exists());
    }

    #[test]
    fn planning_marks_only_missing_or_non_regular_sources_missing() {
        let root = tempfile::tempdir().expect("create test root");
        let missing = track(1, "missing.flac");

        let plan =
            plan_library_consolidation(root.path(), std::slice::from_ref(&missing)).expect("plan");

        assert_eq!(plan.missing_tracks, 1);
        assert_eq!(plan.missing_track_updates.len(), 1);
        assert!(plan.missing_track_updates[0].location.is_missing());

        let unprobeable_name = format!("{}.flac", "x".repeat(256));
        let unprobeable = track(2, &unprobeable_name);
        assert!(
            matches!(
                plan_library_consolidation(root.path(), &[unprobeable]),
                Err(ApplicationRuntimeError::LibraryConsolidationFailed)
            ),
            "resource and pathname errors must abort instead of becoming missing rows"
        );

        let outside = tempfile::tempdir().expect("create outside root");
        fs::write(outside.path().join("song.flac"), b"audio").expect("write outside source");
        symlink(outside.path(), root.path().join("redirect")).expect("create parent symlink");
        let redirected = track(3, "redirect/song.flac");
        assert!(
            matches!(
                plan_library_consolidation(root.path(), &[redirected]),
                Err(ApplicationRuntimeError::LibraryConsolidationFailed)
            ),
            "unsafe source-parent resolution must abort instead of becoming a missing row"
        );
    }

    #[test]
    fn planning_failure_cleans_prejournal_recovery_links() {
        let root = tempfile::tempdir().expect("create test root");
        fs::write(root.path().join("loose.flac"), b"original").expect("write source");
        let unprobeable_name = format!("{}.flac", "x".repeat(256));

        assert!(matches!(
            plan_library_consolidation(
                root.path(),
                &[track(1, "loose.flac"), track(2, &unprobeable_name)]
            ),
            Err(ApplicationRuntimeError::LibraryConsolidationFailed)
        ));
        assert_eq!(
            fs::read(root.path().join("loose.flac")).expect("read source"),
            b"original"
        );
        assert!(!root.path().join(".sustain-consolidation-recovery").exists());
    }

    #[test]
    fn concurrent_planning_refuses_to_reuse_recovery_namespace() {
        let root = tempfile::tempdir().expect("create test root");
        fs::write(root.path().join("loose.flac"), b"original").expect("write source");
        let plan = plan_library_consolidation(root.path(), &[track(1, "loose.flac")])
            .expect("plan consolidation");

        assert!(matches!(
            plan_library_consolidation(root.path(), &[track(1, "loose.flac")]),
            Err(ApplicationRuntimeError::LibraryConsolidationFailed)
        ));
        assert_eq!(
            fs::read(
                root.path()
                    .join(".sustain-consolidation-recovery/track-1.backup")
            )
            .expect("read first plan's pinned source"),
            b"original"
        );
        drop(plan);
        assert!(!root.path().join(".sustain-consolidation-recovery").exists());
    }

    #[test]
    fn journal_publication_rejects_source_replacement_after_planning() {
        let root = tempfile::tempdir().expect("create test root");
        let source = root.path().join("loose.flac");
        fs::write(&source, b"original").expect("write source");
        let plan = plan_library_consolidation(root.path(), &[track(1, "loose.flac")])
            .expect("plan consolidation");
        assert_eq!(
            fs::read(
                root.path()
                    .join(".sustain-consolidation-recovery/track-1.backup")
            )
            .expect("read pinned source"),
            b"original"
        );
        fs::remove_file(&source).expect("remove planned source");
        fs::write(&source, b"replacement").expect("write replacement");
        assert_ne!(
            open_regular_file(&source)
                .expect("open replacement")
                .identity(),
            plan.moves[0].source_identity,
            "the recovery hard link must prevent immediate inode reuse"
        );

        assert!(write_consolidation_journal(root.path(), &plan.moves).is_err());
        assert_eq!(fs::read(&source).expect("read replacement"), b"replacement");
        assert!(!root.path().join(".sustain-consolidation-journal").exists());
        drop(plan);
        assert!(!root.path().join(".sustain-consolidation-recovery").exists());
    }

    fn track(id: i64, relative_path: &str) -> Track {
        Track {
            id: TrackId::new(id).expect("positive track id"),
            location: TrackLocation::available(
                TrackRelativePath::new(relative_path).expect("valid relative path"),
            ),
            metadata: TrackMetadata {
                title: Some(format!("Song {id}")),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn open_file_descriptors_beneath(root: &Path) -> Vec<PathBuf> {
        fs::read_dir("/proc/self/fd")
            .expect("read process fd directory")
            .filter_map(Result::ok)
            .filter_map(|entry| fs::read_link(entry.path()).ok())
            .filter(|path| path.starts_with(root))
            .collect()
    }
}
