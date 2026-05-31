// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adding external files to the library, in either management mode: copying
//! verified files into the canonical layout, or referencing them in place.
//! Both modes deduplicate by relative path and content hash and roll back any
//! filesystem side effects when cancelled or on failure.

use std::{
    collections::{BTreeSet, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use sustain_domain::{
    ManagedTrackPathInput, ManagedTrackPathPlanner, PlayStatistics, Rating, Track,
    TrackContentHash, TrackLocation,
};
use sustain_metadata::{InitialTags, audio_format_from_path, hash_file_content};

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult, LibraryImportResult, LibraryImportSummary,
    LibraryImportTask, library_scan,
};

use super::file_ops::{copy_file_verified, remove_copied_files};

pub fn run_library_import_task(
    task: LibraryImportTask,
) -> ApplicationRuntimeResult<LibraryImportResult> {
    let mut context = LibraryImportContext {
        settings: task.settings,
        existing_tracks: task.existing_tracks,
        library_store: task.library_store,
        metadata_service: task.metadata_service,
        cancellation_requested: task.cancellation_requested,
    };

    context.add_external_library_items(task.paths)
}

struct LibraryImportContext {
    settings: sustain_domain::UserSettings,
    existing_tracks: Vec<Track>,
    library_store: std::sync::Arc<dyn sustain_library_store::LibraryStore>,
    metadata_service: std::sync::Arc<dyn sustain_metadata::MetadataService>,
    cancellation_requested: Arc<AtomicBool>,
}

impl LibraryImportContext {
    fn add_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        if paths.is_empty() {
            return Ok(LibraryImportResult {
                tracks: Vec::new(),
                summary: LibraryImportSummary::default(),
            });
        }

        match self.settings.library.management_mode {
            sustain_domain::LibraryManagementMode::ReferenceFilesInPlace => {
                self.add_referenced_external_library_items(paths)
            }
            sustain_domain::LibraryManagementMode::CopyAddedFilesIntoLibrary => {
                self.add_managed_external_library_items(paths)
            }
        }
    }

    fn add_managed_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let canonical_library_path = fs::canonicalize(&library_path).ok();

        let discovered_files =
            collect_supported_audio_files(&paths, self.cancellation_requested.as_ref())?;
        if self.cancellation_requested.load(Ordering::SeqCst) {
            return Ok(cancelled_import_result(discovered_files.len()));
        }

        let mut occupied_paths = self
            .existing_tracks
            .iter()
            .map(|track| track.location.relative_path.clone())
            .collect::<BTreeSet<_>>();
        // Within-batch dedup only: catch two byte-identical source files
        // dropped in the same import. Cross-existing dedup is decided
        // against ground truth on disk by `library_contains_matching_content`,
        // never against the stored content hash, which is import-time only
        // and may be stale or absent (see #72).
        let mut seen_hashes: HashSet<String> = HashSet::new();

        let planner = ManagedTrackPathPlanner::default();
        let mut imports = Vec::new();
        let mut duplicate_files = 0;

        for source_path in &discovered_files {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return Ok(cancelled_import_result(discovered_files.len()));
            }
            let source_path = fs::canonicalize(source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            if let Some(relative_path) =
                source_relative_path_inside_library(&source_path, canonical_library_path.as_deref())
                && occupied_paths.contains(&relative_path)
            {
                duplicate_files += 1;
                continue;
            }

            let source_size = fs::metadata(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?
                .len();
            let content_hash = hash_file_content(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            if seen_hashes.contains(content_hash.as_str())
                || self.library_contains_matching_content(
                    &library_path,
                    source_size,
                    &content_hash,
                )?
            {
                duplicate_files += 1;
                continue;
            }

            let InitialTags {
                metadata,
                rating,
                has_embedded_artwork,
            } = self
                .metadata_service
                .read_initial_tags(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            let plan = match plan_destination(
                &planner,
                &mut occupied_paths,
                &library_path,
                &source_path,
                &metadata,
                source_size,
                &content_hash,
            )? {
                PlannedManagedDestination::Fresh(plan) => plan,
                PlannedManagedDestination::AlreadyPresent => {
                    duplicate_files += 1;
                    continue;
                }
            };
            seen_hashes.insert(content_hash.as_str().to_owned());
            imports.push(PlannedManagedImport {
                source_path,
                destination_path: library_path.join(plan.relative_path.as_path()),
                relative_path: plan.relative_path,
                content_hash,
                metadata,
                rating,
                file_size_bytes: source_size,
                has_embedded_artwork,
            });
        }

        let mut copied_paths = Vec::new();
        for import in &imports {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                // Roll back the files we have copied so far so a
                // cancelled import leaves zero filesystem side
                // effects.
                remove_copied_files(&copied_paths)
                    .map_err(|()| ApplicationRuntimeError::LibraryImportFailed)?;
                return Ok(cancelled_import_result(discovered_files.len()));
            }
            match copy_file_verified(
                &import.source_path,
                &import.destination_path,
                &import.content_hash,
            ) {
                Ok(_) => copied_paths.push(import.destination_path.clone()),
                Err(_) => {
                    let _ = remove_copied_files(&copied_paths);
                    return Err(ApplicationRuntimeError::LibraryImportFailed);
                }
            }
        }

        let first_track_id = library_scan::next_track_id(&self.existing_tracks)?;
        let mut tracks = Vec::new();
        for (next_track_id, import) in (first_track_id..).zip(imports) {
            let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                let _ = remove_copied_files(&copied_paths);
                return Err(ApplicationRuntimeError::LibraryStoreFailed);
            };
            tracks.push(Track {
                id: track_id,
                location: TrackLocation::available(import.relative_path),
                metadata: import.metadata,
                rating: import.rating,
                statistics: PlayStatistics {
                    date_added_at: Some(SystemTime::now()),
                    ..PlayStatistics::default()
                },
                file_size_bytes: Some(import.file_size_bytes),
                has_embedded_artwork: Some(import.has_embedded_artwork),
            });
        }

        if self.library_store.save_tracks(&tracks).is_err() {
            let _ = remove_copied_files(&copied_paths);
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        }

        Ok(LibraryImportResult {
            tracks,
            summary: LibraryImportSummary {
                discovered_files: discovered_files.len(),
                imported_tracks: copied_paths.len(),
                duplicate_files,
                cancelled: false,
            },
        })
    }

    fn add_referenced_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let canonical_library_path = fs::canonicalize(&library_path)
            .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;

        let discovered_files =
            collect_supported_audio_files(&paths, self.cancellation_requested.as_ref())?;
        if self.cancellation_requested.load(Ordering::SeqCst) {
            return Ok(cancelled_import_result(discovered_files.len()));
        }
        let mut seen_locations = self
            .existing_tracks
            .iter()
            .map(|track| track.location.relative_path.clone())
            .collect::<BTreeSet<_>>();

        let mut next_track_id = library_scan::next_track_id(&self.existing_tracks)?;
        let mut tracks = Vec::new();
        let mut duplicate_files = 0;

        for source_path in &discovered_files {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return Ok(cancelled_import_result(discovered_files.len()));
            }
            let source_path = fs::canonicalize(source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            let relative_path =
                reference_relative_path_for_source(&source_path, &canonical_library_path)?;
            if !seen_locations.insert(relative_path.clone()) {
                duplicate_files += 1;
                continue;
            }

            let InitialTags {
                metadata,
                rating,
                has_embedded_artwork,
            } = self
                .metadata_service
                .read_initial_tags(&source_path)
                .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            let file_size_bytes = fs::metadata(&source_path)
                .map(|metadata| metadata.len())
                .ok();

            let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                return Err(ApplicationRuntimeError::LibraryStoreFailed);
            };
            next_track_id += 1;
            tracks.push(Track {
                id: track_id,
                location: TrackLocation::available(relative_path),
                metadata,
                rating,
                statistics: PlayStatistics {
                    date_added_at: Some(SystemTime::now()),
                    ..PlayStatistics::default()
                },
                file_size_bytes,
                has_embedded_artwork: Some(has_embedded_artwork),
            });
        }

        if self.library_store.save_tracks(&tracks).is_err() {
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        }

        let imported_tracks = tracks.len();
        Ok(LibraryImportResult {
            tracks,
            summary: LibraryImportSummary {
                discovered_files: discovered_files.len(),
                imported_tracks,
                duplicate_files,
                cancelled: false,
            },
        })
    }

    /// Whether a file with the given size and freshly computed content
    /// hash is already in the library, decided against ground truth on
    /// disk. The stored `content_hash` column is import-time only and is
    /// not refreshed after in-place tag / rating / artwork / enrichment
    /// writes (and is absent for scan-imported tracks), so it is never
    /// consulted here — an existing track whose on-disk size matches is
    /// hashed fresh and compared byte-for-byte. The size pre-filter keeps
    /// this from hashing the whole library on every import (see #72).
    fn library_contains_matching_content(
        &self,
        library_path: &Path,
        source_size: u64,
        content_hash: &TrackContentHash,
    ) -> ApplicationRuntimeResult<bool> {
        for track in &self.existing_tracks {
            let track_path = track.location.absolute_path(library_path);
            let Ok(metadata) = fs::metadata(&track_path) else {
                continue;
            };
            if !metadata.is_file() || metadata.len() != source_size {
                continue;
            }
            let Ok(existing_hash) = hash_file_content(&track_path) else {
                continue;
            };
            if &existing_hash == content_hash {
                return Ok(true);
            }
        }

        Ok(false)
    }
}

struct PlannedManagedImport {
    source_path: PathBuf,
    destination_path: PathBuf,
    relative_path: sustain_domain::TrackRelativePath,
    content_hash: TrackContentHash,
    metadata: sustain_domain::TrackMetadata,
    rating: Rating,
    file_size_bytes: u64,
    has_embedded_artwork: bool,
}

fn collect_supported_audio_files(
    paths: &[PathBuf],
    cancellation: &AtomicBool,
) -> ApplicationRuntimeResult<Vec<PathBuf>> {
    let mut files = Vec::new();
    for path in paths {
        if cancellation.load(Ordering::SeqCst) {
            return Ok(files);
        }
        collect_supported_audio_path(path, &mut files, cancellation)?;
    }
    files.sort();
    files.dedup();
    Ok(files)
}

fn collect_supported_audio_path(
    path: &Path,
    files: &mut Vec<PathBuf>,
    cancellation: &AtomicBool,
) -> ApplicationRuntimeResult<()> {
    if cancellation.load(Ordering::SeqCst) {
        return Ok(());
    }
    let metadata =
        fs::symlink_metadata(path).map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
    if metadata.file_type().is_dir() {
        let entries =
            fs::read_dir(path).map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
        for entry in entries {
            if cancellation.load(Ordering::SeqCst) {
                return Ok(());
            }
            let entry = entry.map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
            collect_supported_audio_path(&entry.path(), files, cancellation)?;
        }
    } else if metadata.file_type().is_file() && audio_format_from_path(path).is_ok() {
        files.push(path.to_path_buf());
    }
    Ok(())
}

fn cancelled_import_result(discovered_files: usize) -> LibraryImportResult {
    LibraryImportResult {
        tracks: Vec::new(),
        summary: LibraryImportSummary {
            discovered_files,
            imported_tracks: 0,
            duplicate_files: 0,
            cancelled: true,
        },
    }
}

/// Outcome of planning a managed destination for an incoming file.
enum PlannedManagedDestination {
    /// A free canonical path the file should be copied to.
    Fresh(sustain_domain::ManagedTrackPathPlan),
    /// The canonical destination is already occupied on disk by a
    /// byte-identical file, so the track is already in the library and
    /// the import must skip it rather than write a numbered copy.
    AlreadyPresent,
}

fn plan_destination(
    planner: &ManagedTrackPathPlanner,
    occupied_paths: &mut BTreeSet<sustain_domain::TrackRelativePath>,
    library_path: &Path,
    source_path: &Path,
    metadata: &sustain_domain::TrackMetadata,
    source_size: u64,
    content_hash: &TrackContentHash,
) -> ApplicationRuntimeResult<PlannedManagedDestination> {
    for _attempt in 0..10_000 {
        let plan = planner
            .plan(
                ManagedTrackPathInput {
                    metadata,
                    source_path,
                },
                occupied_paths,
            )
            .map_err(|_| ApplicationRuntimeError::LibraryImportFailed)?;
        let candidate = library_path.join(plan.relative_path.as_path());
        if candidate.exists() {
            // Disk-anchored strict-exact guard. The hash-based dedup
            // above trusts the database, which can disagree with the
            // disk: the row may be absent (dropped database), carry no
            // hash (added by scan), or carry a stale one (tag edits and
            // online enrichment rewrite the file without refreshing it).
            // When any of those let a file that is physically already
            // here slip through, the planner would otherwise bump to a
            // numbered name and copy_file_verified would write a
            // byte-identical duplicate. The occupant on disk is ground
            // truth: if it matches the source byte for byte, the track
            // is already in the library, so skip it.
            if destination_holds_identical_content(&candidate, source_size, content_hash) {
                return Ok(PlannedManagedDestination::AlreadyPresent);
            }
            occupied_paths.insert(plan.relative_path);
            continue;
        }
        occupied_paths.insert(plan.relative_path.clone());
        return Ok(PlannedManagedDestination::Fresh(plan));
    }

    Err(ApplicationRuntimeError::LibraryImportFailed)
}

fn destination_holds_identical_content(
    candidate: &Path,
    source_size: u64,
    content_hash: &TrackContentHash,
) -> bool {
    let Ok(metadata) = fs::metadata(candidate) else {
        return false;
    };
    if !metadata.is_file() || metadata.len() != source_size {
        return false;
    }
    matches!(hash_file_content(candidate), Ok(hash) if &hash == content_hash)
}

fn reference_relative_path_for_source(
    source_path: &Path,
    library_path: &Path,
) -> ApplicationRuntimeResult<sustain_domain::TrackRelativePath> {
    if let Ok(relative_path) = source_path.strip_prefix(library_path)
        && let Some(relative_path) =
            sustain_domain::TrackRelativePath::new(relative_path.to_path_buf())
    {
        return Ok(relative_path);
    }

    Err(ApplicationRuntimeError::LibraryImportFailed)
}

fn source_relative_path_inside_library(
    source_path: &Path,
    library_path: Option<&Path>,
) -> Option<sustain_domain::TrackRelativePath> {
    let library_path = library_path?;
    source_path
        .strip_prefix(library_path)
        .ok()
        .and_then(|relative_path| {
            sustain_domain::TrackRelativePath::new(relative_path.to_path_buf())
        })
}
