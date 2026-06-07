// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Worker state-machine tests for CD import, driven entirely with fakes —
//! no optical drive, no GStreamer, no real audio files. The test module is
//! a child of `cd_import`, so it can construct the otherwise-opaque
//! [`CdImportTask`] directly and exercise [`run_cd_import_task`] in
//! isolation.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use sustain_cd_import::{
    CdEncoder, CdImportError, EncodeProgress, EncodeRequest, OpticalProbe, RawTocTrack, TocSnapshot,
};
use sustain_domain::{CdEncodingProfile, CdReadMode, Track, TrackMetadata};
use sustain_library_store::{InMemoryLibraryStore, LibraryStore};
use sustain_metadata::{InitialTags, MetadataResult, MetadataService};

use super::{
    CdImportProgress, CdImportResult, CdImportTask, CdTrackPlan, plan, run_cd_import_task,
};
use crate::managed_library::ManagedLibraryFilesystemValidator;

const DEVICE: &str = "/dev/sr0";

fn unique_root() -> PathBuf {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let root = std::env::temp_dir().join(format!(
        "sustain_cd_import_worker_{}_{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::create_dir_all(&root).expect("create worker test root");
    root
}

fn snapshot(disc_id: &str) -> TocSnapshot {
    TocSnapshot::from_raw(
        PathBuf::from(DEVICE),
        disc_id.to_owned(),
        String::new(),
        &[
            RawTocTrack {
                number: 1,
                offset: 150,
                sectors: 13350,
            },
            RawTocTrack {
                number: 2,
                offset: 13500,
                sectors: 18000,
            },
        ],
    )
}

/// Probe whose responses are scripted, so disc-swap behavior is exercised
/// deterministically: the worker re-probes once before each track, popping
/// the next scripted response (the last response repeats once exhausted).
struct FakeProbe {
    script: Mutex<VecDeque<Option<TocSnapshot>>>,
    last: Mutex<Option<TocSnapshot>>,
}

impl FakeProbe {
    fn holding(snapshot: TocSnapshot) -> Self {
        Self::scripted(vec![Some(snapshot)])
    }

    fn scripted(responses: Vec<Option<TocSnapshot>>) -> Self {
        let last = responses.last().cloned().flatten();
        Self {
            script: Mutex::new(responses.into_iter().collect()),
            last: Mutex::new(last),
        }
    }

    fn set_script(&self, responses: Vec<Option<TocSnapshot>>) {
        *self.last.lock().expect("probe last") = responses.last().cloned().flatten();
        *self.script.lock().expect("probe script") = responses.into_iter().collect();
    }
}

impl OpticalProbe for FakeProbe {
    fn candidate_devices(&self) -> Vec<PathBuf> {
        vec![PathBuf::from(DEVICE)]
    }

    fn probe(&self, _device: &Path) -> Option<TocSnapshot> {
        let mut script = self.script.lock().expect("probe script");
        match script.pop_front() {
            Some(response) => response,
            None => self.last.lock().expect("probe last").clone(),
        }
    }
}

/// Encoder that writes placeholder bytes for each track and can be told to
/// fail one track or cancel after a track.
struct FakeEncoder {
    fail_track: Option<u32>,
    cancel_after_track: Option<u32>,
    cancellation: Arc<AtomicBool>,
}

impl FakeEncoder {
    fn new(cancellation: Arc<AtomicBool>) -> Self {
        Self {
            fail_track: None,
            cancel_after_track: None,
            cancellation,
        }
    }
}

impl CdEncoder for FakeEncoder {
    fn ensure_available(&self, _profile: CdEncodingProfile) -> Result<(), CdImportError> {
        Ok(())
    }

    fn encode_track(
        &self,
        request: &EncodeRequest,
        progress: &mut dyn FnMut(EncodeProgress),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<(), CdImportError> {
        if cancelled() {
            return Err(CdImportError::Cancelled);
        }
        progress(EncodeProgress { percent: 50 });
        progress(EncodeProgress { percent: 100 });
        if self.fail_track == Some(request.track) {
            return Err(CdImportError::EncodeFailed("synthetic failure".to_owned()));
        }
        std::fs::write(&request.destination, b"placeholder audio bytes")
            .map_err(|_| CdImportError::EncodeFailed("write".to_owned()))?;
        if self.cancel_after_track == Some(request.track) {
            self.cancellation.store(true, Ordering::SeqCst);
        }
        Ok(())
    }
}

/// Metadata service that no-ops the writes (the placeholder files are not
/// real audio) and reports fixed technical fields for the read-back.
struct FakeMetadata;

impl MetadataService for FakeMetadata {
    fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
        Ok(InitialTags {
            metadata: TrackMetadata {
                duration: Some(Duration::from_secs(178)),
                bitrate_kbps: Some(1000),
                sample_rate_hz: Some(44_100),
                channels: Some(2),
                ..TrackMetadata::default()
            },
            rating: sustain_domain::Rating::unrated(),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(
        &self,
        _path: &Path,
        _change: sustain_domain::MetadataChange,
    ) -> MetadataResult<()> {
        Ok(())
    }

    fn write_rating(&self, _path: &Path, _rating: sustain_domain::Rating) -> MetadataResult<()> {
        Ok(())
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        Ok(())
    }
}

struct Harness {
    root: PathBuf,
    store: Arc<InMemoryLibraryStore>,
    probe: Arc<FakeProbe>,
    encoder: Arc<FakeEncoder>,
    cancellation: Arc<AtomicBool>,
}

impl Harness {
    fn new() -> (Self, TocSnapshot) {
        Self::with_probe_script(vec![])
    }

    /// Build a harness whose probe follows `script` (empty = always report
    /// the original disc).
    fn with_probe_script(script: Vec<Option<TocSnapshot>>) -> (Self, TocSnapshot) {
        let disc = snapshot("disc-a");
        let probe = if script.is_empty() {
            FakeProbe::holding(disc.clone())
        } else {
            FakeProbe::scripted(script)
        };
        let cancellation = Arc::new(AtomicBool::new(false));
        let harness = Self {
            root: unique_root(),
            store: Arc::new(InMemoryLibraryStore::new()),
            probe: Arc::new(probe),
            encoder: Arc::new(FakeEncoder::new(cancellation.clone())),
            cancellation,
        };
        (harness, disc)
    }

    fn task(&self, snapshot: &TocSnapshot, tracks: &[u32]) -> CdImportTask {
        let existing_tracks: Vec<Track> = self.store.tracks().expect("tracks");
        let plans = tracks
            .iter()
            .map(|&number| CdTrackPlan {
                track_number: number,
                metadata: plan::build_track_metadata(snapshot, None, number),
            })
            .collect();
        CdImportTask {
            library_path: self.root.clone(),
            profile: CdEncodingProfile::Flac,
            read_mode: CdReadMode::default(),
            expected_identity: snapshot.identity(),
            snapshot: snapshot.clone(),
            plans,
            cover: None,
            existing_tracks,
            library_store: self.store.clone(),
            metadata_service: Arc::new(FakeMetadata),
            probe: self.probe.clone(),
            encoder: self.encoder.clone(),
            managed_library_filesystem_validator: ManagedLibraryFilesystemValidator::default(),
            cancellation_requested: self.cancellation.clone(),
        }
    }

    fn run(&self, task: CdImportTask) -> CdImportResult {
        run_cd_import_task(task, |_progress| {}).expect("worker completes")
    }

    fn cleanup(self) {
        let _ = std::fs::remove_dir_all(self.root);
    }
}

fn published_files(root: &Path) -> Vec<PathBuf> {
    fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if path.is_dir() {
                walk(&path, out);
            } else if !name.starts_with('.') {
                out.push(path);
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &mut out);
    out.sort();
    out
}

#[test]
fn imports_all_tracks_and_inserts_durable_rows() {
    let (harness, disc) = Harness::new();
    let task = harness.task(&disc, &[1, 2]);

    let result = harness.run(task);

    assert!(result.summary.failure.is_none());
    assert!(!result.summary.cancelled);
    assert_eq!(result.summary.imported_tracks, 2);
    assert_eq!(result.tracks.len(), 2);
    let rows = harness.store.tracks().expect("rows");
    assert_eq!(rows.len(), 2);
    for track in &result.tracks {
        let path = track.location.absolute_path(&harness.root);
        assert!(path.exists(), "published file exists");
        assert!(
            path.starts_with(&harness.root),
            "ripped file is beneath the library root regardless of management mode"
        );
        // Technical fields came from the read-back, not the request.
        assert_eq!(track.metadata.duration, Some(Duration::from_secs(178)));
        assert_eq!(track.metadata.sample_rate_hz, Some(44_100));
        assert_eq!(
            track.file_size_bytes,
            Some(b"placeholder audio bytes".len() as u64)
        );
    }
    let leftover = std::fs::read_dir(&harness.root)
        .expect("read root")
        .flatten()
        .any(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains(".sustain-cd-rip-")
        });
    assert!(!leftover, "no staging files left behind");
    harness.cleanup();
}

#[test]
fn cancellation_keeps_completed_tracks() {
    let (mut harness, disc) = Harness::new();
    Arc::get_mut(&mut harness.encoder)
        .expect("unique encoder")
        .cancel_after_track = Some(1);
    let task = harness.task(&disc, &[1, 2]);

    let result = harness.run(task);

    assert!(result.summary.cancelled);
    assert!(result.summary.failure.is_none());
    assert_eq!(result.summary.imported_tracks, 1);
    assert_eq!(harness.store.tracks().expect("rows").len(), 1);
    assert_eq!(published_files(&harness.root).len(), 1);
    harness.cleanup();
}

#[test]
fn encoder_error_keeps_earlier_tracks_and_removes_staging() {
    let (mut harness, disc) = Harness::new();
    Arc::get_mut(&mut harness.encoder)
        .expect("unique encoder")
        .fail_track = Some(2);
    let task = harness.task(&disc, &[1, 2]);

    let result = harness.run(task);

    assert!(result.summary.failure.is_some(), "the failure is reported");
    assert_eq!(
        result.summary.imported_tracks, 1,
        "track 1 survives track 2's failure"
    );
    assert_eq!(harness.store.tracks().expect("rows").len(), 1);
    assert_eq!(published_files(&harness.root).len(), 1);
    harness.cleanup();
}

#[test]
fn disc_swap_before_a_track_stops_and_keeps_completed() {
    // Probe reports disc A before track 1, then disc B before track 2.
    let (harness, disc) =
        Harness::with_probe_script(vec![Some(snapshot("disc-a")), Some(snapshot("disc-b"))]);
    let task = harness.task(&disc, &[1, 2]);

    let result = harness.run(task);

    assert_eq!(
        result.summary.imported_tracks, 1,
        "track 1 imported under disc A"
    );
    assert!(
        result.summary.failure.is_some(),
        "the swap is reported as a failure"
    );
    assert!(!result.summary.cancelled);
    assert_eq!(harness.store.tracks().expect("rows").len(), 1);
    assert_eq!(published_files(&harness.root).len(), 1);
    harness.cleanup();
}

#[test]
fn disc_swap_at_start_imports_nothing() {
    let (harness, disc) = Harness::new();
    // The drive now holds a different disc than the panel was built from.
    harness.probe.set_script(vec![Some(snapshot("disc-b"))]);
    let task = harness.task(&disc, &[1, 2]);

    let result = harness.run(task);

    assert_eq!(result.summary.imported_tracks, 0);
    assert!(result.summary.failure.is_some());
    assert!(harness.store.tracks().expect("rows").is_empty());
    assert!(published_files(&harness.root).is_empty());
    harness.cleanup();
}

#[test]
fn progress_is_monotonic_and_bounded() {
    let (harness, disc) = Harness::new();
    let task = harness.task(&disc, &[1, 2]);
    let events: Arc<Mutex<Vec<CdImportProgress>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();

    let result = run_cd_import_task(task, move |progress| {
        sink.lock().expect("events").push(progress);
    })
    .expect("worker completes");

    let events = events.lock().expect("events");
    assert!(!events.is_empty());
    let mut last_completed = 0;
    for event in events.iter() {
        assert!(event.current_track_percent <= 100);
        assert!(event.completed_tracks <= event.total_tracks);
        assert!(
            event.completed_tracks >= last_completed,
            "completed-track count never decreases"
        );
        if let Some(number) = event.current_track_number {
            assert!(
                number == 1 || number == 2,
                "the ripping track is one of the selected tracks"
            );
        }
        last_completed = event.completed_tracks;
    }
    assert_eq!(result.summary.imported_tracks, 2);
    let last = events.last().expect("last event");
    assert_eq!(last.completed_tracks, 2);
    assert_eq!(
        last.current_track_number, None,
        "no track is ripping once the run has finished"
    );
    harness.cleanup();
}

#[test]
fn canonical_collision_never_overwrites_an_existing_file() {
    let (harness, disc) = Harness::new();
    let first = harness.run(harness.task(&disc, &[1]));
    let first_path = first.tracks[0].location.absolute_path(&harness.root);
    let first_bytes = std::fs::read(&first_path).expect("read first file");

    // Re-rip the same track: a deliberate new import must not clobber the
    // first file — it lands at a numbered sibling.
    let second = harness.run(harness.task(&disc, &[1]));
    let second_path = second.tracks[0].location.absolute_path(&harness.root);

    assert_ne!(first_path, second_path, "the re-rip used a fresh path");
    assert!(first_path.exists() && second_path.exists());
    assert_eq!(
        std::fs::read(&first_path).expect("first file intact"),
        first_bytes,
        "the original file is untouched"
    );
    assert_eq!(harness.store.tracks().expect("rows").len(), 2);
    harness.cleanup();
}

// --- Runtime-integration: mutation exclusion, cancellation, both modes ---

use sustain_domain::{LibraryManagementMode, UserSettings};
use sustain_settings::{SettingsResult, SettingsStore};

use crate::{ApplicationRuntime, ApplicationRuntimeError, BackgroundTaskStatus};

#[derive(Debug)]
struct StubSettingsStore(UserSettings);

impl SettingsStore for StubSettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings> {
        Ok(self.0.clone())
    }

    fn save_settings(&self, _settings: UserSettings) -> SettingsResult<()> {
        Ok(())
    }
}

fn runtime_with(
    root: &Path,
    mode: LibraryManagementMode,
    store: Arc<InMemoryLibraryStore>,
    probe: Arc<FakeProbe>,
    encoder: Arc<FakeEncoder>,
) -> ApplicationRuntime {
    let mut settings = UserSettings::default();
    settings.library.path = Some(root.to_path_buf());
    settings.library.management_mode = mode;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(StubSettingsStore(settings)))
            .expect("runtime loads");
    runtime
        .set_library_services(store, Arc::new(FakeMetadata))
        .expect("library services install");
    runtime.set_cd_backend(probe, encoder);
    runtime
}

fn request(snapshot: &TocSnapshot, tracks: Vec<u32>) -> super::CdImportRequest {
    super::CdImportRequest {
        snapshot: snapshot.clone(),
        selected_tracks: tracks,
        release: None,
        cover: None,
        overrides: std::collections::BTreeMap::new(),
    }
}

#[test]
fn track_override_replaces_fields_but_ignores_blank_ones() {
    use super::{CdTrackOverride, apply_track_override};
    use sustain_domain::TrackMetadata;

    let mut metadata = TrackMetadata {
        title: Some("Track 01".to_owned()),
        artist: Some("Audio CD".to_owned()),
        ..TrackMetadata::default()
    };

    // A blank / whitespace-only override leaves the looked-up values alone.
    apply_track_override(
        &mut metadata,
        Some(&CdTrackOverride {
            title: Some("   ".to_owned()),
            artist: None,
        }),
    );
    assert_eq!(metadata.title.as_deref(), Some("Track 01"));
    assert_eq!(metadata.artist.as_deref(), Some("Audio CD"));

    // Non-blank fields win, trimmed.
    apply_track_override(
        &mut metadata,
        Some(&CdTrackOverride {
            title: Some("  Real Title  ".to_owned()),
            artist: Some("Real Artist".to_owned()),
        }),
    );
    assert_eq!(metadata.title.as_deref(), Some("Real Title"));
    assert_eq!(metadata.artist.as_deref(), Some("Real Artist"));
}

#[test]
fn running_cd_import_excludes_other_mutations_and_joins_global_cancellation() {
    let root = unique_root();
    let disc = snapshot("disc-a");
    let store = Arc::new(InMemoryLibraryStore::new());
    let probe = Arc::new(FakeProbe::holding(disc.clone()));
    let cancellation = Arc::new(AtomicBool::new(false));
    let encoder = Arc::new(FakeEncoder::new(cancellation));
    let mut runtime = runtime_with(
        &root,
        LibraryManagementMode::ReferenceFilesInPlace,
        store,
        probe,
        encoder,
    );

    let task = runtime
        .prepare_cd_import(request(&disc, vec![1, 2]))
        .expect("prepare claims the slot");
    assert_eq!(
        *runtime.background_task_status(),
        BackgroundTaskStatus::CdImportRunning
    );
    // Another library mutation is refused while the rip owns the slot.
    assert!(matches!(
        runtime.prepare_library_import(Vec::new()),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    ));
    // The global Cancel button reaches the rip's token.
    runtime.request_background_task_cancellation();
    assert!(runtime.background_task_cancellation_requested());

    // Draining the (now cancelled) task returns the slot to Idle.
    let result = run_cd_import_task(task, |_progress| {}).expect("worker completes");
    runtime.apply_cd_import_result(result);
    assert_eq!(
        *runtime.background_task_status(),
        BackgroundTaskStatus::Idle
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn both_library_modes_place_ripped_files_beneath_the_library_root() {
    for mode in [
        LibraryManagementMode::ReferenceFilesInPlace,
        LibraryManagementMode::CopyAddedFilesIntoLibrary,
    ] {
        let root = unique_root();
        let disc = snapshot("disc-a");
        let store = Arc::new(InMemoryLibraryStore::new());
        let probe = Arc::new(FakeProbe::holding(disc.clone()));
        let cancellation = Arc::new(AtomicBool::new(false));
        let encoder = Arc::new(FakeEncoder::new(cancellation));
        let mut runtime = runtime_with(&root, mode, store.clone(), probe, encoder);

        let task = runtime
            .prepare_cd_import(request(&disc, vec![1]))
            .expect("prepare");
        let result = run_cd_import_task(task, |_progress| {}).expect("worker completes");
        runtime.apply_cd_import_result(result);

        let rows = store.tracks().expect("rows");
        assert_eq!(rows.len(), 1, "mode {mode:?} imported one track");
        let path = rows[0].location.absolute_path(&root);
        assert!(
            path.starts_with(&root) && path.exists(),
            "mode {mode:?} owns the ripped file beneath the library root"
        );
        let _ = std::fs::remove_dir_all(root);
    }
}
