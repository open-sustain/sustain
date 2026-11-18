// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The filesystem walk that imports a library directory.
//!
//! [`LibraryScanner`] recurses the library root, parsing each supported
//! file's initial tags via the injected [`crate::MetadataService`]. Files
//! whose size + mtime fingerprint already matches the stored one are reported
//! as `unchanged` and never re-parsed (#71). The walk follows links as
//! [`crate::walk`] describes, and a cancellation flag stops it cleanly.

use super::*;
use crate::walk::{StdWalkFilesystem, WalkFilesystem, WalkRequest, walk_files_with};

/// The name prefix of every file and folder Sustain itself creates inside a
/// library: staged rips, journals, write handoffs, recovery folders. They
/// are in-flight state owned by the operation that made them, never library
/// tracks, so the scan does not descend into or adopt them.
pub const SUSTAIN_WORKSPACE_NAME_PREFIX: &str = ".sustain-";

fn is_sustain_workspace_entry(path: &Path) -> bool {
    path.file_name().is_some_and(|name| {
        name.as_encoded_bytes()
            .starts_with(SUSTAIN_WORKSPACE_NAME_PREFIX.as_bytes())
    })
}

pub struct LibraryScanner<'a, S: ?Sized> {
    metadata_service: &'a S,
    repair_malformed_tags: bool,
}

/// What scanning one file needs to know and record.
struct ScanFile<'a> {
    library_path: &'a Path,
    scan: &'a mut LibraryScan,
    known_tracks: &'a KnownTracks,
    filesystem: &'a dyn WalkFilesystem,
}

/// The library's tracks, by path, and the size + mtime fingerprint each
/// was last scanned with, if any.
pub type KnownTracks = BTreeMap<TrackRelativePath, Option<ScanFingerprint>>;

impl<'a, S> LibraryScanner<'a, S>
where
    S: MetadataService + ?Sized,
{
    pub const fn new(metadata_service: &'a S) -> Self {
        Self {
            metadata_service,
            repair_malformed_tags: false,
        }
    }

    pub const fn with_malformed_tag_repair(mut self, repair_malformed_tags: bool) -> Self {
        self.repair_malformed_tags = repair_malformed_tags;
        self
    }

    /// Walks `library_path` as [`crate::walk`] describes, parsing each
    /// supported file's tags. Files whose `(size, mtime)` fingerprint
    /// matches their entry in `known_tracks` are reported as `unchanged`
    /// and not re-parsed (#71); pass an empty map to parse everything (a
    /// cold scan).
    pub fn scan(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_tracks: &KnownTracks,
    ) -> Result<LibraryScan, LibraryScanError> {
        self.scan_with_filesystem(library_path, cancellation, known_tracks, &StdWalkFilesystem)
    }

    pub(crate) fn scan_with_filesystem(
        &self,
        library_path: &Path,
        cancellation: &AtomicBool,
        known_tracks: &KnownTracks,
        filesystem: &dyn WalkFilesystem,
    ) -> Result<LibraryScan, LibraryScanError> {
        if !filesystem
            .metadata(library_path)
            .is_ok_and(|metadata| metadata.is_dir())
        {
            return Err(LibraryScanError::LibraryPathUnavailable);
        }

        let known: HashSet<PathBuf> = known_tracks
            .keys()
            .map(|track| track.resolve(library_path))
            .collect();
        let walk = walk_files_with(
            &WalkRequest {
                roots: &[library_path.to_path_buf()],
                known: &known,
                accept: &is_audio_name_for,
                skip: &is_sustain_workspace_entry,
                cancellation,
            },
            filesystem,
        );
        let mut scan = LibraryScan {
            skipped_unsupported_files: walk.rejected_names,
            ..LibraryScan::default()
        };
        for failure in &walk.failures {
            scan.record_failure(failure.clone(), MetadataError::ReadFailed);
        }
        let kept = walk.files.iter().flat_map(|file| {
            walk.kept_names(file)
                .into_iter()
                .map(move |name| (name.path.clone(), &file.metadata))
        });
        for (path, metadata) in kept {
            if cancellation.load(Ordering::SeqCst) {
                break;
            }
            self.scan_file(
                path,
                metadata.len(),
                metadata.modified().ok(),
                &mut ScanFile {
                    library_path,
                    scan: &mut scan,
                    known_tracks,
                    filesystem,
                },
            );
        }
        // Read once the last file is scanned, so that a cancellation
        // requested at any point of the scan is seen.
        scan.cancelled = walk.cancelled || cancellation.load(Ordering::SeqCst);
        scan.complete_for_missing_reconciliation &= !scan.cancelled;
        scan.tracks
            .sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        scan.unchanged.sort();
        Ok(scan)
    }

    fn scan_file(
        &self,
        path: PathBuf,
        file_size_bytes: u64,
        modified_at: Option<SystemTime>,
        walk: &mut ScanFile<'_>,
    ) {
        let relative_path = match path
            .strip_prefix(walk.library_path)
            .ok()
            .and_then(|path| TrackRelativePath::new(path.to_path_buf()))
        {
            Some(relative_path) => relative_path,
            None => {
                walk.scan.record_failure(path, MetadataError::ReadFailed);
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
        if let Some(fingerprint) = fingerprint
            && walk.known_tracks.get(&relative_path) == Some(&Some(fingerprint))
        {
            walk.scan.unchanged.push(relative_path);
            return;
        }

        let mut initial_read = match self
            .metadata_service
            .read_initial_tags_with_diagnostics(&path)
        {
            Ok(read) => read,
            Err(error) => {
                walk.scan.record_failure(path, error);
                return;
            }
        };

        let mut repaired = false;
        if self.repair_malformed_tags && initial_read.repair.is_some() {
            // Repaired where the file is, so a link to it stays a link.
            let own_path = match walk.filesystem.canonicalize(&path) {
                Ok(own_path) => own_path,
                Err(_) => {
                    walk.scan.record_failure(path, MetadataError::ReadFailed);
                    return;
                }
            };
            let pass = self
                .metadata_service
                .repair_diagnosed_malformations(&own_path, initial_read);
            initial_read = pass.read;
            repaired = pass.file_changed;
            if let Some(error) = pass.error {
                walk.scan.record_failure(path.clone(), error);
            }
        }

        let (file_size_bytes, mut modified_at) = if repaired {
            match walk.filesystem.metadata(&path) {
                Ok(metadata) if metadata.file_type().is_file() => {
                    (metadata.len(), metadata.modified().ok())
                }
                _ => {
                    walk.scan.record_failure(path, MetadataError::ReadFailed);
                    return;
                }
            }
        } else {
            (file_size_bytes, modified_at)
        };
        if self.repair_malformed_tags && initial_read.repair.is_some() {
            // The tagless fallback keeps the audio file indexed, but its tag
            // is still malformed. Leave the fingerprint incomplete so a later
            // scan retries the repair instead of treating the file as
            // unchanged.
            modified_at = None;
        }

        let tags = initial_read.tags;

        let InitialTags {
            metadata,
            rating,
            has_embedded_artwork,
        } = tags;
        walk.scan.tracks.push(ScannedTrack {
            relative_path,
            metadata,
            rating,
            file_size_bytes: Some(file_size_bytes),
            has_embedded_artwork,
            file_modified_at: modified_at,
        });
    }
}
