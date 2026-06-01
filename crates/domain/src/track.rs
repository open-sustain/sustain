// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    path::{Component, Path, PathBuf},
    time::SystemTime,
};

use crate::{PlayStatistics, Rating, TrackId, TrackMetadata};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Track {
    pub id: TrackId,
    pub location: TrackLocation,
    // No authoritative stored content hash. Device sync maintains a
    // disposable derived cache keyed by the complete live source-stat tuple;
    // the Track row itself remains SQLite library metadata, not a filesystem
    // cache. Import hashes bytes transiently for dedup and copy verification.
    pub metadata: TrackMetadata,
    pub rating: Rating,
    pub statistics: PlayStatistics,
    /// On-disk size of the audio file in bytes, captured at scan time.
    /// `None` when the file was never successfully stat'd (e.g. a record
    /// imported from iTunes XML before its file was located).
    pub file_size_bytes: Option<u64>,
    /// True when the file carries at least one embedded picture
    /// (any `PictureType`), false when it does not, `None` when the
    /// state has never been observed by a scan. Populated by the
    /// library scanner via lofty; the online artwork scheduler trusts
    /// this flag as a SQL filter so it never refetches artwork for a
    /// track that already has its own cover embedded.
    pub has_embedded_artwork: Option<bool>,
    /// The file's last-modified time as observed by the most recent
    /// scan, truncated to whole seconds (the resolution persisted in
    /// `file_modified_at_unix`). Paired with `file_size_bytes` as a
    /// cheap change-detection fingerprint: when both still match on a
    /// rescan, the scanner skips re-parsing the file's tags because none
    /// of the file-derived values a rescan consumes for an existing
    /// track (audio-stream properties, size, embedded-artwork bit) can
    /// have changed. `None` when no scan has fingerprinted the file yet
    /// (e.g. a row imported from an external source, or one predating
    /// the column) — such a row is always parsed on the next scan, which
    /// then records its mtime.
    pub file_modified_at: Option<SystemTime>,
}

impl Track {
    pub fn has_lyrics(&self) -> bool {
        self.metadata.has_lyrics()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackLocation {
    pub relative_path: TrackRelativePath,
    pub availability: TrackAvailability,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackRelativePath(PathBuf);

/// A file content hash (lower-case hex SHA-256). Import uses it transiently to
/// dedup and verify managed copies. Device sync may persist it only inside a
/// disposable derived cache tied to a complete [`SourceFileStat`] tuple.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct TrackContentHash(String);

/// Linux source-file identity and change-detection tuple captured from one
/// `stat(2)` result. Device export reuses a cached SHA-256 only when every
/// field still matches the live source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SourceFileStat {
    pub device: u64,
    pub inode: u64,
    pub size_bytes: u64,
    pub modified_at_ns: i64,
    pub changed_at_ns: i64,
}

/// A SHA-256 proven for one exact live source-stat tuple.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SourceFingerprint {
    pub stat: SourceFileStat,
    pub content_hash: TrackContentHash,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum TrackAvailability {
    #[default]
    Available,
    Missing,
}

impl TrackLocation {
    pub const fn available(relative_path: TrackRelativePath) -> Self {
        Self {
            relative_path,
            availability: TrackAvailability::Available,
        }
    }

    pub const fn missing(relative_path: TrackRelativePath) -> Self {
        Self {
            relative_path,
            availability: TrackAvailability::Missing,
        }
    }

    pub fn is_missing(&self) -> bool {
        self.availability == TrackAvailability::Missing
    }

    pub fn path(&self) -> &Path {
        self.relative_path.as_path()
    }

    pub fn absolute_path(&self, library_root: &Path) -> PathBuf {
        self.relative_path.resolve(library_root)
    }

    pub fn with_availability(self, availability: TrackAvailability) -> Self {
        Self {
            relative_path: self.relative_path,
            availability,
        }
    }
}

impl TrackRelativePath {
    pub fn new(path: impl Into<PathBuf>) -> Option<Self> {
        let path = normalize_relative_path(path.into())?;
        Some(Self(path))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn resolve(&self, library_root: &Path) -> PathBuf {
        library_root.join(&self.0)
    }

    pub fn to_path_buf(&self) -> PathBuf {
        self.0.clone()
    }
}

impl TrackContentHash {
    pub fn new(value: impl AsRef<str>) -> Option<Self> {
        let value = value.as_ref().trim();
        if value.len() != 64 || !value.chars().all(|character| character.is_ascii_hexdigit()) {
            return None;
        }
        Some(Self(value.to_ascii_lowercase()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn normalize_relative_path(path: PathBuf) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    let mut has_file_component = false;
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                normalized.push(value);
                has_file_component = true;
            }
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return None,
        }
    }

    has_file_component.then_some(normalized)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{TrackContentHash, TrackRelativePath};

    #[test]
    fn track_relative_paths_reject_absolute_paths() {
        assert_eq!(TrackRelativePath::new("/music/track.flac"), None);
    }

    #[test]
    fn track_relative_paths_reject_parent_components() {
        assert_eq!(TrackRelativePath::new("../track.flac"), None);
        assert_eq!(TrackRelativePath::new("artist/../track.flac"), None);
    }

    #[test]
    fn track_relative_paths_normalize_current_directory_components() {
        assert_eq!(
            TrackRelativePath::new("./artist/album/track.flac").map(|path| path.to_path_buf()),
            Some(PathBuf::from("artist/album/track.flac"))
        );
    }

    #[test]
    fn content_hash_accepts_sha256_hex_values() {
        let hash = TrackContentHash::new(
            "ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789",
        )
        .expect("valid hash");

        assert_eq!(
            hash.as_str(),
            "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
        );
    }

    #[test]
    fn content_hash_rejects_invalid_values() {
        assert_eq!(TrackContentHash::new("abc"), None);
        assert_eq!(
            TrackContentHash::new(
                "xyzdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789"
            ),
            None
        );
    }
}
