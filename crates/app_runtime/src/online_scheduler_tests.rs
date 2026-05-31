// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    fs::File,
    io::Write,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU32, Ordering},
        mpsc as std_mpsc,
    },
    time::{Duration, Instant},
};

use sustain_domain::{
    MetadataChange, OnlineSettings, Rating, SyncedLyrics, Track, TrackLocation, TrackRelativePath,
};
use sustain_library_store::{
    InMemoryLibraryStore, LibraryStore, OnlineCapabilities, SqliteLibraryStore, TrackId,
};
use sustain_metadata::{InitialTags, MetadataError, MetadataResult, MetadataService};
use sustain_metadata_remote::{
    FetchedArtwork, FetchedLyrics, RemoteError, RemoteMetadataService, RemoteResult, TrackMatch,
    TrackQuery,
};
use tempfile::TempDir;

use crate::metadata_writer::{MetadataWriteHandle, MetadataWriter};

use super::{OnlineScheduler, OnlineSchedulerConfig, ProgressSink, SchedulerProgress, UnixClockFn};

/// Spawn a real [`MetadataWriter`] in front of the test's
/// [`StubMetadata`] so the online scheduler's tag writes flow
/// through the same actor the production path uses. Returns the
/// writer (which must out-live the scheduler so its actor stays
/// alive) alongside a cloneable handle for the scheduler config.
fn spawn_tag_writer(metadata: Arc<StubMetadata>) -> (MetadataWriter, MetadataWriteHandle) {
    let writer = MetadataWriter::start(metadata);
    let handle = writer.handle();
    (writer, handle)
}

fn touch_in(library_root: &Path, relative: &str) -> Track {
    let absolute = library_root.join(relative);
    if let Some(parent) = absolute.parent() {
        std::fs::create_dir_all(parent).expect("create parent");
    }
    File::create(&absolute)
        .and_then(|mut f| f.write_all(b""))
        .expect("create file");
    let relative_path = TrackRelativePath::new(relative).expect("valid relative path");
    Track {
        id: TrackId::new(1).expect("non-zero"),
        location: TrackLocation::available(relative_path),
        metadata: Default::default(),
        rating: Default::default(),
        statistics: Default::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
    }
}

fn fixed_clock(value: i64) -> UnixClockFn {
    Arc::new(move || value)
}

fn capturing_sink() -> (ProgressSink, std_mpsc::Receiver<SchedulerProgress>) {
    let (tx, rx) = std_mpsc::channel();
    let sink: ProgressSink = Arc::new(move |progress| {
        let _ = tx.send(progress);
    });
    (sink, rx)
}

fn wait_for(
    rx: &std_mpsc::Receiver<SchedulerProgress>,
    timeout: Duration,
    predicate: impl Fn(&SchedulerProgress) -> bool,
) -> SchedulerProgress {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let progress = rx
            .recv_timeout(remaining)
            .expect("scheduler progress within timeout");
        if predicate(&progress) {
            return progress;
        }
    }
}

/// Test double that returns canned fetch responses and records
/// every call so assertions can verify "the scheduler did /
/// did not contact the provider".
#[derive(Default)]
struct StubRemote {
    identify: Mutex<Option<RemoteResult<Option<TrackMatch>>>>,
    lyrics: Mutex<Option<RemoteResult<Option<FetchedLyrics>>>>,
    artwork: Mutex<Option<RemoteResult<Option<FetchedArtwork>>>>,
    artwork_for_match: Mutex<Option<RemoteResult<Option<FetchedArtwork>>>>,
    identify_calls: AtomicU32,
    lyrics_calls: AtomicU32,
    artwork_calls: AtomicU32,
    artwork_for_match_calls: AtomicU32,
}

impl StubRemote {
    fn with_lyrics(self, value: RemoteResult<Option<FetchedLyrics>>) -> Self {
        *self.lyrics.lock().expect("lock") = Some(value);
        self
    }
    fn with_artwork(self, value: RemoteResult<Option<FetchedArtwork>>) -> Self {
        *self.artwork.lock().expect("lock") = Some(value);
        self
    }
    fn with_identify(self, value: RemoteResult<Option<TrackMatch>>) -> Self {
        *self.identify.lock().expect("lock") = Some(value);
        self
    }
    fn with_artwork_for_match(self, value: RemoteResult<Option<FetchedArtwork>>) -> Self {
        *self.artwork_for_match.lock().expect("lock") = Some(value);
        self
    }
}

impl RemoteMetadataService for StubRemote {
    fn identify_track(&self, _query: &TrackQuery) -> RemoteResult<Option<TrackMatch>> {
        self.identify_calls.fetch_add(1, Ordering::SeqCst);
        self.identify
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or(Ok(None))
    }
    fn fetch_artwork_for_match(
        &self,
        _track_match: &TrackMatch,
    ) -> RemoteResult<Option<FetchedArtwork>> {
        self.artwork_for_match_calls.fetch_add(1, Ordering::SeqCst);
        self.artwork_for_match
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or(Ok(None))
    }
    fn fetch_artwork(&self, _query: &TrackQuery) -> RemoteResult<Option<FetchedArtwork>> {
        self.artwork_calls.fetch_add(1, Ordering::SeqCst);
        self.artwork
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or(Ok(None))
    }
    fn fetch_lyrics(&self, _query: &TrackQuery) -> RemoteResult<Option<FetchedLyrics>> {
        self.lyrics_calls.fetch_add(1, Ordering::SeqCst);
        self.lyrics
            .lock()
            .expect("lock")
            .clone()
            .unwrap_or(Ok(None))
    }
}

struct BlockingIdentifyRemote {
    matched: TrackMatch,
    started: std_mpsc::SyncSender<()>,
    resume: Mutex<std_mpsc::Receiver<()>>,
}

impl RemoteMetadataService for BlockingIdentifyRemote {
    fn identify_track(&self, _query: &TrackQuery) -> RemoteResult<Option<TrackMatch>> {
        self.started.send(()).expect("report identify barrier");
        self.resume
            .lock()
            .expect("resume lock")
            .recv()
            .expect("resume identify");
        Ok(Some(self.matched.clone()))
    }

    fn fetch_artwork_for_match(
        &self,
        _track_match: &TrackMatch,
    ) -> RemoteResult<Option<FetchedArtwork>> {
        Ok(None)
    }

    fn fetch_lyrics(&self, _query: &TrackQuery) -> RemoteResult<Option<FetchedLyrics>> {
        Ok(None)
    }
}

/// Test double for the metadata service. Records every write so
/// assertions can verify what the scheduler did.
#[derive(Default)]
struct StubMetadata {
    artwork_writes: Mutex<Vec<Option<Vec<u8>>>>,
    metadata_writes: Mutex<Vec<MetadataChange>>,
}

impl MetadataService for StubMetadata {
    fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
        Ok(InitialTags {
            metadata: Default::default(),
            rating: sustain_domain::Rating::unrated(),
            has_embedded_artwork: false,
        })
    }
    fn write_metadata(&self, _path: &Path, change: MetadataChange) -> MetadataResult<()> {
        self.metadata_writes.lock().expect("lock").push(change);
        Ok(())
    }
    fn write_rating(&self, _path: &Path, _rating: sustain_domain::Rating) -> MetadataResult<()> {
        Err(MetadataError::WriteFailed)
    }
    fn write_artwork(&self, _path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        self.artwork_writes.lock().expect("lock").push(artwork);
        Ok(())
    }
    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }
}

fn track_with_metadata(library_root: &Path, relative: &str) -> Track {
    let mut t = touch_in(library_root, relative);
    t.metadata.artist = Some("Artist".to_owned());
    t.metadata.title = Some("Title".to_owned());
    t
}

#[test]
fn scheduler_idles_with_no_capabilities_enabled() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    store
        .save_track(track_with_metadata(temp.path(), "alpha.flac"))
        .expect("save");

    let remote = Arc::new(StubRemote::default());
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store,
        progress: sink,
        track_updated: None,
        clock: fixed_clock(0),
        initial_settings: OnlineSettings::default(),
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let first = rx
        .recv_timeout(Duration::from_secs(2))
        .expect("first progress");
    assert!(matches!(first, SchedulerProgress::Idle { .. }));
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(remote.lyrics_calls.load(Ordering::SeqCst), 0);
    assert_eq!(remote.artwork_calls.load(Ordering::SeqCst), 0);

    scheduler.shutdown();
}

#[test]
fn lyrics_capability_pulls_and_persists() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_lyrics(Ok(Some(FetchedLyrics {
        plain: Some("Plain text".to_owned()),
        synced_lrc: Some("[00:01.50]Hello\n[00:03.00]World".to_owned()),
    }))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1_700_000_000),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    // Plain lyrics mirrored into tracks.lyrics and written via tag.
    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.metadata.lyrics.as_deref(), Some("Plain text"));
    assert_eq!(metadata.metadata_writes.lock().expect("lock").len(), 1);

    // Synced parsed and persisted.
    let synced = store
        .load_synced_lyrics(track.id)
        .expect("load")
        .expect("present");
    assert_eq!(synced.source, "lrclib");
    assert_eq!(
        synced.lyrics,
        SyncedLyrics::parse_lrc("[00:01.50]Hello\n[00:03.00]World").expect("parse")
    );

    // Attempt stamped — track no longer qualifies.
    assert!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: false,
                    tags: false,
                    lyrics: true,
                },
                1,
                10,
            )
            .expect("query")
            .is_empty()
    );

    scheduler.shutdown();
}

#[test]
fn lyrics_skipped_when_both_plain_and_synced_already_present() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.metadata.lyrics = Some("Existing".to_owned());
    store.save_track(track.clone()).expect("save");
    store
        .record_synced_lyrics(
            track.id,
            &SyncedLyrics::parse_lrc("[00:01.00]Already").expect("parse"),
            "test",
        )
        .expect("seed synced");

    let remote = Arc::new(StubRemote::default().with_lyrics(Ok(Some(FetchedLyrics {
        plain: Some("Should not overwrite".to_owned()),
        synced_lrc: Some("[00:02.00]Should not overwrite".to_owned()),
    }))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { .. })
    });

    // Remote should never have been called — both fields are
    // already present, so the worker short-circuits.
    assert_eq!(remote.lyrics_calls.load(Ordering::SeqCst), 0);
    // Existing values preserved.
    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.metadata.lyrics.as_deref(), Some("Existing"));
    let synced = store
        .load_synced_lyrics(track.id)
        .expect("load")
        .expect("present");
    assert_eq!(synced.source, "test");

    scheduler.shutdown();
}

#[test]
fn artwork_capability_skips_when_embedded_present() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    // The scan-time bit is the contract here: when the file
    // already carries a picture, `tracks_needing_online` must
    // never offer this id for artwork even at a fresh
    // `provider_version`.
    track.has_embedded_artwork = Some(true);
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default());
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: true,
            tags: false,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    // The candidate list filters this id out at the SQL layer,
    // so the scheduler reaches Idle without ever invoking the
    // remote — no Tick is emitted.
    let _idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });

    assert_eq!(
        remote.artwork_calls.load(Ordering::SeqCst),
        0,
        "track already has embedded artwork; no remote call needed"
    );
    assert!(metadata.artwork_writes.lock().expect("lock").is_empty());

    scheduler.shutdown();
}

#[test]
fn explicit_artwork_run_skips_track_with_embedded_artwork() {
    // The manual (force) path bypasses the `tracks_needing_online`
    // SQL filter, so the per-track embedded-artwork guard inside
    // `attempt_artwork` is the *only* thing standing between a
    // "Retrieve → Artwork" click and an unsolicited overwrite of an
    // existing embedded cover. With background artwork off, the
    // track only reaches the worker via the explicit queue — and it
    // must still be skipped (issue #61).
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.has_embedded_artwork = Some(true);
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_artwork(Ok(Some(FetchedArtwork {
        bytes: vec![9, 9, 9, 9],
        release_mbid: "release".to_owned(),
    }))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    scheduler.request_explicit_run(
        vec![track.id],
        OnlineCapabilities {
            artwork: true,
            tags: false,
            lyrics: false,
        },
    );

    let _idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });

    assert_eq!(
        remote.artwork_calls.load(Ordering::SeqCst),
        0,
        "embedded-artwork track must be skipped even on a forced run"
    );
    assert!(metadata.artwork_writes.lock().expect("lock").is_empty());

    scheduler.shutdown();
}

#[test]
fn artwork_capability_fetches_and_writes_when_missing() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_artwork(Ok(Some(FetchedArtwork {
        bytes: vec![1, 2, 3, 4],
        release_mbid: "release".to_owned(),
    }))));
    let metadata = Arc::new(StubMetadata::default());
    // track.has_embedded_artwork left None → tracks_needing_online
    // treats it as "not yet scanned" and the artwork capability
    // still applies, so the scheduler asks the remote.
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: true,
            tags: false,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    assert_eq!(remote.artwork_calls.load(Ordering::SeqCst), 1);
    let writes = metadata.artwork_writes.lock().expect("lock");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].as_deref(), Some(&[1u8, 2, 3, 4][..]));

    scheduler.shutdown();
}

#[test]
fn remote_error_records_attempt_and_is_not_retried() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_lyrics(Err(RemoteError::Network)));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { failed: 1, .. })
    });

    // Attempt stamped — track no longer qualifies even though the
    // provider errored.
    assert!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: false,
                    tags: false,
                    lyrics: true,
                },
                1,
                10,
            )
            .expect("query")
            .is_empty()
    );

    scheduler.shutdown();
}

#[test]
fn toggling_capabilities_off_stops_the_running_worker() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    for i in 0..16 {
        let relative = format!("track_{i:02}.flac");
        let mut t = track_with_metadata(temp.path(), &relative);
        t.id = TrackId::new(i + 1).expect("non-zero");
        store.save_track(t).expect("save");
    }

    let remote = Arc::new(StubRemote::default().with_lyrics(Ok(None)));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _first = wait_for(
        &rx,
        Duration::from_secs(2),
        |progress| matches!(progress, SchedulerProgress::Tick { completed, .. } if *completed >= 1),
    );
    scheduler.update_settings(OnlineSettings::default());
    let _idle = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Idle { .. })
    });

    let before = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            100,
        )
        .expect("query")
        .len();
    std::thread::sleep(Duration::from_millis(500));
    let after = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            100,
        )
        .expect("query")
        .len();
    assert_eq!(
        before, after,
        "worker must stop attempting tracks once capabilities go to zero"
    );
    assert!(after > 0, "some tracks should still be un-attempted");

    scheduler.shutdown();
}

#[test]
fn tags_fill_recording_level_fields_when_album_is_missing_but_skip_positional() {
    // When the user has no album yet, MusicBrainz's "first
    // release" is just a guess: filling track/disc positional
    // fields from it would frequently write the wrong values
    // (the same recording lives on multiple releases). Album
    // does get the first-release guess because there is no
    // useful alternative. Year is sourced from the recording's
    // first-release-date, not from a particular release.
    use sustain_metadata_remote::{GenreCandidate, TrackMatchRelease, TrackMatchSource};

    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.metadata.artist = Some("Existing Artist".to_owned());
    track.metadata.title = Some("Existing Title".to_owned());
    store.save_track(track.clone()).expect("save");

    let track_match = TrackMatch {
        recording_mbid: "rec-mbid".to_owned(),
        title: Some("Other Title".to_owned()),
        artist: Some("Other Artist".to_owned()),
        first_release_year: Some(2014),
        genres: vec![GenreCandidate {
            name: "trip-hop".to_owned(),
            vote_count: 9,
        }],
        releases: vec![TrackMatchRelease {
            release_mbid: "rel-mbid".to_owned(),
            release_group_mbid: None,
            title: Some("Filled Album".to_owned()),
            year: Some(2018),
            track_number: Some(3),
            track_total: Some(12),
            disc_number: Some(1),
        }],
        source: TrackMatchSource::MusicBrainzTags,
    };
    let remote = Arc::new(
        StubRemote::default()
            .with_identify(Ok(Some(track_match)))
            .with_artwork_for_match(Ok(Some(FetchedArtwork {
                bytes: vec![0xAA, 0xBB],
                release_mbid: "rel-mbid".to_owned(),
            }))),
    );
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: true,
            tags: true,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.metadata.artist.as_deref(), Some("Existing Artist"));
    assert_eq!(stored.metadata.title.as_deref(), Some("Existing Title"));
    assert_eq!(stored.metadata.album.as_deref(), Some("Filled Album"));
    // Year comes from recording's first-release-date, NOT the
    // release's date — even though both are populated in the
    // match, the recording-level value is what got written.
    assert_eq!(stored.metadata.year, Some(2014));
    assert_eq!(stored.metadata.genre.as_deref(), Some("trip-hop"));
    // Positional fields stay None: we had no album to align the
    // matched release against. The release's positional fields
    // are ignored entirely.
    assert_eq!(stored.metadata.track_number, None);
    assert_eq!(stored.metadata.track_total, None);
    assert_eq!(stored.metadata.disc_number, None);

    assert_eq!(remote.identify_calls.load(Ordering::SeqCst), 1);
    assert_eq!(remote.artwork_for_match_calls.load(Ordering::SeqCst), 1);
    assert_eq!(remote.artwork_calls.load(Ordering::SeqCst), 0);

    scheduler.shutdown();
}

#[test]
fn tags_fill_positional_fields_only_when_album_matches_a_matched_release() {
    // With an album already set, the matched release with the
    // same title is used for track_number / track_total /
    // disc_number. Other matched releases (different
    // compilations) are ignored.
    use sustain_metadata_remote::{GenreCandidate, TrackMatchRelease, TrackMatchSource};

    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.metadata.album = Some("Mezzanine".to_owned());
    store.save_track(track.clone()).expect("save");

    let track_match = TrackMatch {
        recording_mbid: "rec-mbid".to_owned(),
        title: Some("Angel".to_owned()),
        artist: Some("Massive Attack".to_owned()),
        first_release_year: Some(1998),
        genres: vec![GenreCandidate {
            name: "trip-hop".to_owned(),
            vote_count: 9,
        }],
        releases: vec![
            TrackMatchRelease {
                release_mbid: "comp-mbid".to_owned(),
                release_group_mbid: None,
                title: Some("Greatest Hits".to_owned()),
                year: Some(2006),
                track_number: Some(7),
                track_total: Some(18),
                disc_number: Some(1),
            },
            TrackMatchRelease {
                release_mbid: "rel-mbid".to_owned(),
                release_group_mbid: None,
                // Casing/whitespace differs from the user's
                // stored value to verify normalized matching.
                title: Some(" mezzanine ".to_owned()),
                year: Some(1998),
                track_number: Some(1),
                track_total: Some(11),
                disc_number: Some(1),
            },
        ],
        source: TrackMatchSource::MusicBrainzTags,
    };
    let remote = Arc::new(StubRemote::default().with_identify(Ok(Some(track_match))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: true,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.metadata.album.as_deref(), Some("Mezzanine"));
    // Positional fields come from the *matching* release, not
    // the first release.
    assert_eq!(stored.metadata.track_number, Some(1));
    assert_eq!(stored.metadata.track_total, Some(11));
    assert_eq!(stored.metadata.disc_number, Some(1));

    scheduler.shutdown();
}

#[test]
fn tag_enrichment_snapshot_cannot_clobber_concurrent_rating_edit() {
    use sustain_metadata_remote::TrackMatchSource;

    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> =
        Arc::new(SqliteLibraryStore::open_in_memory().expect("store"));
    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.metadata.title = None;
    store.save_track(track.clone()).expect("save");

    let (started_tx, started_rx) = std_mpsc::sync_channel(1);
    let (resume_tx, resume_rx) = std_mpsc::sync_channel(1);
    let remote = Arc::new(BlockingIdentifyRemote {
        matched: TrackMatch {
            recording_mbid: "recording".to_owned(),
            title: Some("Remote title".to_owned()),
            artist: None,
            first_release_year: None,
            genres: Vec::new(),
            releases: Vec::new(),
            source: TrackMatchSource::MusicBrainzTags,
        },
        started: started_tx,
        resume: Mutex::new(resume_rx),
    });
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();
    let mut runtime = crate::ApplicationRuntime::new()
        .with_library_services(store.clone(), metadata.clone())
        .expect("library services");
    let (notify_tx, notify_rx) = std_mpsc::channel::<TrackId>();
    let track_updated: super::TrackUpdatedSink = Arc::new(move |id| {
        let _ = notify_tx.send(id);
    });
    let (_writer, tag_writer) = spawn_tag_writer(metadata);

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote,
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: Some(track_updated),
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: true,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    // The worker loaded its stale row before entering identify_track. Commit
    // the user edit while the provider request is paused, then let enrichment
    // finish. Its missing-only write may fill title, but rating must survive.
    started_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("identify barrier reached");
    store
        .update_track_rating(track.id, Rating::new(5).expect("rating"))
        .expect("commit concurrent rating");
    runtime.apply_track_updated(track.id);
    resume_tx.send(()).expect("resume identify");
    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });
    let observed = notify_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("track_updated sink fires after persist");
    runtime.apply_track_updated(observed);

    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.rating, Rating::new(5).expect("rating"));
    assert_eq!(stored.metadata.title.as_deref(), Some("Remote title"));
    let runtime_track = runtime
        .library_tracks()
        .iter()
        .find(|candidate| candidate.id == track.id)
        .expect("runtime track");
    assert_eq!(runtime_track.rating, Rating::new(5).expect("rating"));
    assert_eq!(
        runtime_track.metadata.title.as_deref(),
        Some("Remote title")
    );

    scheduler.shutdown();
}

#[test]
fn genre_prefers_a_candidate_already_present_in_the_library() {
    // Library already has House. A match returns Electronica (top
    // voted) and House (lower voted). Electronica must NOT win —
    // we must converge on the library's existing genre.
    use sustain_metadata_remote::{GenreCandidate, TrackMatchSource};

    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    // Seed an unrelated track carrying "House" so the library
    // exposes it through distinct_genres().
    let mut seed = track_with_metadata(temp.path(), "seed.flac");
    seed.id = TrackId::new(99).expect("non-zero");
    seed.metadata.genre = Some("House".to_owned());
    store.save_track(seed).expect("save seed");

    let mut track = track_with_metadata(temp.path(), "alpha.flac");
    track.metadata.album = Some("Album".to_owned());
    store.save_track(track.clone()).expect("save");

    let track_match = TrackMatch {
        recording_mbid: "rec-mbid".to_owned(),
        title: None,
        artist: None,
        first_release_year: None,
        genres: vec![
            GenreCandidate {
                name: "electronica".to_owned(),
                vote_count: 9,
            },
            GenreCandidate {
                name: "house".to_owned(),
                vote_count: 3,
            },
        ],
        releases: vec![],
        source: TrackMatchSource::MusicBrainzTags,
    };
    let remote = Arc::new(StubRemote::default().with_identify(Ok(Some(track_match))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: true,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    // Two ticks may fire (one per track in pending). Wait for the
    // alpha.flac track to settle.
    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { .. })
    });

    let stored = store.track(track.id).expect("load").expect("present");
    // Library spelling preserved ("House"), not MB's lowercase.
    assert_eq!(stored.metadata.genre.as_deref(), Some("House"));

    scheduler.shutdown();
}

#[test]
fn track_updated_sink_fires_after_successful_lyrics_persist() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_lyrics(Ok(Some(FetchedLyrics {
        plain: Some("Plain".to_owned()),
        synced_lrc: None,
    }))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (notify_tx, notify_rx) = std_mpsc::channel::<TrackId>();
    let track_updated: super::TrackUpdatedSink = Arc::new(move |id| {
        let _ = notify_tx.send(id);
    });

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote,
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: Some(track_updated),
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    let observed = notify_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("track_updated sink fires after a successful persist");
    assert_eq!(observed, track.id);

    scheduler.shutdown();
}

#[test]
fn rate_limited_lyrics_does_not_stamp_attempt_so_track_stays_eligible() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(
        StubRemote::default().with_lyrics(Err(RemoteError::RateLimited {
            cool_down: Duration::from_secs(60),
        })),
    );
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: false,
            tags: false,
            lyrics: true,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    // The tick reports the failure exactly like any other; the
    // distinguishing behaviour lives in what didn't get written.
    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { failed: 1, .. })
    });

    // After the batch, the track must still qualify — a rate-limited
    // capability is never stamped, so the next pass picks it up
    // again once the HTTP client's per-host cool-down has elapsed.
    let still_pending = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            10,
        )
        .expect("query");
    assert_eq!(
        still_pending,
        vec![track.id],
        "rate-limited track must remain eligible for the next batch"
    );

    scheduler.shutdown();
}

#[test]
fn rate_limited_in_one_capability_still_stamps_other_completed_capabilities() {
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    // tags runs first and succeeds (NoMatch), artwork then hits
    // a 429. After the batch, tags must be stamped (won't retry);
    // artwork must be left un-stamped (will retry after cool-down).
    let remote = Arc::new(
        StubRemote::default()
            .with_identify(Ok(None))
            .with_artwork(Err(RemoteError::RateLimited {
                cool_down: Duration::from_secs(30),
            })),
    );
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();

    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1),
        initial_settings: OnlineSettings {
            artwork: true,
            tags: true,
            lyrics: false,
        },
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { .. })
    });

    // tags is stamped → no longer a tags candidate.
    assert!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: false,
                    tags: true,
                    lyrics: false,
                },
                1,
                10,
            )
            .expect("query")
            .is_empty(),
        "completed tags capability should be stamped"
    );
    // artwork is NOT stamped → still a candidate.
    assert_eq!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: true,
                    tags: false,
                    lyrics: false,
                },
                1,
                10,
            )
            .expect("query"),
        vec![track.id],
        "rate-limited artwork capability must remain eligible"
    );

    scheduler.shutdown();
}

#[test]
fn shutdown_returns_after_join() {
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let remote = Arc::new(StubRemote::default());
    let metadata = Arc::new(StubMetadata::default());
    let (sink, _rx) = capturing_sink();
    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote,
        tag_writer,
        library_store: store,
        progress: sink,
        track_updated: None,
        clock: fixed_clock(0),
        initial_settings: OnlineSettings::default(),
        library_path: None,
        provider_version: 1,
    });
    let start = Instant::now();
    scheduler.shutdown();
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[test]
fn explicit_run_processes_tracks_with_global_settings_off() {
    // Per-playlist "Fetch Lyrics" must work even though the
    // global lyrics toggle is off — the user explicitly asked
    // for it on this playlist.
    let temp = TempDir::new().expect("temp");
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let track = track_with_metadata(temp.path(), "alpha.flac");
    store.save_track(track.clone()).expect("save");

    let remote = Arc::new(StubRemote::default().with_lyrics(Ok(Some(FetchedLyrics {
        plain: Some("Plain text".to_owned()),
        synced_lrc: None,
    }))));
    let metadata = Arc::new(StubMetadata::default());
    let (sink, rx) = capturing_sink();
    let (_writer, tag_writer) = spawn_tag_writer(metadata.clone());

    let scheduler = OnlineScheduler::start(OnlineSchedulerConfig {
        remote_service: remote.clone(),
        tag_writer,
        library_store: store.clone(),
        progress: sink,
        track_updated: None,
        clock: fixed_clock(1_700_000_000),
        // Background settings all off — without the explicit
        // command, nothing would happen.
        initial_settings: OnlineSettings::default(),
        library_path: Some(temp.path().to_path_buf()),
        provider_version: 1,
    });

    scheduler.request_explicit_run(
        vec![track.id],
        OnlineCapabilities {
            artwork: false,
            tags: false,
            lyrics: true,
        },
    );

    let _tick = wait_for(&rx, Duration::from_secs(2), |progress| {
        matches!(progress, SchedulerProgress::Tick { completed: 1, .. })
    });

    // Lyrics were fetched and persisted, proving the explicit
    // capability mask routed all the way through.
    let stored = store.track(track.id).expect("load").expect("present");
    assert_eq!(stored.metadata.lyrics.as_deref(), Some("Plain text"));
    assert_eq!(remote.lyrics_calls.load(Ordering::SeqCst), 1);

    scheduler.shutdown();
}
