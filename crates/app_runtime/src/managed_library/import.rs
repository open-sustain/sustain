// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adding external files to the library, in either management mode: copying
//! verified files into the canonical layout, or referencing them in place.
//! Both modes deduplicate by relative path and content hash and roll back any
//! filesystem side effects when cancelled or on failure.

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
    ApplicationRuntimeError, ApplicationRuntimeResult, LibraryImportProgress, LibraryImportResult,
    LibraryImportSummary, LibraryImportTask, library_scan,
};

use super::{
    capabilities::ManagedLibraryFilesystemValidator,
    file_ops::{
        VerifiedFileCopy, copy_file_to_staging_verified,
        prune_empty_ancestor_directories_for_sources, publish_staged_file, remove_copied_files,
        remove_staged_file,
    },
};

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
        self.managed_library_filesystem_validator
            .validate(&library_path)
            .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
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
        // against ground truth on disk by `LibraryContentIndex`, never
        // against the stored content hash, which is import-time only and
        // may be stale or absent (see #72).
        let mut seen_hashes: HashSet<String> = HashSet::new();

        // Decide existing-library duplicates against a size-bucketed,
        // hash-cached index built once from disk. The per-file linear scan
        // this replaces was O(new × existing) and re-hashed existing files on
        // every size collision, which dominated import time on a large
        // library (#146).
        let mut content_index = LibraryContentIndex::build(&self.existing_tracks, &library_path);

        let planner = ManagedTrackPathPlanner::default();
        let mut copied_files = Vec::new();
        let mut tracks = Vec::new();
        let mut duplicate_files = 0;
        let mut next_track_id = library_scan::next_track_id(&self.existing_tracks)?;
        let total_files = discovered_files.len();

        for source_path in &discovered_files {
            if self.cancellation_requested.load(Ordering::SeqCst) {
                return self.finish_cancelled_managed_import(
                    total_files,
                    tracks,
                    copied_files,
                    duplicate_files,
                );
            }
            let source_path = match fs::canonicalize(source_path) {
                Ok(path) => path,
                Err(_) => {
                    let _ = rollback_managed_import_files(&library_path, &copied_files, None);
                    return Err(ApplicationRuntimeError::LibraryImportFailed);
                }
            };
            if let Some(relative_path) =
                source_relative_path_inside_library(&source_path, canonical_library_path.as_deref())
                && occupied_paths.contains(&relative_path)
            {
                duplicate_files += 1;
                self.report_progress(tracks.len() + duplicate_files, total_files);
                continue;
            }

            // Hash the source while copying it once into an unpublished
            // staging file. The staged descriptor is then rewound and hashed
            // independently before publication, retaining the read-back
            // integrity assertion without a second source read (#146).
            let staging = match copy_file_to_staging_verified(&source_path, &library_path) {
                Ok(staging) => staging,
                Err(_) => {
                    let _ = rollback_managed_import_files(&library_path, &copied_files, None);
                    return Err(ApplicationRuntimeError::LibraryImportFailed);
                }
            };
            let source_size = staging.bytes_copied;
            let content_hash = staging.content_hash.clone();
            if seen_hashes.contains(content_hash.as_str())
                || content_index.contains_matching(source_size, &content_hash)
            {
                remove_staged_file(staging);
                duplicate_files += 1;
                self.report_progress(tracks.len() + duplicate_files, total_files);
                continue;
            }

            let initial_tags = match self.metadata_service.read_initial_tags(&source_path) {
                Ok(tags) => tags,
                Err(_) => {
                    remove_staged_file(staging);
                    let _ = rollback_managed_import_files(&library_path, &copied_files, None);
                    return Err(ApplicationRuntimeError::LibraryImportFailed);
                }
            };
            let InitialTags {
                metadata,
                rating,
                has_embedded_artwork,
            } = initial_tags;
            let planned_destination = plan_destination(
                &planner,
                &mut occupied_paths,
                &library_path,
                &source_path,
                &metadata,
                source_size,
                &content_hash,
            );
            let plan = match planned_destination {
                Ok(PlannedManagedDestination::Fresh(plan)) => plan,
                Ok(PlannedManagedDestination::AlreadyPresent) => {
                    remove_staged_file(staging);
                    duplicate_files += 1;
                    self.report_progress(tracks.len() + duplicate_files, total_files);
                    continue;
                }
                Err(error) => {
                    remove_staged_file(staging);
                    let _ = rollback_managed_import_files(&library_path, &copied_files, None);
                    return Err(error);
                }
            };
            let destination_path = library_path.join(plan.relative_path.as_path());
            let copy = match publish_staged_file(staging, &destination_path) {
                Ok(copy) => copy,
                Err(_) => {
                    let _ = rollback_managed_import_files(
                        &library_path,
                        &copied_files,
                        Some(&destination_path),
                    );
                    return Err(ApplicationRuntimeError::LibraryImportFailed);
                }
            };
            let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                let mut cleanup = copied_files;
                cleanup.push(copy);
                let _ = rollback_managed_import_files(&library_path, &cleanup, None);
                return Err(ApplicationRuntimeError::LibraryStoreFailed);
            };
            next_track_id += 1;
            tracks.push(Track {
                id: track_id,
                location: TrackLocation::available(plan.relative_path),
                metadata,
                rating,
                statistics: PlayStatistics {
                    date_added_at: Some(SystemTime::now()),
                    ..PlayStatistics::default()
                },
                file_size_bytes: Some(source_size),
                has_embedded_artwork: Some(has_embedded_artwork),
                // The first scan after import fingerprints the file and
                // records its mtime; until then it parses, which is correct
                // and self-heals (#71).
                file_modified_at: None,
            });
            seen_hashes.insert(content_hash.as_str().to_owned());
            copied_files.push(copy);
            self.report_progress(tracks.len() + duplicate_files, total_files);
        }

        if self.cancellation_requested.load(Ordering::SeqCst) {
            return self.finish_cancelled_managed_import(
                total_files,
                tracks,
                copied_files,
                duplicate_files,
            );
        }
        if self.library_store.save_tracks(&tracks).is_err() {
            let _ = rollback_managed_import_files(&library_path, &copied_files, None);
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        }
        if !tracks.is_empty() {
            self.library_store
                .flush_durable()
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }

        Ok(LibraryImportResult {
            tracks,
            summary: LibraryImportSummary {
                discovered_files: total_files,
                imported_tracks: copied_files.len(),
                duplicate_files,
                cancelled: false,
            },
        })
    }

    fn finish_cancelled_managed_import(
        &mut self,
        discovered_files: usize,
        tracks: Vec<Track>,
        copied_files: Vec<VerifiedFileCopy>,
        duplicate_files: usize,
    ) -> ApplicationRuntimeResult<LibraryImportResult> {
        if self.library_store.save_tracks(&tracks).is_err() {
            let _ = rollback_managed_import_files(
                self.settings
                    .library_path()
                    .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?,
                &copied_files,
                None,
            );
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        }
        if !tracks.is_empty() {
            self.library_store
                .flush_durable()
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        Ok(LibraryImportResult {
            summary: LibraryImportSummary {
                discovered_files,
                imported_tracks: tracks.len(),
                duplicate_files,
                cancelled: true,
            },
            tracks,
        })
    }

    fn report_progress(&mut self, processed_files: usize, total_files: usize) {
        (self.progress)(LibraryImportProgress {
            processed_files,
            total_files,
        });
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
        let total_files = discovered_files.len();

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
                self.report_progress(tracks.len() + duplicate_files, total_files);
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
                // Recorded by the first post-import scan; see above (#71).
                file_modified_at: None,
            });
            self.report_progress(tracks.len() + duplicate_files, total_files);
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
    additional_cleanup_path: Option<&Path>,
) -> Result<(), ()> {
    let remove_result = remove_copied_files(copied_files);
    let mut cleanup_paths = copied_files
        .iter()
        .map(|copy| copy.destination_path.clone())
        .collect::<Vec<_>>();
    if let Some(path) = additional_cleanup_path {
        cleanup_paths.push(path.to_path_buf());
    }
    prune_empty_ancestor_directories_for_sources(library_path, &cleanup_paths);
    remove_result
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

#[cfg(test)]
mod tests {
    use std::{fs, path::PathBuf};

    use sustain_metadata::hash_file_content;

    use super::rollback_managed_import_files;
    use crate::managed_library::file_ops::copy_file_verified;

    #[test]
    fn managed_import_rollback_prunes_copied_and_unpublished_destination_folders() {
        let root = unique_test_directory();
        let copied = root.join("Artist/Album/song.flac");
        let unpublished = root.join("Other/Album/song.flac");
        let source = root.join("source.flac");
        fs::create_dir_all(copied.parent().expect("copied parent")).expect("create copied parent");
        fs::create_dir_all(unpublished.parent().expect("unpublished parent"))
            .expect("create unpublished parent");
        fs::write(&source, b"audio").expect("write source");
        let hash = hash_file_content(&source).expect("hash source");
        let copied_file = copy_file_verified(&source, &copied, &hash).expect("copy file");

        assert_eq!(
            rollback_managed_import_files(
                &root,
                std::slice::from_ref(&copied_file),
                Some(&unpublished),
            ),
            Ok(())
        );

        assert!(root.exists());
        assert!(!root.join("Artist").exists());
        assert!(!root.join("Other").exists());

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
