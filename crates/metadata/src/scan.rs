// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The filesystem walk that imports a library directory.
//!
//! [`LibraryScanner`] recurses the library root, parsing each supported
//! file's initial tags via the injected [`crate::MetadataService`]. Files
//! whose size + mtime fingerprint already matches the stored one are reported
//! as `unchanged` and never re-parsed (#71). The walk is symlink-aware and
//! cycle-safe, and a cancellation flag stops it cleanly mid-directory.

use super::*;

pub struct LibraryScanner<'a, S: ?Sized> {
    metadata_service: &'a S,
}

pub(crate) trait ScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool;
    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>>;
    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata>;
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct StdScanFilesystem;

impl ScanFilesystem for StdScanFilesystem {
    fn is_directory(&self, path: &Path) -> bool {
        path.is_dir()
    }

    fn read_directory(
        &self,
        path: &Path,
    ) -> io::Result<Box<dyn Iterator<Item = io::Result<PathBuf>>>> {
        Ok(Box::new(
            fs::read_dir(path)?.map(|entry| entry.map(|entry| entry.path())),
        ))
    }

    fn symlink_metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::symlink_metadata(path)
    }

    fn metadata(&self, path: &Path) -> io::Result<fs::Metadata> {
        fs::metadata(path)
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        fs::canonicalize(path)
    }
}

struct ScanWalk<'a, S: ScanFilesystem> {
    library_path: &'a Path,
    scan: &'a mut LibraryScan,
    cancellation: &'a AtomicBool,
    known_fingerprints: &'a BTreeMap<TrackRelativePath, ScanFingerprint>,
    filesystem: &'a S,
    visited_directories: HashSet<PathBuf>,
}

impl<'a, S> LibraryScanner<'a, S>
where
    S: MetadataService + ?Sized,
{
    pub const fn new(metadata_service: &'a S) -> Self {
        Self { metadata_service }
    }

    /// Walks `library_path`, parsing each supported file's tags. Files
    /// whose `(size, mtime)` fingerprint already matches an entry in
    /// `known_fingerprints` are reported as `unchanged` and not re-parsed
    /// (#71); pass an empty map to parse everything (a cold scan).
    pub fn scan(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
    ) -> Result<LibraryScan, LibraryScanError> {
        self.scan_with_filesystem(
            library_path,
            cancellation,
            known_fingerprints,
            &StdScanFilesystem,
        )
    }

    pub(crate) fn scan_with_filesystem(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
        filesystem: &impl ScanFilesystem,
    ) -> Result<LibraryScan, LibraryScanError> {
        if !filesystem.is_directory(library_path) {
            return Err(LibraryScanError::LibraryPathUnavailable);
        }

        let mut scan = LibraryScan::default();
        {
            let mut walk = ScanWalk {
                library_path,
                scan: &mut scan,
                cancellation,
                known_fingerprints,
                filesystem,
                visited_directories: HashSet::new(),
            };
            self.scan_directory(library_path, false, &mut walk);
        }
        scan.cancelled = scan.cancelled || cancellation.load(Ordering::SeqCst);
        scan.complete_for_missing_reconciliation &= !scan.cancelled;
        scan.tracks
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        scan.unchanged.sort();
        Ok(scan)
    }

    fn scan_directory<F: ScanFilesystem>(
        &self,
        directory: &Path,
        reached_by_symlink: bool,
        walk: &mut ScanWalk<'_, F>,
    ) {
        match walk.filesystem.canonicalize(directory) {
            Ok(canonical) => {
                if !walk.visited_directories.insert(canonical) {
                    if reached_by_symlink {
                        walk.scan
                            .record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                    }
                    return;
                }
            }
            Err(_) => {
                walk.scan
                    .record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                return;
            }
        }

        let entries = match walk.filesystem.read_directory(directory) {
            Ok(entries) => entries,
            Err(_) => {
                walk.scan
                    .record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                return;
            }
        };

        for entry in entries {
            if walk.cancellation.load(Ordering::SeqCst) {
                walk.scan.cancelled = true;
                return;
            }
            let path = match entry {
                Ok(path) => path,
                Err(_) => {
                    walk.scan
                        .record_failure(directory.to_path_buf(), MetadataError::ReadFailed);
                    continue;
                }
            };
            let metadata = match walk.filesystem.symlink_metadata(&path) {
                Ok(metadata) => metadata,
                Err(_) => {
                    walk.scan.record_failure(path, MetadataError::ReadFailed);
                    continue;
                }
            };
            let file_type = metadata.file_type();
            let (metadata, reached_by_symlink) = if file_type.is_symlink() {
                match walk.filesystem.metadata(&path) {
                    Ok(target) => (target, true),
                    Err(_) => {
                        walk.scan.record_failure(path, MetadataError::ReadFailed);
                        continue;
                    }
                }
            } else {
                (metadata, false)
            };
            if metadata.file_type().is_dir() {
                self.scan_directory(&path, reached_by_symlink, walk);
                if walk.scan.cancelled {
                    return;
                }
            } else if metadata.file_type().is_file() {
                // Read the mtime from the stat we already have — no extra
                // syscall — so an unchanged file can be fingerprinted and
                // skipped below (#71).
                self.scan_file(
                    walk.library_path,
                    path,
                    metadata.len(),
                    metadata.modified().ok(),
                    walk.known_fingerprints,
                    walk.scan,
                );
            }
        }
    }

    fn scan_file(
        &self,
        library_path: &Path,
        path: PathBuf,
        file_size_bytes: u64,
        modified_at: Option<SystemTime>,
        known_fingerprints: &BTreeMap<TrackRelativePath, ScanFingerprint>,
        scan: &mut LibraryScan,
    ) {
        if audio_format_from_path(&path).is_err() {
            scan.skipped_unsupported_files += 1;
            return;
        }

        let relative_path = match path
            .strip_prefix(library_path)
            .ok()
            .and_then(|path| TrackRelativePath::new(path.to_path_buf()))
        {
            Some(relative_path) => relative_path,
            None => {
                scan.record_failure(path, MetadataError::ReadFailed);
                return;
            }
        };

        // Skip the tag parse entirely when this file is already known with
        // an identical size + mtime fingerprint: its audio-stream
        // properties, size, and embedded-artwork bit cannot have changed,
        // and SQLite already owns everything else for an imported track
        // (#71). A missing mtime (None) cannot produce a fingerprint, so
        // such a file is always parsed and re-records its mtime.
        let fingerprint =
            modified_at.and_then(|modified_at| ScanFingerprint::new(file_size_bytes, modified_at));
        if let Some(fingerprint) = fingerprint {
            if known_fingerprints.get(&relative_path) == Some(&fingerprint) {
                scan.unchanged.push(relative_path);
                return;
            }
        }

        let InitialTags {
            metadata,
            rating,
            has_embedded_artwork,
        } = match self.metadata_service.read_initial_tags(&path) {
            Ok(tags) => tags,
            Err(error) => {
                scan.record_failure(path, error);
                return;
            }
        };
        scan.tracks.push(ScannedTrack {
            relative_path,
            metadata,
            rating,
            file_size_bytes: Some(file_size_bytes),
            has_embedded_artwork,
            file_modified_at: modified_at,
        });
    }
}
