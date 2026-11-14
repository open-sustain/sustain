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
use sustain_metadata::{InitialTags, audio_format_from_path, hash_file_content};

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
        refresh_verified_staging, remove_copied_files, remove_staged_file,
    },
};

const MANAGED_IMPORT_COMMIT_CHUNK_SIZE: usize = 32;

#[derive(Default)]
struct ManagedImportState {
    imported_tracks: Vec<Track>,
    pending_tracks: Vec<Track>,
    pending_copied_files: Vec<VerifiedFileCopy>,
    duplicate_files: usize,
    /// Files skipped one by one, the import carrying on past each.
    failures: Vec<ImportFailure>,
    /// Copied files whose chunk the library store refused, rolled back
    /// before the import stopped.
    rolled_back_files: usize,
}

impl ManagedImportState {
    fn failed_files(&self) -> usize {
        self.failures.len() + self.rolled_back_files
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

        let DiscoveredFiles { files, failures } =
            collect_supported_audio_files(&paths, self.cancellation_requested.as_ref());
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

        for (index, source_path) in files.iter().enumerate() {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return self.finish_cancelled_managed_import(
                    &batch.library_path,
                    total_files,
                    state,
                );
            }
            match batch.import_file(self.metadata_service.as_ref(), source_path) {
                Err(reason) => state.failures.push(ImportFailure {
                    path: source_path.clone(),
                    reason,
                }),
                Ok(ManagedFileOutcome::Duplicate) => state.duplicate_files += 1,
                Ok(ManagedFileOutcome::Copied(copied)) => {
                    let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                        state.pending_copied_files.push(copied.copy);
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
                    state.pending_copied_files.push(copy);
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
            state.rolled_back_files += state.pending_tracks.len();
            let _ = rollback_managed_import_files(library_path, &state.pending_copied_files);
            state.pending_tracks.clear();
            state.pending_copied_files.clear();
            return Err(ManagedImportCommitError::SaveFailed);
        }

        let flush_result = self.library_store.flush_durable();
        state.imported_tracks.append(&mut state.pending_tracks);
        state.pending_copied_files.clear();
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
        state.rolled_back_files += state.pending_copied_files.len();
        let _ = rollback_managed_import_files(library_path, &state.pending_copied_files);
        state.pending_tracks.clear();
        state.pending_copied_files.clear();
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

        let DiscoveredFiles {
            files,
            mut failures,
        } = collect_supported_audio_files(&paths, self.cancellation_requested.as_ref());
        let total_files = files.len();
        if self.cancellation_requested.load(Ordering::SeqCst) {
            return Ok(cancelled_import_result(total_files));
        }
        let mut seen_locations = self
            .existing_tracks
            .iter()
            .map(|track| track.location.relative_path.clone())
            .collect::<BTreeSet<_>>();

        let mut next_track_id = library_scan::next_track_id(&self.existing_tracks)?;
        let mut tracks = Vec::new();
        let mut duplicate_files = 0;

        for (index, source_path) in files.iter().enumerate() {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return Ok(cancelled_import_result(total_files));
            }
            match self.reference_file(source_path, &canonical_library_path, &mut seen_locations) {
                Err(reason) => failures.push(ImportFailure {
                    path: source_path.clone(),
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
    /// place, or `None` when the library already references it.
    fn reference_file(
        &self,
        source_path: &Path,
        canonical_library_path: &Path,
        seen_locations: &mut BTreeSet<sustain_domain::TrackRelativePath>,
    ) -> Result<
        Option<(sustain_domain::TrackRelativePath, InitialTags, Option<u64>)>,
        ImportFailureReason,
    > {
        let source_path =
            fs::canonicalize(source_path).map_err(|_| ImportFailureReason::Unreadable)?;
        let relative_path =
            reference_relative_path_for_source(&source_path, canonical_library_path)
                .ok_or(ImportFailureReason::OutsideLibraryFolder)?;
        if !seen_locations.insert(relative_path.clone()) {
            return Ok(None);
        }
        let tags = self
            .metadata_service
            .read_initial_tags(&source_path)
            .map_err(|_| ImportFailureReason::UnreadableTags)?;
        let file_size_bytes = fs::metadata(&source_path)
            .map(|metadata| metadata.len())
            .ok();
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
            failed_files: state.failed_files(),
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

    /// Copies one file into the library, or says why it cannot be.
    fn import_file(
        &mut self,
        metadata_service: &dyn sustain_metadata::MetadataService,
        source_path: &Path,
    ) -> Result<ManagedFileOutcome, ImportFailureReason> {
        let source_path =
            fs::canonicalize(source_path).map_err(|_| ImportFailureReason::Unreadable)?;
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

        let initial_read = match metadata_service.read_initial_tags_with_diagnostics(&source_path) {
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
            &source_path,
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
            let path = track.location.absolute_path(library_path);
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

fn rollback_managed_import_files(
    library_path: &Path,
    copied_files: &[VerifiedFileCopy],
) -> Result<(), ()> {
    let remove_result = remove_copied_files(copied_files);
    let cleanup_paths = copied_files
        .iter()
        .map(|copy| copy.destination_path.clone())
        .collect::<Vec<_>>();
    prune_empty_ancestor_directories_for_sources(library_path, &cleanup_paths);
    remove_result
}

/// The supported audio files under the paths an import was given, and the
/// paths it could not look into.
struct DiscoveredFiles {
    files: Vec<PathBuf>,
    failures: Vec<ImportFailure>,
}

fn collect_supported_audio_files(paths: &[PathBuf], cancellation: &AtomicBool) -> DiscoveredFiles {
    let mut discovered = DiscoveredFiles {
        files: Vec::new(),
        failures: Vec::new(),
    };
    for path in paths {
        if cancellation.load(Ordering::SeqCst) {
            break;
        }
        collect_supported_audio_path(path, &mut discovered, cancellation);
    }
    discovered.files.sort();
    discovered.files.dedup();
    discovered
}

/// Adds the supported audio files at or under `path`. A path that cannot
/// be read is recorded and the walk carries on with its siblings.
fn collect_supported_audio_path(
    path: &Path,
    discovered: &mut DiscoveredFiles,
    cancellation: &AtomicBool,
) {
    if cancellation.load(Ordering::SeqCst) {
        return;
    }
    let unreadable = |discovered: &mut DiscoveredFiles| {
        discovered.failures.push(ImportFailure {
            path: path.to_path_buf(),
            reason: ImportFailureReason::Unreadable,
        });
    };
    let Ok(metadata) = fs::symlink_metadata(path) else {
        unreadable(discovered);
        return;
    };
    if metadata.file_type().is_dir() {
        let Ok(entries) = fs::read_dir(path) else {
            unreadable(discovered);
            return;
        };
        for entry in entries {
            if cancellation.load(Ordering::SeqCst) {
                return;
            }
            match entry {
                Ok(entry) => collect_supported_audio_path(&entry.path(), discovered, cancellation),
                // An entry the folder listing itself failed on has no path
                // of its own: the folder is what could not be read in full.
                Err(_) => unreadable(discovered),
            }
        }
    } else if metadata.file_type().is_file() && audio_format_from_path(path).is_ok() {
        discovered.files.push(path.to_path_buf());
    }
}

fn cancelled_import_result(discovered_files: usize) -> LibraryImportResult {
    LibraryImportResult {
        tracks: Vec::new(),
        summary: LibraryImportSummary {
            discovered_files,
            cancelled: true,
            ..LibraryImportSummary::default()
        },
        failures: Vec::new(),
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

    use super::rollback_managed_import_files;
    use crate::managed_library::file_ops::copy_file_verified;

    #[test]
    fn managed_import_rollback_removes_copies_and_prunes_their_folders() {
        let root = unique_test_directory();
        let copied = root.join("Artist/Album/song.flac");
        let source = root.join("source.flac");
        fs::create_dir_all(copied.parent().expect("copied parent")).expect("create copied parent");
        fs::write(&source, b"audio").expect("write source");
        let hash = hash_file_content(&source).expect("hash source");
        let copied_file = copy_file_verified(&source, &copied, &hash).expect("copy file");

        assert_eq!(
            rollback_managed_import_files(&root, std::slice::from_ref(&copied_file)),
            Ok(())
        );

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
