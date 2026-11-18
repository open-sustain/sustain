// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adding external files to the library, in either management mode: copying
//! verified files into the canonical layout, or referencing them in place.
//! Both modes deduplicate by relative path and content hash. Managed imports
//! commit copied files in bounded durable chunks; completed chunks remain in
//! the library, while the active chunk is rolled back if its database write
//! fails before SQLite accepts the rows.

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::SystemTime,
};

use sustain_domain::{
    ManagedTrackPathInput, ManagedTrackPathPlanner, PlayStatistics, Track, TrackContentHash,
    TrackLocation,
};
use sustain_metadata::{InitialTags, hash_file_content, is_audio_name_for};

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult, ImportFailure, ImportFailureReason,
    LibraryImportProgress, LibraryImportResult, LibraryImportSummary, LibraryImportTask,
    library_scan,
};

use super::{
    capabilities::ManagedLibraryFilesystemValidator,
    file_ops::{
        VerifiedFileCopy, VerifiedFileCopyError, copy_file_to_staging_verified,
        prune_empty_ancestor_directories_for_sources, publish_staged_file,
        refresh_verified_staging, remove_staged_file,
    },
};

const MANAGED_IMPORT_COMMIT_CHUNK_SIZE: usize = 32;

#[derive(Default)]
struct ManagedImportState {
    imported_tracks: Vec<Track>,
    pending_tracks: Vec<Track>,
    pending_copies: Vec<PendingCopy>,
    duplicate_files: usize,
    /// Files left out, each with its reason: those skipped one by one as
    /// the import carried on, and those of a chunk the library refused.
    failures: Vec<ImportFailure>,
}

/// A copy published into the library folder that no committed chunk
/// references yet, with the file it was copied from.
struct PendingCopy {
    source_path: PathBuf,
    copy: VerifiedFileCopy,
}

impl ManagedImportState {
    /// Removes the copies no committed chunk references, recording each
    /// source file as left out, and whether its copy is gone again.
    fn roll_back_pending_copies(&mut self, library_path: &Path) {
        self.pending_tracks.clear();
        let pending_copies = std::mem::take(&mut self.pending_copies);
        for pending in &pending_copies {
            let removed = pending.copy.remove().is_ok();
            self.failures.push(ImportFailure {
                path: pending.source_path.clone(),
                reason: if removed {
                    ImportFailureReason::NotRecorded
                } else {
                    ImportFailureReason::NotRecordedCopyKept
                },
            });
        }
        let destinations = pending_copies
            .iter()
            .map(|pending| pending.copy.destination_path.clone())
            .collect::<Vec<_>>();
        prune_empty_ancestor_directories_for_sources(library_path, &destinations);
    }
}

/// How an import ended, for its summary.
#[derive(Clone, Copy, Eq, PartialEq)]
enum ImportEnd {
    Completed,
    Cancelled,
    StoppedOnError,
}

pub fn run_library_import_task(
    task: LibraryImportTask,
) -> ApplicationRuntimeResult<LibraryImportResult> {
    run_library_import_task_with_progress(task, |_| {})
}

pub fn run_library_import_task_with_progress(
    task: LibraryImportTask,
    mut progress: impl FnMut(LibraryImportProgress),
) -> ApplicationRuntimeResult<LibraryImportResult> {
    let mut context = LibraryImportContext {
        settings: task.settings,
        existing_tracks: task.existing_tracks,
        library_store: task.library_store,
        metadata_service: task.metadata_service,
        managed_library_filesystem_validator: task.managed_library_filesystem_validator,
        cancellation_requested: task.cancellation_requested,
        progress: &mut progress,
    };

    context.add_external_library_items(task.paths)
}

struct LibraryImportContext<'a> {
    settings: sustain_domain::UserSettings,
    existing_tracks: Vec<Track>,
    library_store: std::sync::Arc<dyn sustain_library_store::LibraryStore>,
    metadata_service: std::sync::Arc<dyn sustain_metadata::MetadataService>,
    managed_library_filesystem_validator: ManagedLibraryFilesystemValidator,
    cancellation_requested: Arc<AtomicBool>,
    progress: &'a mut dyn FnMut(LibraryImportProgress),
}

impl LibraryImportContext<'_> {
    fn add_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        if paths.is_empty() {
            return Ok(LibraryImportResult {
                tracks: Vec::new(),
                summary: LibraryImportSummary::default(),
                failures: Vec::new(),
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

    /// Copies each discovered file into the library. A file that cannot be
    /// brought in is recorded and skipped (#298); only a failure of the
    /// library itself (its store, its folder, its track ids) stops the
    /// import.
    fn add_managed_external_library_items(
        &mut self,
        paths: Vec<PathBuf>,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        self.managed_library_filesystem_validator
            .validate(&library_path)
            .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;

        let DiscoveredFiles { files, failures } = collect_supported_audio_files(
            paths,
            &library_path,
            &HashSet::new(),
            self.cancellation_requested.as_ref(),
        );
        let mut state = ManagedImportState {
            failures,
            ..ManagedImportState::default()
        };
        let total_files = files.len();
        if self.cancellation_requested.load(Ordering::SeqCst) {
            return Ok(managed_import_result(
                total_files,
                state,
                ImportEnd::Cancelled,
            ));
        }

        let mut batch = ManagedImportBatch::new(&self.existing_tracks, library_path);
        let mut next_track_id = library_scan::next_track_id(&self.existing_tracks)?;

        for (index, file) in files.iter().enumerate() {
            let source_path = file.first_name();
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return self.finish_cancelled_managed_import(
                    &batch.library_path,
                    total_files,
                    state,
                );
            }
            match batch.import_file(self.metadata_service.as_ref(), source_path) {
                Err(reason) => state.failures.push(ImportFailure {
                    path: source_path.to_path_buf(),
                    reason,
                }),
                Ok(ManagedFileOutcome::Duplicate) => state.duplicate_files += 1,
                Ok(ManagedFileOutcome::Copied(copied)) => {
                    let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                        state.pending_copies.push(PendingCopy {
                            source_path: source_path.to_path_buf(),
                            copy: copied.copy,
                        });
                        return self.finish_failed_managed_import(
                            &batch.library_path,
                            total_files,
                            state,
                            ApplicationRuntimeError::LibraryStoreFailed,
                        );
                    };
                    next_track_id += 1;
                    let ManagedCopy {
                        copy,
                        relative_path,
                        tags,
                        size,
                    } = *copied;
                    state.pending_tracks.push(Track {
                        id: track_id,
                        location: TrackLocation::available(relative_path),
                        metadata: tags.metadata,
                        rating: tags.rating,
                        statistics: PlayStatistics::added_at(SystemTime::now()),
                        file_size_bytes: Some(size),
                        has_embedded_artwork: Some(tags.has_embedded_artwork),
                        // The first scan after import fingerprints the file and
                        // records its mtime; until then it parses, which is correct
                        // and self-heals (#71).
                        file_modified_at: None,
                    });
                    state.pending_copies.push(PendingCopy {
                        source_path: source_path.to_path_buf(),
                        copy,
                    });
                    if state.pending_tracks.len() >= MANAGED_IMPORT_COMMIT_CHUNK_SIZE
                        && let Err(error) =
                            self.commit_managed_import_chunk(&batch.library_path, &mut state)
                    {
                        return stop_after_commit_error(total_files, state, error);
                    }
                }
            }
            self.report_progress(index + 1, total_files);
        }

        if self.cancellation_requested.load(Ordering::SeqCst) {
            return self.finish_cancelled_managed_import(&batch.library_path, total_files, state);
        }
        if let Err(error) = self.commit_managed_import_chunk(&batch.library_path, &mut state) {
            return stop_after_commit_error(total_files, state, error);
        }
        Ok(managed_import_result(
            total_files,
            state,
            ImportEnd::Completed,
        ))
    }

    fn commit_managed_import_chunk(
        &mut self,
        library_path: &Path,
        state: &mut ManagedImportState,
    ) -> Result<(), ManagedImportCommitError> {
        if state.pending_tracks.is_empty() {
            return Ok(());
        }

        if self
            .library_store
            .save_tracks(&state.pending_tracks)
            .is_err()
        {
            state.roll_back_pending_copies(library_path);
            return Err(ManagedImportCommitError::SaveFailed);
        }

        let flush_result = self.library_store.flush_durable();
        state.imported_tracks.append(&mut state.pending_tracks);
        state.pending_copies.clear();
        if flush_result.is_err() {
            return Err(ManagedImportCommitError::FlushFailed);
        }
        Ok(())
    }

    fn finish_cancelled_managed_import(
        &mut self,
        library_path: &Path,
        discovered_files: usize,
        mut state: ManagedImportState,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        if let Err(error) = self.commit_managed_import_chunk(library_path, &mut state) {
            return stop_after_commit_error(discovered_files, state, error);
        }
        Ok(managed_import_result(
            discovered_files,
            state,
            ImportEnd::Cancelled,
        ))
    }

    /// Stops the import on an error of the library itself, rolling back the
    /// copies no committed chunk references yet.
    fn finish_failed_managed_import(
        &mut self,
        library_path: &Path,
        discovered_files: usize,
        mut state: ManagedImportState,
        error: ApplicationRuntimeError,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        state.roll_back_pending_copies(library_path);
        stopped_managed_import_or_error(discovered_files, state, error)
    }

    fn report_progress(&mut self, processed_files: usize, total_files: usize) {
        (self.progress)(LibraryImportProgress {
            processed_files,
            total_files,
        });
    }

    /// References each discovered file where it lies. A file that cannot be
    /// referenced is recorded and skipped (#298); the tracks are saved in
    /// one write at the end, and a refused write fails the whole import.
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

        // The names the library holds tracks by, so a file it holds under a
        // name the walk passes by is still told apart from a new one.
        let known: HashSet<PathBuf> = self
            .existing_tracks
            .iter()
            .map(|track| track.location.absolute_path(&library_path))
            .collect();
        let DiscoveredFiles {
            files,
            mut failures,
        } = collect_supported_audio_files(
            paths,
            &library_path,
            &known,
            self.cancellation_requested.as_ref(),
        );
        let total_files = files.len();
        if self.cancellation_requested.load(Ordering::SeqCst) {
            return Ok(cancelled_import_result(total_files, failures, 0));
        }
        let mut seen_locations = self
            .existing_tracks
            .iter()
            .map(|track| track.location.relative_path.clone())
            .collect::<BTreeSet<_>>();

        let mut next_track_id = library_scan::next_track_id(&self.existing_tracks)?;
        let mut tracks = Vec::new();
        let mut duplicate_files = 0;

        for (index, file) in files.iter().enumerate() {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return Ok(cancelled_import_result(
                    total_files,
                    failures,
                    duplicate_files,
                ));
            }
            match self.reference_file(
                file,
                [&library_path, &canonical_library_path],
                &mut seen_locations,
            ) {
                Err(reason) => failures.push(ImportFailure {
                    path: file.first_name().to_path_buf(),
                    reason,
                }),
                Ok(None) => duplicate_files += 1,
                Ok(Some((relative_path, tags, file_size_bytes))) => {
                    let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                        return Err(ApplicationRuntimeError::LibraryStoreFailed);
                    };
                    next_track_id += 1;
                    tracks.push(Track {
                        id: track_id,
                        location: TrackLocation::available(relative_path),
                        metadata: tags.metadata,
                        rating: tags.rating,
                        statistics: PlayStatistics::added_at(SystemTime::now()),
                        file_size_bytes,
                        has_embedded_artwork: Some(tags.has_embedded_artwork),
                        // Recorded by the first post-import scan; see above (#71).
                        file_modified_at: None,
                    });
                }
            }
            self.report_progress(index + 1, total_files);
        }

        if self.library_store.save_tracks(&tracks).is_err() {
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        }

        Ok(LibraryImportResult {
            summary: LibraryImportSummary {
                discovered_files: total_files,
                imported_tracks: tracks.len(),
                duplicate_files,
                failed_files: failures.len(),
                cancelled: false,
                stopped_on_error: false,
            },
            tracks,
            failures,
        })
    }

    /// The library location, tags and size of one file to reference in
    /// place, given every name the import reached it by, or `None` when
    /// the library already references it under one of them.
    ///
    /// The file is referenced where it is, if that is in the library
    /// folder. A file the library folder only links to is referenced
    /// under its first name in the library folder, as a scan would find
    /// it.
    /// `library_paths` are the library folder's path as configured and as
    /// resolved.
    fn reference_file(
        &self,
        file: &DiscoveredFile,
        library_paths: [&Path; 2],
        seen_locations: &mut BTreeSet<sustain_domain::TrackRelativePath>,
    ) -> Result<
        Option<(sustain_domain::TrackRelativePath, InitialTags, Option<u64>)>,
        ImportFailureReason,
    > {
        let [library_path, canonical_library_path] = library_paths;
        let resolved = sustain_metadata::resolve_track_file(file.first_name())
            .ok()
            .flatten()
            .ok_or(ImportFailureReason::Unreadable)?;
        let where_it_is = reference_relative_path_for_source(&resolved, canonical_library_path);
        let as_named = file.names.iter().filter_map(|name| {
            [library_path, canonical_library_path]
                .into_iter()
                .find_map(|library_path| reference_relative_path_for_source(name, library_path))
        });
        let locations: Vec<_> = where_it_is.into_iter().chain(as_named).collect();
        let Some(relative_path) = locations.first().cloned() else {
            return Err(ImportFailureReason::OutsideLibraryFolder);
        };
        let tag_source = relative_path.resolve(library_path);
        let locations: BTreeSet<_> = locations.into_iter().collect();
        let already_referenced = locations
            .iter()
            .any(|location| seen_locations.contains(location));
        seen_locations.extend(locations);
        if already_referenced {
            return Ok(None);
        }
        let tags = self
            .metadata_service
            .read_initial_tags(&tag_source)
            .map_err(|_| ImportFailureReason::UnreadableTags)?;
        let file_size_bytes = fs::metadata(&resolved).map(|metadata| metadata.len()).ok();
        Ok(Some((relative_path, tags, file_size_bytes)))
    }
}

enum ManagedImportCommitError {
    SaveFailed,
    FlushFailed,
}

/// Ends an import whose chunk commit failed. A refused save rolled its
/// chunk back and stops the import; a failed flush keeps the saved tracks
/// and reports the import as stopped.
fn stop_after_commit_error(
    discovered_files: usize,
    state: ManagedImportState,
    error: ManagedImportCommitError,
) -> ApplicationRuntimeResult<LibraryImportResult> {
    match error {
        ManagedImportCommitError::SaveFailed => stopped_managed_import_or_error(
            discovered_files,
            state,
            ApplicationRuntimeError::LibraryStoreFailed,
        ),
        ManagedImportCommitError::FlushFailed => Ok(managed_import_result(
            discovered_files,
            state,
            ImportEnd::StoppedOnError,
        )),
    }
}

/// A stopped import that has something to report (committed tracks,
/// skipped duplicates, files it left out) is reported as stopped; one that
/// has nothing is the error itself.
fn stopped_managed_import_or_error(
    discovered_files: usize,
    state: ManagedImportState,
    error: ApplicationRuntimeError,
) -> ApplicationRuntimeResult<LibraryImportResult> {
    if state.imported_tracks.is_empty() && state.duplicate_files == 0 && state.failures.is_empty() {
        return Err(error);
    }
    Ok(managed_import_result(
        discovered_files,
        state,
        ImportEnd::StoppedOnError,
    ))
}

fn managed_import_result(
    discovered_files: usize,
    state: ManagedImportState,
    end: ImportEnd,
) -> LibraryImportResult {
    LibraryImportResult {
        summary: LibraryImportSummary {
            discovered_files,
            imported_tracks: state.imported_tracks.len(),
            duplicate_files: state.duplicate_files,
            failed_files: state.failures.len(),
            cancelled: end == ImportEnd::Cancelled,
            stopped_on_error: end == ImportEnd::StoppedOnError,
        },
        tracks: state.imported_tracks,
        failures: state.failures,
    }
}

/// What became of one file a managed import met.
enum ManagedFileOutcome {
    /// The library already holds it.
    Duplicate,
    /// It was copied into the library and awaits its track row.
    Copied(Box<ManagedCopy>),
}

struct ManagedCopy {
    copy: VerifiedFileCopy,
    relative_path: sustain_domain::TrackRelativePath,
    tags: InitialTags,
    size: u64,
}

/// What a managed import knows across its files: where the library is,
/// which paths and contents it already holds, and those this batch added.
struct ManagedImportBatch {
    library_path: PathBuf,
    canonical_library_path: Option<PathBuf>,
    planner: ManagedTrackPathPlanner,
    occupied_paths: BTreeSet<sustain_domain::TrackRelativePath>,
    // Within-batch dedup only: catch two byte-identical source files
    // dropped in the same import. Cross-existing dedup is decided
    // against ground truth on disk by `LibraryContentIndex`, never
    // against the stored content hash, which is import-time only and
    // may be stale or absent (see #72).
    seen_hashes: HashSet<String>,
    // Decide existing-library duplicates against a size-bucketed,
    // hash-cached index built once from disk. The per-file linear scan
    // this replaces was O(new × existing) and re-hashed existing files on
    // every size collision, which dominated import time on a large
    // library (#146).
    content_index: LibraryContentIndex,
}

impl ManagedImportBatch {
    fn new(existing_tracks: &[Track], library_path: PathBuf) -> Self {
        Self {
            canonical_library_path: fs::canonicalize(&library_path).ok(),
            planner: ManagedTrackPathPlanner::default(),
            occupied_paths: existing_tracks
                .iter()
                .map(|track| track.location.relative_path.clone())
                .collect(),
            seen_hashes: HashSet::new(),
            content_index: LibraryContentIndex::build(existing_tracks, &library_path),
            library_path,
        }
    }

    fn is_duplicate(&mut self, size: u64, content_hash: &TrackContentHash) -> bool {
        self.seen_hashes.contains(content_hash.as_str())
            || self.content_index.contains_matching(size, content_hash)
    }

    /// Copies the file `name` resolves to into the library, or says why it
    /// cannot be. Its tags are read through `name`, as a scan reads a
    /// track's: the same file, under the name it was dropped by.
    fn import_file(
        &mut self,
        metadata_service: &dyn sustain_metadata::MetadataService,
        name: &Path,
    ) -> Result<ManagedFileOutcome, ImportFailureReason> {
        let source_path = sustain_metadata::resolve_track_file(name)
            .ok()
            .flatten()
            .ok_or(ImportFailureReason::Unreadable)?;
        if let Some(relative_path) = source_relative_path_inside_library(
            &source_path,
            self.canonical_library_path.as_deref(),
        ) && self.occupied_paths.contains(&relative_path)
        {
            return Ok(ManagedFileOutcome::Duplicate);
        }

        // Hash the source while copying it once into an unpublished
        // staging file. The staged descriptor is then rewound and hashed
        // independently before publication, retaining the read-back
        // integrity assertion without a second source read (#146).
        let mut staging =
            copy_file_to_staging_verified(&source_path, &self.library_path).map_err(|error| {
                match error {
                    VerifiedFileCopyError::SourceUnavailable
                    | VerifiedFileCopyError::SourceIsNotFile => ImportFailureReason::Unreadable,
                    _ => ImportFailureReason::CopyFailed,
                }
            })?;
        let original_content_hash = staging.content_hash.clone();
        if self.is_duplicate(staging.bytes_copied, &original_content_hash) {
            remove_staged_file(staging);
            return Ok(ManagedFileOutcome::Duplicate);
        }

        let initial_read = match metadata_service.read_initial_tags_with_diagnostics(name) {
            Ok(read) => read,
            Err(_) => {
                remove_staged_file(staging);
                return Err(ImportFailureReason::UnreadableTags);
            }
        };
        let mut tags = initial_read.tags;
        let repaired_staging = initial_read.repair.is_some_and(|repair| {
            matches!(
                metadata_service.repair_malformed_tags(staging.path(), repair),
                Ok(true)
            )
        });
        if repaired_staging {
            if refresh_verified_staging(&mut staging).is_err() {
                remove_staged_file(staging);
                return Err(ImportFailureReason::CopyFailed);
            }
            if let Ok(repaired_tags) = metadata_service.read_initial_tags(staging.path()) {
                tags = repaired_tags;
            }
            if self.is_duplicate(staging.bytes_copied, &staging.content_hash) {
                remove_staged_file(staging);
                return Ok(ManagedFileOutcome::Duplicate);
            }
        }
        let size = staging.bytes_copied;
        let content_hash = staging.content_hash.clone();

        let plan = match plan_destination(
            &self.planner,
            &mut self.occupied_paths,
            &self.library_path,
            name,
            &tags.metadata,
            size,
            &content_hash,
        ) {
            Some(PlannedManagedDestination::Fresh(plan)) => plan,
            Some(PlannedManagedDestination::AlreadyPresent) => {
                remove_staged_file(staging);
                return Ok(ManagedFileOutcome::Duplicate);
            }
            None => {
                remove_staged_file(staging);
                return Err(ImportFailureReason::NoDestination);
            }
        };
        let destination_path = self.library_path.join(plan.relative_path.as_path());
        let Ok(copy) = publish_staged_file(staging, &destination_path) else {
            // Publishing removed its staging and any link it made; only the
            // folders it created for the destination may be left.
            prune_empty_ancestor_directories_for_sources(&self.library_path, &[destination_path]);
            return Err(ImportFailureReason::CopyFailed);
        };
        self.seen_hashes
            .insert(original_content_hash.as_str().to_owned());
        self.seen_hashes.insert(content_hash.as_str().to_owned());
        Ok(ManagedFileOutcome::Copied(Box::new(ManagedCopy {
            copy,
            relative_path: plan.relative_path,
            tags,
            size,
        })))
    }
}

/// Disk-truth duplicate index for one managed-import batch.
///
/// Deciding whether an imported file already exists means comparing content
/// hashes, but hashing every existing track for every imported file is
/// O(new × existing) and re-reads colliding files repeatedly — it dominated
/// import wall time on a large library (#146). This narrows the comparison to
/// existing files of the *same byte size*, gathered once, and hashes each
/// existing file at most once across the whole batch.
///
/// Sizes and hashes come from disk, never from the stored `file_size_bytes`
/// or the (import-time-only, possibly stale or absent) content-hash column —
/// the dedup stays grounded in current ground truth, exactly as before (#72).
struct LibraryContentIndex {
    paths_by_size: HashMap<u64, Vec<PathBuf>>,
    hashes: HashMap<PathBuf, TrackContentHash>,
}

impl LibraryContentIndex {
    fn build(existing_tracks: &[Track], library_path: &Path) -> Self {
        let mut paths_by_size: HashMap<u64, Vec<PathBuf>> = HashMap::new();
        for track in existing_tracks {
            // A track whose path names no file of its own is in no state
            // to stand for a dropped file.
            let Ok(Some(path)) =
                sustain_metadata::resolve_track_file(&track.location.absolute_path(library_path))
            else {
                continue;
            };
            let Ok(metadata) = fs::metadata(&path) else {
                continue;
            };
            if metadata.is_file() {
                paths_by_size.entry(metadata.len()).or_default().push(path);
            }
        }
        Self {
            paths_by_size,
            hashes: HashMap::new(),
        }
    }

    /// Whether a file with this size and freshly computed content hash is
    /// already in the library. Only existing files of a matching size are
    /// hashed, and each is hashed at most once — its hash is memoized for the
    /// rest of the batch.
    fn contains_matching(&mut self, source_size: u64, content_hash: &TrackContentHash) -> bool {
        let Self {
            paths_by_size,
            hashes,
        } = self;
        let Some(candidates) = paths_by_size.get(&source_size) else {
            return false;
        };
        for candidate in candidates {
            if let Some(existing_hash) = hashes.get(candidate) {
                if existing_hash == content_hash {
                    return true;
                }
                continue;
            }
            let Ok(existing_hash) = hash_file_content(candidate) else {
                continue;
            };
            let matches = &existing_hash == content_hash;
            hashes.insert(candidate.clone(), existing_hash);
            if matches {
                return true;
            }
        }
        false
    }
}

/// The supported audio files under the paths an import was given, and the
/// paths it could not look into.
struct DiscoveredFiles {
    files: Vec<DiscoveredFile>,
    failures: Vec<ImportFailure>,
}

/// A file an import found, under every supported audio name it reached
/// it by: the names with no link in them first, as a scan prefers them,
/// each kind in name order.
struct DiscoveredFile {
    names: Vec<PathBuf>,
}

impl DiscoveredFile {
    fn first_name(&self) -> &Path {
        &self.names[0]
    }
}

/// The folders and files an import of `paths` walks, in the order it walks
/// them. A folder is walked once, under the name walked first, so the
/// library folder's names come first: a path in the library folder is
/// spelled as the library's path is, and the library folder is walked
/// before a dropped folder that holds it. The rest of the order is the
/// paths', not the drop's.
fn import_roots(paths: Vec<PathBuf>, library_path: &Path) -> Vec<PathBuf> {
    let canonical_library_path = fs::canonicalize(library_path).ok();
    let mut roots = BTreeSet::new();
    for path in paths {
        let in_library = canonical_library_path
            .as_deref()
            .filter(|_| !path.starts_with(library_path))
            .and_then(|canonical| path.strip_prefix(canonical).ok())
            .map(|relative| library_path.join(relative));
        let path = in_library.unwrap_or(path);
        let holds_library = library_path.starts_with(&path)
            || canonical_library_path
                .as_deref()
                .is_some_and(|canonical| canonical.starts_with(&path));
        if holds_library {
            roots.insert((false, library_path.to_path_buf()));
        }
        roots.insert((!path.starts_with(library_path), path));
    }
    roots.into_iter().map(|(_, path)| path).collect()
}

/// The supported audio files at or under `paths`, found by a walk that
/// follows links as [`sustain_metadata::walk`] describes, given the names
/// the library at `library_path` already knows files by.
fn collect_supported_audio_files(
    paths: Vec<PathBuf>,
    library_path: &Path,
    known: &HashSet<PathBuf>,
    cancellation: &AtomicBool,
) -> DiscoveredFiles {
    let paths = import_roots(paths, library_path);
    let walk = sustain_metadata::walk::walk_files(&sustain_metadata::walk::WalkRequest {
        roots: &paths,
        known,
        accept: &is_audio_name_for,
        skip: &|_| false,
        cancellation,
    });
    let mut files: Vec<DiscoveredFile> = walk
        .files
        .into_iter()
        .map(|file| {
            // Sorted, so the name an import picks is not the drop's order.
            let names: BTreeSet<(bool, PathBuf)> = file
                .names
                .into_iter()
                .map(|name| (name.through_link, name.path))
                .collect();
            DiscoveredFile {
                names: names.into_iter().map(|(_, path)| path).collect(),
            }
        })
        .collect();
    files.sort_by(|left, right| left.first_name().cmp(right.first_name()));
    DiscoveredFiles {
        files,
        failures: walk
            .failures
            .into_iter()
            .map(|path| ImportFailure {
                path,
                reason: ImportFailureReason::Unreadable,
            })
            .collect(),
    }
}

/// A reference import the user cancelled. Its tracks are saved in one write
/// at the end, so none were added; what it had already found out about the
/// files (failures, duplicates) is still reported.
fn cancelled_import_result(
    discovered_files: usize,
    failures: Vec<ImportFailure>,
    duplicate_files: usize,
) -> LibraryImportResult {
    LibraryImportResult {
        tracks: Vec::new(),
        summary: LibraryImportSummary {
            discovered_files,
            duplicate_files,
            failed_files: failures.len(),
            cancelled: true,
            ..LibraryImportSummary::default()
        },
        failures,
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
) -> Option<PlannedManagedDestination> {
    let candidates = planner
        .candidates(ManagedTrackPathInput {
            metadata,
            source_path,
        })
        .ok()?;
    for candidate in candidates {
        let plan = candidate.ok()?;
        if occupied_paths.contains(&plan.relative_path) {
            continue;
        }
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
                return Some(PlannedManagedDestination::AlreadyPresent);
            }
            continue;
        }
        occupied_paths.insert(plan.relative_path.clone());
        return Some(PlannedManagedDestination::Fresh(plan));
    }

    None
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
) -> Option<sustain_domain::TrackRelativePath> {
    let relative_path = source_path.strip_prefix(library_path).ok()?;
    sustain_domain::TrackRelativePath::new(relative_path.to_path_buf())
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use sustain_metadata::hash_file_content;

    use super::{ManagedImportState, PendingCopy};
    use crate::{
        ImportFailure, ImportFailureReason, managed_library::file_ops::copy_file_verified,
    };

    #[test]
    fn managed_import_rollback_removes_copies_names_their_sources_and_prunes_folders() {
        let root = unique_test_directory();
        let copied = root.join("Artist/Album/song.flac");
        let source = root.join("source.flac");
        fs::create_dir_all(copied.parent().expect("copied parent")).expect("create copied parent");
        fs::write(&source, b"audio").expect("write source");
        let hash = hash_file_content(&source).expect("hash source");
        let copy = copy_file_verified(&source, &copied, &hash).expect("copy file");
        let mut state = ManagedImportState {
            pending_copies: vec![PendingCopy {
                source_path: source.clone(),
                copy,
            }],
            ..ManagedImportState::default()
        };

        state.roll_back_pending_copies(&root);

        assert_eq!(
            state.failures,
            vec![ImportFailure {
                path: source,
                reason: ImportFailureReason::NotRecorded,
            }]
        );
        assert!(state.pending_copies.is_empty());
        assert!(root.exists());
        assert!(!root.join("Artist").exists());

        fs::remove_dir_all(root).expect("remove test root");
    }

    fn unique_test_directory() -> PathBuf {
        let unique_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("sustain_managed_import_test_{unique_suffix}"))
    }
}
