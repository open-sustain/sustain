// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Durable file-tag mirror intents and content-addressed artwork payloads.

use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
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

    pub(crate) fn garbage_collect(&self, referenced: &BTreeSet<String>) -> StoreResult<()> {
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
