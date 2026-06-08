// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Durable file-tag mirror intents and content-addressed artwork payloads.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    time::SystemTime,
};

use sha2::{Digest, Sha256};
use sustain_artwork::{MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

use crate::{StoreError, StoreResult, TrackId};

static TEMPORARY_SUFFIX: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TagMirrorKinds {
    pub metadata: bool,
    pub rating: bool,
    pub artwork: bool,
}

impl TagMirrorKinds {
    pub const fn is_empty(self) -> bool {
        !(self.metadata || self.rating || self.artwork)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct StoredTagMirrorArtwork {
    digest: String,
    size_bytes: u64,
}

impl StoredTagMirrorArtwork {
    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub const fn size_bytes(&self) -> u64 {
        self.size_bytes
    }

    pub(crate) fn from_stored_parts(digest: String, size_bytes: u64) -> StoreResult<Self> {
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || size_bytes > MAX_ENCODED_ARTWORK_BYTES as u64
        {
            return Err(StoreError::InvalidStoredArtwork);
        }
        Ok(Self { digest, size_bytes })
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum TagMirrorArtwork {
    #[default]
    Unchanged,
    Clear,
    Set(StoredTagMirrorArtwork),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PendingTagMirror {
    pub track_id: TrackId,
    pub generation: u64,
    pub kinds: TagMirrorKinds,
    pub artwork: TagMirrorArtwork,
    pub attempt_count: u32,
    pub next_attempt_at_unix: i64,
    pub last_error: Option<String>,
}

#[derive(Debug)]
pub(crate) struct TagMirrorBlobStore {
    root: PathBuf,
    remove_on_drop: bool,
}

impl TagMirrorBlobStore {
    pub(crate) fn new(root: PathBuf) -> Self {
        Self {
            root,
            remove_on_drop: false,
        }
    }

    pub(crate) fn new_ephemeral() -> Self {
        Self {
            root: std::env::temp_dir().join(format!(
                "sustain-tag-outbox-{}-{}",
                std::process::id(),
                TEMPORARY_SUFFIX.fetch_add(1, Ordering::Relaxed)
            )),
            remove_on_drop: true,
        }
    }

    pub(crate) fn publish(&self, bytes: &[u8]) -> StoreResult<StoredTagMirrorArtwork> {
        validate_encoded_artwork(bytes).map_err(|_| StoreError::InvalidArtworkPayload)?;
        let digest = sha256_hex(bytes);
        let artwork = StoredTagMirrorArtwork {
            digest,
            size_bytes: bytes.len() as u64,
        };
        ensure_directory_all(&self.root)?;
        let destination = self.path_for(&artwork);
        if destination.exists() {
            self.read(&artwork)?;
            return Ok(artwork);
        }

        let mut temporary = None;
        for _ in 0..100 {
            let candidate = self.root.join(format!(
                ".tmp-{}-{}",
                std::process::id(),
                TEMPORARY_SUFFIX.fetch_add(1, Ordering::Relaxed)
            ));
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(file) => {
                    temporary = Some((candidate, file));
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(StoreError::Database(error.to_string())),
            }
        }
        let (temporary_path, mut temporary_file) =
            temporary.ok_or_else(|| StoreError::Database("exhausted artwork temp names".into()))?;
        let publish_result = (|| {
            temporary_file
                .write_all(bytes)
                .and_then(|()| temporary_file.sync_all())
                .map_err(|error| StoreError::Database(error.to_string()))?;
            drop(temporary_file);
            match fs::rename(&temporary_path, &destination) {
                Ok(()) => sync_directory(&self.root),
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    fs::remove_file(&temporary_path)
                        .map_err(|remove| StoreError::Database(remove.to_string()))?;
                    self.read(&artwork).map(|_| ())
                }
                Err(error) => Err(StoreError::Database(error.to_string())),
            }
        })();
        if publish_result.is_err() {
            let _ = fs::remove_file(&temporary_path);
        }
        publish_result?;
        Ok(artwork)
    }

    pub(crate) fn read(&self, artwork: &StoredTagMirrorArtwork) -> StoreResult<Vec<u8>> {
        let path = self.path_for(artwork);
        let file = File::open(path).map_err(|error| StoreError::Database(error.to_string()))?;
        let mut bytes = Vec::new();
        file.take((MAX_ENCODED_ARTWORK_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|error| StoreError::Database(error.to_string()))?;
        if bytes.len() as u64 != artwork.size_bytes
            || bytes.len() > MAX_ENCODED_ARTWORK_BYTES
            || sha256_hex(&bytes) != artwork.digest
        {
            return Err(StoreError::InvalidStoredArtwork);
        }
        validate_encoded_artwork(&bytes).map_err(|_| StoreError::InvalidStoredArtwork)?;
        Ok(bytes)
    }

    pub(crate) fn garbage_collect(
        &self,
        referenced: &BTreeSet<String>,
        snapshot: SystemTime,
    ) -> StoreResult<()> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(StoreError::Database(error.to_string())),
        };
        let mut removed = false;
        for entry in entries {
            let entry = entry.map_err(|error| StoreError::Database(error.to_string()))?;
            if !entry
                .file_type()
                .map_err(|error| StoreError::Database(error.to_string()))?
                .is_file()
            {
                continue;
            }
            let name = entry.file_name();
            let name = name.to_string_lossy();
            let referenced_blob = name
                .strip_prefix("sha256-")
                .is_some_and(|digest| referenced.contains(digest));
            if !referenced_blob {
                if entry
                    .metadata()
                    .and_then(|metadata| metadata.modified())
                    .is_ok_and(|modified| modified.duration_since(snapshot).is_ok())
                {
                    continue;
                }
                fs::remove_file(entry.path())
                    .map_err(|error| StoreError::Database(error.to_string()))?;
                removed = true;
            }
        }
        if removed {
            sync_directory(&self.root)?;
        }
        Ok(())
    }

    fn path_for(&self, artwork: &StoredTagMirrorArtwork) -> PathBuf {
        self.root.join(format!("sha256-{}", artwork.digest))
    }
}

impl Drop for TagMirrorBlobStore {
    fn drop(&mut self) {
        if self.remove_on_drop {
            let _ = fs::remove_dir_all(&self.root);
        }
    }
}

pub(crate) fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn ensure_directory_all(path: &Path) -> StoreResult<()> {
    if path.is_dir() {
        return Ok(());
    }
    let parent = path
        .parent()
        .ok_or_else(|| StoreError::Database("artwork blob directory has no parent".into()))?;
    ensure_directory_all(parent)?;
    match fs::create_dir(path) {
        Ok(()) => sync_directory(parent),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
        Err(error) => Err(StoreError::Database(error.to_string())),
    }
}

fn sync_directory(path: &Path) -> StoreResult<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| StoreError::Database(error.to_string()))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicU64, Ordering},
        time::{Duration, SystemTime},
    };

    use super::TagMirrorBlobStore;

    static NEXT_TEST_DIR: AtomicU64 = AtomicU64::new(1);

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn garbage_collect_skips_blobs_newer_than_snapshot() {
        let (_dir, store) = test_store();
        let artwork = store.publish(VALID_PNG).expect("publish artwork");
        let path = store.path_for(&artwork);

        store
            .garbage_collect(&Default::default(), SystemTime::UNIX_EPOCH)
            .expect("collect with old snapshot");
        assert!(path.exists());

        store
            .garbage_collect(
                &Default::default(),
                SystemTime::now() + Duration::from_secs(1),
            )
            .expect("collect with future snapshot");
        assert!(!path.exists());
    }

    fn test_store() -> (TestDir, TagMirrorBlobStore) {
        let index = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let root = std::env::temp_dir().join(format!(
            "sustain-tag-mirror-test-{}-{index}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        let store = TagMirrorBlobStore::new(root.clone());
        (TestDir(root), store)
    }

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
}
