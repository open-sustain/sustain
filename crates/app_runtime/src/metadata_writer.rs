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

enum MetadataWriterCommand {
    Nudge,
    SetLibraryPath(Option<PathBuf>),
    Shutdown,
}

/// Owns the single file-tag mirror worker. The channel carries wakeups and
/// configuration changes only; durable work lives in the library store.
pub(crate) struct MetadataWriter {
    sender: Sender<MetadataWriterCommand>,
    library_store: Arc<dyn LibraryStore>,
    result_sink: Arc<Mutex<Option<async_channel::Sender<MetadataWriteResult>>>>,
    handle: Option<JoinHandle<()>>,
}

impl MetadataWriter {
    pub(crate) fn start(
        metadata_service: Arc<dyn MetadataService>,
        library_store: Arc<dyn LibraryStore>,
        library_path: Option<PathBuf>,
        result_sink: Option<async_channel::Sender<MetadataWriteResult>>,
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

    pub(crate) fn set_result_sink(&self, sink: async_channel::Sender<MetadataWriteResult>) {
        if let Ok(mut slot) = self.result_sink.lock() {
            *slot = Some(sink);
        }
    }

    pub(crate) fn nudge(&self) {
        let _ = self.sender.send(MetadataWriterCommand::Nudge);
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
    result_sink: Arc<Mutex<Option<async_channel::Sender<MetadataWriteResult>>>>,
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
                if !apply_command(command, &mut library_path, &mut active) {
                    break;
                }
                while let Ok(command) = receiver.try_recv() {
                    if !apply_command(command, &mut library_path, &mut active) {
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
    result_sink: Option<&async_channel::Sender<MetadataWriteResult>>,
) {
    let sink = Mutex::new(result_sink.cloned());
    while drain_due_batch(metadata_service, library_store, library_path, &sink) {}
}

fn drain_due_batch(
    metadata_service: &dyn MetadataService,
    library_store: &dyn LibraryStore,
    library_path: Option<&PathBuf>,
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriteResult>>>,
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
    library_path: &mut Option<PathBuf>,
    active: &mut bool,
) -> bool {
    match command {
        MetadataWriterCommand::Nudge => {
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
    if pending.kinds.metadata {
        metadata_service
            .write_metadata(&path, full_metadata_mirror(&track.metadata))
            .map_err(metadata_error)?;
    }
    if pending.kinds.rating {
        metadata_service
            .write_rating(&path, track.rating)
            .map_err(metadata_error)?;
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
    }
    Ok(())
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
    result_sink: &Mutex<Option<async_channel::Sender<MetadataWriteResult>>>,
    result: MetadataWriteResult,
) {
    if let Ok(sink) = result_sink.lock()
        && let Some(sink) = sink.as_ref()
    {
        let _ = sink.try_send(result);
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

    fn recv_result(
        receiver: &async_channel::Receiver<MetadataWriteResult>,
        timeout: Duration,
    ) -> MetadataWriteResult {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(result) = receiver.try_recv() {
                return result;
            }
            assert!(Instant::now() < deadline, "timed out waiting for result");
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
