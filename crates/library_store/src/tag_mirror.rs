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

use sustain_domain::TrackFields;

use crate::{StoreError, StoreResult, TrackId};

static TEMPORARY_SUFFIX: AtomicU64 = AtomicU64::new(1);

/// The fields a tag mirror can write: the tags' metadata and the rating.
pub const TAG_MIRROR_FIELDS: TrackFields = TrackFields::TAG_METADATA.union(TrackFields::RATING);

/// The bit each mirrored field is stored as in `tag_mirror_outbox.fields`.
/// Persisted: a field keeps its bit forever, whatever `TrackFields` does
/// with its own representation (migration 24).
const STORED_FIELDS: [(TrackFields, i64); 23] = [
    (TrackFields::TITLE, 1 << 0),
    (TrackFields::ARTIST, 1 << 1),
    (TrackFields::ALBUM, 1 << 2),
    (TrackFields::ALBUM_ARTIST, 1 << 3),
    (TrackFields::COMPOSER, 1 << 4),
    (TrackFields::GROUPING, 1 << 5),
    (TrackFields::GENRE, 1 << 6),
    (TrackFields::TRACK_NUMBER, 1 << 7),
    (TrackFields::TRACK_TOTAL, 1 << 8),
    (TrackFields::DISC_NUMBER, 1 << 9),
    (TrackFields::DISC_TOTAL, 1 << 10),
    (TrackFields::YEAR, 1 << 11),
    (TrackFields::COMPILATION, 1 << 12),
    (TrackFields::BPM, 1 << 13),
    (TrackFields::KEY, 1 << 14),
    (TrackFields::COMMENTS, 1 << 15),
    (TrackFields::LYRICS, 1 << 16),
    (TrackFields::TITLE_SORT, 1 << 17),
    (TrackFields::ARTIST_SORT, 1 << 18),
    (TrackFields::ALBUM_SORT, 1 << 19),
    (TrackFields::ALBUM_ARTIST_SORT, 1 << 20),
    (TrackFields::COMPOSER_SORT, 1 << 21),
    (TrackFields::RATING, 1 << 22),
];

/// `fields`, which must lie within [`TAG_MIRROR_FIELDS`], as stored.
pub(crate) fn stored_fields(fields: TrackFields) -> i64 {
    debug_assert!(TAG_MIRROR_FIELDS.contains(fields));
    STORED_FIELDS
        .iter()
        .filter(|(field, _)| fields.contains(*field))
        .fold(0, |stored, (_, bit)| stored | bit)
}

/// The fields `stored` names, or `None` for a bit no field is stored as.
pub(crate) fn fields_from_stored(stored: i64) -> Option<TrackFields> {
    let mut remaining = stored;
    let mut fields = TrackFields::NONE;
    for (field, bit) in STORED_FIELDS {
        if stored & bit != 0 {
            fields |= field;
            remaining &= !bit;
        }
    }
    (remaining == 0).then_some(fields)
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
    /// The fields to write, within [`TAG_MIRROR_FIELDS`]: the library's
    /// current value of each, whatever it was when the mirror was queued.
    pub fields: TrackFields,
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
        if remove_unreferenced_store_files(&self.root, referenced, snapshot)? {
            sync_directory(&self.root)?;
        }
        Ok(())
    }

    /// Take over the payloads this database left in `legacy_root`, the
    /// folder every database in one directory used to share and collect.
    /// The payloads `referenced` by its outbox are copied into this store;
    /// the folder is then collected as this database's garbage collection
    /// used to collect it — keeping what it still refers to, and what was
    /// written since `snapshot` — so another database beside it keeps a
    /// payload both refer to until this one's writes are done, and the
    /// folder goes once nothing in it is pending.
    pub(crate) fn adopt_legacy_payloads(
        &self,
        legacy_root: &Path,
        referenced: &BTreeSet<String>,
        snapshot: SystemTime,
    ) -> StoreResult<()> {
        let Some(entries) = store_entries(legacy_root)? else {
            return Ok(());
        };
        for (name, file) in entries {
            if let StoreFile::Payload(digest) = file
                && referenced.contains(&digest)
                && !self.root.join(&name).exists()
            {
                let mut bytes = Vec::new();
                let read = File::open(legacy_root.join(&name)).and_then(|file| {
                    file.take((MAX_ENCODED_ARTWORK_BYTES + 1) as u64)
                        .read_to_end(&mut bytes)
                });
                // Each payload is taken on its own: one that cannot be read,
                // is damaged or cannot be written here is left where it is,
                // for the write waiting for it to fail on as it would here,
                // and the others are taken all the same.
                if read.is_ok() && sha256_hex(&bytes) == digest {
                    let _ = self.publish(&bytes);
                }
            }
        }
        remove_unreferenced_store_files(legacy_root, referenced, snapshot)?;
        // Tidying only: a folder that cannot go yet (one still holding
        // pending payloads or other files, or a link) stays, and is
        // tried again on the next open.
        for folder in [Some(legacy_root), legacy_root.parent()]
            .into_iter()
            .flatten()
        {
            let _ = fs::remove_dir(folder);
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

/// A file the blob store writes in its folder.
enum StoreFile {
    /// `sha256-<digest>`, a published payload.
    Payload(String),
    /// `.tmp-…`, a payload being published.
    Temporary,
}

impl StoreFile {
    fn from_name(name: &str) -> Option<Self> {
        if let Some(digest) = name.strip_prefix("sha256-") {
            return (digest.len() == 64
                && digest
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)))
            .then(|| Self::Payload(digest.to_owned()));
        }
        // `.tmp-<pid>-<counter>`, as `publish` names it.
        let temporary = name
            .strip_prefix(".tmp-")
            .and_then(|rest| rest.split_once('-'));
        temporary
            .is_some_and(|(pid, counter)| is_decimal(pid) && is_decimal(counter))
            .then_some(Self::Temporary)
    }
}

fn is_decimal(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// The regular files the blob store wrote in `root`, by name, or `None`
/// when there is no such folder. A collector only ever removes what the
/// store wrote, whatever else the folder holds or wherever it leads.
fn store_entries(root: &Path) -> StoreResult<Option<Vec<(String, StoreFile)>>> {
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Ok(None);
        }
        Err(error) => return Err(StoreError::Database(error.to_string())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| StoreError::Database(error.to_string()))?;
        if !entry
            .file_type()
            .map_err(|error| StoreError::Database(error.to_string()))?
            .is_file()
        {
            continue;
        }
        if let Some(name) = entry.file_name().to_str()
            && let Some(file) = StoreFile::from_name(name)
        {
            files.push((name.to_owned(), file));
        }
    }
    // In the order of their names, not the order the folder lists them.
    files.sort_by(|(left, _), (right, _)| left.cmp(right));
    Ok(Some(files))
}

/// Remove the files the blob store wrote in `root` that are neither a
/// payload `referenced` nor written since `snapshot`. Whether any was.
fn remove_unreferenced_store_files(
    root: &Path,
    referenced: &BTreeSet<String>,
    snapshot: SystemTime,
) -> StoreResult<bool> {
    let Some(entries) = store_entries(root)? else {
        return Ok(false);
    };
    let mut removed = false;
    for (name, file) in entries {
        if matches!(&file, StoreFile::Payload(digest) if referenced.contains(digest)) {
            continue;
        }
        let path = root.join(&name);
        if fs::symlink_metadata(&path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified.duration_since(snapshot).is_ok())
        {
            continue;
        }
        match fs::remove_file(&path) {
            Ok(()) => removed = true,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(StoreError::Database(error.to_string())),
        }
    }
    Ok(removed)
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
    // A bare name's parent is the empty path, the working directory.
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        ensure_directory_all(parent)?;
        parent
    };
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

    use sustain_domain::TrackFields;

    use super::{
        STORED_FIELDS, TAG_MIRROR_FIELDS, TagMirrorBlobStore, fields_from_stored, stored_fields,
    };

    #[test]
    fn every_mirrored_field_has_its_own_stored_bit() {
        let union = STORED_FIELDS
            .iter()
            .fold(TrackFields::NONE, |fields, (field, _)| fields | *field);
        assert_eq!(union, TAG_MIRROR_FIELDS);
        for (index, (field, bit)) in STORED_FIELDS.iter().enumerate() {
            assert_eq!(*bit, 1 << index, "{field:?} keeps its stored bit");
            assert_eq!(fields_from_stored(*bit), Some(*field));
        }
        // Migration 24 stores a pending mirror's old flags as these values.
        assert_eq!(stored_fields(TrackFields::TAG_METADATA), (1 << 22) - 1);
        assert_eq!(stored_fields(TrackFields::RATING), 1 << 22);
        assert_eq!(
            fields_from_stored(stored_fields(TAG_MIRROR_FIELDS)),
            Some(TAG_MIRROR_FIELDS)
        );
        assert_eq!(fields_from_stored(1 << 23), None);
    }

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

    #[test]
    fn a_folder_named_by_a_bare_name_is_made_in_the_working_directory() {
        let name = format!("sustain-bare-outbox-{}", std::process::id());
        let _ = fs::remove_dir(&name);

        super::ensure_directory_all(std::path::Path::new(&name)).expect("make the folder");

        assert!(std::path::Path::new(&name).is_dir());
        fs::remove_dir(&name).expect("remove the folder");
    }
}
