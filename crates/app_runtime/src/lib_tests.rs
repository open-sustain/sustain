// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
};

use super::{
    AnalysisCapability, AnalysisRunRequest, OnlineCapability, OnlineRunRequest, RunDecision,
    SmartPlaylistTrackStatus,
};
use sustain_domain::{
    ApplicationCommand, Clock, FieldChange, LibraryManagementMode, MonotonicClock, PlayStatistics,
    PlaybackCommand, PlaybackOptions, PlaybackState, Playlist, PlaylistFolderId, PlaylistId,
    PlaylistItem, Rating, RepeatMode, ShuffleMode, SmartPlaylist, SmartPlaylistDateField,
    SmartPlaylistId, SmartPlaylistLimit, SmartPlaylistLimitSelection, SmartPlaylistMatchKind,
    SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField, SmartPlaylistTextOperator,
    Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath, UiSettings,
    UiSidebarSelection, UserSettings, VolumePercent,
};
use sustain_library_store::{
    AcousticFeatures, AnalysisCapabilities, AnalysisContext, DuplicateConsolidationPlan,
    InMemoryLibraryStore, LibraryStore, OnlineCapabilities, OnlineContext, PendingTagMirror,
    PlaylistFolder, SourceFingerprint, SqliteLibraryStore, StoreError, StoreResult,
    StoredSmartShuffleIndex, StoredSyncedLyrics, StoredTagMirrorArtwork, StoredWaveform,
    SyncDevice, SyncDeviceId, SyncManifestEntry, SyncedLyrics, TagMirrorArtwork, TrackAnalysis,
    TrackColumnLayout, TrackColumnLayoutScope,
};
use sustain_metadata::{
    InitialTags, LibraryScan, MetadataChange, MetadataError, MetadataResult, ScannedTrack,
};
use sustain_playback::NullPlaybackService;
use sustain_settings::{SettingsError, SettingsResult, SettingsStore};

use super::{
    ApplicationRuntime, ApplicationRuntimeError, LibraryConsolidationSummary, LibraryScanSummary,
    MetadataService, NotificationCategory, NotificationSeverity, PlaybackQueueEntryKind,
    PlaybackQueueRequest, PlaybackQueueSource, normalize_query, run_library_consolidation_task,
    run_library_import_task, run_library_scan_task,
};
use crate::{
    file_presence::{FilePresence, probe_file_presence, probe_path_entry_presence},
    managed_library::{
        ManagedLibraryFilesystemError, ManagedLibraryFilesystemValidator, file_ops::FileIdentity,
    },
};

#[test]
fn runtime_starts_with_default_settings() {
    let runtime = ApplicationRuntime::new();

    assert_eq!(runtime.settings().library_path(), None);
}

#[test]
fn runtime_accepts_settings_command() {
    let mut runtime = ApplicationRuntime::new();

    let settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(settings.clone())),
        Ok(())
    );

    assert_eq!(runtime.settings(), &settings);
}

#[test]
fn device_sync_manifest_save_failure_surfaces_error_instead_of_success() {
    let backing: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let store = Arc::new(crate::test_store::FaultyStore::new(backing));
    store.set_fail_device_manifest(true);
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");
    let device_id = SyncDeviceId::new("device-id").expect("device id");
    let receiver = runtime.device_sync_event_receiver();
    assert!(matches!(
        runtime
            .device_sync_scheduler
            .start_test_task(device_id, |_progress, _cancel| Ok(
                sustain_device_sync::SyncOutcome {
                    copied: 1,
                    manifest_is_authoritative: true,
                    ..Default::default()
                }
            ),),
        crate::DeviceSyncStartOutcome::Started(_)
    ));

    let event = receiver.recv_blocking().expect("device-sync completion");
    runtime.apply_device_sync_event(event);

    assert_eq!(store.device_manifest_calls(), 1);
    let notification = runtime
        .notifications()
        .current_ephemeral()
        .expect("manifest failure notification");
    assert_eq!(notification.category, NotificationCategory::DeviceSync);
    assert_eq!(notification.severity, NotificationSeverity::Error);
    assert!(notification.body.contains("could not save"));
}

#[test]
fn device_sync_preparation_cancellation_preserves_saved_manifest() {
    let backing: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let store = Arc::new(crate::test_store::FaultyStore::new(backing));
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");
    let device_id = SyncDeviceId::new("device-id").expect("device id");
    let receiver = runtime.device_sync_event_receiver();
    assert!(matches!(
        runtime
            .device_sync_scheduler
            .start_test_task(device_id, |_progress, _cancel| Ok(
                sustain_device_sync::SyncOutcome {
                    cancelled: true,
                    ..Default::default()
                }
            ),),
        crate::DeviceSyncStartOutcome::Started(_)
    ));

    let event = receiver.recv_blocking().expect("device-sync completion");
    runtime.apply_device_sync_event(event);

    assert_eq!(store.device_manifest_calls(), 0);
    let notification = runtime
        .notifications()
        .current_ephemeral()
        .expect("cancellation notification");
    assert_eq!(notification.category, NotificationCategory::DeviceSync);
    assert_eq!(notification.severity, NotificationSeverity::Warning);
    assert!(notification.body.contains("Sync stopped"));
}

#[test]
fn stale_device_sync_completion_is_ignored() {
    let backing: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let store = Arc::new(crate::test_store::FaultyStore::new(backing));
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");
    let device_id = SyncDeviceId::new("device-id").expect("device id");
    let receiver = runtime.device_sync_event_receiver();
    let started = runtime
        .device_sync_scheduler
        .start_test_task(device_id, |_progress, _cancel| {
            Ok(sustain_device_sync::SyncOutcome {
                manifest_is_authoritative: true,
                ..Default::default()
            })
        });
    assert!(matches!(started, crate::DeviceSyncStartOutcome::Started(_)));
    let crate::DeviceSyncStartOutcome::Started(run_id) = started else {
        return;
    };

    runtime.apply_device_sync_event(receiver.recv_blocking().expect("completion"));
    assert_eq!(store.device_manifest_calls(), 1);
    runtime.apply_device_sync_event(crate::DeviceSyncEvent::Finished {
        run_id,
        completion: crate::DeviceSyncCompletion {
            device_id: SyncDeviceId::new("device-id").expect("device id"),
            result: Ok(sustain_device_sync::SyncOutcome::default()),
        },
    });
    assert_eq!(
        store.device_manifest_calls(),
        1,
        "a stale completion must not persist state twice"
    );
}

#[test]
fn runtime_handles_every_application_command_intentionally() {
    let track_id = track_id(1);
    let playlist_id = playlist_id(1);
    let rating = Rating::new(4).expect("valid test rating");
    let metadata_change = MetadataChange::default();
    let settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));

    let cases = vec![
        (
            ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
                track_id,
                queue: PlaybackQueueRequest::Library,
            }),
            Err(ApplicationRuntimeError::TrackUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::PlayPreviousTrack),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::PlayQueueTrack(track_id)),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::CycleShuffleMode),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::SetShuffleMode(ShuffleMode::Off)),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::ToggleRepeat),
            Ok(()),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::Pause),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::Resume),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::TogglePlayPause),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::Stop),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::Seek(std::time::Duration::ZERO)),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::Playback(PlaybackCommand::SetVolume(VolumePercent::from_clamped(
                50,
            ))),
            Err(ApplicationRuntimeError::PlaybackServiceUnavailable),
        ),
        (
            ApplicationCommand::SetRating { track_id, rating },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::CreatePlaylist {
                name: "Favorites".to_owned(),
                parent_folder_id: None,
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::RenamePlaylist {
                playlist_id,
                name: "Renamed".to_owned(),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::DeletePlaylist { playlist_id },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::AddTracksToPlaylist {
                playlist_id,
                track_ids: vec![track_id],
            },
            Err(ApplicationRuntimeError::TrackUnavailable),
        ),
        (
            ApplicationCommand::RemoveTracksFromPlaylist {
                playlist_id,
                track_ids: vec![track_id],
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::MovePlaylistEntries {
                playlist_id,
                track_ids: vec![track_id],
                new_position: 2,
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::CreatePlaylistFolder {
                name: "Mixes".to_owned(),
                parent_folder_id: None,
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::RenamePlaylistFolder {
                folder_id: folder_id(1),
                name: "Renamed".to_owned(),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::DeletePlaylistFolder {
                folder_id: folder_id(1),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::CreateSmartPlaylist {
                name: "Recent".to_owned(),
                parent_folder_id: None,
                rules: test_rule_set(),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::UpdateSmartPlaylist {
                smart_playlist_id: smart_id(1),
                name: "Updated".to_owned(),
                rules: test_rule_set(),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::DeleteSmartPlaylist {
                smart_playlist_id: smart_id(1),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::MovePlaylistItem {
                item: PlaylistItem::Playlist(playlist_id),
                target_parent_folder_id: None,
                position: 0,
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::UpdateMetadata {
                track_id,
                change: Box::new(metadata_change.clone()),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::RemoveTrackFromLibrary { track_id },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (
            ApplicationCommand::MoveTrackToTrash { track_id },
            Err(ApplicationRuntimeError::TrackUnavailable),
        ),
        (
            ApplicationCommand::RelocateMissingTrack {
                track_id,
                replacement_path: PathBuf::from("/music/replacement.flac"),
            },
            Err(ApplicationRuntimeError::TrackUnavailable),
        ),
        (
            ApplicationCommand::FetchArtwork { track_id },
            Err(ApplicationRuntimeError::TrackUnavailable),
        ),
        (
            ApplicationCommand::AddExternalLibraryItems {
                paths: vec![PathBuf::from("/music/track.flac")],
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
        (ApplicationCommand::UpdateSettings(settings.clone()), Ok(())),
        (
            ApplicationCommand::ScanLibrary {
                library_path: PathBuf::from("/music"),
            },
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
        ),
    ];

    for (command, expected_result) in cases {
        let mut runtime = ApplicationRuntime::new();

        assert_eq!(runtime.handle_command(command), expected_result);
    }
}

#[test]
fn runtime_records_manual_scan_request() {
    let mut runtime = ApplicationRuntime::new();
    let library_path = PathBuf::from("/music");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: library_path.clone()
        }),
        Err(ApplicationRuntimeError::LibraryServicesUnavailable)
    );

    assert_eq!(
        runtime.last_scan_library_path(),
        Some(library_path.as_path())
    );
}

#[test]
fn runtime_scans_library_with_services() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.mp3");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let metadata_service = Arc::new(TestMetadataService);
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, metadata_service)
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: root.clone()
        }),
        Ok(())
    );

    assert_eq!(runtime.library_tracks().len(), 1);
    assert_eq!(
        runtime
            .last_scan_summary()
            .map(|summary| summary.added_tracks),
        Some(1)
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn deferred_library_hydration_loads_after_start_and_gates_mutations_until_publication() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let track = test_track(track_id(1), "loaded-later.flac");
    assert_eq!(store.save_track(track.clone()), Ok(()));
    let mut runtime = ApplicationRuntime::new();
    runtime
        .set_library_services_deferred_hydration(store, Arc::new(TestMetadataService))
        .expect("install deferred library services");

    assert_eq!(
        runtime.library_hydration_state(),
        crate::LibraryHydrationState::Pending
    );
    assert!(runtime.library_tracks().is_empty());
    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylist {
            name: "Too early".to_owned(),
            parent_folder_id: None,
        }),
        Err(ApplicationRuntimeError::LibraryHydrationPending)
    );

    let receiver = runtime.library_hydration_result_receiver();
    assert!(runtime.start_library_hydration());
    assert_eq!(
        runtime.library_hydration_state(),
        crate::LibraryHydrationState::Loading
    );
    assert_eq!(
        runtime
            .notifications()
            .current_persistent()
            .map(|n| n.category),
        Some(NotificationCategory::LibraryHydration)
    );

    let result = receiver.recv_blocking().expect("hydration result");
    assert!(runtime.apply_library_hydration_result(result));
    assert_eq!(
        runtime.library_hydration_state(),
        crate::LibraryHydrationState::Publishing
    );
    assert_eq!(runtime.library_tracks(), std::slice::from_ref(&track));
    assert!(runtime.search_matches(track.id, &normalize_query("loaded-later")));
    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylist {
            name: "Still too early".to_owned(),
            parent_folder_id: None,
        }),
        Err(ApplicationRuntimeError::LibraryHydrationPending)
    );

    runtime.finish_library_hydration_publication();
    assert_eq!(
        runtime.library_hydration_state(),
        crate::LibraryHydrationState::Ready
    );
    assert!(runtime.notifications().current_persistent().is_none());
    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylist {
            name: "Ready".to_owned(),
            parent_folder_id: None,
        }),
        Ok(())
    );
}

#[test]
fn deferred_library_hydration_failure_surfaces_persistent_error() {
    use std::sync::atomic::Ordering;

    let counts = Arc::new(StoreCallCounts::default());
    let store: Arc<dyn LibraryStore> = Arc::new(CallCountingLibraryStore {
        inner: InMemoryLibraryStore::new(),
        counts: counts.clone(),
        statistics_failures_remaining: std::sync::atomic::AtomicUsize::new(0),
        tracks_failures_remaining: std::sync::atomic::AtomicUsize::new(1),
    });
    let mut runtime = ApplicationRuntime::new();
    runtime
        .set_library_services_deferred_hydration(store, Arc::new(TestMetadataService))
        .expect("install deferred library services");
    assert_eq!(
        counts.tracks.load(Ordering::SeqCst),
        0,
        "service installation must not decode tracks before first idle",
    );

    let receiver = runtime.library_hydration_result_receiver();
    assert!(runtime.start_library_hydration());
    let result = receiver.recv_blocking().expect("hydration result");
    assert!(!runtime.apply_library_hydration_result(result));

    assert_eq!(
        runtime.library_hydration_state(),
        crate::LibraryHydrationState::Failed
    );
    let notification = runtime
        .notifications()
        .current_persistent()
        .expect("failure notification");
    assert_eq!(
        notification.category,
        NotificationCategory::LibraryHydration
    );
    assert_eq!(notification.severity, NotificationSeverity::Error);
}

#[test]
fn cancelled_scan_preserves_existing_tracks_without_marking_them_missing() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");

    let store = Arc::new(InMemoryLibraryStore::new());
    let existing_track = test_track(track_id(1), "leftover.mp3");
    assert_eq!(store.save_track(existing_track.clone()), Ok(()));

    let metadata_service = Arc::new(TestMetadataService);
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, metadata_service)
        .expect("library services initialize");

    // Trip the cancellation flag *before* the worker observes it.
    // That is the worst case for the missing-track sweep: the
    // walker aborts on its first iteration without indexing the
    // empty library, and we must not interpret the unwalked
    // existing track as missing.
    let task = runtime
        .prepare_library_scan(root.clone())
        .expect("prepare scan");
    runtime.request_library_scan_cancellation();
    let result = run_library_scan_task(task).expect("scan finishes cleanly");
    runtime.apply_library_scan_result(result);

    let summary = runtime.last_scan_summary().expect("scan summary present");
    assert!(summary.cancelled, "cancellation flag must propagate");
    assert_eq!(
        summary.missing_tracks, 0,
        "a partial scan must not mark unwalked tracks as missing"
    );
    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1, "the pre-existing track must be preserved");
    assert_eq!(tracks[0].id, existing_track.id);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn incomplete_scan_keeps_unvisited_rows_unchanged_but_applies_safe_rows() {
    let known = test_track(track_id(1), "known.mp3");
    let unvisited = test_track(track_id(2), "unvisited.mp3");
    let scan = LibraryScan {
        tracks: vec![
            test_scanned_track("known.mp3"),
            test_scanned_track("new.mp3"),
        ],
        complete_for_missing_reconciliation: false,
        ..LibraryScan::default()
    };

    let result = crate::library_scan::reconcile_library_scan_with_probe(
        Path::new("/library"),
        vec![known, unvisited.clone()],
        scan,
        |_| panic!("partial scans must not probe unvisited rows"),
    )
    .expect("reconcile incomplete scan");

    assert_eq!(result.summary.added_tracks, 1);
    assert_eq!(result.summary.updated_tracks, 1);
    assert_eq!(result.summary.missing_tracks, 0);
    assert!(result.summary.missing_reconciliation_skipped);
    assert_eq!(
        result
            .tracks
            .iter()
            .find(|track| track.id == unvisited.id)
            .expect("unvisited track survives"),
        &unvisited
    );
}

#[test]
fn reconciliation_probe_failure_preserves_every_unvisited_row() {
    let first = test_track(track_id(1), "first.mp3");
    let second = test_track(track_id(2), "second.mp3");
    let result = crate::library_scan::reconcile_library_scan_with_probe(
        Path::new("/library"),
        vec![first.clone(), second.clone()],
        LibraryScan::default(),
        |path| {
            if path.ends_with("first.mp3") {
                FilePresence::Absent
            } else {
                FilePresence::ProbeFailed
            }
        },
    )
    .expect("reconcile probe failure");

    assert!(result.summary.missing_reconciliation_skipped);
    assert_eq!(result.summary.missing_tracks, 0);
    assert_eq!(result.tracks, vec![first, second]);
}

#[test]
fn reconcile_keeps_unchanged_rows_present_without_reparsing() {
    // Files the scanner skipped because their fingerprint matched: the row
    // is kept verbatim, a previously-Missing one is restored to Available,
    // they count as unchanged (not added/updated/missing), and they are
    // never probed (#71).
    let mut present = test_track(track_id(1), "present.mp3");
    present.statistics.play_count = 9; // a library-owned value must survive untouched
    let mut reappeared = test_track(track_id(2), "reappeared.mp3");
    reappeared.location = missing_track_location("reappeared.mp3");

    let scan = LibraryScan {
        unchanged: vec![
            relative_path("present.mp3"),
            relative_path("reappeared.mp3"),
        ],
        ..LibraryScan::default()
    };

    let result = crate::library_scan::reconcile_library_scan_with_probe(
        Path::new("/library"),
        vec![present.clone(), reappeared.clone()],
        scan,
        |_| panic!("unchanged files are already known present; must not be probed"),
    )
    .expect("reconcile unchanged scan");

    assert_eq!(result.summary.unchanged_tracks, 2);
    assert_eq!(result.summary.added_tracks, 0);
    assert_eq!(result.summary.updated_tracks, 0);
    assert_eq!(result.summary.missing_tracks, 0);

    let present_row = result
        .tracks
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("present row survives");
    assert_eq!(present_row, &present, "unchanged row kept verbatim");

    let reappeared_row = result
        .tracks
        .iter()
        .find(|track| track.id == track_id(2))
        .expect("reappeared row survives");
    assert!(
        !reappeared_row.location.is_missing(),
        "a reappeared unchanged file is restored to Available"
    );
}

#[test]
fn rescan_with_no_on_disk_changes_reparses_nothing() {
    // End-to-end seam for #71: a first scan imports + records each file's
    // mtime; a second scan with nothing changed on disk must re-parse zero
    // files (the cold-scan cost is not paid again) yet still report every
    // track present.
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct CountingMetadataService {
        parses: AtomicUsize,
    }
    impl MetadataService for CountingMetadataService {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            self.parses.fetch_add(1, Ordering::SeqCst);
            Ok(InitialTags {
                metadata: TrackMetadata {
                    title: Some("Track".to_owned()),
                    ..TrackMetadata::default()
                },
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            })
        }
        fn write_metadata(&self, _: &Path, _: MetadataChange) -> MetadataResult<()> {
            Ok(())
        }
        fn write_rating(&self, _: &Path, _: Rating) -> MetadataResult<()> {
            Ok(())
        }
        fn read_artwork(&self, _: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }
        fn write_artwork(&self, _: &Path, _: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.mp3"), b"audio a").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio b").expect("write b");

    let store = Arc::new(SqliteLibraryStore::open_in_memory().expect("store"));
    let metadata = Arc::new(CountingMetadataService::default());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), metadata.clone())
        .expect("library services initialize");

    // First scan: both files are new, so both are parsed.
    let task = runtime
        .prepare_library_scan(root.clone())
        .expect("prepare scan 1");
    let result = run_library_scan_task(task).expect("run scan 1");
    runtime.apply_library_scan_result(result);
    assert_eq!(metadata.parses.load(Ordering::SeqCst), 2);
    assert_eq!(
        runtime.last_scan_summary().expect("summary 1").added_tracks,
        2
    );

    // Second scan, nothing changed on disk: zero re-parses, both present.
    let task = runtime
        .prepare_library_scan(root.clone())
        .expect("prepare scan 2");
    let result = run_library_scan_task(task).expect("run scan 2");
    runtime.apply_library_scan_result(result);
    assert_eq!(
        metadata.parses.load(Ordering::SeqCst),
        2,
        "an unchanged rescan must not re-parse any file"
    );
    let summary = runtime.last_scan_summary().expect("summary 2").clone();
    assert_eq!(summary.unchanged_tracks, 2);
    assert_eq!(summary.added_tracks, 0);
    assert_eq!(summary.updated_tracks, 0);
    assert_eq!(runtime.library_tracks().len(), 2);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_scan_preserves_existing_track_identity_for_known_location() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.mp3");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(7);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut existing_track = test_track(track_id, "track.mp3");
    existing_track.statistics.play_count = 12;
    assert_eq!(store.save_track(existing_track), Ok(()));

    let metadata_service = Arc::new(TestMetadataService);
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, metadata_service)
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: root.clone()
        }),
        Ok(())
    );

    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, track_id);
    assert_eq!(tracks[0].statistics.play_count, 12);
    assert_eq!(
        runtime.last_scan_summary(),
        Some(&LibraryScanSummary {
            added_tracks: 0,
            updated_tracks: 1,
            unchanged_tracks: 0,
            missing_tracks: 0,
            skipped_unsupported_files: 0,
            failed_files: 0,
            missing_reconciliation_skipped: false,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn scan_snapshot_cannot_clobber_newer_statistics_in_store_or_runtime() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("track.mp3"), b"not real audio").expect("write fake track");

    let track_id = track_id(8);
    let store = Arc::new(SqliteLibraryStore::open_in_memory().expect("store"));
    let existing = test_track(track_id, "track.mp3");
    store.save_track(existing).expect("seed track");
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    // `prepare` is the deterministic barrier: the task now owns its stale
    // pre-scan snapshot. Commit newer playback statistics before allowing
    // that task to reconcile and publish its result.
    let task = runtime
        .prepare_library_scan(root.clone())
        .expect("prepare scan");
    let newer_statistics = PlayStatistics {
        play_count: 41,
        ..PlayStatistics::default()
    };
    store
        .update_track_statistics(track_id, &newer_statistics)
        .expect("commit newer statistics");
    runtime.apply_track_updated(track_id);

    let result = run_library_scan_task(task).expect("run scan");
    runtime.apply_library_scan_result(result);

    assert_eq!(
        store
            .track(track_id)
            .expect("load stored track")
            .expect("stored track")
            .statistics
            .play_count,
        41
    );
    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 41);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_scan_preserves_existing_track_identity_after_library_root_changes() {
    let root = unique_test_directory();
    let old_root = root.join("old-library");
    let new_root = root.join("new-library");
    let relative_path = "Artist/Album/track.mp3";
    let track_path = new_root.join(relative_path);
    std::fs::create_dir_all(track_path.parent().expect("test path has parent"))
        .expect("create test album directory");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(11);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut existing_track = test_track(track_id, relative_path);
    existing_track.statistics.play_count = 22;
    assert_eq!(store.save_track(existing_track), Ok(()));

    let settings_store = Box::new(TestSettingsStore::new(UserSettings::with_library_path(
        Some(old_root),
    )));
    let mut runtime =
        ApplicationRuntime::with_settings_store(settings_store).expect("load settings");
    runtime = runtime
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    // Startup no longer re-polls every track's on-disk existence (iTunes-
    // like lazy availability), so the loaded track keeps the persisted
    // Available flag here. The scan below is what reconciles availability
    // against the new library root.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(
            UserSettings::with_library_path(Some(new_root.clone()))
        )),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: new_root.clone()
        }),
        Ok(())
    );

    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, track_id);
    assert_eq!(tracks[0].statistics.play_count, 22);
    assert!(!tracks[0].location.is_missing());
    assert_eq!(
        runtime.last_scan_summary(),
        Some(&LibraryScanSummary {
            added_tracks: 0,
            updated_tracks: 1,
            unchanged_tracks: 0,
            missing_tracks: 0,
            skipped_unsupported_files: 0,
            failed_files: 0,
            missing_reconciliation_skipped: false,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn managed_import_copies_external_files_into_planned_library_path() {
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let source_path = external_root.join("source.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write external source");
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![source_path.clone()]
        }),
        Ok(())
    );

    let expected_destination = library_root
        .join("Unknown Artist")
        .join("Unknown Album")
        .join("Track.flac");
    assert_eq!(
        std::fs::read(&expected_destination).expect("copied file exists"),
        b"audio bytes"
    );
    assert_eq!(
        std::fs::read(&source_path).expect("source remains untouched"),
        b"audio bytes"
    );
    assert_eq!(
        runtime.last_library_import_summary(),
        Some(&super::LibraryImportSummary {
            discovered_files: 1,
            imported_tracks: 1,
            duplicate_files: 0,
            cancelled: false,
        })
    );

    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(
        tracks[0].location.relative_path.as_path(),
        std::path::Path::new("Unknown Artist/Unknown Album/Track.flac")
    );
    assert_eq!(tracks[0].rating, Rating::new(3).expect("valid rating"));
    assert_eq!(store.tracks().expect("store tracks"), tracks);

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn managed_import_revalidates_root_before_worker_filesystem_mutation() {
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let source_path = external_root.join("source.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write external source");
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let validator = ManagedLibraryFilesystemValidator::fail_after(
        1,
        ManagedLibraryFilesystemError::HardLinkFailed,
    );
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_managed_library_filesystem_validator(validator)
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    let task = runtime
        .prepare_library_import(vec![source_path.clone()])
        .expect("preparation validates the first root state");
    let error = run_library_import_task(task).expect_err("worker revalidation rejects remount");
    assert_eq!(
        error,
        ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(
            ManagedLibraryFilesystemError::HardLinkFailed
        )
    );
    runtime.fail_library_import(error);

    assert_eq!(
        std::fs::read(&source_path).expect("source intact"),
        b"audio bytes"
    );
    assert!(runtime.library_tracks().is_empty());
    assert_eq!(store.tracks(), Ok(Vec::new()));
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::ManagedLibraryFilesystem
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn managed_import_skips_duplicate_content_hashes_in_same_batch() {
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let first_source = external_root.join("first.flac");
    let second_source = external_root.join("second.flac");
    std::fs::write(&first_source, b"same audio").expect("write first source");
    std::fs::write(&second_source, b"same audio").expect("write second source");
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store, Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![first_source, second_source]
        }),
        Ok(())
    );

    assert_eq!(runtime.library_tracks().len(), 1);
    assert_eq!(
        runtime.last_library_import_summary(),
        Some(&super::LibraryImportSummary {
            discovered_files: 2,
            imported_tracks: 1,
            duplicate_files: 1,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn managed_import_lazily_hashes_same_size_existing_tracks_for_duplicates() {
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let existing_path = library_root.join("existing.flac");
    let source_path = external_root.join("source.flac");
    std::fs::write(&existing_path, b"same audio").expect("write existing track");
    std::fs::write(&source_path, b"same audio").expect("write external source");

    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let store = Arc::new(InMemoryLibraryStore::new());
    let existing_track = test_track(track_id(7), "existing.flac");
    assert_eq!(store.save_track(existing_track.clone()), Ok(()));
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store, Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![source_path]
        }),
        Ok(())
    );

    assert_eq!(runtime.library_tracks(), &[existing_track]);
    assert_eq!(
        runtime.last_library_import_summary(),
        Some(&super::LibraryImportSummary {
            discovered_files: 1,
            imported_tracks: 0,
            duplicate_files: 1,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn managed_import_skips_strict_exact_duplicate_already_on_disk_without_a_row() {
    // The disk is ground truth even when the database does not know a
    // file. After dropping the database, the library folder still holds
    // every copied file; importing one of those again before a scan
    // re-indexes it used to find the canonical name free of any row, bump
    // to a numbered name, and write a byte-identical copy. With no row to
    // dedup against, the disk-anchored guard in plan_destination catches
    // the identical occupant and skips it.
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    let canonical_dir = library_root.join("Unknown Artist").join("Unknown Album");
    std::fs::create_dir_all(&canonical_dir).expect("create canonical dir");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let canonical_path = canonical_dir.join("Track.flac");
    let source_path = external_root.join("source.flac");
    std::fs::write(&canonical_path, b"same audio").expect("write orphan library file");
    std::fs::write(&source_path, b"same audio").expect("write external source");

    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store, Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![source_path]
        }),
        Ok(())
    );

    assert!(
        runtime.library_tracks().is_empty(),
        "an already-present file must not be imported as a new row"
    );
    assert!(
        !canonical_dir.join("Track 2.flac").exists(),
        "import must not write a byte-identical numbered copy"
    );
    assert_eq!(
        runtime.last_library_import_summary(),
        Some(&super::LibraryImportSummary {
            discovered_files: 1,
            imported_tracks: 0,
            duplicate_files: 1,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn unmanaged_external_import_indexes_library_files_in_place() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let source_path = library_root.join("source.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write source");
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![source_path.clone()]
        }),
        Ok(())
    );

    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(
        tracks[0].location.relative_path.as_path(),
        Path::new("source.flac")
    );
    assert_eq!(store.tracks().expect("store tracks"), tracks);
    assert_eq!(
        runtime.last_library_import_summary(),
        Some(&super::LibraryImportSummary {
            discovered_files: 1,
            imported_tracks: 1,
            duplicate_files: 0,
            cancelled: false,
        })
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn unmanaged_external_import_rejects_files_outside_library_path() {
    let library_root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let source_path = external_root.join("source.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write source");
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddExternalLibraryItems {
            paths: vec![source_path]
        }),
        Err(ApplicationRuntimeError::LibraryImportFailed)
    );
    assert_eq!(runtime.library_tracks(), &[]);
    assert_eq!(store.tracks(), Ok(Vec::new()));

    std::fs::remove_dir_all(library_root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn managed_consolidation_moves_existing_tracks_to_planned_paths() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let source_path = library_root.join("Loose/Album/loose.flac");
    std::fs::create_dir_all(source_path.parent().expect("source parent"))
        .expect("create source parent");
    std::fs::write(&source_path, b"audio bytes").expect("write existing file");

    let track_id = track_id(21);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "Loose/Album/loose.flac");
    track.metadata.artist = Some("Artist".to_owned());
    track.metadata.album = Some("Album".to_owned());
    track.metadata.title = Some("Song".to_owned());
    track.metadata.track_number = Some(1);
    track.rating = Rating::new(5).expect("valid rating");
    track.statistics.play_count = 9;
    assert_eq!(store.save_track(track), Ok(()));

    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    let task = runtime
        .prepare_library_consolidation()
        .expect("prepare consolidation");
    let result = run_library_consolidation_task(task).expect("run consolidation");
    runtime.apply_library_consolidation_result(result);

    let destination_path = library_root.join("Artist/Album/01 Song.flac");
    assert!(!source_path.exists());
    assert_eq!(
        std::fs::read(&destination_path).expect("destination exists"),
        b"audio bytes"
    );
    assert_eq!(
        runtime.last_library_consolidation_summary(),
        Some(&LibraryConsolidationSummary {
            planned_tracks: 1,
            moved_tracks: 1,
            already_organized_tracks: 0,
            missing_tracks: 0,
            empty_directory_cleanup_failed: false,
            cancelled: false,
        })
    );
    assert!(!library_root.join("Loose").exists());

    let runtime_track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id)
        .expect("runtime track exists");
    assert_eq!(
        runtime_track.location.relative_path.as_path(),
        Path::new("Artist/Album/01 Song.flac")
    );
    assert_eq!(runtime_track.rating, Rating::new(5).expect("valid rating"));
    assert_eq!(runtime_track.statistics.play_count, 9);
    assert_eq!(
        store
            .track(track_id)
            .expect("load stored track")
            .map(|track| track.location.relative_path.to_path_buf()),
        Some(PathBuf::from("Artist/Album/01 Song.flac"))
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn auto_resume_preparation_revalidates_already_configured_managed_root() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_managed_library_filesystem_validator(
                ManagedLibraryFilesystemValidator::rejecting(
                    ManagedLibraryFilesystemError::HardLinkFailed,
                ),
            )
            .with_library_services(store, Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert!(matches!(
        runtime.prepare_library_consolidation(),
        Err(
            ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(
                ManagedLibraryFilesystemError::HardLinkFailed
            )
        )
    ));
    assert!(!runtime.background_task_status().is_running());
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::ManagedLibraryFilesystem
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn managed_consolidation_revalidates_root_before_worker_filesystem_mutation() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let source_path = library_root.join("loose.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write existing file");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id(25), "loose.flac");
    track.metadata.title = Some("Song".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let validator = ManagedLibraryFilesystemValidator::fail_after(
        1,
        ManagedLibraryFilesystemError::HardLinkFailed,
    );
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_managed_library_filesystem_validator(validator)
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    let task = runtime
        .prepare_library_consolidation()
        .expect("preparation validates the first root state");
    let error =
        run_library_consolidation_task(task).expect_err("worker revalidation rejects remount");
    assert_eq!(
        error,
        ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(
            ManagedLibraryFilesystemError::HardLinkFailed
        )
    );
    runtime.fail_library_consolidation(error);

    assert_eq!(
        std::fs::read(&source_path).expect("source intact"),
        b"audio bytes"
    );
    assert!(
        !library_root
            .join("Unknown Artist/Unknown Album/Song.flac")
            .exists()
    );
    assert_eq!(
        store
            .track(track_id(25))
            .expect("stored track")
            .expect("track exists")
            .location
            .relative_path
            .as_path(),
        Path::new("loose.flac")
    );
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::ManagedLibraryFilesystem
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn consolidation_snapshot_cannot_clobber_newer_statistics_in_store_or_runtime() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    std::fs::write(library_root.join("loose.flac"), b"audio bytes").expect("write source");

    let track_id = track_id(24);
    let store = Arc::new(SqliteLibraryStore::open_in_memory().expect("store"));
    let mut track = test_track(track_id, "loose.flac");
    track.metadata.artist = Some("Artist".to_owned());
    track.metadata.album = Some("Album".to_owned());
    track.metadata.title = Some("Song".to_owned());
    track.metadata.track_number = Some(1);
    store.save_track(track).expect("seed track");

    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    // The prepared task carries the stale location-planning snapshot. Commit
    // newer listening statistics before resuming the filesystem move.
    let task = runtime
        .prepare_library_consolidation()
        .expect("prepare consolidation");
    let newer_statistics = PlayStatistics {
        play_count: 73,
        ..PlayStatistics::default()
    };
    store
        .update_track_statistics(track_id, &newer_statistics)
        .expect("commit newer statistics");
    runtime.apply_track_updated(track_id);

    let result = run_library_consolidation_task(task).expect("run consolidation");
    runtime.apply_library_consolidation_result(result);

    let stored = store
        .track(track_id)
        .expect("load stored track")
        .expect("stored track");
    assert_eq!(stored.statistics.play_count, 73);
    assert_eq!(
        stored.location.relative_path.as_path(),
        Path::new("Artist/Album/01 Song.flac")
    );
    let runtime_track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id)
        .expect("runtime track");
    assert_eq!(runtime_track.statistics.play_count, 73);
    assert_eq!(
        runtime_track.location.relative_path.as_path(),
        Path::new("Artist/Album/01 Song.flac")
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn disabling_managed_mode_requests_consolidation_cancellation() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let source_path = library_root.join("loose.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write existing file");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id(22), "loose.flac");
    track.metadata.title = Some("Song".to_owned());
    assert_eq!(store.save_track(track), Ok(()));

    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    let task = runtime
        .prepare_library_consolidation()
        .expect("prepare consolidation");
    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: library_root.clone()
        }),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    );

    let mut updated_settings = runtime.settings().clone();
    updated_settings.library.management_mode = LibraryManagementMode::ReferenceFilesInPlace;
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(updated_settings)),
        Ok(())
    );

    let result = run_library_consolidation_task(task).expect("run cancelled consolidation");
    runtime.apply_library_consolidation_result(result);

    assert!(source_path.exists());
    assert!(
        !library_root
            .join("Unknown Artist/Unknown Album/Song.flac")
            .exists()
    );
    assert_eq!(
        runtime.last_library_consolidation_summary(),
        Some(&LibraryConsolidationSummary {
            planned_tracks: 1,
            moved_tracks: 0,
            already_organized_tracks: 0,
            missing_tracks: 0,
            empty_directory_cleanup_failed: false,
            cancelled: true,
        })
    );
    assert_eq!(
        runtime.settings().library.management_mode,
        LibraryManagementMode::ReferenceFilesInPlace
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn consolidation_journal_recovery_retargets_moved_tracks_on_startup() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(library_root.join("Artist/Album"))
        .expect("create destination directory");
    std::fs::create_dir_all(library_root.join("Loose/Album")).expect("create old source directory");
    let destination_path = library_root.join("Artist/Album/01 Song.flac");
    std::fs::write(&destination_path, b"audio bytes").expect("write moved file");
    let destination_metadata = std::fs::metadata(&destination_path).expect("destination metadata");
    std::fs::write(
        library_root.join(".sustain-consolidation-journal"),
        format!(
            "# sustain managed library consolidation journal v3\nmove\t23\t{}\t{}\tlocation\t{}\t{}\n",
            destination_metadata.dev(),
            destination_metadata.ino(),
            hex_path("Loose/Album/loose.flac"),
            hex_path("Artist/Album/01 Song.flac")
        ),
    )
    .expect("write journal");

    let store = Arc::new(InMemoryLibraryStore::new());
    let track_id = track_id(23);
    assert_eq!(
        store.save_track(test_track(track_id, "Loose/Album/loose.flac")),
        Ok(())
    );
    let mut settings = UserSettings::with_library_path(Some(library_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;

    let runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.library_tracks()[0].location.relative_path.as_path(),
        Path::new("Artist/Album/01 Song.flac")
    );
    assert!(!library_root.join(".sustain-consolidation-journal").exists());
    assert!(!library_root.join("Loose").exists());
    assert_eq!(
        store
            .track(track_id)
            .expect("load recovered track")
            .map(|track| track.location.relative_path.to_path_buf()),
        Some(PathBuf::from("Artist/Album/01 Song.flac"))
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn reference_mode_accepts_a_root_that_cannot_support_managed_moves() {
    let first_root = unique_test_directory();
    let second_root = unique_test_directory();
    std::fs::create_dir_all(&first_root).expect("create first root");
    std::fs::create_dir_all(&second_root).expect("create second root");
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(first_root.clone())),
    )))
    .expect("load settings")
    .with_managed_library_filesystem_validator(ManagedLibraryFilesystemValidator::rejecting(
        ManagedLibraryFilesystemError::HardLinkFailed,
    ));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(
            UserSettings::with_library_path(Some(second_root.clone()))
        )),
        Ok(())
    );
    assert_eq!(
        runtime.settings().library_path(),
        Some(second_root.as_path())
    );
    assert!(runtime.notifications().persistent_stack().is_empty());

    std::fs::remove_dir_all(first_root).expect("remove first root");
    std::fs::remove_dir_all(second_root).expect("remove second root");
}

#[test]
fn enabling_managed_mode_rejects_incompatible_root_before_settings_save() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create root");
    let settings = UserSettings::with_library_path(Some(root.clone()));
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings.clone())))
            .expect("load settings")
            .with_managed_library_filesystem_validator(
                ManagedLibraryFilesystemValidator::rejecting(
                    ManagedLibraryFilesystemError::HardLinkFailed,
                ),
            );
    let mut updated = settings.clone();
    updated.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(updated)),
        Err(
            ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(
                ManagedLibraryFilesystemError::HardLinkFailed
            )
        )
    );
    assert_eq!(runtime.settings(), &settings);
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::ManagedLibraryFilesystem
    );

    std::fs::remove_dir_all(root).expect("remove root");
}

#[test]
fn managed_path_change_rejects_incompatible_root_before_persist_or_reconciliation() {
    let old_root = unique_test_directory();
    let new_root = unique_test_directory();
    std::fs::create_dir_all(&old_root).expect("create old root");
    std::fs::create_dir_all(&new_root).expect("create new root");
    std::fs::write(old_root.join("track.flac"), b"audio").expect("write old track");
    let track_id = track_id(26);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));
    let mut settings = UserSettings::with_library_path(Some(old_root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings.clone())))
            .expect("load settings")
            .with_managed_library_filesystem_validator(
                ManagedLibraryFilesystemValidator::rejecting(
                    ManagedLibraryFilesystemError::HardLinkFailed,
                ),
            )
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");
    let mut updated = settings.clone();
    updated.library.path = Some(new_root.clone());

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(updated)),
        Err(
            ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(
                ManagedLibraryFilesystemError::HardLinkFailed
            )
        )
    );
    assert_eq!(runtime.settings(), &settings);
    assert!(!runtime.library_tracks()[0].location.is_missing());
    assert!(
        !store
            .track(track_id)
            .expect("stored track")
            .expect("track exists")
            .location
            .is_missing()
    );
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::ManagedLibraryFilesystem
    );

    std::fs::remove_dir_all(old_root).expect("remove old root");
    std::fs::remove_dir_all(new_root).expect("remove new root");
}

#[test]
fn update_settings_does_not_re_stat_existing_tracks_when_path_is_unchanged() {
    // UpdateSettings re-stats tracks ONLY when the user changes
    // `library.path` (see
    // `update_settings_re_stats_existing_tracks_when_library_path_changes`).
    // Every other settings mutation — management-mode toggle,
    // playback volume, anything stored on `UserSettings` — must
    // stay free of stat() syscalls so toggling a Preferences
    // checkbox on a 10k library does not freeze the UI thread.
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create test library");
    let track_path = library_root.join("track.flac");
    std::fs::write(&track_path, b"audio bytes").expect("write track");

    let track_id = track_id(7);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert!(!runtime.library_tracks()[0].location.is_missing());

    // Remove the file behind the runtime's back, then dispatch
    // UpdateSettings. The track must keep its persisted
    // Available flag — UpdateSettings has no business
    // discovering missing files.
    std::fs::remove_file(&track_path).expect("remove track from disk");
    let settings = runtime.settings().clone();
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(settings)),
        Ok(())
    );
    assert!(!runtime.library_tracks()[0].location.is_missing());

    std::fs::remove_dir_all(library_root).expect("remove test library");
}

#[test]
fn update_settings_re_stats_existing_tracks_when_library_path_changes() {
    // A library-path change is structural reconciliation: every
    // persisted track must be re-stat'd against the new root and
    // its availability flag flushed to SQLite, so the missing-file
    // indicator lights up the moment the user confirms the new
    // path instead of waiting for the next scan.
    let old_root = unique_test_directory();
    let new_root = unique_test_directory();
    std::fs::create_dir_all(&old_root).expect("create old library root");
    std::fs::create_dir_all(&new_root).expect("create new library root");
    std::fs::write(old_root.join("present.flac"), b"audio").expect("write present file");
    std::fs::write(new_root.join("present.flac"), b"audio").expect("mirror present file");
    // `vanished.flac` lives under the OLD root only. After the
    // path change, its persisted relative path resolves to a
    // non-existent file under `new_root`.
    std::fs::write(old_root.join("vanished.flac"), b"audio").expect("write vanished file");

    let store = Arc::new(InMemoryLibraryStore::new());
    let present_id = track_id(101);
    let vanished_id = track_id(102);
    assert_eq!(
        store.save_track(test_track(present_id, "present.flac")),
        Ok(())
    );
    assert_eq!(
        store.save_track(test_track(vanished_id, "vanished.flac")),
        Ok(())
    );

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(old_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    for track in runtime.library_tracks() {
        assert!(!track.location.is_missing());
    }

    let new_settings = UserSettings::with_library_path(Some(new_root.clone()));
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(new_settings)),
        Ok(())
    );

    let present = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == present_id)
        .expect("present track survives path change");
    let vanished = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == vanished_id)
        .expect("vanished track survives path change");
    assert!(!present.location.is_missing(), "mirrored file resolves");
    assert!(
        vanished.location.is_missing(),
        "absent file flips to Missing"
    );

    // SQLite is the source of truth — the flag must be durable
    // across a reload, not merely flipped in memory.
    let reloaded = store
        .track(vanished_id)
        .expect("reload vanished")
        .expect("vanished row exists");
    assert!(reloaded.location.is_missing());

    std::fs::remove_dir_all(old_root).expect("remove old library root");
    std::fs::remove_dir_all(new_root).expect("remove new library root");
}

#[test]
fn library_path_change_probe_failure_preserves_existing_availability() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(103);
    assert_eq!(store.save_track(test_track(id, "track.flac")), Ok(()));
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .reconcile_track_availability_after_library_path_change_with(root.clone(), |_| {
            FilePresence::ProbeFailed
        })
        .expect("preserve availability after unresolved probe");

    assert!(!runtime.library_tracks()[0].location.is_missing());
    assert!(
        !store
            .track(id)
            .expect("reload track")
            .expect("track row")
            .location
            .is_missing()
    );
    let notification = runtime
        .notifications()
        .current_ephemeral()
        .expect("path-change warning");
    assert_eq!(notification.severity, NotificationSeverity::Warning);
    assert!(notification.body.contains("1 could not be checked"));

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn play_track_flips_is_missing_when_file_has_vanished() {
    // Lazy availability detection: clicking a track whose file is
    // no longer on disk must (a) return TrackUnavailable so the
    // UI shows the missing-file feedback, and (b) flip the
    // persisted `is_missing` flag so the table's warning
    // indicator lights up immediately and subsequent reads of
    // SQLite see the corrected state.
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let track_path = library_root.join("ghost.flac");
    std::fs::write(&track_path, b"audio").expect("write track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(33);
    assert_eq!(store.save_track(test_track(id, "ghost.flac")), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert!(!runtime.library_tracks()[0].location.is_missing());

    std::fs::remove_file(&track_path).expect("remove track");

    let outcome =
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: sustain_domain::PlaybackQueueRequest::Library,
        }));
    assert_eq!(outcome, Err(ApplicationRuntimeError::TrackUnavailable));
    assert!(runtime.library_tracks()[0].location.is_missing());

    let reloaded = store
        .track(id)
        .expect("reload track")
        .expect("track row exists");
    assert!(reloaded.location.is_missing());

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn play_track_recovers_availability_when_file_reappears() {
    // The `is_missing` flag is a cache of the last observed
    // availability, never a gate. Once a track has been flipped
    // to Missing, a subsequent play attempt must still re-stat
    // the path: if the file is back (rename undone, volume
    // remounted, restored from trash), the flag flips back to
    // Available and playback proceeds. Without this, a typo'd
    // rename would soft-brick the row forever.
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let track_path = library_root.join("returning.flac");
    std::fs::write(&track_path, b"audio").expect("write track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(34);
    assert_eq!(store.save_track(test_track(id, "returning.flac")), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    // Step 1: remove the file, fail a play, observe the flag flip.
    std::fs::remove_file(&track_path).expect("remove track");
    let first = runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
        track_id: id,
        queue: sustain_domain::PlaybackQueueRequest::Library,
    }));
    assert_eq!(first, Err(ApplicationRuntimeError::TrackUnavailable));
    assert!(runtime.library_tracks()[0].location.is_missing());

    // Step 2: put the file back. The flag still says Missing —
    // nothing else has touched the row.
    std::fs::write(&track_path, b"audio").expect("restore track");
    assert!(runtime.library_tracks()[0].location.is_missing());

    // Step 3: a fresh play succeeds because `play_track` re-stats
    // the resolved path; both the in-memory and persisted flags
    // flip back to Available.
    let second = runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
        track_id: id,
        queue: sustain_domain::PlaybackQueueRequest::Library,
    }));
    assert_eq!(second, Ok(()));
    assert!(!runtime.library_tracks()[0].location.is_missing());

    let reloaded = store
        .track(id)
        .expect("reload track")
        .expect("track row exists");
    assert!(!reloaded.location.is_missing());

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn play_track_probe_failure_does_not_create_a_missing_marker() {
    let library_root = unique_test_directory();
    std::fs::create_dir_all(&library_root).expect("create library root");
    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(35);
    assert_eq!(store.save_track(test_track(id, "unresolved.flac")), Ok(()));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(library_root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert_eq!(
        runtime.play_track_with(id, |_| FilePresence::ProbeFailed),
        Err(ApplicationRuntimeError::TrackUnavailable)
    );
    assert!(!runtime.library_tracks()[0].location.is_missing());
    assert!(
        !store
            .track(id)
            .expect("reload track")
            .expect("track row")
            .location
            .is_missing()
    );

    std::fs::remove_dir_all(library_root).expect("remove library root");
}

#[test]
fn reference_relocation_preserves_authoritative_track_identity_and_library_data() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create library root");
    let replacement_path = root.join("replacement.flac");
    std::fs::write(&replacement_path, b"replacement audio").expect("write replacement");

    let id = track_id(36);
    let mut missing = test_track(id, "missing.flac");
    missing.location = missing_track_location("missing.flac");
    missing.metadata.title = Some("Canonical title".to_owned());
    missing.rating = Rating::new(4).expect("rating");
    missing.statistics = PlayStatistics {
        play_count: 7,
        skip_count: 3,
        last_played_at: Some(std::time::SystemTime::UNIX_EPOCH),
        ..PlayStatistics::default()
    };
    missing.file_size_bytes = Some(12);
    missing.has_embedded_artwork = Some(true);
    missing.file_modified_at = Some(std::time::SystemTime::UNIX_EPOCH);
    let store = Arc::new(InMemoryLibraryStore::new());
    store.save_track(missing.clone()).expect("seed missing row");
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RelocateMissingTrack {
            track_id: id,
            replacement_path: replacement_path.clone(),
        }),
        Ok(())
    );

    let relocated = store.track(id).expect("load row").expect("track exists");
    assert_eq!(relocated.id, id);
    assert_eq!(relocated.location, track_location("replacement.flac"));
    assert_eq!(relocated.metadata, missing.metadata);
    assert_eq!(relocated.rating, missing.rating);
    assert_eq!(relocated.statistics, missing.statistics);
    assert_eq!(
        relocated.file_size_bytes,
        Some(b"replacement audio".len() as u64)
    );
    assert_eq!(relocated.has_embedded_artwork, None);
    assert_eq!(relocated.file_modified_at, None);
    assert_eq!(runtime.library_track(id), Some(&relocated));

    std::fs::remove_dir_all(root).expect("remove library root");
}

#[test]
fn reference_relocation_rejects_a_path_attached_to_another_track() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create library root");
    let replacement_path = root.join("existing.flac");
    std::fs::write(&replacement_path, b"audio").expect("write replacement");

    let missing_id = track_id(37);
    let mut missing = test_track(missing_id, "missing.flac");
    missing.location = missing_track_location("missing.flac");
    let store = Arc::new(InMemoryLibraryStore::new());
    store.save_track(missing).expect("seed missing row");
    store
        .save_track(test_track(track_id(38), "existing.flac"))
        .expect("seed existing row");
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RelocateMissingTrack {
            track_id: missing_id,
            replacement_path,
        }),
        Err(ApplicationRuntimeError::TrackReplacementAlreadyInLibrary)
    );

    std::fs::remove_dir_all(root).expect("remove library root");
}

#[test]
fn managed_relocation_copies_external_replacement_into_owned_layout() {
    let root = unique_test_directory();
    let external_root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create library root");
    std::fs::create_dir_all(&external_root).expect("create external root");
    let replacement_path = external_root.join("replacement.flac");
    std::fs::write(&replacement_path, b"replacement audio").expect("write replacement");

    let id = track_id(39);
    let mut missing = test_track(id, "missing.flac");
    missing.location = missing_track_location("missing.flac");
    missing.metadata.artist = Some("Artist".to_owned());
    missing.metadata.album = Some("Album".to_owned());
    missing.metadata.title = Some("Title".to_owned());
    missing.metadata.track_number = Some(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    store.save_track(missing).expect("seed missing row");
    let mut settings = UserSettings::with_library_path(Some(root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RelocateMissingTrack {
            track_id: id,
            replacement_path: replacement_path.clone(),
        }),
        Ok(())
    );

    let relocated = store.track(id).expect("load row").expect("track exists");
    let owned_path = relocated.location.absolute_path(&root);
    assert!(!relocated.location.is_missing());
    assert_eq!(
        std::fs::read(&owned_path).expect("read owned copy"),
        b"replacement audio"
    );
    assert_eq!(
        std::fs::read(&replacement_path).expect("read original"),
        b"replacement audio"
    );

    std::fs::remove_dir_all(root).expect("remove library root");
    std::fs::remove_dir_all(external_root).expect("remove external root");
}

#[test]
fn runtime_scan_keeps_missing_tracks_visible() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");

    let track_id = track_id(9);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(
        store.save_track(test_track(track_id, "missing.mp3")),
        Ok(())
    );

    let metadata_service = Arc::new(TestMetadataService);
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, metadata_service)
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::ScanLibrary {
            library_path: root.clone()
        }),
        Ok(())
    );

    let tracks = runtime.library_tracks();
    assert_eq!(tracks.len(), 1);
    assert_eq!(tracks[0].id, track_id);
    assert!(tracks[0].location.is_missing());
    assert_eq!(
        runtime
            .last_scan_summary()
            .map(|summary| summary.missing_tracks),
        Some(1)
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_loads_and_saves_with_settings_store() {
    let store = Box::new(TestSettingsStore::new(UserSettings::with_library_path(
        Some(PathBuf::from("/initial")),
    )));
    let mut runtime =
        ApplicationRuntime::with_settings_store(store).expect("load settings from test store");
    let updated_settings = UserSettings::with_library_path(Some(PathBuf::from("/updated")));

    assert_eq!(
        runtime.settings(),
        &UserSettings::with_library_path(Some(PathBuf::from("/initial")))
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(updated_settings.clone())),
        Ok(())
    );
    assert_eq!(runtime.settings(), &updated_settings);
}

#[test]
fn runtime_saves_ui_settings_with_settings_store() {
    let store = Box::new(TestSettingsStore::new(UserSettings::default()));
    let mut runtime =
        ApplicationRuntime::with_settings_store(store).expect("load settings from test store");
    let ui = UiSettings {
        search_text: "jazz".to_owned(),
        sidebar_selection: UiSidebarSelection::Albums,
        sidebar_collapsed: true,
        sidebar_width: Some(212),
        library_section_collapsed: true,
        playlists_section_collapsed: false,
        sidebar_show_duplicates: false,
        sidebar_show_statistics: true,
    };

    assert_eq!(runtime.save_ui_settings(ui.clone()), Ok(()));

    assert_eq!(runtime.settings().ui, ui);
}

#[test]
fn live_settings_save_failure_surfaces_a_notification() {
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(FailingSettingsStore)).expect("load");

    // A debounced volume change is a normal-operation save: a failure must
    // reach the user through the NotificationCenter and propagate as Err,
    // never be silently discarded.
    let result = runtime.save_playback_volume(VolumePercent::from_clamped(42));
    assert_eq!(result, Err(ApplicationRuntimeError::SettingsSaveFailed));

    let notification = runtime
        .notifications()
        .current_ephemeral()
        .expect("a settings-save failure notification");
    assert_eq!(notification.category, NotificationCategory::Settings);
    assert_eq!(notification.severity, NotificationSeverity::Warning);
}

#[test]
fn runtime_plays_tracks_through_playback_service() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("track.flac"), b"not real audio").expect("write fake track");

    let track_id = positive_track_id();
    let store = Arc::new(InMemoryLibraryStore::new());
    let track = Track {
        id: track_id,
        location: track_location("track.flac"),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    assert_eq!(store.save_track(track), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id,
            queue: PlaybackQueueRequest::Library,
        })),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id,
            position: std::time::Duration::ZERO,
        }
    );
    assert_eq!(
        runtime.now_playing().track.map(|track| track.id),
        Some(track_id)
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_cycles_shuffle_mode_without_playback_service() {
    let mut runtime = ApplicationRuntime::new();

    assert_eq!(runtime.playback_options(), PlaybackOptions::default());
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode
        )),
        Ok(())
    );

    assert_eq!(
        runtime.playback_options(),
        PlaybackOptions {
            shuffle_mode: ShuffleMode::Pure,
            repeat_mode: RepeatMode::Off,
        }
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode
        )),
        Ok(())
    );
    assert_eq!(runtime.playback_options().shuffle_mode, ShuffleMode::Smart);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode
        )),
        Ok(())
    );
    assert_eq!(runtime.playback_options().shuffle_mode, ShuffleMode::Off);
}

#[test]
fn runtime_persists_shuffle_cycle_to_settings_store() {
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::default(),
    )))
    .expect("load settings from test store");

    assert_eq!(
        runtime.settings().playback.shuffle_mode,
        ShuffleMode::Off,
        "fresh settings start with shuffle off"
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode
        )),
        Ok(())
    );
    assert_eq!(runtime.settings().playback.shuffle_mode, ShuffleMode::Pure);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SetShuffleMode(ShuffleMode::Off)
        )),
        Ok(())
    );
    assert_eq!(runtime.settings().playback.shuffle_mode, ShuffleMode::Off);
}

#[test]
fn runtime_restores_persisted_shuffle_at_startup() {
    let mut initial_settings = UserSettings::default();
    initial_settings.playback.shuffle_mode = ShuffleMode::Smart;
    let runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(initial_settings)))
            .expect("load settings from test store");

    assert_eq!(runtime.playback_options().shuffle_mode, ShuffleMode::Smart);
}

#[test]
fn runtime_sets_shuffle_mode_without_playback_service() {
    let mut runtime = ApplicationRuntime::new();

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SetShuffleMode(ShuffleMode::Pure)
        )),
        Ok(())
    );
    assert_eq!(runtime.playback_options().shuffle_mode, ShuffleMode::Pure);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SetShuffleMode(ShuffleMode::Off)
        )),
        Ok(())
    );
    assert_eq!(runtime.playback_options().shuffle_mode, ShuffleMode::Off);
}

#[test]
fn runtime_toggles_repeat_without_playback_service() {
    let mut runtime = ApplicationRuntime::new();

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::ToggleRepeat)),
        Ok(())
    );

    assert_eq!(
        runtime.playback_options(),
        PlaybackOptions {
            shuffle_mode: ShuffleMode::Off,
            repeat_mode: RepeatMode::All,
        }
    );
}

#[test]
fn now_playing_reports_playback_options() {
    let mut runtime = ApplicationRuntime::new();

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode
        )),
        Ok(())
    );

    assert_eq!(
        runtime.now_playing().options,
        PlaybackOptions {
            shuffle_mode: ShuffleMode::Pure,
            repeat_mode: RepeatMode::Off,
        }
    );
}

#[test]
fn runtime_play_next_track_skips_missing_tracks() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("first.flac"), b"not real audio").expect("write first track");
    std::fs::write(root.join("third.flac"), b"not real audio").expect("write third track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let first_track = test_track(track_id(1), "first.flac");
    let mut missing_track = test_track(track_id(2), "missing.flac");
    missing_track.location = missing_track_location("missing.flac");
    let third_track = test_track(track_id(3), "third.flac");
    assert_eq!(store.save_track(first_track), Ok(()));
    assert_eq!(store.save_track(missing_track), Ok(()));
    assert_eq!(store.save_track(third_track), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        })),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );

    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(3),
            position: std::time::Duration::ZERO,
        }
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

/// #78: a track played from a 1-result search has a single-track queue;
/// clearing the search must widen the queue to the full view so
/// auto-advance continues instead of stopping.
fn three_track_playback_runtime(root: &std::path::Path) -> ApplicationRuntime {
    std::fs::create_dir_all(root).expect("create test library");
    for name in ["a.flac", "b.flac", "c.flac"] {
        std::fs::write(root.join(name), b"not real audio").expect("write track");
    }
    let store = Arc::new(InMemoryLibraryStore::new());
    for (id, name) in [(1, "a.flac"), (2, "b.flac"), (3, "c.flac")] {
        assert_eq!(store.save_track(test_track(track_id(id), name)), Ok(()));
    }
    ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.to_path_buf())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()))
}

#[test]
fn repopulate_queue_widens_searched_results_so_auto_advance_continues() {
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    // Play track 1 from a search narrowed to just that track.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::SearchResults,
                ordered_track_ids: vec![track_id(1)],
            },
        })),
        Ok(())
    );

    // Clearing the search widens the queue to the whole library.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::RepopulateQueue(PlaybackQueueRequest::Library)
        )),
        Ok(())
    );
    // The playing track and transport are untouched — no reload.
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(1),
            position: std::time::Duration::ZERO,
        }
    );

    // Auto-advance now continues through the full library instead of
    // stopping at the end of the one-track search queue.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(2),
            position: std::time::Duration::ZERO,
        }
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn play_queue_track_starts_upcoming_entry_without_rebuilding_the_queue() {
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        })),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::PlayQueueTrack(track_id(2))
        )),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(2),
            position: std::time::Duration::ZERO,
        }
    );
    assert_eq!(
        runtime
            .playback_queue_upcoming_preview(10)
            .iter()
            .map(|entry| entry.track_id())
            .collect::<Vec<_>>(),
        &[track_id(3)]
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(3),
            position: std::time::Duration::ZERO,
        }
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn repopulate_queue_preserves_curated_up_next_before_widened_continuation() {
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::SearchResults,
                ordered_track_ids: vec![track_id(1)],
            },
        })),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::EnqueueLast(
            vec![track_id(3)]
        ))),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::RepopulateQueue(PlaybackQueueRequest::Library)
        )),
        Ok(())
    );

    let preview = runtime.playback_queue_upcoming_preview(10);
    assert_eq!(
        preview
            .iter()
            .map(|entry| (entry.track_id(), entry.kind()))
            .collect::<Vec<_>>(),
        &[
            (track_id(3), PlaybackQueueEntryKind::Curated),
            (track_id(2), PlaybackQueueEntryKind::Continuation),
        ]
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn repopulate_queue_is_left_alone_when_playing_track_absent_from_request() {
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    // Play track 1 from a two-track search result.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::SearchResults,
                ordered_track_ids: vec![track_id(1), track_id(2)],
            },
        })),
        Ok(())
    );

    // A repopulate request that does not contain the playing track (the
    // user switched views before clearing the search) must be ignored so
    // the queue keeps its anchor.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::RepopulateQueue(PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::SearchResults,
                ordered_track_ids: vec![track_id(3)],
            })
        )),
        Ok(())
    );

    // The original [1, 2] queue is intact, so Next still advances to 2.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(2),
            position: std::time::Duration::ZERO,
        }
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn repopulate_queue_never_widens_an_album_queue() {
    // "Playing in an album ALWAYS queues the album, nothing more, nothing
    // less" (#78): clearing a search must not widen an album queue, even
    // when the user has since switched to the Songs view (which would ask
    // to repopulate from the full library).
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    // Play track 1 as part of a two-track album that excludes track 3.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Explicit {
                source: PlaybackQueueSource::Album,
                ordered_track_ids: vec![track_id(1), track_id(2)],
            },
        })),
        Ok(())
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::RepopulateQueue(PlaybackQueueRequest::Library)
        )),
        Ok(())
    );

    // The album queue is intact: Next reaches the album's second track,
    // and a further Next stops rather than spilling into track 3.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );
    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(2),
            position: std::time::Duration::ZERO,
        }
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack)),
        Ok(())
    );
    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn repopulate_queue_is_a_noop_when_nothing_is_playing() {
    let root = unique_test_directory();
    let mut runtime = three_track_playback_runtime(&root);

    // No track playing: there is no anchor to preserve, so the command
    // must succeed without starting playback.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::RepopulateQueue(PlaybackQueueRequest::Library)
        )),
        Ok(())
    );
    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_play_previous_track_skips_missing_tracks() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("first.flac"), b"not real audio").expect("write first track");
    std::fs::write(root.join("third.flac"), b"not real audio").expect("write third track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let first_track = test_track(track_id(1), "first.flac");
    let mut missing_track = test_track(track_id(2), "missing.flac");
    missing_track.location = missing_track_location("missing.flac");
    let third_track = test_track(track_id(3), "third.flac");
    assert_eq!(store.save_track(first_track), Ok(()));
    assert_eq!(store.save_track(missing_track), Ok(()));
    assert_eq!(store.save_track(third_track), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(3),
            queue: PlaybackQueueRequest::Library,
        })),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(
            PlaybackCommand::PlayPreviousTrack
        )),
        Ok(())
    );

    assert_eq!(
        runtime.playback_state(),
        PlaybackState::Playing {
            track_id: track_id(1),
            position: std::time::Duration::ZERO,
        }
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_set_rating_writes_metadata_and_updates_store_cache() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));
    let metadata_service = Arc::new(RecordingMetadataService::new(false));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), metadata_service.clone())
    .expect("library services initialize");
    let rating = Rating::new(5).expect("valid test rating");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::SetRating { track_id, rating }),
        Ok(())
    );

    assert_eq!(
        metadata_service.rating_writes(),
        vec![(track_path.clone(), rating)]
    );
    assert_eq!(runtime.library_tracks()[0].rating, rating);
    assert_eq!(
        store
            .track(track_id)
            .expect("load updated track")
            .map(|track| track.rating),
        Some(rating)
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_set_rating_applies_optimistic_update_and_reports_tag_write_failure() {
    // The new contract: the in-memory + SQLite update is applied
    // immediately and SetRating returns Ok(()) synchronously, so the
    // UI never blocks on the tag write. Tag-write failure surfaces
    // through the result sink rather than as a command error — the
    // durable outbox keeps retrying the courtesy file-tag mirror.
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));
    let metadata_service = Arc::new(RecordingMetadataService::new(true));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), metadata_service.clone())
    .expect("library services initialize");
    let (result_tx, result_rx) = async_channel::unbounded::<crate::MetadataWriterEvent>();
    runtime.set_metadata_writer_event_sink(result_tx);
    let rating = Rating::new(4).expect("valid test rating");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::SetRating { track_id, rating }),
        Ok(())
    );

    assert_eq!(
        metadata_service.rating_writes(),
        vec![(track_path.clone(), rating)]
    );
    // Optimistic state: in-memory + SQLite both reflect the new rating,
    // even though the disk tag write failed.
    assert_eq!(runtime.library_tracks()[0].rating, rating);
    assert_eq!(
        store
            .track(track_id)
            .expect("load updated track")
            .map(|track| track.rating),
        Some(rating)
    );
    // Failure is reported to the sink (UI surfaces a status-bar
    // message and refreshes the affected row).
    let crate::MetadataWriterEvent::Mirror(posted) = result_rx
        .try_recv()
        .expect("metadata writer posts the failure")
    else {
        panic!("expected mirror result");
    };
    assert_eq!(posted.track_id, track_id);
    assert_eq!(posted.kind, crate::MetadataWriteKind::Rating);
    assert_eq!(posted.outcome, crate::MetadataWriteOutcome::Failed);
    assert_eq!(
        std::fs::read(&track_path).expect("reference-mode audio remains readable"),
        b"not real audio"
    );
    assert_eq!(
        store
            .tag_mirrors_due(i64::MAX, 10)
            .expect("pending mirror")
            .len(),
        1
    );
    runtime.apply_metadata_writer_event(crate::MetadataWriterEvent::Mirror(posted));
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::MetadataWrite
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_rejects_invalid_artwork_before_submitting_tag_write() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("track.flac"), b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));
    let metadata_service = Arc::new(RecordingMetadataService::new(false));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, metadata_service.clone())
    .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::SetArtwork {
            track_id,
            artwork: Some(b"not an image".to_vec()),
        }),
        Err(ApplicationRuntimeError::ArtworkRejected)
    );
    assert!(metadata_service.artwork_writes().is_empty());

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_update_metadata_writes_tags_and_updates_store_cache() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "track.flac");
    track.metadata.title = Some("Old".to_owned());
    track.metadata.artist = Some("Artist".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let metadata_service = Arc::new(RecordingMetadataService::new(false));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), metadata_service.clone())
    .expect("library services initialize");
    let change = MetadataChange {
        title: FieldChange::Set("New".to_owned()),
        artist: FieldChange::Clear,
        year: FieldChange::Set(2001),
        ..MetadataChange::default()
    };

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(change.clone()),
        }),
        Ok(())
    );

    let writes = metadata_service.metadata_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, track_path);
    assert_eq!(writes[0].1.title, FieldChange::Set("New".to_owned()));
    assert_eq!(writes[0].1.artist, FieldChange::Clear);
    assert_eq!(writes[0].1.album, FieldChange::Clear);
    assert_eq!(writes[0].1.year, FieldChange::Set(2001));
    assert_eq!(
        runtime.library_tracks()[0].metadata.title.as_deref(),
        Some("New")
    );
    assert_eq!(runtime.library_tracks()[0].metadata.artist, None);
    assert_eq!(runtime.library_tracks()[0].metadata.year, Some(2001));
    let stored = store
        .track(track_id)
        .expect("load updated track")
        .expect("track exists");
    assert_eq!(stored.metadata.title.as_deref(), Some("New"));
    assert_eq!(stored.metadata.artist, None);
    assert_eq!(stored.metadata.year, Some(2001));

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn metadata_edit_updates_the_search_index_immediately() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("track.flac"), b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    // Path is neutral ("track.flac") so the title is the only carrier of
    // "Before"/"After" in the search document.
    let mut track = test_track(track_id, "track.flac");
    track.metadata.title = Some("Before".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(RecordingMetadataService::new(false)))
    .expect("library services initialize");

    // Built during library load.
    assert!(runtime.search_matches(track_id, &normalize_query("before")));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(MetadataChange {
                title: FieldChange::Set("After".to_owned()),
                ..MetadataChange::default()
            }),
        }),
        Ok(())
    );

    assert!(
        runtime.search_matches(track_id, &normalize_query("after")),
        "the edited title is searchable immediately"
    );
    assert!(
        !runtime.search_matches(track_id, &normalize_query("before")),
        "the stale title no longer matches after the edit"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn managed_metadata_update_moves_file_when_planned_path_changes() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let source_path = root.join("loose.flac");
    std::fs::write(&source_path, b"audio bytes").expect("write fake track");

    let track_id = track_id(31);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "loose.flac");
    track.metadata.title = Some("Old".to_owned());
    track.metadata.artist = Some("Old Artist".to_owned());
    track.metadata.album = Some("Old Album".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let mut settings = UserSettings::with_library_path(Some(root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let metadata_service = Arc::new(RecordingMetadataService::new(false));
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), metadata_service.clone())
            .expect("library services initialize");
    let change = MetadataChange {
        title: FieldChange::Set("Song".to_owned()),
        artist: FieldChange::Set("Artist".to_owned()),
        album: FieldChange::Set("Album".to_owned()),
        track_number: FieldChange::Set(3),
        ..MetadataChange::default()
    };

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(change.clone()),
        }),
        Ok(())
    );

    let destination_path = root.join("Artist/Album/03 Song.flac");
    assert!(!source_path.exists());
    assert_eq!(
        std::fs::read(&destination_path).expect("destination exists"),
        b"audio bytes"
    );
    assert_eq!(
        metadata_service.metadata_writes(),
        vec![(
            destination_path.clone(),
            MetadataChange {
                title: FieldChange::Set("Song".to_owned()),
                artist: FieldChange::Set("Artist".to_owned()),
                album: FieldChange::Set("Album".to_owned()),
                track_number: FieldChange::Set(3),
                album_artist: FieldChange::Clear,
                composer: FieldChange::Clear,
                grouping: FieldChange::Clear,
                genre: FieldChange::Clear,
                track_total: FieldChange::Clear,
                disc_number: FieldChange::Clear,
                disc_total: FieldChange::Clear,
                year: FieldChange::Clear,
                compilation: FieldChange::Clear,
                bpm: FieldChange::Clear,
                key: FieldChange::Clear,
                comments: FieldChange::Clear,
                lyrics: FieldChange::Clear,
            }
        )]
    );
    assert_eq!(
        runtime.library_tracks()[0].location.relative_path.as_path(),
        Path::new("Artist/Album/03 Song.flac")
    );
    assert_eq!(
        store
            .track(track_id)
            .expect("load updated track")
            .map(|track| track.location.relative_path.to_path_buf()),
        Some(PathBuf::from("Artist/Album/03 Song.flac"))
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn managed_metadata_update_keeps_file_in_place_for_non_path_fields() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("Artist/Album/01 Song.flac");
    std::fs::create_dir_all(track_path.parent().expect("test path has parent"))
        .expect("create album directory");
    std::fs::write(&track_path, b"audio bytes").expect("write fake track");

    let track_id = track_id(32);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "Artist/Album/01 Song.flac");
    track.metadata.title = Some("Song".to_owned());
    track.metadata.artist = Some("Artist".to_owned());
    track.metadata.album = Some("Album".to_owned());
    track.metadata.track_number = Some(1);
    assert_eq!(store.save_track(track), Ok(()));
    let mut settings = UserSettings::with_library_path(Some(root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let metadata_service = Arc::new(RecordingMetadataService::new(false));
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store.clone(), metadata_service.clone())
            .expect("library services initialize");
    let change = MetadataChange {
        year: FieldChange::Set(1999),
        genre: FieldChange::Set("Rock".to_owned()),
        ..MetadataChange::default()
    };

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(change.clone()),
        }),
        Ok(())
    );

    assert!(track_path.exists());
    let writes = metadata_service.metadata_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, track_path);
    assert_eq!(writes[0].1.title, FieldChange::Set("Song".to_owned()));
    assert_eq!(writes[0].1.artist, FieldChange::Set("Artist".to_owned()));
    assert_eq!(writes[0].1.album, FieldChange::Set("Album".to_owned()));
    assert_eq!(writes[0].1.genre, FieldChange::Set("Rock".to_owned()));
    assert_eq!(writes[0].1.year, FieldChange::Set(1999));
    assert_eq!(
        runtime.library_tracks()[0].location.relative_path.as_path(),
        Path::new("Artist/Album/01 Song.flac")
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_update_metadata_applies_optimistic_update_and_reports_tag_write_failure() {
    // Same contract as set_rating in the non-managed-rename branch:
    // in-memory + SQLite update is applied synchronously, tag write
    // is dispatched to the async writer, failure surfaces on the
    // result sink.
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "track.flac");
    track.metadata.title = Some("Old".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let metadata_service = Arc::new(RecordingMetadataService::with_metadata_write_failure());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), metadata_service.clone())
    .expect("library services initialize");
    let (result_tx, result_rx) = async_channel::unbounded::<crate::MetadataWriterEvent>();
    runtime.set_metadata_writer_event_sink(result_tx);
    let change = MetadataChange {
        title: FieldChange::Set("New".to_owned()),
        ..MetadataChange::default()
    };

    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(change.clone()),
        }),
        Ok(())
    );

    let writes = metadata_service.metadata_writes();
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, track_path);
    assert_eq!(writes[0].1.title, FieldChange::Set("New".to_owned()));
    assert_eq!(writes[0].1.artist, FieldChange::Clear);
    assert_eq!(writes[0].1.album, FieldChange::Clear);
    // Optimistic state holds even though the disk tag write failed.
    assert_eq!(
        runtime.library_tracks()[0].metadata.title.as_deref(),
        Some("New")
    );
    assert_eq!(
        store
            .track(track_id)
            .expect("load updated track")
            .and_then(|track| track.metadata.title),
        Some("New".to_owned())
    );
    let crate::MetadataWriterEvent::Mirror(posted) = result_rx
        .try_recv()
        .expect("metadata writer posts the failure")
    else {
        panic!("expected mirror result");
    };
    assert_eq!(posted.track_id, track_id);
    assert_eq!(posted.kind, crate::MetadataWriteKind::Metadata);
    assert_eq!(posted.outcome, crate::MetadataWriteOutcome::Failed);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn metadata_write_retry_notice_is_persistent_deduplicated_and_dismissed_on_success() {
    let mut runtime = ApplicationRuntime::new();
    let failed = crate::MetadataWriteResult {
        track_id: track_id(1),
        kind: crate::MetadataWriteKind::Metadata,
        outcome: crate::MetadataWriteOutcome::Failed,
    };

    runtime.apply_metadata_write_result(failed);
    runtime.apply_metadata_write_result(failed);
    assert_eq!(runtime.notifications().persistent_stack().len(), 1);
    assert!(
        runtime.notifications().persistent_stack()[0]
            .body
            .contains("retry automatically")
    );

    runtime.apply_metadata_write_result(crate::MetadataWriteResult {
        outcome: crate::MetadataWriteOutcome::Succeeded,
        ..failed
    });
    assert!(runtime.notifications().persistent_stack().is_empty());
}

#[test]
fn runtime_removes_tracks_from_library_and_stops_playback() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("track.flac"), b"not real audio").expect("write fake track");

    let removed_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(
        store.save_track(test_track(removed_id, "track.flac")),
        Ok(())
    );

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: removed_id,
            queue: PlaybackQueueRequest::Library,
        })),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::RemoveTrackFromLibrary {
            track_id: removed_id,
        }),
        Ok(())
    );

    assert!(runtime.library_tracks().is_empty());
    assert_eq!(store.track(removed_id), Ok(None));
    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_moves_tracks_to_trash_and_removes_underlying_file() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");

    let trashed_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(
        store.save_track(test_track(trashed_id, "track.flac")),
        Ok(())
    );

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::MoveTrackToTrash {
            track_id: trashed_id,
        }),
        Ok(())
    );

    assert!(runtime.library_tracks().is_empty());
    assert_eq!(store.track(trashed_id), Ok(None));
    assert!(!track_path.exists(), "audio file should be moved to trash");

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn managed_metadata_retarget_event_reloads_sqlite_and_surfaces_failure() {
    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut track = test_track(track_id, "loose.flac");
    track.metadata.title = Some("Old".to_owned());
    assert_eq!(store.save_track(track), Ok(()));
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");
    store
        .apply_track_metadata_change_and_location_and_enqueue_mirror(
            track_id,
            &MetadataChange {
                title: FieldChange::Set("New".to_owned()),
                ..MetadataChange::default()
            },
            &track_location("Artist/Album/New.flac"),
        )
        .expect("commit retarget");
    runtime.apply_metadata_write_result(crate::MetadataWriteResult {
        track_id,
        kind: crate::MetadataWriteKind::Metadata,
        outcome: crate::MetadataWriteOutcome::Failed,
    });

    runtime.apply_metadata_writer_event(crate::MetadataWriterEvent::ManagedRetarget(
        crate::ManagedMetadataRetargetResult {
            track_id,
            outcome: Ok(()),
            empty_directory_cleanup_failed: false,
        },
    ));

    assert_eq!(
        runtime.library_tracks()[0].location.relative_path.as_path(),
        Path::new("Artist/Album/New.flac")
    );
    assert_eq!(
        runtime.library_tracks()[0].metadata.title.as_deref(),
        Some("New")
    );
    assert_eq!(
        runtime.notifications().persistent_stack().len(),
        1,
        "durable retarget does not prove the queued mirror converged"
    );
    runtime.apply_metadata_writer_event(crate::MetadataWriterEvent::Mirror(
        crate::MetadataWriteResult {
            track_id,
            kind: crate::MetadataWriteKind::Metadata,
            outcome: crate::MetadataWriteOutcome::Succeeded,
        },
    ));
    assert!(runtime.notifications().persistent_stack().is_empty());

    runtime.apply_metadata_writer_event(crate::MetadataWriterEvent::ManagedRetarget(
        crate::ManagedMetadataRetargetResult {
            track_id,
            outcome: Err(ApplicationRuntimeError::LibraryConsolidationFailed),
            empty_directory_cleanup_failed: false,
        },
    ));
    assert_eq!(runtime.notifications().persistent_stack().len(), 1);
    assert!(
        runtime.notifications().persistent_stack()[0]
            .body
            .contains("could not finish safely")
    );
}

#[test]
fn pending_managed_metadata_retarget_blocks_structural_library_operations_only() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let track_path = root.join("track.flac");
    std::fs::write(&track_path, b"not real audio").expect("write fake track");
    let track_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    assert_eq!(store.save_track(test_track(track_id, "track.flac")), Ok(()));
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");
    runtime.register_pending_managed_metadata_retarget(track_id);

    assert!(matches!(
        runtime.prepare_library_scan(root.clone()),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    ));
    assert!(matches!(
        runtime.prepare_library_import(vec![track_path.clone()]),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    ));
    assert!(matches!(
        runtime.prepare_library_consolidation(),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    ));
    assert_eq!(
        runtime.handle_command(ApplicationCommand::RemoveTrackFromLibrary { track_id }),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::MoveTrackToTrash { track_id }),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    );
    let mut settings = runtime.settings().clone();
    settings.library.path = Some(root.join("other-root"));
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateSettings(settings)),
        Err(ApplicationRuntimeError::BackgroundTaskRunning)
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::SetRating {
            track_id,
            rating: Rating::new(4).expect("rating"),
        }),
        Ok(())
    );
    assert_eq!(
        runtime.handle_command(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(MetadataChange {
                genre: FieldChange::Set("Rock".to_owned()),
                ..MetadataChange::default()
            }),
        }),
        Ok(())
    );
    assert_eq!(
        store
            .track(track_id)
            .expect("load track")
            .map(|track| (track.rating, track.metadata.genre)),
        Some((Rating::new(4).expect("rating"), Some("Rock".to_owned())))
    );
    assert!(track_path.exists());

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_move_to_trash_succeeds_when_file_is_already_missing() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");

    let trashed_id = track_id(1);
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut missing = test_track(trashed_id, "absent.flac");
    missing.location = missing_track_location("absent.flac");
    assert_eq!(store.save_track(missing), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    assert_eq!(
        runtime.handle_command(ApplicationCommand::MoveTrackToTrash {
            track_id: trashed_id,
        }),
        Ok(())
    );
    assert!(runtime.library_tracks().is_empty());
    assert_eq!(store.track(trashed_id), Ok(None));

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn runtime_creates_renames_and_deletes_playlists() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylist {
            name: "  Favorites  ".to_owned(),
            parent_folder_id: None,
        }),
        Ok(())
    );
    let playlist_id = playlist_id(1);
    assert_eq!(runtime.playlists()[0].name, "Favorites");
    assert_eq!(
        store
            .playlist(playlist_id)
            .expect("playlist loads")
            .map(|playlist| playlist.name),
        Some("Favorites".to_owned())
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RenamePlaylist {
            playlist_id,
            name: "Road".to_owned(),
        }),
        Ok(())
    );
    assert_eq!(runtime.playlists()[0].name, "Road");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::DeletePlaylist { playlist_id }),
        Ok(())
    );
    assert!(runtime.playlists().is_empty());
    assert_eq!(store.playlist(playlist_id), Ok(None));
}

#[test]
fn runtime_updates_playlist_entries_in_store_and_cache() {
    let store = Arc::new(InMemoryLibraryStore::new());
    for id in [1, 2, 3] {
        assert_eq!(
            store.save_track(test_track(track_id(id), &format!("track-{id}.flac"))),
            Ok(())
        );
    }
    let playlist_id = playlist_id(1);
    assert_eq!(
        store.save_playlist(Playlist {
            id: playlist_id,
            name: "Favorites".to_owned(),
            parent_folder_id: None,
            position: 0,
            entries: Vec::new(),
        }),
        Ok(())
    );
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddTracksToPlaylist {
            playlist_id,
            track_ids: vec![track_id(2), track_id(1), track_id(2)],
        }),
        Ok(())
    );
    assert_playlist_track_ids(
        runtime.playlists(),
        playlist_id,
        &[track_id(2), track_id(1)],
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::MovePlaylistEntries {
            playlist_id,
            track_ids: vec![track_id(1)],
            new_position: 0,
        }),
        Ok(())
    );
    assert_playlist_track_ids(
        runtime.playlists(),
        playlist_id,
        &[track_id(1), track_id(2)],
    );

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RemoveTracksFromPlaylist {
            playlist_id,
            track_ids: vec![track_id(2)],
        }),
        Ok(())
    );
    assert_playlist_track_ids(runtime.playlists(), playlist_id, &[track_id(1)]);
    assert_playlist_track_ids(
        &[store
            .playlist(playlist_id)
            .expect("playlist loads")
            .expect("playlist exists")],
        playlist_id,
        &[track_id(1)],
    );
}

#[test]
fn runtime_move_playlist_entries_relocates_a_contiguous_block_atomically() {
    let store = Arc::new(InMemoryLibraryStore::new());
    for id in 1..=5 {
        assert_eq!(
            store.save_track(test_track(track_id(id), &format!("track-{id}.flac"))),
            Ok(())
        );
    }
    let playlist_id = playlist_id(1);
    assert_eq!(
        store.save_playlist(Playlist {
            id: playlist_id,
            name: "Set".to_owned(),
            parent_folder_id: None,
            position: 0,
            entries: Vec::new(),
        }),
        Ok(())
    );
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::AddTracksToPlaylist {
            playlist_id,
            track_ids: (1..=5).map(track_id).collect(),
        }),
        Ok(())
    );
    assert_playlist_track_ids(
        runtime.playlists(),
        playlist_id,
        &[
            track_id(1),
            track_id(2),
            track_id(3),
            track_id(4),
            track_id(5),
        ],
    );

    // Move tracks 3 and 4 to the head: post-removal list is
    // [1, 2, 5] (len 3), insertion at index 0 lands the block ahead
    // of every other entry.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::MovePlaylistEntries {
            playlist_id,
            track_ids: vec![track_id(3), track_id(4)],
            new_position: 0,
        }),
        Ok(())
    );
    assert_playlist_track_ids(
        runtime.playlists(),
        playlist_id,
        &[
            track_id(3),
            track_id(4),
            track_id(1),
            track_id(2),
            track_id(5),
        ],
    );

    // Move tracks 4 and 1 to the tail: caller passes them in arbitrary
    // order, but the post-removal block must reflect the playlist's
    // own current order (1 comes before 4 in [3, 4, 1, 2, 5]),
    // landing as [..., 4, 1] would be wrong; the correct outcome is
    // [3, 2, 5, 4, 1] because at extraction time 4 still precedes 1
    // in the playlist's entries. Saturating new_position to u32::MAX
    // pins the block at the tail.
    assert_eq!(
        runtime.handle_command(ApplicationCommand::MovePlaylistEntries {
            playlist_id,
            track_ids: vec![track_id(1), track_id(4)],
            new_position: u32::MAX,
        }),
        Ok(())
    );
    assert_playlist_track_ids(
        runtime.playlists(),
        playlist_id,
        &[
            track_id(3),
            track_id(2),
            track_id(5),
            track_id(4),
            track_id(1),
        ],
    );

    // Same outcome must be visible in the underlying store, not just
    // the runtime cache.
    assert_playlist_track_ids(
        &[store
            .playlist(playlist_id)
            .expect("playlist loads")
            .expect("playlist exists")],
        playlist_id,
        &[
            track_id(3),
            track_id(2),
            track_id(5),
            track_id(4),
            track_id(1),
        ],
    );
}

#[test]
fn runtime_move_playlist_entries_rejects_empty_track_list() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let playlist_id = playlist_id(1);
    assert_eq!(
        store.save_playlist(Playlist {
            id: playlist_id,
            name: "Set".to_owned(),
            parent_folder_id: None,
            position: 0,
            entries: Vec::new(),
        }),
        Ok(())
    );
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::MovePlaylistEntries {
            playlist_id,
            track_ids: Vec::new(),
            new_position: 0,
        }),
        Err(ApplicationRuntimeError::PlaylistEntryNotFound),
    );
}

#[test]
fn runtime_rejects_blank_playlist_names() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylist {
            name: "   ".to_owned(),
            parent_folder_id: None,
        }),
        Err(ApplicationRuntimeError::InvalidPlaylistName)
    );
}

#[test]
fn runtime_creates_renames_and_deletes_playlist_folders() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "  Mixes  ".to_owned(),
            parent_folder_id: None,
        }),
        Ok(())
    );
    let folder_id = folder_id(1);
    assert_eq!(runtime.playlist_folders().len(), 1);
    assert_eq!(runtime.playlist_folders()[0].name, "Mixes");
    assert_eq!(runtime.playlist_folders()[0].position, 0);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::RenamePlaylistFolder {
            folder_id,
            name: "Long Drives".to_owned(),
        }),
        Ok(())
    );
    assert_eq!(runtime.playlist_folders()[0].name, "Long Drives");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::DeletePlaylistFolder { folder_id }),
        Ok(())
    );
    assert!(runtime.playlist_folders().is_empty());
    assert_eq!(store.playlist_folder(folder_id), Ok(None));
}

#[test]
fn runtime_rejects_blank_playlist_folder_names() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "  ".to_owned(),
            parent_folder_id: None,
        }),
        Err(ApplicationRuntimeError::InvalidPlaylistFolderName)
    );
}

#[test]
fn runtime_rejects_creating_folder_under_missing_parent() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Inside".to_owned(),
            parent_folder_id: Some(folder_id(999)),
        }),
        Err(ApplicationRuntimeError::PlaylistFolderNotFound)
    );
}

#[test]
fn deleting_a_folder_cascades_and_reloads_runtime_state() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Mixes".to_owned(),
            parent_folder_id: None,
        })
        .expect("create folder");
    let folder_id_value = folder_id(1);

    runtime
        .handle_command(ApplicationCommand::CreatePlaylist {
            name: "Inside".to_owned(),
            parent_folder_id: Some(folder_id_value),
        })
        .expect("create playlist inside folder");
    runtime
        .handle_command(ApplicationCommand::CreateSmartPlaylist {
            name: "Smart Inside".to_owned(),
            parent_folder_id: Some(folder_id_value),
            rules: test_rule_set(),
        })
        .expect("create smart playlist inside folder");

    assert_eq!(runtime.playlists().len(), 1);
    assert_eq!(runtime.smart_playlists().len(), 1);

    runtime
        .handle_command(ApplicationCommand::DeletePlaylistFolder {
            folder_id: folder_id_value,
        })
        .expect("delete folder cascades");

    assert!(runtime.playlist_folders().is_empty());
    assert!(runtime.playlists().is_empty());
    assert!(runtime.smart_playlists().is_empty());
}

#[test]
fn runtime_creates_updates_and_deletes_smart_playlists() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store.clone(), Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreateSmartPlaylist {
            name: "Recent".to_owned(),
            parent_folder_id: None,
            rules: test_rule_set(),
        })
        .expect("create smart playlist");
    let smart_id_value = smart_id(1);
    assert_eq!(runtime.smart_playlists().len(), 1);
    assert_eq!(runtime.smart_playlists()[0].name, "Recent");

    let new_rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::Any,
        limit: None,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Genre,
            operator: SmartPlaylistTextOperator::Is,
            value: "Trip-Hop".to_owned(),
        }],
    };
    runtime
        .handle_command(ApplicationCommand::UpdateSmartPlaylist {
            smart_playlist_id: smart_id_value,
            name: "Renamed".to_owned(),
            rules: new_rules.clone(),
        })
        .expect("update smart playlist");
    assert_eq!(runtime.smart_playlists()[0].name, "Renamed");
    assert_eq!(runtime.smart_playlists()[0].rules, new_rules);

    runtime
        .handle_command(ApplicationCommand::DeleteSmartPlaylist {
            smart_playlist_id: smart_id_value,
        })
        .expect("delete smart playlist");
    assert!(runtime.smart_playlists().is_empty());
}

#[test]
fn smart_playlist_track_status_distinguishes_included_excluded_and_unknowable() {
    // Three scenarios in one test:
    //   1. Limit-less rule, track matches      -> Included.
    //   2. Limit-less rule, track doesn't      -> Excluded.
    //   3. Limit-bearing rule, any track       -> RequiresFullRebuild
    //      (single-track inspection can't reason about eviction).
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    let store = Arc::new(InMemoryLibraryStore::new());

    let matching = Track {
        id: track_id(1),
        location: track_location("portishead.flac"),
        metadata: TrackMetadata {
            artist: Some("Portishead".to_owned()),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    let non_matching = Track {
        id: track_id(2),
        location: track_location("other.flac"),
        metadata: TrackMetadata {
            artist: Some("Some Other Band".to_owned()),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    store.save_track(matching.clone()).expect("save matching");
    store.save_track(non_matching.clone()).expect("save other");

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root)),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreateSmartPlaylist {
            name: "Portishead-only".to_owned(),
            parent_folder_id: None,
            rules: test_rule_set(),
        })
        .expect("create smart playlist");
    let smart_id_value = smart_id(1);

    assert_eq!(
        runtime.smart_playlist_track_status(smart_id_value, matching.id),
        SmartPlaylistTrackStatus::Included
    );
    assert_eq!(
        runtime.smart_playlist_track_status(smart_id_value, non_matching.id),
        SmartPlaylistTrackStatus::Excluded
    );

    // Re-rule with a limit; even the previously-Included track
    // must now report RequiresFullRebuild.
    let limited_rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Artist,
            operator: SmartPlaylistTextOperator::Contains,
            value: "Portishead".to_owned(),
        }],
        limit: Some(SmartPlaylistLimit {
            count: std::num::NonZeroU32::new(5).expect("non-zero"),
            selection: SmartPlaylistLimitSelection::MostRecentlyAdded,
        }),
    };
    runtime
        .handle_command(ApplicationCommand::UpdateSmartPlaylist {
            smart_playlist_id: smart_id_value,
            name: "Limited".to_owned(),
            rules: limited_rules,
        })
        .expect("update smart playlist");
    assert_eq!(
        runtime.smart_playlist_track_status(smart_id_value, matching.id),
        SmartPlaylistTrackStatus::RequiresFullRebuild
    );
}

#[test]
fn seeding_default_smart_playlists_installs_the_starter_set() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .seed_default_smart_playlists()
        .expect("seed succeeds on fresh library");

    let names: Vec<&str> = runtime
        .smart_playlists()
        .iter()
        .map(|smart| smart.name.as_str())
        .collect();
    assert_eq!(
        names,
        vec![
            "Recently Added",
            "Recently Played",
            "Top 25 Most Played",
            "4+ Stars",
            "Unplayed",
            "Missing Tags",
            "Missing Files",
        ]
    );
}

#[test]
fn smart_playlist_evaluation_uses_injected_clock() {
    use std::num::NonZeroU32;
    use std::time::{Duration, SystemTime};

    let last_played = SystemTime::UNIX_EPOCH + Duration::from_secs(1_000_000);
    let store = Arc::new(InMemoryLibraryStore::new());

    let mut track = test_track(track_id(1), "track.flac");
    track.statistics.last_played_at = Some(last_played);
    store.save_track(track).expect("save track");

    let recently_played = SmartPlaylist {
        id: smart_id(1),
        name: "Recently Played".to_owned(),
        parent_folder_id: None,
        position: 0,
        rules: SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::All,
            rules: vec![SmartPlaylistRule::DateInLast {
                field: SmartPlaylistDateField::LastPlayed,
                days: NonZeroU32::new(7).expect("positive days"),
            }],
            limit: None,
        },
    };
    store
        .save_smart_playlist(recently_played)
        .expect("save smart playlist");

    let fake_clock = Arc::new(FakeClock::new(last_played));
    let runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize")
        .with_clock(fake_clock.clone());

    fake_clock.set(last_played + Duration::from_secs(86_400));
    assert_eq!(
        runtime.smart_playlist_matching_tracks(smart_id(1)).len(),
        1,
        "track played within the window must match"
    );

    fake_clock.set(last_played + Duration::from_secs(86_400 * 10));
    assert_eq!(
        runtime.smart_playlist_matching_tracks(smart_id(1)).len(),
        0,
        "track played outside the window must not match"
    );
}

#[derive(Debug)]
struct FakeClock {
    now: Mutex<std::time::SystemTime>,
}

impl FakeClock {
    fn new(now: std::time::SystemTime) -> Self {
        Self {
            now: Mutex::new(now),
        }
    }

    fn set(&self, now: std::time::SystemTime) {
        *self.now.lock().expect("fake clock lock") = now;
    }
}

impl Clock for FakeClock {
    fn now(&self) -> std::time::SystemTime {
        *self.now.lock().expect("fake clock lock")
    }
}

#[derive(Debug, Default)]
struct FakeMonotonicClock {
    now: Mutex<std::time::Duration>,
}

impl FakeMonotonicClock {
    fn advance(&self, elapsed: std::time::Duration) {
        let mut now = self.now.lock().expect("fake monotonic clock lock");
        *now = now.saturating_add(elapsed);
    }
}

impl MonotonicClock for FakeMonotonicClock {
    fn now(&self) -> std::time::Duration {
        *self.now.lock().expect("fake monotonic clock lock")
    }
}

fn advance_playback_tick(
    runtime: &mut ApplicationRuntime,
    clock: &FakeMonotonicClock,
    elapsed: std::time::Duration,
) -> super::ApplicationRuntimeResult<()> {
    clock.advance(elapsed);
    runtime.on_playback_tick()
}

#[test]
fn runtime_rejects_smart_playlist_without_rules() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    let empty_rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: Vec::new(),
        limit: None,
    };
    assert_eq!(
        runtime.handle_command(ApplicationCommand::CreateSmartPlaylist {
            name: "Empty".to_owned(),
            parent_folder_id: None,
            rules: empty_rules,
        }),
        Err(ApplicationRuntimeError::InvalidSmartPlaylistRules)
    );
}

#[test]
fn new_siblings_get_distinct_positions_across_types() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Mixes".to_owned(),
            parent_folder_id: None,
        })
        .expect("folder");
    runtime
        .handle_command(ApplicationCommand::CreatePlaylist {
            name: "Manual".to_owned(),
            parent_folder_id: None,
        })
        .expect("playlist");
    runtime
        .handle_command(ApplicationCommand::CreateSmartPlaylist {
            name: "Smart".to_owned(),
            parent_folder_id: None,
            rules: test_rule_set(),
        })
        .expect("smart");

    assert_eq!(runtime.playlist_folders()[0].position, 0);
    assert_eq!(runtime.playlists()[0].position, 1);
    assert_eq!(runtime.smart_playlists()[0].position, 2);
}

#[test]
fn moving_a_playlist_within_its_folder_reorders_siblings() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    for name in ["A", "B", "C"] {
        runtime
            .handle_command(ApplicationCommand::CreatePlaylist {
                name: name.to_owned(),
                parent_folder_id: None,
            })
            .expect("create");
    }
    let playlist_b_id = runtime
        .playlists()
        .iter()
        .find(|playlist| playlist.name == "B")
        .map(|playlist| playlist.id)
        .expect("playlist B exists");

    runtime
        .handle_command(ApplicationCommand::MovePlaylistItem {
            item: PlaylistItem::Playlist(playlist_b_id),
            target_parent_folder_id: None,
            position: 0,
        })
        .expect("move within folder");

    let mut ordered: Vec<&Playlist> = runtime.playlists().iter().collect();
    ordered.sort_by_key(|playlist| playlist.position);
    let names: Vec<&str> = ordered
        .iter()
        .map(|playlist| playlist.name.as_str())
        .collect();
    assert_eq!(names, vec!["B", "A", "C"]);
}

#[test]
fn moving_a_playlist_across_folders_resequences_both_sides() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Folder".to_owned(),
            parent_folder_id: None,
        })
        .expect("folder");
    let folder = folder_id(1);
    runtime
        .handle_command(ApplicationCommand::CreatePlaylist {
            name: "Top A".to_owned(),
            parent_folder_id: None,
        })
        .expect("top a");
    runtime
        .handle_command(ApplicationCommand::CreatePlaylist {
            name: "Top B".to_owned(),
            parent_folder_id: None,
        })
        .expect("top b");
    let top_a_id = runtime
        .playlists()
        .iter()
        .find(|playlist| playlist.name == "Top A")
        .map(|playlist| playlist.id)
        .expect("Top A exists");

    runtime
        .handle_command(ApplicationCommand::MovePlaylistItem {
            item: PlaylistItem::Playlist(top_a_id),
            target_parent_folder_id: Some(folder),
            position: 0,
        })
        .expect("move into folder");

    let in_folder: Vec<&Playlist> = runtime
        .playlists()
        .iter()
        .filter(|playlist| playlist.parent_folder_id == Some(folder))
        .collect();
    assert_eq!(in_folder.len(), 1);
    assert_eq!(in_folder[0].name, "Top A");
    assert_eq!(in_folder[0].position, 0);

    let at_top: Vec<&Playlist> = runtime
        .playlists()
        .iter()
        .filter(|playlist| playlist.parent_folder_id.is_none())
        .collect();
    assert_eq!(at_top.len(), 1);
    assert_eq!(at_top[0].name, "Top B");
    assert_eq!(at_top[0].position, 1);
    assert_eq!(runtime.playlist_folders()[0].position, 0);
}

#[test]
fn moving_a_folder_into_its_own_descendant_is_rejected() {
    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    runtime
        .handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Outer".to_owned(),
            parent_folder_id: None,
        })
        .expect("outer");
    let outer = folder_id(1);
    runtime
        .handle_command(ApplicationCommand::CreatePlaylistFolder {
            name: "Inner".to_owned(),
            parent_folder_id: Some(outer),
        })
        .expect("inner");
    let inner = folder_id(2);

    assert_eq!(
        runtime.handle_command(ApplicationCommand::MovePlaylistItem {
            item: PlaylistItem::Folder(outer),
            target_parent_folder_id: Some(inner),
            position: 0,
        }),
        Err(ApplicationRuntimeError::PlaylistFolderWouldCycle)
    );
}

fn folder_id(value: i64) -> PlaylistFolderId {
    match PlaylistFolderId::new(value) {
        Some(folder_id) => folder_id,
        None => unreachable!("hard-coded positive folder id should be valid"),
    }
}

fn smart_id(value: i64) -> SmartPlaylistId {
    match SmartPlaylistId::new(value) {
        Some(smart_id) => smart_id,
        None => unreachable!("hard-coded positive smart-playlist id should be valid"),
    }
}

fn test_rule_set() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Artist,
            operator: SmartPlaylistTextOperator::Contains,
            value: "Portishead".to_owned(),
        }],
        limit: None,
    }
}

#[derive(Debug)]
struct TestSettingsStore {
    settings: Mutex<UserSettings>,
}

impl TestSettingsStore {
    fn new(settings: UserSettings) -> Self {
        Self {
            settings: Mutex::new(settings),
        }
    }

    fn settings_guard(&self) -> SettingsResult<MutexGuard<'_, UserSettings>> {
        self.settings
            .lock()
            .map_err(|_| SettingsError::StoreUnavailable)
    }
}

impl SettingsStore for TestSettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings> {
        Ok(self.settings_guard()?.clone())
    }

    fn save_settings(&self, settings: UserSettings) -> SettingsResult<()> {
        *self.settings_guard()? = settings;
        Ok(())
    }
}

/// Loads cleanly but rejects every save, standing in for a disk-full or
/// read-only `settings.toml` so a live persistence failure can be observed.
#[derive(Debug, Default)]
struct FailingSettingsStore;

impl SettingsStore for FailingSettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings> {
        Ok(UserSettings::default())
    }

    fn save_settings(&self, _settings: UserSettings) -> SettingsResult<()> {
        Err(SettingsError::SaveFailed)
    }
}

#[derive(Debug)]
struct TestMetadataService;

impl MetadataService for TestMetadataService {
    fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
        Ok(InitialTags {
            metadata: TrackMetadata {
                title: Some("Track".to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::new(3).expect("valid test rating"),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, _path: &Path, _change: MetadataChange) -> MetadataResult<()> {
        Ok(())
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

#[derive(Debug)]
struct RecordingMetadataService {
    fail_rating_writes: bool,
    fail_metadata_writes: bool,
    rating_writes: Mutex<Vec<(PathBuf, Rating)>>,
    metadata_writes: Mutex<Vec<(PathBuf, MetadataChange)>>,
    artwork_writes: Mutex<Vec<Option<Vec<u8>>>>,
}

impl RecordingMetadataService {
    fn new(fail_rating_writes: bool) -> Self {
        Self {
            fail_rating_writes,
            fail_metadata_writes: false,
            rating_writes: Mutex::new(Vec::new()),
            metadata_writes: Mutex::new(Vec::new()),
            artwork_writes: Mutex::new(Vec::new()),
        }
    }

    fn with_metadata_write_failure() -> Self {
        Self {
            fail_rating_writes: false,
            fail_metadata_writes: true,
            rating_writes: Mutex::new(Vec::new()),
            metadata_writes: Mutex::new(Vec::new()),
            artwork_writes: Mutex::new(Vec::new()),
        }
    }

    fn rating_writes(&self) -> Vec<(PathBuf, Rating)> {
        self.rating_writes
            .lock()
            .expect("rating writes lock is available")
            .clone()
    }

    fn metadata_writes(&self) -> Vec<(PathBuf, MetadataChange)> {
        self.metadata_writes
            .lock()
            .expect("metadata writes lock is available")
            .clone()
    }

    fn artwork_writes(&self) -> Vec<Option<Vec<u8>>> {
        self.artwork_writes
            .lock()
            .expect("artwork writes lock is available")
            .clone()
    }
}

impl MetadataService for RecordingMetadataService {
    fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
        Ok(InitialTags {
            metadata: TrackMetadata {
                title: Some("Track".to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::new(3).expect("valid test rating"),
            has_embedded_artwork: false,
        })
    }

    fn write_metadata(&self, path: &Path, change: MetadataChange) -> MetadataResult<()> {
        self.metadata_writes
            .lock()
            .expect("metadata writes lock is available")
            .push((path.to_path_buf(), change));
        if self.fail_metadata_writes {
            Err(MetadataError::WriteFailed)
        } else {
            Ok(())
        }
    }

    fn write_rating(&self, path: &Path, rating: Rating) -> MetadataResult<()> {
        self.rating_writes
            .lock()
            .expect("rating writes lock is available")
            .push((path.to_path_buf(), rating));
        if self.fail_rating_writes {
            Err(MetadataError::WriteFailed)
        } else {
            Ok(())
        }
    }

    fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
        Ok(None)
    }

    fn write_artwork(&self, _path: &Path, artwork: Option<Vec<u8>>) -> MetadataResult<()> {
        self.artwork_writes
            .lock()
            .expect("artwork writes lock is available")
            .push(artwork);
        Ok(())
    }
}

#[test]
fn on_playback_tick_registers_play_after_threshold() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("song.flac"), b"not real audio").expect("write fake track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(1);
    let mut track = test_track(id, "song.flac");
    track.metadata.duration = Some(std::time::Duration::from_secs(60));
    assert_eq!(store.save_track(track), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play track");

    // Threshold for a 60s track is 30s. 29 ticks of 1s each must
    // not be enough to register the play.
    for _ in 0..29 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("tick");
    }
    let track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == id)
        .expect("track present");
    assert_eq!(
        track.statistics.play_count, 0,
        "play count must not increment before threshold cross"
    );

    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(1),
    )
    .expect("tick that crosses threshold");
    let track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == id)
        .expect("track present");
    assert_eq!(
        track.statistics.play_count, 1,
        "play count must increment exactly once when threshold is crossed"
    );
    assert!(
        track.statistics.last_played_at.is_some(),
        "last_played_at must be set when play registers"
    );

    // Further ticks past threshold must not re-increment within the
    // same listening session.
    for _ in 0..60 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("post-threshold tick");
    }
    let track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == id)
        .expect("track present");
    assert_eq!(
        track.statistics.play_count, 1,
        "play must register exactly once per session"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn delayed_playback_tick_accounts_real_monotonic_interval() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("song.flac"), b"not real audio").expect("write fake track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(1);
    let mut track = test_track(id, "song.flac");
    track.metadata.duration = Some(std::time::Duration::from_secs(60));
    assert_eq!(store.save_track(track), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play track");
    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(30),
    )
    .expect("single delayed tick");

    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 1);
    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn pause_and_seek_forward_do_not_count_as_listening() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("song.flac"), b"not real audio").expect("write fake track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(1);
    let mut track = test_track(id, "song.flac");
    track.metadata.duration = Some(std::time::Duration::from_secs(60));
    assert_eq!(store.save_track(track), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play track");
    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(10),
    )
    .expect("listen before pause");
    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::Pause))
        .expect("pause");
    monotonic_clock.advance(std::time::Duration::from_secs(100));
    runtime.on_playback_tick().expect("paused tick");
    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::Resume))
        .expect("resume");
    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(19),
    )
    .expect("listen after resume");
    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::Seek(
            std::time::Duration::from_secs(59),
        )))
        .expect("seek forward");
    runtime
        .on_playback_tick()
        .expect("same-instant tick after seek");
    assert_eq!(
        runtime.library_tracks()[0].statistics.play_count,
        0,
        "paused duration and seek distance must not cross the threshold"
    );

    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(1),
    )
    .expect("one final listened second");
    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 1);
    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn failed_play_registration_retries_after_stop_without_double_incrementing() {
    use std::sync::atomic::Ordering;

    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("song.flac"), b"not real audio").expect("write fake track");

    let inner = InMemoryLibraryStore::new();
    let id = track_id(1);
    let mut track = test_track(id, "song.flac");
    track.metadata.duration = Some(std::time::Duration::from_secs(60));
    inner.save_track(track).expect("seed inner store");
    let counts = Arc::new(StoreCallCounts::default());
    let store = Arc::new(CallCountingLibraryStore {
        inner,
        counts: counts.clone(),
        statistics_failures_remaining: std::sync::atomic::AtomicUsize::new(1),
        tracks_failures_remaining: std::sync::atomic::AtomicUsize::new(0),
    });
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play track");
    assert_eq!(
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(30),
        ),
        Err(ApplicationRuntimeError::LibraryStoreFailed)
    );
    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 0);
    assert_eq!(
        store
            .track(id)
            .expect("stored track")
            .expect("track exists")
            .statistics
            .play_count,
        0
    );
    assert_eq!(runtime.notifications().persistent_stack().len(), 1);
    assert_eq!(
        runtime.notifications().persistent_stack()[0].category,
        NotificationCategory::PlaybackStatistics
    );

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::Stop))
        .expect("stop retries pending registration");
    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 1);
    assert_eq!(
        store
            .track(id)
            .expect("stored track")
            .expect("track exists")
            .statistics
            .play_count,
        1
    );
    assert!(runtime.notifications().persistent_stack().is_empty());
    assert_eq!(counts.statistics_updates.load(Ordering::SeqCst), 2);

    runtime
        .on_playback_tick()
        .expect("stopped retry is a no-op");
    runtime
        .on_playback_tick()
        .expect("repeated retry is a no-op");
    assert_eq!(runtime.library_tracks()[0].statistics.play_count, 1);
    assert_eq!(counts.statistics_updates.load(Ordering::SeqCst), 2);
    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn registering_a_play_fires_track_data_observer() {
    // Regression for issue #46: committing a play increment must
    // notify the UI so the table row repaints its play-count and
    // last-played columns live, rather than only after a restart.
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("song.flac"), b"not real audio").expect("write fake track");

    let store = Arc::new(InMemoryLibraryStore::new());
    let id = track_id(1);
    let mut track = test_track(id, "song.flac");
    track.metadata.duration = Some(std::time::Duration::from_secs(60));
    assert_eq!(store.save_track(track), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    let observed: Arc<std::sync::Mutex<Vec<TrackId>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_clone = observed.clone();
    runtime.set_track_data_observer(Box::new(move |id| {
        observed_clone.lock().expect("lock").push(id);
    }));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: id,
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play track");

    // Ticks below the 30s threshold mutate no statistics, so the
    // observer must stay silent.
    for _ in 0..29 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("tick");
    }
    assert!(
        observed.lock().expect("lock").is_empty(),
        "observer must not fire before a play is committed"
    );

    // The tick that crosses the threshold commits the play and must
    // notify the observer exactly once, for the played track.
    advance_playback_tick(
        &mut runtime,
        monotonic_clock.as_ref(),
        std::time::Duration::from_secs(1),
    )
    .expect("tick that crosses threshold");
    assert_eq!(
        observed.lock().expect("lock").as_slice(),
        &[id],
        "committing a play must fire the data observer for that track"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn skip_current_track_registers_skip_before_play_threshold() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio").expect("write b");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut a = test_track(track_id(1), "a.flac");
    a.metadata.duration = Some(std::time::Duration::from_secs(60));
    let mut b = test_track(track_id(2), "b.flac");
    b.metadata.duration = Some(std::time::Duration::from_secs(60));
    assert_eq!(store.save_track(a), Ok(()));
    assert_eq!(store.save_track(b), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play A");

    // Listen briefly — well short of the 30s threshold — then skip.
    for _ in 0..5 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("tick");
    }

    runtime
        .handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        ))
        .expect("skip current track");

    let track_a = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("track A present");
    assert_eq!(
        track_a.statistics.skip_count, 1,
        "skip must increment when threshold not yet reached"
    );
    assert!(
        track_a.statistics.last_skipped_at.is_some(),
        "last_skipped_at must be set on skip"
    );
    assert_eq!(
        track_a.statistics.play_count, 0,
        "skip must not also register a play"
    );

    // Track B is now playing as a result of the advance.
    match runtime.playback_state() {
        PlaybackState::Playing {
            track_id: playing, ..
        } => assert_eq!(playing, track_id(2)),
        other => panic!("expected B to be playing, got {other:?}"),
    }

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn skip_current_track_does_not_register_skip_after_play_threshold() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio").expect("write b");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut a = test_track(track_id(1), "a.flac");
    a.metadata.duration = Some(std::time::Duration::from_secs(60));
    let b = test_track(track_id(2), "b.flac");
    assert_eq!(store.save_track(a), Ok(()));
    assert_eq!(store.save_track(b), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play A");

    // Cross the play threshold for the 60s track.
    for _ in 0..30 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("tick");
    }

    runtime
        .handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        ))
        .expect("skip after play registered");

    let track_a = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("track A present");
    assert_eq!(
        track_a.statistics.play_count, 1,
        "play already counted before skip"
    );
    assert_eq!(
        track_a.statistics.skip_count, 0,
        "post-threshold next must not increment skip"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn play_next_track_never_registers_skip() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio").expect("write b");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut a = test_track(track_id(1), "a.flac");
    a.metadata.duration = Some(std::time::Duration::from_secs(60));
    let b = test_track(track_id(2), "b.flac");
    assert_eq!(store.save_track(a), Ok(()));
    assert_eq!(store.save_track(b), Ok(()));
    let monotonic_clock = Arc::new(FakeMonotonicClock::default());

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_monotonic_clock(monotonic_clock.clone())
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play A");

    // Briefly listen — well short of the play threshold.
    for _ in 0..5 {
        advance_playback_tick(
            &mut runtime,
            monotonic_clock.as_ref(),
            std::time::Duration::from_secs(1),
        )
        .expect("tick");
    }

    // EOS-style auto-advance must never affect skip statistics,
    // regardless of how much of the previous track was listened.
    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayNextTrack))
        .expect("auto-advance");

    let track_a = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("track A present");
    assert_eq!(
        track_a.statistics.skip_count, 0,
        "auto-advance must never inflate skip count"
    );
    assert_eq!(
        track_a.statistics.play_count, 0,
        "auto-advance below threshold must not register a play either"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn on_playback_tick_does_not_accumulate_when_stopped() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    // No PlayTrack — runtime is in the Stopped state, no session.
    for _ in 0..100 {
        runtime
            .on_playback_tick()
            .expect("tick is a no-op while stopped");
    }
    assert!(
        runtime.playback_session.is_none(),
        "no session should be created when nothing is playing"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn play_track_starts_session_immediately_so_rapid_skip_counts() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio").expect("write b");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut a = test_track(track_id(1), "a.flac");
    a.metadata.duration = Some(std::time::Duration::from_secs(60));
    let b = test_track(track_id(2), "b.flac");
    assert_eq!(store.save_track(a), Ok(()));
    assert_eq!(store.save_track(b), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play A");

    // No ticks have fired yet. Skip immediately. The session must
    // already exist (populated synchronously by play_track) so the
    // skip is captured rather than silently dropped.
    runtime
        .handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        ))
        .expect("immediate skip");

    let track_a = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("track A present");
    assert_eq!(
        track_a.statistics.skip_count, 1,
        "skip must register even with zero listened time"
    );

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn rapid_double_skip_does_not_double_count() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");
    std::fs::write(root.join("b.flac"), b"audio").expect("write b");
    std::fs::write(root.join("c.flac"), b"audio").expect("write c");

    let store = Arc::new(InMemoryLibraryStore::new());
    let mut a = test_track(track_id(1), "a.flac");
    a.metadata.duration = Some(std::time::Duration::from_secs(60));
    let mut b = test_track(track_id(2), "b.flac");
    b.metadata.duration = Some(std::time::Duration::from_secs(60));
    let c = test_track(track_id(3), "c.flac");
    assert_eq!(store.save_track(a), Ok(()));
    assert_eq!(store.save_track(b), Ok(()));
    assert_eq!(store.save_track(c), Ok(()));

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("library services initialize")
    .with_playback_service(Box::new(NullPlaybackService::new()));

    runtime
        .handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: track_id(1),
            queue: PlaybackQueueRequest::Library,
        }))
        .expect("play A");
    runtime
        .handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        ))
        .expect("first skip — A → B");
    // Immediately skip again before any tick has accumulated time
    // on B. A second skip on A would be a double-count bug; this
    // exercises the "play_track installs a fresh session" guard.
    runtime
        .handle_command(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        ))
        .expect("second skip — B → C");

    let track_a = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(1))
        .expect("track A present");
    let track_b = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id(2))
        .expect("track B present");
    assert_eq!(track_a.statistics.skip_count, 1, "A skipped exactly once");
    assert_eq!(track_b.statistics.skip_count, 1, "B skipped exactly once");

    std::fs::remove_dir_all(root).expect("remove test library");
}

fn unique_test_directory() -> PathBuf {
    static NEXT_SUFFIX: AtomicU64 = AtomicU64::new(0);

    let unique_suffix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock after unix epoch")
        .as_nanos();
    let sequence = NEXT_SUFFIX.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("sustain_runtime_test_{unique_suffix}_{sequence}"))
}

fn positive_track_id() -> TrackId {
    track_id(1)
}

fn track_id(value: i64) -> TrackId {
    match TrackId::new(value) {
        Some(track_id) => track_id,
        None => unreachable!("hard-coded positive track id should be valid"),
    }
}

fn playlist_id(value: i64) -> PlaylistId {
    match PlaylistId::new(value) {
        Some(playlist_id) => playlist_id,
        None => unreachable!("hard-coded positive playlist id should be valid"),
    }
}

fn assert_playlist_track_ids(
    playlists: &[Playlist],
    playlist_id: PlaylistId,
    expected_track_ids: &[TrackId],
) {
    let playlist = playlists
        .iter()
        .find(|playlist| playlist.id == playlist_id)
        .expect("playlist exists");
    let track_ids = playlist
        .entries
        .iter()
        .map(|entry| entry.track_id)
        .collect::<Vec<_>>();
    let positions = playlist
        .entries
        .iter()
        .map(|entry| entry.position)
        .collect::<Vec<_>>();

    assert_eq!(track_ids, expected_track_ids);
    assert_eq!(
        positions,
        (0..expected_track_ids.len() as u32).collect::<Vec<_>>()
    );
}

#[test]
fn apply_track_updated_reloads_from_store_and_fires_observer() {
    let root = unique_test_directory();
    std::fs::create_dir_all(&root).expect("create test library");
    std::fs::write(root.join("a.flac"), b"audio").expect("write a");

    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    let mut original = test_track(track_id(1), "a.flac");
    original.metadata.title = Some("Before".to_owned());
    store.save_track(original.clone()).expect("seed");

    let mut runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::with_library_path(Some(root.clone())),
    )))
    .expect("load settings")
    .with_library_services(store.clone(), Arc::new(TestMetadataService))
    .expect("library services initialize");

    // The in-memory library copy starts with the seeded value.
    assert_eq!(
        runtime
            .library_tracks()
            .iter()
            .find(|track| track.id == track_id(1))
            .and_then(|t| t.metadata.title.as_deref()),
        Some("Before")
    );

    // Mutate the store out-of-band (simulates a worker write).
    store
        .apply_track_metadata_change(
            original.id,
            &MetadataChange {
                title: FieldChange::Set("After".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("mutate");

    // Hook the observer so we can prove it ran with the right id.
    let observed: Arc<std::sync::Mutex<Vec<TrackId>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let observed_clone = observed.clone();
    runtime.set_track_data_observer(Box::new(move |id| {
        observed_clone.lock().expect("lock").push(id);
    }));

    runtime.apply_track_updated(track_id(1));

    assert_eq!(
        runtime
            .library_tracks()
            .iter()
            .find(|track| track.id == track_id(1))
            .and_then(|t| t.metadata.title.as_deref()),
        Some("After"),
        "in-memory copy must be refreshed from the store"
    );
    assert_eq!(observed.lock().expect("lock").as_slice(), &[track_id(1)]);

    std::fs::remove_dir_all(root).expect("remove test library");
}

#[test]
fn apply_track_updated_performs_one_keyed_lookup_and_no_full_scan() {
    use std::sync::atomic::Ordering;

    let inner = InMemoryLibraryStore::new();
    inner
        .save_track(test_track(track_id(7), "a.flac"))
        .expect("seed inner store");

    let counts = Arc::new(StoreCallCounts::default());
    let store: Arc<dyn LibraryStore> = Arc::new(CallCountingLibraryStore {
        inner,
        counts: counts.clone(),
        statistics_failures_remaining: std::sync::atomic::AtomicUsize::new(0),
        tracks_failures_remaining: std::sync::atomic::AtomicUsize::new(0),
    });

    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("install library services");

    // Setup (load_library_tracks) legitimately calls tracks() once; reset
    // so the assertion measures only the per-track refresh.
    counts.track.store(0, Ordering::SeqCst);
    counts.tracks.store(0, Ordering::SeqCst);

    runtime.apply_track_updated(track_id(7));

    assert_eq!(
        counts.track.load(Ordering::SeqCst),
        1,
        "applying one update must perform exactly one keyed track(id) lookup"
    );
    assert_eq!(
        counts.tracks.load(Ordering::SeqCst),
        0,
        "applying one update must never decode the whole library via tracks()"
    );
}

#[test]
fn library_track_keyed_lookup_finds_by_id_and_reports_absence() {
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    for id in [1, 2, 3] {
        store
            .save_track(test_track(track_id(id), &format!("track-{id}.flac")))
            .expect("seed track");
    }
    let runtime = ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(
        UserSettings::default(),
    )))
    .expect("load settings")
    .with_library_services(store, Arc::new(TestMetadataService))
    .expect("install library services");

    assert_eq!(
        runtime.library_track(track_id(2)).map(|track| track.id),
        Some(track_id(2))
    );
    assert!(runtime.library_track(track_id(99)).is_none());
}

fn runtime_with_one_track(library_path: Option<PathBuf>) -> ApplicationRuntime {
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    store
        .save_track(test_track(track_id(1), "a.flac"))
        .expect("seed track");
    let settings = match library_path {
        Some(path) => UserSettings::with_library_path(Some(path)),
        None => UserSettings::default(),
    };
    ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
        .expect("load settings")
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize")
}

fn library_has_track(runtime: &ApplicationRuntime, id: TrackId) -> bool {
    runtime.library_tracks().iter().any(|track| track.id == id)
}

fn dummy_file_identity() -> FileIdentity {
    FileIdentity {
        device: 1,
        inode: 1,
    }
}

#[test]
fn move_to_trash_removes_the_row_only_after_a_successful_trash() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut runtime = runtime_with_one_track(Some(PathBuf::from("/library")));
    let trash_calls = Arc::new(AtomicUsize::new(0));
    let calls = trash_calls.clone();

    let result = runtime.move_track_to_trash_with(
        track_id(1),
        |_| Ok(Some(dummy_file_identity())),
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );

    assert_eq!(result, Ok(()));
    assert!(!library_has_track(&runtime, track_id(1)));
    assert_eq!(trash_calls.load(Ordering::SeqCst), 1);
}

#[test]
fn move_to_trash_prunes_empty_folders_only_in_managed_mode() {
    let root = unique_test_directory();
    let track_path = root.join("Loose/Album/song.flac");
    std::fs::create_dir_all(track_path.parent().expect("track parent"))
        .expect("create track parent");
    std::fs::write(&track_path, b"audio").expect("write track");
    let track_id = track_id(1);
    let store: Arc<dyn LibraryStore> = Arc::new(InMemoryLibraryStore::new());
    store
        .save_track(test_track(track_id, "Loose/Album/song.flac"))
        .expect("seed track");
    let mut settings = UserSettings::with_library_path(Some(root.clone()));
    settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;
    let mut runtime =
        ApplicationRuntime::with_settings_store(Box::new(TestSettingsStore::new(settings)))
            .expect("load settings")
            .with_library_services(store, Arc::new(TestMetadataService))
            .expect("library services initialize");

    assert_eq!(
        runtime.move_track_to_trash_with(
            track_id,
            |_| Ok(Some(dummy_file_identity())),
            |path, _| std::fs::remove_file(path).map_err(|_| ()),
        ),
        Ok(())
    );

    assert!(root.exists());
    assert!(!root.join("Loose").exists());
    assert!(!library_has_track(&runtime, track_id));

    std::fs::remove_dir_all(root).expect("remove test root");
}

#[test]
fn move_to_trash_keeps_the_row_when_the_trash_backend_fails() {
    let mut runtime = runtime_with_one_track(Some(PathBuf::from("/library")));

    let result = runtime.move_track_to_trash_with(
        track_id(1),
        |_| Ok(Some(dummy_file_identity())),
        |_, _| Err(()),
    );

    assert_eq!(result, Err(ApplicationRuntimeError::TrackTrashFailed));
    assert!(
        library_has_track(&runtime, track_id(1)),
        "a failed trash must not delete the library record"
    );
}

#[test]
fn move_to_trash_removes_a_proven_absent_file_without_trashing() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let mut runtime = runtime_with_one_track(Some(PathBuf::from("/library")));
    let trash_calls = Arc::new(AtomicUsize::new(0));
    let calls = trash_calls.clone();

    let result = runtime.move_track_to_trash_with(
        track_id(1),
        |_| Ok(None),
        move |_, _| {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        },
    );

    assert_eq!(result, Ok(()));
    assert!(!library_has_track(&runtime, track_id(1)));
    assert_eq!(
        trash_calls.load(Ordering::SeqCst),
        0,
        "a confirmed-absent file is removed from the library without a trash call"
    );
}

#[test]
fn move_to_trash_keeps_the_row_on_a_probe_error() {
    let mut runtime = runtime_with_one_track(Some(PathBuf::from("/library")));

    // A permission or transient-I/O probe failure must not read as "the
    // file is gone".
    let result = runtime.move_track_to_trash_with(track_id(1), |_| Err(()), |_, _| Ok(()));

    assert_eq!(result, Err(ApplicationRuntimeError::TrackTrashFailed));
    assert!(library_has_track(&runtime, track_id(1)));
}

#[test]
fn move_to_trash_fails_closed_when_the_library_root_is_unresolved() {
    let mut runtime = runtime_with_one_track(None);

    // The probe would say "present", but the path cannot be resolved
    // without a library root, so the row must be preserved.
    let result = runtime.move_track_to_trash_with(
        track_id(1),
        |_| Ok(Some(dummy_file_identity())),
        |_, _| Ok(()),
    );

    assert_eq!(result, Err(ApplicationRuntimeError::TrackTrashFailed));
    assert!(library_has_track(&runtime, track_id(1)));
}

#[test]
fn file_presence_probes_distinguish_reachable_files_from_directory_entries() {
    let dir = unique_test_directory();
    std::fs::create_dir_all(&dir).expect("create probe directory");
    let present = dir.join("here.flac");
    std::fs::write(&present, b"x").expect("write present file");
    let absent = dir.join("gone.flac");

    assert_eq!(probe_file_presence(&present), FilePresence::Present);
    assert_eq!(probe_file_presence(&absent), FilePresence::Absent);
    let dangling_link = dir.join("dangling");
    std::os::unix::fs::symlink(&absent, &dangling_link).expect("create dangling symlink");
    assert_eq!(probe_file_presence(&dangling_link), FilePresence::Absent);
    assert_eq!(
        probe_path_entry_presence(&dangling_link),
        FilePresence::Present
    );

    std::fs::remove_dir_all(&dir).expect("remove probe directory");
}

fn test_track(track_id: TrackId, path: &str) -> Track {
    Track {
        id: track_id,
        location: track_location(path),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

fn test_scanned_track(path: &str) -> ScannedTrack {
    ScannedTrack {
        relative_path: TrackRelativePath::new(PathBuf::from(path)).expect("valid relative path"),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        file_size_bytes: None,
        has_embedded_artwork: false,
        file_modified_at: None,
    }
}

fn track_location(path: &str) -> TrackLocation {
    TrackLocation::available(relative_path(path))
}

fn missing_track_location(path: &str) -> TrackLocation {
    TrackLocation::missing(relative_path(path))
}

fn relative_path(path: &str) -> super::TrackRelativePath {
    super::TrackRelativePath::new(PathBuf::from(path)).expect("test path is relative")
}

fn hex_path(path: &str) -> String {
    path.as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn _assert_store_result_is_public<T>(result: StoreResult<T>) -> StoreResult<T> {
    result
}

fn _assert_playlist_types_are_public(playlist: Playlist, playlist_id: PlaylistId) {
    let _value = (playlist, playlist_id);
}

fn _assert_metadata_error_is_public(error: MetadataError) -> MetadataError {
    error
}

#[test]
fn request_run_decides_per_global_setting_and_target() {
    // The decision tree for the per-set right-click actions:
    //   * Single(cap) with the matching global toggle on
    //                              -> DeniedBackgroundEnabled
    //   * empty track set / folder -> TargetEmpty
    //   * scheduler not started    -> SchedulerUnavailable
    // The Accepted path needs a live scheduler and is covered by
    // the scheduler's own integration tests.
    //
    // `All` is also exercised here: even with every global toggle
    // on the runtime accepts the request and forwards the full
    // mask to the scheduler (the explicit run is the user's
    // override for the bundle case).
    let store = Arc::new(InMemoryLibraryStore::new());

    let track = Track {
        id: track_id(1),
        location: track_location("t.flac"),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    store.save_track(track.clone()).expect("save");
    let playlist = Playlist {
        id: PlaylistId::new(1).expect("non-zero"),
        name: "Mix Set".to_owned(),
        parent_folder_id: None,
        position: 0,
        entries: vec![sustain_domain::PlaylistEntry {
            playlist_id: PlaylistId::new(1).expect("non-zero"),
            track_id: track.id,
            position: 0,
        }],
    };
    store.save_playlist(playlist.clone()).expect("save");

    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    // Background analysis off, no scheduler started -> the
    // scheduler is missing, so we surface that uniformly.
    assert_eq!(
        runtime.request_playlist_analysis_run(
            PlaylistItem::Playlist(playlist.id),
            AnalysisRunRequest::Single(AnalysisCapability::Bpm),
        ),
        RunDecision::SchedulerUnavailable
    );

    // Flip background BPM on -> deny path fires before the
    // scheduler check (the rule is purely about the global
    // toggle).
    let mut settings = runtime.settings().clone();
    settings.analysis.bpm = true;
    runtime
        .handle_command(ApplicationCommand::UpdateSettings(settings.clone()))
        .expect("apply settings");
    assert_eq!(
        runtime.request_playlist_analysis_run(
            PlaylistItem::Playlist(playlist.id),
            AnalysisRunRequest::Single(AnalysisCapability::Bpm),
        ),
        RunDecision::DeniedBackgroundEnabled
    );

    // Key capability is still off globally -> deny does not
    // trigger, but the scheduler is still missing.
    assert_eq!(
        runtime.request_playlist_analysis_run(
            PlaylistItem::Playlist(playlist.id),
            AnalysisRunRequest::Single(AnalysisCapability::Key),
        ),
        RunDecision::SchedulerUnavailable
    );

    // All-capabilities request ignores every per-capability
    // global toggle: the user explicitly asked for the bundle.
    let mut settings = runtime.settings().clone();
    settings.analysis.key = true;
    settings.analysis.audio = true;
    runtime
        .handle_command(ApplicationCommand::UpdateSettings(settings))
        .expect("apply settings");
    assert_eq!(
        runtime.request_playlist_analysis_run(
            PlaylistItem::Playlist(playlist.id),
            AnalysisRunRequest::All,
        ),
        RunDecision::SchedulerUnavailable
    );

    // Unknown playlist id -> TargetEmpty, regardless of which
    // request the user picked.
    let phantom = PlaylistId::new(999).expect("non-zero");
    assert_eq!(
        runtime.request_playlist_analysis_run(
            PlaylistItem::Playlist(phantom),
            AnalysisRunRequest::Single(AnalysisCapability::Key),
        ),
        RunDecision::TargetEmpty
    );

    // The online runner is a force path: it never denies based on
    // the global toggle. With no scheduler started, a non-empty
    // target surfaces SchedulerUnavailable...
    assert_eq!(
        runtime.request_playlist_online_run(
            PlaylistItem::Playlist(playlist.id),
            OnlineRunRequest::Single(OnlineCapability::Lyrics),
        ),
        RunDecision::SchedulerUnavailable
    );
    // ...and turning the matching background sweep on does NOT
    // change that — a manual retrieval still fires (issue #61),
    // unlike the analysis path which would deny here.
    let mut settings = runtime.settings().clone();
    settings.online.lyrics = true;
    runtime
        .handle_command(ApplicationCommand::UpdateSettings(settings))
        .expect("apply settings");
    assert_eq!(
        runtime.request_playlist_online_run(
            PlaylistItem::Playlist(playlist.id),
            OnlineRunRequest::Single(OnlineCapability::Lyrics),
        ),
        RunDecision::SchedulerUnavailable
    );

    // Folders are never a valid target for the per-track-set
    // actions.
    let phantom_folder = sustain_domain::PlaylistFolderId::new(1).expect("non-zero");
    assert_eq!(
        runtime.request_playlist_online_run(
            PlaylistItem::Folder(phantom_folder),
            OnlineRunRequest::Single(OnlineCapability::Artwork),
        ),
        RunDecision::TargetEmpty
    );

    // Track-scoped path: `All` bypasses the deny check entirely
    // and resolves to TargetEmpty for an empty Vec.
    assert_eq!(
        runtime.request_tracks_analysis_run(Vec::new(), AnalysisRunRequest::All),
        RunDecision::TargetEmpty
    );
    assert_eq!(
        runtime.request_tracks_online_run(Vec::new(), OnlineRunRequest::All),
        RunDecision::TargetEmpty
    );
    // A Single request with the matching global toggle on stops
    // at the deny check before the emptiness check fires: deny
    // is a stronger signal ("the work is already being done") than
    // "no targets". Same precedence as the playlist-scoped path.
    assert_eq!(
        runtime.request_tracks_analysis_run(
            Vec::new(),
            AnalysisRunRequest::Single(AnalysisCapability::Key),
        ),
        RunDecision::DeniedBackgroundEnabled
    );
}

#[test]
fn request_run_skips_tracks_whose_capability_is_already_cached() {
    // A re-run of BPM analysis on a track that already has BPM
    // recorded must NOT queue the track. The scheduler is never
    // started in this test — if the filter were skipped, the
    // dispatch would surface SchedulerUnavailable. AlreadyComplete
    // proves the filter caught the work before the scheduler
    // would have run. (Online retrieval is deliberately a force
    // path with no such runtime-level pre-filter — see
    // `online_run_is_a_force_path_that_does_not_pre_filter`.)
    use sustain_library_store::{AnalysisCapabilities, AnalysisContext};

    let store = Arc::new(InMemoryLibraryStore::new());
    let track = Track {
        id: track_id(1),
        location: track_location("t.flac"),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    store.save_track(track.clone()).expect("save");

    // Stamp BPM analysis and lyrics retrieval as already complete
    // at the current versions used by the runtime.
    let empty_analysis = sustain_domain::TrackAnalysis {
        bpm: None,
        key: None,
        beatgrid: None,
        waveform_preview: sustain_domain::WaveformSegments {
            segment_duration_ms: 0.0,
            segments: Vec::new(),
        },
        waveform_detail: sustain_domain::WaveformSegments {
            segment_duration_ms: 0.0,
            segments: Vec::new(),
        },
        acoustics: None,
    };
    store
        .record_analysis(
            track.id,
            &empty_analysis,
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            AnalysisContext {
                now_unix: 100,
                analyzer_version: sustain_analysis::ANALYZER_VERSION,
            },
        )
        .expect("record bpm");

    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");

    // BPM is cached -> AlreadyComplete, no SchedulerUnavailable.
    assert_eq!(
        runtime.request_tracks_analysis_run(
            vec![track.id],
            AnalysisRunRequest::Single(AnalysisCapability::Bpm),
        ),
        RunDecision::AlreadyComplete
    );
    // Key has never been analyzed -> filter passes, scheduler
    // check then fires.
    assert_eq!(
        runtime.request_tracks_analysis_run(
            vec![track.id],
            AnalysisRunRequest::Single(AnalysisCapability::Key),
        ),
        RunDecision::SchedulerUnavailable
    );
    // `All` finds at least one missing capability (key, audio)
    // -> filter passes the track through.
    assert_eq!(
        runtime.request_tracks_analysis_run(vec![track.id], AnalysisRunRequest::All),
        RunDecision::SchedulerUnavailable
    );
}

#[test]
fn online_run_is_a_force_path_that_does_not_pre_filter() {
    // Manual retrieval ignores the attempt stamp: a track whose
    // lyrics were already attempted (with the background toggle on)
    // must NOT short-circuit to AlreadyComplete the way analysis
    // does. With no scheduler started the runtime reaches the
    // dispatch and surfaces SchedulerUnavailable, proving both the
    // runtime-level pre-filter and the background-enabled deny are
    // gone (issue #61). Skipping already-satisfied tracks is the
    // scheduler's missing-only job, covered by the online_scheduler
    // tests.
    use sustain_library_store::{OnlineCapabilities, OnlineContext};

    let store = Arc::new(InMemoryLibraryStore::new());
    let track = Track {
        id: track_id(1),
        location: track_location("t.flac"),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    };
    store.save_track(track.clone()).expect("save");
    store
        .record_online_attempt(
            track.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            OnlineContext {
                now_unix: 100,
                provider_version: super::ONLINE_PROVIDER_VERSION,
            },
        )
        .expect("record lyrics attempt");

    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");
    // Turn the lyrics background sweep on; the force path ignores it.
    let mut settings = runtime.settings().clone();
    settings.online.lyrics = true;
    runtime
        .handle_command(ApplicationCommand::UpdateSettings(settings))
        .expect("apply settings");

    assert_eq!(
        runtime.request_tracks_online_run(
            vec![track.id],
            OnlineRunRequest::Single(OnlineCapability::Lyrics),
        ),
        RunDecision::SchedulerUnavailable
    );
}

/// Counts the two read paths whose amplification #95 fixed.
#[derive(Default)]
struct StoreCallCounts {
    track: std::sync::atomic::AtomicUsize,
    tracks: std::sync::atomic::AtomicUsize,
    statistics_updates: std::sync::atomic::AtomicUsize,
}

/// A transparent [`LibraryStore`] decorator that counts `track` and
/// `tracks` calls and delegates everything else to an inner
/// [`InMemoryLibraryStore`]. It lets a test prove `apply_track_updated`
/// reloads a single row by keyed id rather than decoding the whole
/// library.
struct CallCountingLibraryStore {
    inner: InMemoryLibraryStore,
    counts: Arc<StoreCallCounts>,
    statistics_failures_remaining: std::sync::atomic::AtomicUsize,
    tracks_failures_remaining: std::sync::atomic::AtomicUsize,
}

impl LibraryStore for CallCountingLibraryStore {
    fn save_track(&self, track: Track) -> StoreResult<()> {
        self.inner.save_track(track)
    }

    fn reconcile_scanned_tracks(&self, tracks: &[Track]) -> StoreResult<()> {
        self.inner.reconcile_scanned_tracks(tracks)
    }

    fn update_track_location(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        self.inner.update_track_location(track_id, location)
    }

    fn relocate_track_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        file_size_bytes: u64,
    ) -> StoreResult<()> {
        self.inner
            .relocate_track_and_enqueue_mirror(track_id, location, file_size_bytes)
    }

    fn replace_track_audio(
        &self,
        track_id: TrackId,
        location: &TrackLocation,
        audio_properties: sustain_library_store::TrackAudioProperties,
        file_size_bytes: u64,
        has_embedded_artwork: bool,
    ) -> StoreResult<()> {
        self.inner.replace_track_audio(
            track_id,
            location,
            audio_properties,
            file_size_bytes,
            has_embedded_artwork,
        )
    }

    fn update_track_rating(&self, track_id: TrackId, rating: Rating) -> StoreResult<()> {
        self.inner.update_track_rating(track_id, rating)
    }

    fn update_track_rating_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        rating: Rating,
    ) -> StoreResult<()> {
        self.inner
            .update_track_rating_and_enqueue_mirror(track_id, rating)
    }

    fn update_track_statistics(
        &self,
        track_id: TrackId,
        statistics: &PlayStatistics,
    ) -> StoreResult<()> {
        self.counts
            .statistics_updates
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .statistics_failures_remaining
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(StoreError::StoreUnavailable);
        }
        self.inner.update_track_statistics(track_id, statistics)
    }

    fn apply_track_metadata_change(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner.apply_track_metadata_change(track_id, change)
    }

    fn apply_track_metadata_change_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner
            .apply_track_metadata_change_and_enqueue_mirror(track_id, change)
    }

    fn fill_missing_track_metadata(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<()> {
        self.inner.fill_missing_track_metadata(track_id, change)
    }

    fn fill_missing_track_metadata_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
    ) -> StoreResult<bool> {
        self.inner
            .fill_missing_track_metadata_and_enqueue_mirror(track_id, change)
    }

    fn apply_track_metadata_change_and_location_and_enqueue_mirror(
        &self,
        track_id: TrackId,
        change: &MetadataChange,
        location: &TrackLocation,
    ) -> StoreResult<()> {
        self.inner
            .apply_track_metadata_change_and_location_and_enqueue_mirror(track_id, change, location)
    }

    fn delete_track(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.delete_track(track_id)
    }

    fn commit_duplicate_consolidation(&self, plan: &DuplicateConsolidationPlan) -> StoreResult<()> {
        self.inner.commit_duplicate_consolidation(plan)
    }

    fn track(&self, track_id: TrackId) -> StoreResult<Option<Track>> {
        self.counts
            .track
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.track(track_id)
    }

    fn tracks(&self) -> StoreResult<Vec<Track>> {
        self.counts
            .tracks
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if self
            .tracks_failures_remaining
            .fetch_update(
                std::sync::atomic::Ordering::SeqCst,
                std::sync::atomic::Ordering::SeqCst,
                |remaining| remaining.checked_sub(1),
            )
            .is_ok()
        {
            return Err(StoreError::StoreUnavailable);
        }
        self.inner.tracks()
    }

    fn flush_durable(&self) -> StoreResult<()> {
        self.inner.flush_durable()
    }

    fn publish_tag_mirror_artwork(&self, bytes: &[u8]) -> StoreResult<StoredTagMirrorArtwork> {
        self.inner.publish_tag_mirror_artwork(bytes)
    }

    fn enqueue_tag_mirror_artwork(
        &self,
        track_id: TrackId,
        artwork: TagMirrorArtwork,
    ) -> StoreResult<()> {
        self.inner.enqueue_tag_mirror_artwork(track_id, artwork)
    }

    fn tag_mirrors_due(&self, now_unix: i64, limit: usize) -> StoreResult<Vec<PendingTagMirror>> {
        self.inner.tag_mirrors_due(now_unix, limit)
    }

    fn next_tag_mirror_attempt_at(&self) -> StoreResult<Option<i64>> {
        self.inner.next_tag_mirror_attempt_at()
    }

    fn complete_tag_mirror(&self, track_id: TrackId, generation: u64) -> StoreResult<bool> {
        self.inner.complete_tag_mirror(track_id, generation)
    }

    fn record_tag_mirror_failure(
        &self,
        track_id: TrackId,
        generation: u64,
        next_attempt_at_unix: i64,
        error: &str,
    ) -> StoreResult<bool> {
        self.inner
            .record_tag_mirror_failure(track_id, generation, next_attempt_at_unix, error)
    }

    fn read_tag_mirror_artwork(&self, artwork: &StoredTagMirrorArtwork) -> StoreResult<Vec<u8>> {
        self.inner.read_tag_mirror_artwork(artwork)
    }

    fn garbage_collect_tag_mirror_artwork(&self) -> StoreResult<()> {
        self.inner.garbage_collect_tag_mirror_artwork()
    }

    fn save_playlist(&self, playlist: Playlist) -> StoreResult<()> {
        self.inner.save_playlist(playlist)
    }

    fn playlist(&self, playlist_id: PlaylistId) -> StoreResult<Option<Playlist>> {
        self.inner.playlist(playlist_id)
    }

    fn playlists(&self) -> StoreResult<Vec<Playlist>> {
        self.inner.playlists()
    }

    fn delete_playlist(&self, playlist_id: PlaylistId) -> StoreResult<()> {
        self.inner.delete_playlist(playlist_id)
    }

    fn save_playlist_folder(&self, folder: PlaylistFolder) -> StoreResult<()> {
        self.inner.save_playlist_folder(folder)
    }

    fn playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<Option<PlaylistFolder>> {
        self.inner.playlist_folder(folder_id)
    }

    fn playlist_folders(&self) -> StoreResult<Vec<PlaylistFolder>> {
        self.inner.playlist_folders()
    }

    fn delete_playlist_folder(&self, folder_id: PlaylistFolderId) -> StoreResult<()> {
        self.inner.delete_playlist_folder(folder_id)
    }

    fn save_smart_playlist(&self, smart_playlist: SmartPlaylist) -> StoreResult<()> {
        self.inner.save_smart_playlist(smart_playlist)
    }

    fn smart_playlist(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> StoreResult<Option<SmartPlaylist>> {
        self.inner.smart_playlist(smart_playlist_id)
    }

    fn smart_playlists(&self) -> StoreResult<Vec<SmartPlaylist>> {
        self.inner.smart_playlists()
    }

    fn delete_smart_playlist(&self, smart_playlist_id: SmartPlaylistId) -> StoreResult<()> {
        self.inner.delete_smart_playlist(smart_playlist_id)
    }

    fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> StoreResult<Option<TrackColumnLayout>> {
        self.inner.load_track_column_layout(scope)
    }

    fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> StoreResult<()> {
        self.inner.save_track_column_layout(scope, layout)
    }

    fn delete_track_column_layout(&self, scope: TrackColumnLayoutScope) -> StoreResult<()> {
        self.inner.delete_track_column_layout(scope)
    }

    fn record_analysis(
        &self,
        track_id: TrackId,
        analysis: &TrackAnalysis,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        self.inner
            .record_analysis(track_id, analysis, capabilities, context)
    }

    fn record_analysis_attempt_failure(
        &self,
        track_id: TrackId,
        capabilities: AnalysisCapabilities,
        context: AnalysisContext,
    ) -> StoreResult<()> {
        self.inner
            .record_analysis_attempt_failure(track_id, capabilities, context)
    }

    fn tracks_needing_analysis(
        &self,
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .tracks_needing_analysis(capabilities, analyzer_version, limit)
    }

    fn filter_tracks_needing_analysis(
        &self,
        track_ids: &[TrackId],
        capabilities: AnalysisCapabilities,
        analyzer_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .filter_tracks_needing_analysis(track_ids, capabilities, analyzer_version)
    }

    fn load_waveform(&self, track_id: TrackId) -> StoreResult<Option<StoredWaveform>> {
        self.inner.load_waveform(track_id)
    }

    fn load_all_acoustics(&self) -> StoreResult<Vec<(TrackId, AcousticFeatures)>> {
        self.inner.load_all_acoustics()
    }

    fn record_synced_lyrics(
        &self,
        track_id: TrackId,
        lyrics: &SyncedLyrics,
        source: &str,
    ) -> StoreResult<()> {
        self.inner.record_synced_lyrics(track_id, lyrics, source)
    }

    fn load_synced_lyrics(&self, track_id: TrackId) -> StoreResult<Option<StoredSyncedLyrics>> {
        self.inner.load_synced_lyrics(track_id)
    }

    fn clear_synced_lyrics(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.clear_synced_lyrics(track_id)
    }

    fn record_online_attempt(
        &self,
        track_id: TrackId,
        capabilities: OnlineCapabilities,
        context: OnlineContext,
    ) -> StoreResult<()> {
        self.inner
            .record_online_attempt(track_id, capabilities, context)
    }

    fn tracks_needing_online(
        &self,
        capabilities: OnlineCapabilities,
        provider_version: u32,
        limit: usize,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .tracks_needing_online(capabilities, provider_version, limit)
    }

    fn filter_tracks_needing_online(
        &self,
        track_ids: &[TrackId],
        capabilities: OnlineCapabilities,
        provider_version: u32,
    ) -> StoreResult<Vec<TrackId>> {
        self.inner
            .filter_tracks_needing_online(track_ids, capabilities, provider_version)
    }

    fn save_smart_shuffle_index(&self, index: &StoredSmartShuffleIndex) -> StoreResult<()> {
        self.inner.save_smart_shuffle_index(index)
    }

    fn load_smart_shuffle_index(&self) -> StoreResult<Option<StoredSmartShuffleIndex>> {
        self.inner.load_smart_shuffle_index()
    }

    fn clear_smart_shuffle_index(&self) -> StoreResult<()> {
        self.inner.clear_smart_shuffle_index()
    }

    fn source_fingerprint(&self, track_id: TrackId) -> StoreResult<Option<SourceFingerprint>> {
        self.inner.source_fingerprint(track_id)
    }

    fn save_source_fingerprint(
        &self,
        track_id: TrackId,
        fingerprint: &SourceFingerprint,
    ) -> StoreResult<()> {
        self.inner.save_source_fingerprint(track_id, fingerprint)
    }

    fn invalidate_source_fingerprint(&self, track_id: TrackId) -> StoreResult<()> {
        self.inner.invalidate_source_fingerprint(track_id)
    }

    fn save_sync_device(&self, device: &SyncDevice) -> StoreResult<()> {
        self.inner.save_sync_device(device)
    }

    fn sync_device(&self, id: &SyncDeviceId) -> StoreResult<Option<SyncDevice>> {
        self.inner.sync_device(id)
    }

    fn sync_devices(&self) -> StoreResult<Vec<SyncDevice>> {
        self.inner.sync_devices()
    }

    fn delete_sync_device(&self, id: &SyncDeviceId) -> StoreResult<()> {
        self.inner.delete_sync_device(id)
    }

    fn save_device_selection(
        &self,
        id: &SyncDeviceId,
        selection: &[PlaylistItem],
    ) -> StoreResult<()> {
        self.inner.save_device_selection(id, selection)
    }

    fn device_selection(&self, id: &SyncDeviceId) -> StoreResult<Vec<PlaylistItem>> {
        self.inner.device_selection(id)
    }

    fn save_device_manifest(
        &self,
        id: &SyncDeviceId,
        entries: &[SyncManifestEntry],
    ) -> StoreResult<()> {
        self.inner.save_device_manifest(id, entries)
    }

    fn device_manifest(&self, id: &SyncDeviceId) -> StoreResult<Vec<SyncManifestEntry>> {
        self.inner.device_manifest(id)
    }
}

#[test]
fn smart_shuffle_rebuild_request_does_not_block_the_caller_on_store_reads() {
    use std::time::{Duration, Instant};

    use crate::test_store::FaultyStore;

    // A track is hydrated into the runtime with the store responding
    // instantly, so `library_tracks` is populated and the in-memory
    // emptiness guard passes.
    let faulty = Arc::new(FaultyStore::new(Arc::new(InMemoryLibraryStore::new())));
    faulty
        .save_track(test_track(track_id(1), "alpha.flac"))
        .expect("seed track");
    let store: Arc<dyn LibraryStore> = faulty.clone();
    let mut runtime = ApplicationRuntime::new()
        .with_library_services(store, Arc::new(TestMetadataService))
        .expect("library services initialize");
    assert_eq!(runtime.library_tracks().len(), 1);

    // Now make the bulk reads the rebuild needs (tracks + acoustics)
    // expensive. Before #93 these ran on the calling thread; the request
    // must now return promptly because that work moved to the worker.
    faulty.set_read_delay(Duration::from_millis(400));

    let start = Instant::now();
    let scheduled = runtime.request_smart_shuffle_rebuild();
    let elapsed = start.elapsed();

    assert!(scheduled, "a non-empty library schedules a rebuild");
    assert!(
        elapsed < Duration::from_millis(100),
        "request must not block on the store reads (took {elapsed:?}); they belong on the worker"
    );
}
