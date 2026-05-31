// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Restartable off-thread file-tag mirroring.
//!
//! SQLite is authoritative after import. Every canonical rating or metadata
//! edit commits a compact per-track intent to the library store's durable
//! outbox in the same transaction; embedded artwork references an external
//! content-addressed blob published before its intent commits. This actor only
//! drains that durable state. It never owns stale absolute paths or canonical
//! values: each attempt resolves the current track row and library root, then
//! mirrors the latest SQLite values.
//!
//! One worker remains the sole tag-write surface. That serialization prevents
//! read/modify/write tag replacements from clobbering each other and is the
//! foundation for managed-library retarget operations.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        mpsc::{self, RecvTimeoutError, Sender},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use sustain_domain::{FieldChange, MetadataChange, TrackMetadata};
use sustain_library_store::{
    LibraryStore, PendingTagMirror, StoreError, StoredTagMirrorArtwork, TagMirrorArtwork,
    TagMirrorKinds, TrackId,
};
use sustain_metadata::{MetadataError, MetadataService};

use crate::{
    ApplicationRuntimeError,
    managed_library::{ManagedLibraryFilesystemValidator, retarget_managed_metadata},
};

const DRAIN_BATCH_SIZE: usize = 64;
const MAX_RETRY_DELAY_SECONDS: i64 = 5 * 60;
const IDLE_WAIT: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataWriteOutcome {
    Succeeded,
    Failed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MetadataWriteKind {
    Rating,
    Metadata,
    Artwork,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MetadataWriteResult {
    pub track_id: TrackId,
    pub kind: MetadataWriteKind,
    /// `Failed` means the durable row remains pending and will retry. SQLite
    /// already contains the authoritative user-visible value.
    pub outcome: MetadataWriteOutcome,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedMetadataRetargetResult {
    pub track_id: TrackId,
    pub outcome: Result<(), ApplicationRuntimeError>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum MetadataWriterEvent {
    Mirror(MetadataWriteResult),
    ManagedRetarget(ManagedMetadataRetargetResult),
}

enum MetadataWriterCommand {
    Nudge,
    RetargetManagedMetadata {
        track_id: TrackId,
        change: Box<MetadataChange>,
    },
    SetLibraryPath(Option<PathBuf>),
    Shutdown,
}

/// Owns the single file-tag mirror worker. The channel carries wakeups,
/// configuration changes, and serialized managed-retarget requests; durable
/// mirror work lives in the library store.
pub(crate) struct MetadataWriter {
    sender: Sender<MetadataWriterCommand>,
    library_store: Arc<dyn LibraryStore>,
    result_sink: Arc<Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>>,
    handle: Option<JoinHandle<()>>,
}

impl MetadataWriter {
    pub(crate) fn start(
        metadata_service: Arc<dyn MetadataService>,
        library_store: Arc<dyn LibraryStore>,
        library_path: Option<PathBuf>,
        managed_library_filesystem_validator: ManagedLibraryFilesystemValidator,
        result_sink: Option<async_channel::Sender<MetadataWriterEvent>>,
    ) -> Self {
        let (sender, receiver) = mpsc::channel();
        let result_sink = Arc::new(Mutex::new(result_sink));
        let worker_result_sink = result_sink.clone();
        let worker_library_store = library_store.clone();
        let handle = thread::Builder::new()
            .name("sustain-metadata-writer".to_owned())
            .spawn(move || {
                worker_loop(
                    receiver,
                    metadata_service,
                    worker_library_store,
                    library_path,
                    managed_library_filesystem_validator,
                    worker_result_sink,
                );
            })
            .expect("spawn metadata writer thread");
        Self {
            sender,
            library_store,
            result_sink,
            handle: Some(handle),
        }
    }

    pub(crate) fn set_result_sink(&self, sink: async_channel::Sender<MetadataWriterEvent>) {
        if let Ok(mut slot) = self.result_sink.lock() {
            *slot = Some(sink);
        }
    }

    pub(crate) fn nudge(&self) {
        let _ = self.sender.send(MetadataWriterCommand::Nudge);
    }

    pub(crate) fn retarget_managed_metadata(
        &self,
        track_id: TrackId,
        change: MetadataChange,
    ) -> bool {
        self.sender
            .send(MetadataWriterCommand::RetargetManagedMetadata {
                track_id,
                change: Box::new(change),
            })
            .is_ok()
    }

    pub(crate) fn set_library_path(&self, path: Option<PathBuf>) {
        let _ = self
            .sender
            .send(MetadataWriterCommand::SetLibraryPath(path));
    }

    pub(crate) fn handle(&self) -> MetadataWriteHandle {
        MetadataWriteHandle {
            sender: self.sender.clone(),
            library_store: self.library_store.clone(),
        }
    }

    pub(crate) fn shutdown(mut self) {
        let _ = self.sender.send(MetadataWriterCommand::Shutdown);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for MetadataWriter {
    fn drop(&mut self) {
        let _ = self.sender.send(MetadataWriterCommand::Shutdown);
    }
}

/// Cloneable online-scheduler handle. It persists canonical values plus outbox
/// intent first, then wakes the worker; it never performs tag I/O itself.
#[derive(Clone)]
pub(crate) struct MetadataWriteHandle {
    sender: Sender<MetadataWriterCommand>,
    library_store: Arc<dyn LibraryStore>,
}

impl MetadataWriteHandle {
    pub(crate) fn fill_missing_metadata(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> Result<bool, StoreError> {
        let changed = self
            .library_store
            .fill_missing_track_metadata_and_enqueue_mirror(track_id, change)?;
        if changed {
            self.nudge();
        }
        Ok(changed)
    }

    pub(crate) fn enqueue_artwork(&self, track_id: TrackId, artwork: Option<Vec<u8>>) -> bool {
        let artwork = match artwork {
            Some(bytes) => match self.library_store.publish_tag_mirror_artwork(&bytes) {
                Ok(artwork) => TagMirrorArtwork::Set(artwork),
                Err(_) => return false,
            },
            None => TagMirrorArtwork::Clear,
        };
        if self
            .library_store
            .enqueue_tag_mirror_artwork(track_id, artwork)
            .is_err()
        {
            return false;
        }
        self.nudge();
        true
    }

    fn nudge(&self) {
        let _ = self.sender.send(MetadataWriterCommand::Nudge);
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<MetadataWriterCommand>,
    metadata_service: Arc<dyn MetadataService>,
    library_store: Arc<dyn LibraryStore>,
    mut library_path: Option<PathBuf>,
    managed_library_filesystem_validator: ManagedLibraryFilesystemValidator,
    result_sink: Arc<Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>>,
) {
    let mut active = false;
    let mut cleanup_pending = true;
    loop {
        if active {
            if cleanup_pending {
                if let Err(error) = library_store.garbage_collect_tag_mirror_artwork() {
                    eprintln!("Sustain: tag-mirror artwork cleanup failed: {error:?}");
                }
                cleanup_pending = false;
            }
            drain_due_batch(
                metadata_service.as_ref(),
                library_store.as_ref(),
                library_path.as_ref(),
                &result_sink,
            );
        }
        let wait = if active {
            wait_until_next_attempt(library_store.as_ref())
        } else {
            IDLE_WAIT
        };
        match receiver.recv_timeout(wait) {
            Ok(command) => {
                if !apply_command(
                    command,
                    library_store.as_ref(),
                    &mut library_path,
                    &managed_library_filesystem_validator,
                    &mut active,
                    &result_sink,
                ) {
                    break;
                }
                while let Ok(command) = receiver.try_recv() {
                    if !apply_command(
                        command,
                        library_store.as_ref(),
                        &mut library_path,
                        &managed_library_filesystem_validator,
                        &mut active,
                        &result_sink,
                    ) {
                        return;
                    }
                }
            }
            Err(RecvTimeoutError::Disconnected) => break,
            Err(RecvTimeoutError::Timeout) => {}
        }
    }
}

pub(crate) fn drain_due_synchronously(
    metadata_service: &dyn MetadataService,
    library_store: &dyn LibraryStore,
    library_path: Option<&PathBuf>,
    result_sink: Option<&async_channel::Sender<MetadataWriterEvent>>,
) {
    let sink = Mutex::new(result_sink.cloned());
    while drain_due_batch(metadata_service, library_store, library_path, &sink) {}
}

fn drain_due_batch(
    metadata_service: &dyn MetadataService,
    library_store: &dyn LibraryStore,
    library_path: Option<&PathBuf>,
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>,
) -> bool {
    let now_unix = unix_now();
    let pending = match library_store.tag_mirrors_due(now_unix, DRAIN_BATCH_SIZE) {
        Ok(pending) => pending,
        Err(error) => {
            eprintln!("Sustain: tag-mirror outbox query failed: {error:?}");
            return false;
        }
    };
    if pending.is_empty() {
        return false;
    }
    let batch_is_full = pending.len() == DRAIN_BATCH_SIZE;
    let mut artwork_completed = false;
    for pending in pending {
        let kind = primary_kind(pending.kinds);
        match mirror_one(metadata_service, library_store, library_path, &pending) {
            Ok(()) => {
                if library_store
                    .complete_tag_mirror(pending.track_id, pending.generation)
                    .unwrap_or(false)
                {
                    artwork_completed |= pending.kinds.artwork;
                    emit_result(
                        result_sink,
                        MetadataWriteResult {
                            track_id: pending.track_id,
                            kind,
                            outcome: MetadataWriteOutcome::Succeeded,
                        },
                    );
                }
            }
            Err(error) => {
                let next_attempt_at_unix =
                    now_unix.saturating_add(retry_delay_seconds(pending.attempt_count));
                if library_store
                    .record_tag_mirror_failure(
                        pending.track_id,
                        pending.generation,
                        next_attempt_at_unix,
                        &error,
                    )
                    .unwrap_or(false)
                {
                    emit_result(
                        result_sink,
                        MetadataWriteResult {
                            track_id: pending.track_id,
                            kind,
                            outcome: MetadataWriteOutcome::Failed,
                        },
                    );
                }
            }
        }
    }
    if artwork_completed && let Err(error) = library_store.garbage_collect_tag_mirror_artwork() {
        eprintln!("Sustain: tag-mirror artwork cleanup failed: {error:?}");
    }
    batch_is_full
}

fn apply_command(
    command: MetadataWriterCommand,
    library_store: &dyn LibraryStore,
    library_path: &mut Option<PathBuf>,
    managed_library_filesystem_validator: &ManagedLibraryFilesystemValidator,
    active: &mut bool,
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>,
) -> bool {
    match command {
        MetadataWriterCommand::Nudge => {
            *active = true;
            true
        }
        MetadataWriterCommand::RetargetManagedMetadata { track_id, change } => {
            let outcome = library_path
                .as_deref()
                .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)
                .and_then(|library_path| {
                    retarget_managed_metadata(
                        library_path,
                        library_store,
                        managed_library_filesystem_validator,
                        track_id,
                        &change,
                    )
                });
            emit_event(
                result_sink,
                MetadataWriterEvent::ManagedRetarget(ManagedMetadataRetargetResult {
                    track_id,
                    outcome,
                }),
            );
            *active = true;
            true
        }
        MetadataWriterCommand::SetLibraryPath(path) => {
            *library_path = path;
            true
        }
        MetadataWriterCommand::Shutdown => false,
    }
}

fn mirror_one(
    metadata_service: &dyn MetadataService,
    library_store: &dyn LibraryStore,
    library_path: Option<&PathBuf>,
    pending: &PendingTagMirror,
) -> Result<(), String> {
    let track = library_store
        .track(pending.track_id)
        .map_err(store_error)?
        .ok_or_else(|| "the track no longer exists".to_owned())?;
    if track.location.is_missing() {
        return Err("the track file is currently missing".to_owned());
    }
    let root = library_path.ok_or_else(|| "the library path is unavailable".to_owned())?;
    let path = root.join(track.location.relative_path.as_path());
    let mut replaced_file = false;
    let outcome = (|| {
        if pending.kinds.metadata {
            metadata_service
                .write_metadata(&path, full_metadata_mirror(&track.metadata))
                .map_err(metadata_error)?;
            replaced_file = true;
        }
        if pending.kinds.rating {
            metadata_service
                .write_rating(&path, track.rating)
                .map_err(metadata_error)?;
            replaced_file = true;
        }
        if pending.kinds.artwork {
            let artwork = match &pending.artwork {
                TagMirrorArtwork::Unchanged => {
                    return Err("artwork mirror intent is incomplete".to_owned());
                }
                TagMirrorArtwork::Clear => None,
                TagMirrorArtwork::Set(artwork) => Some(read_artwork(library_store, artwork)?),
            };
            metadata_service
                .write_artwork(&path, artwork)
                .map_err(metadata_error)?;
            replaced_file = true;
        }
        Ok(())
    })();
    if replaced_file {
        // The outbox writes replace audio-file bytes atomically. Invalidate
        // even when a later coalesced write fails: a SHA-256 cached before the
        // first successful replacement no longer describes the live source.
        library_store
            .invalidate_source_fingerprint(pending.track_id)
            .map_err(store_error)?;
    }
    outcome
}

fn read_artwork(
    library_store: &dyn LibraryStore,
    artwork: &StoredTagMirrorArtwork,
) -> Result<Vec<u8>, String> {
    library_store
        .read_tag_mirror_artwork(artwork)
        .map_err(store_error)
}

fn full_metadata_mirror(metadata: &TrackMetadata) -> MetadataChange {
    MetadataChange {
        title: mirror_field(&metadata.title),
        artist: mirror_field(&metadata.artist),
        album: mirror_field(&metadata.album),
        album_artist: mirror_field(&metadata.album_artist),
        composer: mirror_field(&metadata.composer),
        grouping: mirror_field(&metadata.grouping),
        genre: mirror_field(&metadata.genre),
        track_number: mirror_field(&metadata.track_number),
        track_total: mirror_field(&metadata.track_total),
        disc_number: mirror_field(&metadata.disc_number),
        disc_total: mirror_field(&metadata.disc_total),
        year: mirror_field(&metadata.year),
        compilation: mirror_field(&metadata.compilation),
        bpm: mirror_field(&metadata.bpm),
        key: mirror_field(&metadata.key),
        comments: mirror_field(&metadata.comments),
        lyrics: mirror_field(&metadata.lyrics),
    }
}

fn mirror_field<T: Clone>(value: &Option<T>) -> FieldChange<T> {
    match value {
        Some(value) => FieldChange::Set(value.clone()),
        None => FieldChange::Clear,
    }
}

fn wait_until_next_attempt(library_store: &dyn LibraryStore) -> Duration {
    let Ok(Some(next_attempt_at_unix)) = library_store.next_tag_mirror_attempt_at() else {
        return IDLE_WAIT;
    };
    let seconds = next_attempt_at_unix.saturating_sub(unix_now()).max(0) as u64;
    Duration::from_secs(seconds.min(IDLE_WAIT.as_secs()))
}

fn retry_delay_seconds(attempt_count: u32) -> i64 {
    1_i64
        .checked_shl(attempt_count.min(8))
        .unwrap_or(MAX_RETRY_DELAY_SECONDS)
        .min(MAX_RETRY_DELAY_SECONDS)
}

fn primary_kind(kinds: TagMirrorKinds) -> MetadataWriteKind {
    if kinds.artwork {
        MetadataWriteKind::Artwork
    } else if kinds.metadata {
        MetadataWriteKind::Metadata
    } else {
        MetadataWriteKind::Rating
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0)
}

fn emit_result(
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>,
    result: MetadataWriteResult,
) {
    emit_event(result_sink, MetadataWriterEvent::Mirror(result));
}

fn emit_event(
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriterEvent>>>,
    event: MetadataWriterEvent,
) {
    if let Ok(sink) = result_sink.lock()
        && let Some(sink) = sink.as_ref()
    {
        let _ = sink.try_send(event);
    }
}

fn store_error(error: StoreError) -> String {
    format!("library store error: {error:?}")
}

fn metadata_error(error: MetadataError) -> String {
    format!("metadata write error: {error:?}")
}

#[cfg(test)]
mod tests {
    use std::{
        path::Path,
        sync::{
            Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::Instant,
    };

    use sustain_domain::{Rating, Track, TrackLocation, TrackRelativePath};
    use sustain_library_store::InMemoryLibraryStore;
    use sustain_metadata::{InitialTags, MetadataResult};

    use super::*;

    #[test]
    fn startup_drain_retries_transient_failure_and_clears_durable_row() {
        let root = unique_test_directory();
        std::fs::create_dir_all(&root).expect("create library root");
        std::fs::write(root.join("track.flac"), b"audio").expect("write track");

        let track_id = TrackId::new(1).expect("track id");
        let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
        store
            .save_track(Track {
                id: track_id,
                location: TrackLocation::available(
                    TrackRelativePath::new("track.flac").expect("relative path"),
                ),
                metadata: TrackMetadata {
                    title: Some("Old".to_owned()),
                    ..TrackMetadata::default()
                },
                rating: Rating::unrated(),
                statistics: Default::default(),
                file_size_bytes: None,
                has_embedded_artwork: None,
            })
            .expect("save track");
        store
            .apply_track_metadata_change_and_enqueue_mirror(
                track_id,
                &MetadataChange {
                    title: FieldChange::Set("Latest".to_owned()),
                    ..MetadataChange::default()
                },
            )
            .expect("commit canonical edit and outbox row");

        let service = Arc::new(FailOnceMetadataService::default());
        let (result_tx, result_rx) = async_channel::unbounded();
        let writer = MetadataWriter::start(
            service.clone(),
            store.clone(),
            Some(root.clone()),
            ManagedLibraryFilesystemValidator::default(),
            Some(result_tx),
        );
        writer.nudge();

        assert_eq!(
            recv_result(&result_rx, Duration::from_secs(2)).outcome,
            MetadataWriteOutcome::Failed
        );
        assert_eq!(
            store.tag_mirrors_due(i64::MAX, 10).expect("pending").len(),
            1
        );
        assert_eq!(
            recv_result(&result_rx, Duration::from_secs(3)).outcome,
            MetadataWriteOutcome::Succeeded
        );
        assert!(
            store
                .tag_mirrors_due(i64::MAX, 10)
                .expect("cleared outbox")
                .is_empty()
        );
        let writes = service.writes.lock().expect("writes lock");
        assert_eq!(writes.len(), 2);
        assert_eq!(writes[1].title, FieldChange::Set("Latest".to_owned()));
        assert_eq!(writes[1].artist, FieldChange::Clear);
        drop(writes);

        writer.shutdown();
        std::fs::remove_dir_all(root).expect("remove library root");
    }

    #[test]
    fn a_successful_tag_mirror_invalidates_the_source_fingerprint_cache() {
        use sustain_domain::TrackContentHash;
        use sustain_library_store::{SourceFileStat, SourceFingerprint};

        let root = unique_test_directory();
        std::fs::create_dir_all(&root).expect("create library root");
        std::fs::write(root.join("track.flac"), b"audio").expect("write track");

        let track_id = TrackId::new(1).expect("track id");
        let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
        store
            .save_track(Track {
                id: track_id,
                location: TrackLocation::available(
                    TrackRelativePath::new("track.flac").expect("relative path"),
                ),
                metadata: TrackMetadata::default(),
                rating: Rating::new(4).expect("rating"),
                statistics: Default::default(),
                file_size_bytes: None,
                has_embedded_artwork: None,
            })
            .expect("save track");

        // A prior device sync cached this source's SHA-256.
        let fingerprint = SourceFingerprint {
            stat: SourceFileStat {
                device: 1,
                inode: 2,
                size_bytes: 5,
                modified_at_ns: 0,
                changed_at_ns: 0,
            },
            content_hash: TrackContentHash::new("a".repeat(64)).expect("hash"),
        };
        store
            .save_source_fingerprint(track_id, &fingerprint)
            .expect("seed fingerprint cache");

        // #100: mirroring a rating rewrites the file bytes, so the cached hash
        // no longer describes the live source and must be dropped — otherwise
        // the next sync would keep the stale on-device copy.
        let service = RecordingMetadataService::default();
        let pending = PendingTagMirror {
            track_id,
            generation: 1,
            kinds: TagMirrorKinds {
                metadata: false,
                rating: true,
                artwork: false,
            },
            artwork: TagMirrorArtwork::Unchanged,
            attempt_count: 0,
            next_attempt_at_unix: 0,
            last_error: None,
        };
        mirror_one(&service, store.as_ref(), Some(&root), &pending).expect("mirror succeeds");

        assert!(
            store
                .source_fingerprint(track_id)
                .expect("query cache")
                .is_none(),
            "a tag rewrite must drop the stale source fingerprint"
        );

        std::fs::remove_dir_all(root).expect("remove library root");
    }

    #[test]
    fn managed_retarget_coalesces_pending_mirrors_and_resolves_the_new_path() {
        let root = unique_test_directory();
        std::fs::create_dir_all(&root).expect("create library root");
        std::fs::write(root.join("loose.flac"), b"audio").expect("write track");

        let track_id = TrackId::new(1).expect("track id");
        let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
        store
            .save_track(Track {
                id: track_id,
                location: TrackLocation::available(
                    TrackRelativePath::new("loose.flac").expect("relative path"),
                ),
                metadata: TrackMetadata {
                    title: Some("Old".to_owned()),
                    ..TrackMetadata::default()
                },
                rating: Rating::unrated(),
                statistics: Default::default(),
                file_size_bytes: None,
                has_embedded_artwork: None,
            })
            .expect("save track");
        let rating = Rating::new(4).expect("rating");
        store
            .update_track_rating_and_enqueue_mirror(track_id, rating)
            .expect("queue rating");
        let artwork = valid_artwork();
        let published_artwork = store
            .publish_tag_mirror_artwork(&artwork)
            .expect("publish artwork");
        store
            .enqueue_tag_mirror_artwork(track_id, TagMirrorArtwork::Set(published_artwork))
            .expect("queue artwork");

        let service = Arc::new(RecordingMetadataService::default());
        let (event_tx, event_rx) = async_channel::unbounded();
        let writer = MetadataWriter::start(
            service.clone(),
            store.clone(),
            Some(root.clone()),
            ManagedLibraryFilesystemValidator::default(),
            Some(event_tx),
        );
        assert!(writer.retarget_managed_metadata(
            track_id,
            MetadataChange {
                title: FieldChange::Set("Song".to_owned()),
                artist: FieldChange::Set("Artist".to_owned()),
                album: FieldChange::Set("Album".to_owned()),
                track_number: FieldChange::Set(1),
                ..MetadataChange::default()
            },
        ));

        let first = recv_event(&event_rx, Duration::from_secs(2));
        assert_eq!(
            first,
            MetadataWriterEvent::ManagedRetarget(ManagedMetadataRetargetResult {
                track_id,
                outcome: Ok(()),
            })
        );
        assert_eq!(
            recv_event(&event_rx, Duration::from_secs(2)),
            MetadataWriterEvent::Mirror(MetadataWriteResult {
                track_id,
                kind: MetadataWriteKind::Artwork,
                outcome: MetadataWriteOutcome::Succeeded,
            })
        );

        let destination = root.join("Artist/Album/01 Song.flac");
        assert!(!root.join("loose.flac").exists());
        assert!(destination.exists());
        let stored = store.track(track_id).expect("load track").expect("track");
        assert_eq!(
            stored.location.relative_path.as_path(),
            Path::new("Artist/Album/01 Song.flac")
        );
        assert_eq!(stored.metadata.title.as_deref(), Some("Song"));
        assert_eq!(stored.rating, rating);
        assert!(
            store
                .tag_mirrors_due(i64::MAX, 10)
                .expect("cleared outbox")
                .is_empty()
        );
        assert_eq!(
            service
                .metadata_paths
                .lock()
                .expect("metadata paths")
                .as_slice(),
            std::slice::from_ref(&destination)
        );
        assert_eq!(
            service
                .rating_paths
                .lock()
                .expect("rating paths")
                .as_slice(),
            std::slice::from_ref(&destination)
        );
        assert_eq!(
            service
                .artwork_paths
                .lock()
                .expect("artwork paths")
                .as_slice(),
            std::slice::from_ref(&destination)
        );

        writer.shutdown();
        std::fs::remove_dir_all(root).expect("remove library root");
    }

    #[test]
    fn managed_retarget_filesystem_failure_does_not_mutate_tags_or_sqlite() {
        let root = unique_test_directory();
        std::fs::create_dir_all(&root).expect("create library root");
        std::fs::write(root.join("loose.flac"), b"audio").expect("write track");

        let track_id = TrackId::new(1).expect("track id");
        let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
        store
            .save_track(Track {
                id: track_id,
                location: TrackLocation::available(
                    TrackRelativePath::new("loose.flac").expect("relative path"),
                ),
                metadata: TrackMetadata {
                    title: Some("Old".to_owned()),
                    ..TrackMetadata::default()
                },
                rating: Rating::unrated(),
                statistics: Default::default(),
                file_size_bytes: None,
                has_embedded_artwork: None,
            })
            .expect("save track");
        std::fs::remove_file(root.join("loose.flac")).expect("remove source");
        std::fs::create_dir(root.join("loose.flac")).expect("replace source with directory");

        let service = Arc::new(RecordingMetadataService::default());
        let (event_tx, event_rx) = async_channel::unbounded();
        let writer = MetadataWriter::start(
            service.clone(),
            store.clone(),
            Some(root.clone()),
            ManagedLibraryFilesystemValidator::default(),
            Some(event_tx),
        );
        assert!(writer.retarget_managed_metadata(
            track_id,
            MetadataChange {
                title: FieldChange::Set("Song".to_owned()),
                artist: FieldChange::Set("Artist".to_owned()),
                album: FieldChange::Set("Album".to_owned()),
                track_number: FieldChange::Set(1),
                ..MetadataChange::default()
            },
        ));

        assert_eq!(
            recv_event(&event_rx, Duration::from_secs(2)),
            MetadataWriterEvent::ManagedRetarget(ManagedMetadataRetargetResult {
                track_id,
                outcome: Err(ApplicationRuntimeError::LibraryConsolidationFailed),
            })
        );
        let stored = store.track(track_id).expect("load track").expect("track");
        assert_eq!(
            stored.location.relative_path.as_path(),
            Path::new("loose.flac")
        );
        assert_eq!(stored.metadata.title.as_deref(), Some("Old"));
        assert!(
            service
                .metadata_paths
                .lock()
                .expect("metadata paths")
                .is_empty()
        );
        assert!(root.join("loose.flac").is_dir());
        assert!(!root.join(".sustain-consolidation-journal").exists());

        writer.shutdown();
        std::fs::remove_dir_all(root).expect("remove library root");
    }

    fn recv_result(
        receiver: &async_channel::Receiver<MetadataWriterEvent>,
        timeout: Duration,
    ) -> MetadataWriteResult {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(MetadataWriterEvent::Mirror(result)) = receiver.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "timed out waiting for result");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn recv_event(
        receiver: &async_channel::Receiver<MetadataWriterEvent>,
        timeout: Duration,
    ) -> MetadataWriterEvent {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(event) = receiver.try_recv() {
                return event;
            }
            assert!(Instant::now() < deadline, "timed out waiting for event");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[derive(Default)]
    struct FailOnceMetadataService {
        attempts: AtomicUsize,
        writes: Mutex<Vec<MetadataChange>>,
    }

    impl MetadataService for FailOnceMetadataService {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Err(MetadataError::ReadFailed)
        }

        fn write_metadata(&self, _path: &Path, change: MetadataChange) -> MetadataResult<()> {
            self.writes.lock().expect("writes lock").push(change);
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(MetadataError::WriteFailed)
            } else {
                Ok(())
            }
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingMetadataService {
        metadata_paths: Mutex<Vec<PathBuf>>,
        rating_paths: Mutex<Vec<PathBuf>>,
        artwork_paths: Mutex<Vec<PathBuf>>,
    }

    impl MetadataService for RecordingMetadataService {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Err(MetadataError::ReadFailed)
        }

        fn write_metadata(&self, path: &Path, _change: MetadataChange) -> MetadataResult<()> {
            self.metadata_paths
                .lock()
                .expect("metadata paths")
                .push(path.to_path_buf());
            Ok(())
        }

        fn write_rating(&self, path: &Path, _rating: Rating) -> MetadataResult<()> {
            self.rating_paths
                .lock()
                .expect("rating paths")
                .push(path.to_path_buf());
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            self.artwork_paths
                .lock()
                .expect("artwork paths")
                .push(path.to_path_buf());
            Ok(())
        }
    }

    fn valid_artwork() -> Vec<u8> {
        vec![
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00,
            0x00, 0xb5, 0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0x64, 0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66,
            0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
        ]
    }

    fn unique_test_directory() -> PathBuf {
        std::env::temp_dir().join(format!(
            "sustain_metadata_writer_test_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock after unix epoch")
                .as_nanos()
        ))
    }
}
