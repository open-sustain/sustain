// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

// The workspace already denies `unsafe_code`; the only audited
// exceptions are `crate::priority` (Linux scheduling syscalls) and
// `crate::mount` (`statvfs`), each confining a `#![allow(unsafe_code)]`
// to a small module that exposes a safe API.

use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

pub use sustain_domain::{
    AnalysisSettings, ApplicationCommand, ApplicationQuery, BackgroundJobsSettings,
    BackgroundResourceUsage, Clock, DEFAULT_PLAYBACK_VOLUME_PERCENT, DecadeCount, DeviceKind,
    DeviceLayout, DeviceRelativePath, FieldChange, FilesPerFolderCap, GenreDistribution,
    GenrePlayCount, GenreRating, GenreShare, LazyPickContext, LibraryManagementMode,
    LibrarySettings, LibraryStatistics, MetadataChange, OtherGenres, PlayStatistics,
    PlaybackCommand, PlaybackOptions, PlaybackQueue, PlaybackQueueRequest, PlaybackQueueSource,
    PlaybackSession, PlaybackSettings, PlaybackState, Playlist, PlaylistEntry, PlaylistFolder,
    PlaylistFolderId, PlaylistId, PlaylistItem, QualityBucket, QualityDistribution, QualityRange,
    Rating, RepeatMode, ShuffleMode, SmartPlaylist, SmartPlaylistDateField, SmartPlaylistId,
    SmartPlaylistLimit, SmartPlaylistLimitSelection, SmartPlaylistMatchKind,
    SmartPlaylistNumberField, SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet,
    SmartPlaylistTextField, SmartPlaylistTextOperator, SmartShuffleEntropy, SyncDevice,
    SyncDeviceId, SystemClock, Track, TrackAvailability, TrackColumnEntry, TrackColumnLayout,
    TrackColumnLayoutScope, TrackContentHash, TrackId, TrackLocation, TrackMetadata,
    TrackPlaybackSource, TrackRelativePath, UiSettings, UiSidebarSelection, UserSettings,
    VolumePercent, YearCount, compare_optional_text, compute_library_statistics,
    effective_sort_key, matching_tracks, track_matches_rule_set,
};
use sustain_library_store::{AnalysisCapabilities, LibraryStore, OnlineCapabilities};
pub use sustain_metadata::MetadataService;
pub use sustain_metadata_remote::{
    AudioFingerprint, FetchedArtwork, RemoteError, RemoteMetadataService, RemoteResult, TrackMatch,
    TrackMatchSource, TrackQuery,
};
use sustain_playback::PlaybackService;
pub use sustain_playback::TrackEndedCallback;
pub use sustain_search::{
    SearchIndex, album_matches_search_text, filter_tracks_by_search_text, normalize_query,
    track_matches_search_text,
};
use sustain_settings::SettingsStore;

pub mod analysis_scheduler;
pub mod priority;
pub use analysis_scheduler::SchedulerProgress as AnalysisProgress;
pub use priority::{IoPriorityClass, NiceLevel, resolve_worker_count};

/// Watermark stamped into `track_online_status.provider_version` so a
/// future incompatible change to the online-retrieval pipeline (a
/// different provider, a corrected matching heuristic) can invalidate
/// previously-attempted rows without a migration. Bumped centrally,
/// not per-provider; the scheduler doesn't read it for anything other
/// than the bookkeeping write.
///
/// Version 2: tag enrichment now fetches and writes the recording's
/// primary genre and uses the recording's `first-release-date` for
/// year (instead of an arbitrary release's date). Track/disc
/// positional fields are now only filled when an existing album
/// matches one of MusicBrainz's release titles. Version 1 attempts
/// recorded "tags = attempted" against a pipeline that could not
/// produce genres at all; the bump re-opens every previously-
/// stamped track for the corrected pipeline.
pub const ONLINE_PROVIDER_VERSION: u32 = 2;

pub(crate) mod artwork_fetcher;
mod commands;
mod device_sync;
pub mod device_sync_scheduler;
mod library_mutation;
mod library_scan;
pub mod managed_library;
pub(crate) mod metadata_writer;
mod mount;
pub mod notifications;
pub mod online_scheduler;
pub use online_scheduler::SchedulerProgress as OnlineProgress;
mod playback;
mod playlist_folders;
mod playlist_items;
mod playlists;
mod smart_playlists;
pub mod smart_shuffle_scheduler;
pub use device_sync::{DeviceAnalysisReadiness, DeviceCapacity};
pub use device_sync_scheduler::{DeviceSyncCompletion, DeviceSyncEvent, DeviceSyncScheduler};
pub use smart_shuffle_scheduler::{SmartShuffleRebuildResult, SmartShuffleScheduler};
pub use sustain_device_sync::{ConnectedDevice, SyncPlan, SyncProgress, SyncStage};
pub use sustain_smart_shuffle::{
    INDEX_SCHEMA_VERSION as SMART_SHUFFLE_INDEX_SCHEMA_VERSION, PickMode, SmartShuffleError,
    SmartShuffleIndex,
};

pub use artwork_fetcher::{ArtworkFetchOutcome, ArtworkFetchResult};
pub use library_scan::run_library_scan_task;
pub use managed_library::{run_library_consolidation_task, run_library_import_task};
pub use metadata_writer::{MetadataWriteKind, MetadataWriteOutcome, MetadataWriteResult};
pub use notifications::{
    EPHEMERAL_NOTIFICATION_DURATION, NOTIFICATION_QUEUE_HARD_CAP, NOTIFICATION_TRANSITION,
    Notification, NotificationCategory, NotificationCenter, NotificationId, NotificationKind,
    NotificationSeverity, analysis_background_outcome_text, analysis_background_running_text,
    library_consolidation_outcome_text, library_consolidation_running_text,
    library_import_outcome_text, library_import_running_text, library_path_change_outcome_text,
    library_scan_outcome_text, library_scan_running_text, runtime_error_text,
};

pub type ApplicationRuntimeResult<T> = Result<T, ApplicationRuntimeError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ApplicationRuntimeError {
    ArtworkFetchingUnavailable,
    ArtworkRejected,
    LibraryPathUnavailable,
    LibraryConsolidationFailed,
    LibraryImportFailed,
    LibraryScanFailed,
    LibraryServicesUnavailable,
    LibraryStoreFailed,
    MetadataWriteFailed,
    InvalidPlaylistName,
    InvalidPlaylistFolderName,
    InvalidSmartPlaylistName,
    InvalidSmartPlaylistRules,
    BackgroundTaskRunning,
    PlaybackFailed,
    PlaybackServiceUnavailable,
    PlaylistEntryNotFound,
    PlaylistNotFound,
    PlaylistFolderNotFound,
    PlaylistFolderWouldCycle,
    SmartPlaylistNotFound,
    SettingsLoadFailed,
    SettingsSaveFailed,
    TrackUnavailable,
    TrackTrashFailed,
    UnsupportedCommand(ApplicationCommand),
}

/// Trims a user-supplied name and rejects it when blank once trimmed. The three
/// playlist kinds (playlists, folders, smart playlists) share this rule but
/// each reports its own "invalid name" error, supplied via `on_empty`.
pub(crate) fn normalized_name(
    name: String,
    on_empty: fn() -> ApplicationRuntimeError,
) -> ApplicationRuntimeResult<String> {
    let name = name.trim().to_owned();
    if name.is_empty() {
        Err(on_empty())
    } else {
        Ok(name)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LibraryScanSummary {
    pub scanned_tracks: usize,
    pub added_tracks: usize,
    pub updated_tracks: usize,
    pub missing_tracks: usize,
    pub skipped_unsupported_files: usize,
    pub failed_files: usize,
    // True when the scan stopped because the user asked it to. The
    // numbers above reflect the partial work that completed; we do not
    // sweep the unwalked portion of the library for missing tracks.
    pub cancelled: bool,
}

/// Live truth about which (if any) mutually-exclusive background task
/// owns the runtime right now. Outcome and failure messaging is no
/// longer routed through this enum — completed and failed states are
/// reported as notifications via [`NotificationCenter`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackgroundTaskStatus {
    #[default]
    Idle,
    LibraryScanRunning,
    LibraryImportRunning,
    LibraryConsolidationRunning,
}

impl BackgroundTaskStatus {
    pub fn is_running(&self) -> bool {
        !matches!(self, Self::Idle)
    }

    pub fn is_library_consolidation_running(&self) -> bool {
        matches!(self, Self::LibraryConsolidationRunning)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct NowPlaying {
    pub track: Option<Track>,
    pub state: PlaybackState,
    pub options: PlaybackOptions,
}

pub struct LibraryScanTask {
    library_path: PathBuf,
    existing_tracks: Vec<Track>,
    library_store: Arc<dyn LibraryStore>,
    metadata_service: Arc<dyn MetadataService>,
    cancellation_requested: Arc<AtomicBool>,
}

pub struct LibraryImportTask {
    paths: Vec<PathBuf>,
    settings: UserSettings,
    existing_tracks: Vec<Track>,
    library_store: Arc<dyn LibraryStore>,
    metadata_service: Arc<dyn MetadataService>,
    cancellation_requested: Arc<AtomicBool>,
}

pub struct LibraryConsolidationTask {
    settings: UserSettings,
    existing_tracks: Vec<Track>,
    library_store: Arc<dyn LibraryStore>,
    cancellation_requested: Arc<AtomicBool>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryScanResult {
    pub tracks: Vec<Track>,
    pub summary: LibraryScanSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryImportResult {
    pub tracks: Vec<Track>,
    pub summary: LibraryImportSummary,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryConsolidationResult {
    pub tracks: Vec<Track>,
    pub summary: LibraryConsolidationSummary,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LibraryImportSummary {
    pub discovered_files: usize,
    pub imported_tracks: usize,
    pub duplicate_files: usize,
    // True when the import stopped because the user asked it to. The
    // import is all-or-nothing: a cancelled run rolls back any files
    // already copied and never partially populates the library.
    pub cancelled: bool,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct LibraryConsolidationSummary {
    pub planned_tracks: usize,
    pub moved_tracks: usize,
    pub already_organized_tracks: usize,
    pub missing_tracks: usize,
    pub cancelled: bool,
}

pub struct ApplicationRuntime {
    settings: UserSettings,
    settings_store: Option<Box<dyn SettingsStore>>,
    library_store: Option<Arc<dyn LibraryStore>>,
    metadata_service: Option<Arc<dyn MetadataService>>,
    playback_service: Option<Box<dyn PlaybackService>>,
    playback_queue: PlaybackQueue,
    // Tracks how much of the currently playing track the listener has
    // actually heard, so we can decide whether to register a play or a
    // skip when the track changes. `None` whenever nothing is playing.
    pub(crate) playback_session: Option<PlaybackSession>,
    library_tracks: Vec<Track>,
    /// Precomputed normalized search documents, kept in lockstep with
    /// `library_tracks` so per-keystroke search never re-clones or
    /// re-lowercases metadata fields. Rebuilt on a wholesale library change
    /// and updated per-track on edits; the single write path
    /// [`Self::store_library_track`] keeps it from drifting.
    search_index: SearchIndex,
    playlists: Vec<Playlist>,
    playlist_folders: Vec<PlaylistFolder>,
    smart_playlists: Vec<SmartPlaylist>,
    last_scan_library_path: Option<PathBuf>,
    last_scan_summary: Option<LibraryScanSummary>,
    last_library_import_summary: Option<LibraryImportSummary>,
    last_library_consolidation_summary: Option<LibraryConsolidationSummary>,
    background_task_status: BackgroundTaskStatus,
    library_scan_cancellation: Option<Arc<AtomicBool>>,
    library_import_cancellation: Option<Arc<AtomicBool>>,
    library_consolidation_cancellation: Option<Arc<AtomicBool>>,
    // Id of the persistent notification published while a given task
    // is running, so apply/fail can dismiss the exact entry they own.
    library_scan_notification_id: Option<NotificationId>,
    library_import_notification_id: Option<NotificationId>,
    library_consolidation_notification_id: Option<NotificationId>,
    metadata_writer: Option<metadata_writer::MetadataWriter>,
    metadata_write_result_sink: Option<async_channel::Sender<MetadataWriteResult>>,
    remote_metadata_service: Option<Arc<dyn RemoteMetadataService>>,
    artwork_fetcher: Option<artwork_fetcher::ArtworkFetcher>,
    artwork_fetch_result_sink: Option<async_channel::Sender<ArtworkFetchResult>>,
    analysis_scheduler: Option<analysis_scheduler::AnalysisScheduler>,
    analysis_progress_sink: Option<async_channel::Sender<analysis_scheduler::SchedulerProgress>>,
    analysis_notification_id: Option<NotificationId>,
    online_scheduler: Option<online_scheduler::OnlineScheduler>,
    online_progress_sink: Option<async_channel::Sender<online_scheduler::SchedulerProgress>>,
    online_notification_id: Option<NotificationId>,
    // Background worker for Smart Shuffle index rebuilds. Owns the
    // thread spawn + result channel; the index itself lives here in
    // the runtime so the picker can borrow it without crossing thread
    // boundaries.
    smart_shuffle_scheduler: SmartShuffleScheduler,
    // Background worker for device syncs (issues #23/#24). Owns the
    // thread spawn + progress/result channel; the device manifest is
    // persisted by the runtime when a sync completes.
    device_sync_scheduler: DeviceSyncScheduler,
    // Id of the persistent notification shown while a device sync runs,
    // so progress can update it in place and completion can dismiss it.
    device_sync_notification_id: Option<NotificationId>,
    /// In-memory copy of the prepared Smart Shuffle index (genre IDF
    /// and, later, normalization statistics). `None` when the index
    /// has never been built yet, or when the persisted blob's schema
    /// version did not match the current scorer.
    smart_shuffle_index: Option<SmartShuffleIndex>,
    /// Bookkeeping mirrored from the live index so the Preferences
    /// status caption (indexed tracks, analysis coverage, last
    /// rebuild) can reach it without re-reading the index each tick.
    smart_shuffle_metadata: Option<SmartShuffleIndexMetadata>,
    // Channel handed to background workers that mutate the persisted
    // copy of a track behind the runtime's back (analysis fills BPM /
    // key, online lyrics/tags writes a row). Workers push the touched
    // `TrackId`; the UI shell drains the channel on the main loop and
    // calls [`Self::apply_track_updated`], which reloads the row from
    // the library store, replaces it in `library_tracks`, and fires
    // [`Self::track_data_observer`] so visible widgets repaint.
    track_updated_sink: Option<async_channel::Sender<TrackId>>,
    clock: Arc<dyn Clock>,
    notifications: NotificationCenter,
    // Fires after every push/dismiss/expire on `notifications`. Set by
    // the UI shell once during startup; the callback is responsible for
    // deferring its re-render (the runtime is mid-borrow when this
    // fires, so calling back into the runtime synchronously would
    // panic).
    notification_observer: Option<NotificationObserver>,
    // Fires whenever the runtime flips any track's `is_missing` flag
    // outside the scan path (e.g. lazy detection on a failed play, a
    // library-path change that re-stats every track). The UI shell
    // installs this once to drive its narrow per-row icon refresh —
    // see the design note on [`TrackAvailabilityObserver`].
    track_availability_observer: Option<TrackAvailabilityObserver>,
    // Fires from [`Self::apply_track_updated`] after the in-memory
    // copy of a single track has been refreshed from the library
    // store. The UI shell installs this once to drive its targeted
    // per-row refresh — see [`TrackDataObserver`].
    track_data_observer: Option<TrackDataObserver>,
    // Fires whenever Smart Shuffle's user-visible state changes: an
    // index rebuild starts or completes, or a freshly-loaded index is
    // adopted. The Shuffle preferences tab installs this on open and
    // clears it on close so its status caption and Rebuild-index
    // button state stay live. Same re-entrancy contract as the other
    // observers — defer any re-borrow onto the main loop.
    smart_shuffle_state_observer: Option<SmartShuffleStateObserver>,
}

/// Callback fired after every mutation of [`NotificationCenter`]. Held
/// as a trait object so feature crates can plug GTK-specific dispatch
/// without coupling `app_runtime` to GTK.
pub type NotificationObserver = Box<dyn Fn()>;

/// Callback fired whenever the runtime flips at least one persisted
/// track's `is_missing` flag *outside* the bulk scan path. The
/// observer receives no payload — the UI is expected to re-read
/// [`ApplicationRuntime::library_tracks`] and patch its own row data
/// for the (typically narrow) set of tracks whose availability now
/// differs. Like [`NotificationObserver`], the runtime is mid-borrow
/// when this fires; observers must defer their work onto the main
/// loop (e.g. `glib::idle_add_local_once`).
pub type TrackAvailabilityObserver = Box<dyn Fn()>;

/// Callback fired after [`ApplicationRuntime::apply_track_updated`]
/// has refreshed a single track in `library_tracks` from the library
/// store. The UI shell uses this to drive its narrow per-row repaint
/// (analogous to [`TrackAvailabilityObserver`] but scoped to a
/// specific id). Same re-entrancy contract as the other observers:
/// the runtime is mid-borrow when this fires, so the closure must
/// defer back-into-runtime work onto the main loop.
pub type TrackDataObserver = Box<dyn Fn(TrackId)>;

/// Callback fired whenever Smart Shuffle's user-visible state
/// changes. Same shape and re-entrancy contract as
/// [`NotificationObserver`]; the runtime is mid-borrow when this
/// fires, so observers must defer back-into-runtime reads onto the
/// main loop (e.g. `glib::idle_add_local_once`).
pub type SmartShuffleStateObserver = Box<dyn Fn()>;

/// Cached bookkeeping for the live Smart Shuffle index, mirrored from
/// it so the Preferences status caption can read the indexed track
/// count, the DSP analysis coverage, and the last rebuild time without
/// re-walking the index on every observer tick.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SmartShuffleIndexMetadata {
    pub indexed_track_count: u32,
    pub analysis_coverage: f32,
    pub built_at: std::time::SystemTime,
}

impl ApplicationRuntime {
    pub fn new() -> Self {
        Self {
            settings: UserSettings::default(),
            settings_store: None,
            library_store: None,
            metadata_service: None,
            playback_service: None,
            playback_queue: PlaybackQueue::default(),
            playback_session: None,
            library_tracks: Vec::new(),
            search_index: SearchIndex::new(),
            playlists: Vec::new(),
            playlist_folders: Vec::new(),
            smart_playlists: Vec::new(),
            last_scan_library_path: None,
            last_scan_summary: None,
            last_library_import_summary: None,
            last_library_consolidation_summary: None,
            background_task_status: BackgroundTaskStatus::Idle,
            library_scan_cancellation: None,
            library_import_cancellation: None,
            library_consolidation_cancellation: None,
            library_scan_notification_id: None,
            library_import_notification_id: None,
            library_consolidation_notification_id: None,
            metadata_writer: None,
            metadata_write_result_sink: None,
            remote_metadata_service: None,
            artwork_fetcher: None,
            artwork_fetch_result_sink: None,
            analysis_scheduler: None,
            analysis_progress_sink: None,
            analysis_notification_id: None,
            online_scheduler: None,
            online_progress_sink: None,
            online_notification_id: None,
            smart_shuffle_scheduler: SmartShuffleScheduler::new(),
            device_sync_scheduler: DeviceSyncScheduler::new(),
            device_sync_notification_id: None,
            smart_shuffle_index: None,
            smart_shuffle_metadata: None,
            track_updated_sink: None,
            clock: Arc::new(SystemClock),
            notifications: NotificationCenter::new(),
            notification_observer: None,
            track_availability_observer: None,
            track_data_observer: None,
            smart_shuffle_state_observer: None,
        }
    }

    pub fn with_settings_store(
        settings_store: Box<dyn SettingsStore>,
    ) -> ApplicationRuntimeResult<Self> {
        let settings = settings_store
            .load_settings()
            .map_err(|_| ApplicationRuntimeError::SettingsLoadFailed)?;

        let initial_playback_queue = PlaybackQueue::empty(PlaybackOptions {
            shuffle_mode: settings.playback.shuffle_mode,
            repeat_mode: RepeatMode::Off,
        });
        Ok(Self {
            settings,
            settings_store: Some(settings_store),
            library_store: None,
            metadata_service: None,
            playback_service: None,
            playback_queue: initial_playback_queue,
            playback_session: None,
            library_tracks: Vec::new(),
            search_index: SearchIndex::new(),
            playlists: Vec::new(),
            playlist_folders: Vec::new(),
            smart_playlists: Vec::new(),
            last_scan_library_path: None,
            last_scan_summary: None,
            last_library_import_summary: None,
            last_library_consolidation_summary: None,
            background_task_status: BackgroundTaskStatus::Idle,
            library_scan_cancellation: None,
            library_import_cancellation: None,
            library_consolidation_cancellation: None,
            library_scan_notification_id: None,
            library_import_notification_id: None,
            library_consolidation_notification_id: None,
            metadata_writer: None,
            metadata_write_result_sink: None,
            remote_metadata_service: None,
            artwork_fetcher: None,
            artwork_fetch_result_sink: None,
            analysis_scheduler: None,
            analysis_progress_sink: None,
            analysis_notification_id: None,
            online_scheduler: None,
            online_progress_sink: None,
            online_notification_id: None,
            smart_shuffle_scheduler: SmartShuffleScheduler::new(),
            device_sync_scheduler: DeviceSyncScheduler::new(),
            device_sync_notification_id: None,
            smart_shuffle_index: None,
            smart_shuffle_metadata: None,
            track_updated_sink: None,
            clock: Arc::new(SystemClock),
            notifications: NotificationCenter::new(),
            notification_observer: None,
            track_availability_observer: None,
            track_data_observer: None,
            smart_shuffle_state_observer: None,
        })
    }

    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    pub fn with_library_services(
        mut self,
        library_store: Arc<dyn LibraryStore>,
        metadata_service: Arc<dyn MetadataService>,
    ) -> ApplicationRuntimeResult<Self> {
        self.set_library_services(library_store, metadata_service)?;
        Ok(self)
    }

    pub fn set_library_services(
        &mut self,
        library_store: Arc<dyn LibraryStore>,
        metadata_service: Arc<dyn MetadataService>,
    ) -> ApplicationRuntimeResult<()> {
        if let Some(library_path) = self.settings.library_path() {
            managed_library::recover_library_consolidation_journal(
                library_path,
                library_store.as_ref(),
            )?;
        }
        self.library_tracks = library_scan::load_library_tracks(library_store.as_ref())?;
        self.rebuild_search_index();
        self.playlists = library_store
            .playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.playlist_folders = library_store
            .playlist_folders()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.smart_playlists = library_store
            .smart_playlists()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.library_store = Some(library_store);
        self.metadata_service = Some(metadata_service);
        // Restore the previously-built Smart Shuffle index (if any).
        // Sits after the library store assignment because the loader
        // reads `self.library_store`; a fresh database leaves the
        // index fields at `None` and the next Smart-enable triggers a
        // background rebuild.
        self.load_smart_shuffle_index_from_store()?;
        Ok(())
    }

    pub fn with_playback_service(mut self, playback_service: Box<dyn PlaybackService>) -> Self {
        self.playback_service = Some(playback_service);
        self
    }

    /// Starts the async metadata writer, using the same `MetadataService`
    /// the runtime already holds. The writer owns a dedicated worker
    /// thread that drains tag writes off the GTK main loop.
    ///
    /// Pair with [`Self::set_metadata_write_result_sink`] so failures can
    /// be reported to the user. Without a sink, the writer still
    /// processes jobs but completions are silently consumed.
    pub fn start_metadata_writer(&mut self) -> ApplicationRuntimeResult<()> {
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        self.metadata_writer = Some(metadata_writer::MetadataWriter::start(metadata_service));
        Ok(())
    }

    /// Registers the sink that receives per-write completions. Senders
    /// of a closed channel are silently dropped — the worker keeps
    /// processing jobs regardless. UI layer typically holds the
    /// matching receiver and consumes from the GTK main loop.
    pub fn set_metadata_write_result_sink(
        &mut self,
        sink: async_channel::Sender<MetadataWriteResult>,
    ) {
        self.metadata_write_result_sink = Some(sink);
    }

    /// Drains pending tag writes and joins the worker thread. Call from
    /// the app's shutdown path so an in-flight rating click is not lost
    /// when the window closes.
    pub fn shutdown_metadata_writer(&mut self) {
        if let Some(writer) = self.metadata_writer.take() {
            writer.shutdown();
        }
    }

    /// Install a networked metadata service. The service is consumed
    /// by the artwork fetcher (and, in time, by tag-backfill and
    /// fingerprint-identification pipelines). Calling this without
    /// also calling [`Self::start_artwork_fetcher`] simply stores
    /// the handle; submissions return
    /// [`ApplicationRuntimeError::ArtworkFetchingUnavailable`] until
    /// the worker is started.
    pub fn set_remote_metadata_service(&mut self, service: Arc<dyn RemoteMetadataService>) {
        self.remote_metadata_service = Some(service);
    }

    pub fn remote_metadata_service(&self) -> Option<Arc<dyn RemoteMetadataService>> {
        self.remote_metadata_service.clone()
    }

    /// Spin up the artwork-fetcher worker against the previously
    /// installed remote metadata service. Returns
    /// [`ApplicationRuntimeError::ArtworkFetchingUnavailable`] if no
    /// service has been set — that state is legitimate (a build
    /// without a remote service still has to start).
    pub fn start_artwork_fetcher(&mut self) -> ApplicationRuntimeResult<()> {
        let service = self
            .remote_metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::ArtworkFetchingUnavailable)?;
        self.artwork_fetcher = Some(artwork_fetcher::ArtworkFetcher::start(service));
        Ok(())
    }

    /// Register the sink that receives per-fetch outcomes. The UI
    /// layer typically holds the matching receiver and consumes from
    /// the GTK main loop, dispatching `SetArtwork` for successful
    /// outcomes and surfacing a status-bar message otherwise.
    pub fn set_artwork_fetch_result_sink(
        &mut self,
        sink: async_channel::Sender<ArtworkFetchResult>,
    ) {
        self.artwork_fetch_result_sink = Some(sink);
    }

    /// Drop the fetcher's sender and join its worker. Safe at app
    /// shutdown; idempotent across multiple calls.
    pub fn shutdown_artwork_fetcher(&mut self) {
        if let Some(fetcher) = self.artwork_fetcher.take() {
            fetcher.shutdown();
        }
    }

    /// Spin up the background analysis scheduler against the previously
    /// installed [`LibraryStore`]. The scheduler observes the current
    /// `AnalysisSettings` (`bpm` / `key` / `audio` tickboxes) and
    /// the library root; toggling either through the settings command
    /// path automatically propagates to the worker. Returns
    /// [`ApplicationRuntimeError::LibraryServicesUnavailable`] if no
    /// library store has been set yet.
    pub fn start_analysis_scheduler(&mut self) -> ApplicationRuntimeResult<()> {
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        // Production analyzer: compose a `sustain_analysis::Analyzer`
        // per track and call only the band methods the capability mask
        // selects, so a track scheduled with `bpm: true, key: false,
        // audio: false` never pays for chroma extraction or the
        // full-track decode. Tests substitute a stub via
        // `analysis_scheduler::AnalysisScheduler::start` directly.
        let analyzer: analysis_scheduler::AnalyzerFn =
            Arc::new(|path, capabilities, options, duration| {
                // Surface a hard error before constructing the lazy
                // analyzer so the supervisor can route the track to
                // `record_analysis_attempt_failure` instead of
                // silently stamping all-None.
                let _ = std::fs::File::open(path).map_err(|source| {
                    sustain_analysis::AnalysisError::OpenFailed {
                        path: path.display().to_string(),
                        source,
                    }
                })?;

                let analyzer =
                    sustain_analysis::Analyzer::new(path.to_path_buf(), options, duration);

                // The waveform and the perceptual acoustic features come
                // off one decode (the analyzer caches the samples), gated
                // by the single `audio` capability. Decode that larger
                // region FIRST so the BPM/key window below is sliced from
                // its centre rather than decoded again. Long tracks skip
                // the waveform entirely — a whole-track decode of a
                // multi-hour file is gigabytes of working set for what, at
                // that length, is a coarse smear; their acoustics come
                // from a centered sample instead, and the device-specific
                // Pioneer waveforms are generated on demand by that export.
                let (waveform, acoustics) = if capabilities.audio {
                    let waveform = if analyzer.is_long_track() {
                        None
                    } else {
                        analyzer.waveform()
                    };
                    let acoustics = analyzer.acoustics();
                    (waveform, acoustics)
                } else {
                    (None, None)
                };
                // BPM and key are read off the same centered window — for
                // free when the audio pass primed a region above, on their
                // own decode otherwise. With the `audio ⇒ bpm ∧ key`
                // settings invariant, an `audio` run always arrives here
                // with both requested too.
                let bpm = if capabilities.bpm {
                    analyzer.bpm()
                } else {
                    None
                };
                let key = if capabilities.key {
                    analyzer.key()
                } else {
                    None
                };
                let (waveform_preview, waveform_detail) = match waveform {
                    Some(tiers) => (tiers.preview, tiers.detail),
                    None => (
                        sustain_analysis::WaveformSegments {
                            segment_duration_ms: 0.0,
                            segments: Vec::new(),
                        },
                        sustain_analysis::WaveformSegments {
                            segment_duration_ms: 0.0,
                            segments: Vec::new(),
                        },
                    ),
                };
                Ok(sustain_analysis::TrackAnalysis {
                    bpm,
                    key,
                    beatgrid: None,
                    waveform_preview,
                    waveform_detail,
                    acoustics,
                })
            });
        let clock: analysis_scheduler::UnixClockFn = Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

        // Marshal progress events from the worker thread onto whatever
        // sink the UI installed via `set_analysis_progress_sink`. If no
        // sink is installed, drop silently — the worker keeps doing
        // useful work even if the UI does not surface its progress.
        let progress_sink = self.analysis_progress_sink.clone();
        let progress: analysis_scheduler::ProgressSink = Arc::new(move |progress| {
            if let Some(sender) = &progress_sink {
                // try_send so a slow consumer cannot back-pressure the
                // worker thread. Dropping a Tick is fine — the next
                // Tick or Idle carries the same running totals.
                let _ = sender.try_send(progress);
            }
        });

        let track_updated =
            self.track_updated_sink
                .clone()
                .map(|sender| -> analysis_scheduler::TrackUpdatedSink {
                    Arc::new(move |track_id| {
                        let _ = sender.try_send(track_id);
                    })
                });

        let scheduler = analysis_scheduler::AnalysisScheduler::start(
            analysis_scheduler::AnalysisSchedulerConfig {
                analyzer,
                progress,
                track_updated,
                clock,
                library_store,
                initial_settings: self.settings.analysis,
                initial_resource_usage: self.settings.background_jobs.resource_usage,
                library_path: self.settings.library.path.clone(),
                analyzer_version: sustain_analysis::ANALYZER_VERSION,
                analysis_options: sustain_analysis::AnalysisOptions::default(),
            },
        );
        self.analysis_scheduler = Some(scheduler);
        Ok(())
    }

    /// Register the async-channel sink that receives
    /// [`AnalysisProgress`] events. The UI typically holds the
    /// matching receiver and forwards each event into
    /// [`Self::apply_analysis_progress`] from the GTK main loop.
    pub fn set_analysis_progress_sink(
        &mut self,
        sink: async_channel::Sender<analysis_scheduler::SchedulerProgress>,
    ) {
        self.analysis_progress_sink = Some(sink);
    }

    /// Drop the scheduler's sender and join its worker. Safe at app
    /// shutdown; idempotent across calls.
    pub fn shutdown_analysis_scheduler(&mut self) {
        if let Some(id) = self.analysis_notification_id.take() {
            self.dismiss_notification(id);
        }
        if let Some(scheduler) = self.analysis_scheduler.take() {
            scheduler.shutdown();
        }
    }

    /// Apply an [`AnalysisProgress`] event to the notification center.
    /// Called from the UI loop after receiving an event from the sink
    /// installed by [`Self::set_analysis_progress_sink`] — the
    /// scheduler runs on its own thread, so the runtime cannot push
    /// notifications synchronously from inside the worker.
    pub fn apply_analysis_progress(&mut self, progress: analysis_scheduler::SchedulerProgress) {
        match progress {
            analysis_scheduler::SchedulerProgress::Tick {
                completed,
                failed: _,
                remaining,
            } => {
                let body = notifications::analysis_background_running_text(completed, remaining);
                if let Some(existing) = self.analysis_notification_id {
                    self.update_notification_body(existing, body);
                } else {
                    let id = self.push_persistent_notification(
                        NotificationCategory::AnalysisBackground,
                        NotificationSeverity::Info,
                        body,
                        false,
                    );
                    self.analysis_notification_id = Some(id);
                }
            }
            analysis_scheduler::SchedulerProgress::Idle { completed, failed } => {
                if let Some(id) = self.analysis_notification_id.take() {
                    self.dismiss_notification(id);
                }
                // Emit an ephemeral summary only when there is something
                // to summarise — Idle also fires on initial start-up
                // with capabilities disabled, and we do not want a
                // ghost "Analyzed 0 tracks" toast every launch.
                if completed > 0 || failed > 0 {
                    self.push_ephemeral_notification(
                        NotificationCategory::AnalysisBackground,
                        NotificationSeverity::Info,
                        notifications::analysis_background_outcome_text(completed, failed),
                    );
                }
                // Acoustics are the only analysis output the Smart
                // Shuffle index caches, so a finished batch that produced
                // any results while audio analysis was on is an
                // index-changing event — rebuild on the background worker
                // (coalesced, milliseconds). BPM/key-only batches do not
                // touch the index, so they do not trigger one.
                if completed > 0 && self.settings.analysis.audio {
                    self.request_smart_shuffle_rebuild();
                }
            }
        }
    }

    pub(crate) fn analysis_scheduler(&self) -> Option<&analysis_scheduler::AnalysisScheduler> {
        self.analysis_scheduler.as_ref()
    }

    /// Spin up the background online scheduler against the previously
    /// installed library store, metadata service, and remote service.
    /// Mirrors [`Self::start_analysis_scheduler`]: the scheduler
    /// observes the current `OnlineSettings` (`artwork` / `tags` /
    /// `lyrics` tickboxes) and library root, and toggling either
    /// through the settings command path automatically propagates to
    /// the worker. Returns
    /// [`ApplicationRuntimeError::LibraryServicesUnavailable`] if a
    /// dependency is missing.
    pub fn start_online_scheduler(&mut self) -> ApplicationRuntimeResult<()> {
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        // The online scheduler writes tag changes through the single
        // [`metadata_writer::MetadataWriter`] actor so its writes
        // serialise against UI rating clicks and metadata edits. The
        // writer must be started first; if it has not been installed
        // we surface that as a missing-service error rather than
        // silently dropping every tag write.
        let tag_writer = self
            .metadata_writer
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?
            .handle();
        let remote_service = self
            .remote_metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::ArtworkFetchingUnavailable)?;

        let clock: online_scheduler::UnixClockFn = Arc::new(|| {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0)
        });

        let progress_sink = self.online_progress_sink.clone();
        let progress: online_scheduler::ProgressSink = Arc::new(move |progress| {
            if let Some(sender) = &progress_sink {
                let _ = sender.try_send(progress);
            }
        });
        let track_updated =
            self.track_updated_sink
                .clone()
                .map(|sender| -> online_scheduler::TrackUpdatedSink {
                    Arc::new(move |track_id| {
                        let _ = sender.try_send(track_id);
                    })
                });

        let scheduler =
            online_scheduler::OnlineScheduler::start(online_scheduler::OnlineSchedulerConfig {
                remote_service,
                tag_writer,
                library_store,
                progress,
                track_updated,
                clock,
                initial_settings: self.settings.online,
                library_path: self.settings.library.path.clone(),
                provider_version: ONLINE_PROVIDER_VERSION,
            });
        self.online_scheduler = Some(scheduler);
        Ok(())
    }

    /// Register the async-channel sink that receives [`OnlineProgress`]
    /// events. The UI typically holds the matching receiver and
    /// forwards each event into [`Self::apply_online_progress`] from
    /// the GTK main loop.
    pub fn set_online_progress_sink(
        &mut self,
        sink: async_channel::Sender<online_scheduler::SchedulerProgress>,
    ) {
        self.online_progress_sink = Some(sink);
    }

    /// Drop the scheduler's sender and join its worker. Safe at app
    /// shutdown; idempotent across calls.
    pub fn shutdown_online_scheduler(&mut self) {
        if let Some(id) = self.online_notification_id.take() {
            self.dismiss_notification(id);
        }
        if let Some(scheduler) = self.online_scheduler.take() {
            scheduler.shutdown();
        }
    }

    /// Apply an [`OnlineProgress`] event to the notification center.
    /// Symmetric to [`Self::apply_analysis_progress`].
    pub fn apply_online_progress(&mut self, progress: online_scheduler::SchedulerProgress) {
        match progress {
            online_scheduler::SchedulerProgress::Tick {
                completed,
                failed: _,
                remaining,
            } => {
                let body = notifications::online_background_running_text(completed, remaining);
                if let Some(existing) = self.online_notification_id {
                    self.update_notification_body(existing, body);
                } else {
                    let id = self.push_persistent_notification(
                        NotificationCategory::OnlineBackground,
                        NotificationSeverity::Info,
                        body,
                        false,
                    );
                    self.online_notification_id = Some(id);
                }
            }
            online_scheduler::SchedulerProgress::Idle { completed, failed } => {
                if let Some(id) = self.online_notification_id.take() {
                    self.dismiss_notification(id);
                }
                if completed > 0 || failed > 0 {
                    self.push_ephemeral_notification(
                        NotificationCategory::OnlineBackground,
                        NotificationSeverity::Info,
                        notifications::online_background_outcome_text(completed, failed),
                    );
                }
            }
        }
    }

    /// Whether the online retrieval scheduler is actively processing
    /// right now (a batch is in flight). Tracks the same signal the
    /// persistent "Retrieving…" notification uses: it is set while
    /// progress ticks arrive and cleared on Idle. The Retrieve
    /// context-menu entries key their sensitivity off this — a manual
    /// retrieval is offered whenever the process is idle, regardless of
    /// the background toggle (a sweep months ago does not block a fresh
    /// manual run), and suppressed only while a run is in flight
    /// (issue #61).
    pub fn is_online_retrieval_running(&self) -> bool {
        self.online_notification_id.is_some()
    }

    pub(crate) fn online_scheduler(&self) -> Option<&online_scheduler::OnlineScheduler> {
        self.online_scheduler.as_ref()
    }

    pub(crate) fn artwork_fetcher(&self) -> Option<&artwork_fetcher::ArtworkFetcher> {
        self.artwork_fetcher.as_ref()
    }

    pub(crate) fn artwork_fetch_result_sink(
        &self,
    ) -> Option<async_channel::Sender<ArtworkFetchResult>> {
        self.artwork_fetch_result_sink.clone()
    }

    pub(crate) fn metadata_writer(&self) -> Option<&metadata_writer::MetadataWriter> {
        self.metadata_writer.as_ref()
    }

    pub(crate) fn metadata_write_result_sink(
        &self,
    ) -> Option<async_channel::Sender<MetadataWriteResult>> {
        self.metadata_write_result_sink.clone()
    }

    pub fn settings(&self) -> &UserSettings {
        &self.settings
    }

    pub fn metadata_service(&self) -> Option<Arc<dyn MetadataService>> {
        self.metadata_service.clone()
    }

    pub fn library_tracks(&self) -> &[Track] {
        &self.library_tracks
    }

    /// Look up a single in-memory library track by id. `library_tracks` is
    /// maintained sorted by id, so this is an O(log n) binary search — the
    /// keyed path for per-track UI refreshes that must not rescan the whole
    /// library.
    pub fn library_track(&self, track_id: TrackId) -> Option<&Track> {
        self.library_tracks
            .binary_search_by_key(&track_id, |track| track.id)
            .ok()
            .map(|index| &self.library_tracks[index])
    }

    /// Test a library track against an already-normalized query (see
    /// [`normalize_query`]) using the precomputed search index — no
    /// per-track field cloning or re-lowercasing. The caller normalizes the
    /// query once and reuses it across the whole library.
    pub fn search_matches(&self, track_id: TrackId, normalized_query: &str) -> bool {
        self.search_index.matches(track_id, normalized_query)
    }

    /// Replace one in-place library track and keep its search document in
    /// lockstep. The single write path for per-track in-memory updates, so
    /// the index can never silently drift from `library_tracks`.
    fn store_library_track(&mut self, index: usize, track: Track) {
        self.search_index.insert(&track);
        self.library_tracks[index] = track;
    }

    /// Rebuild the whole search index after a wholesale `library_tracks`
    /// change (scan, import, consolidation, availability reconciliation).
    fn rebuild_search_index(&mut self) {
        self.search_index.rebuild(&self.library_tracks);
    }

    pub fn playlists(&self) -> &[Playlist] {
        &self.playlists
    }

    pub fn playlist_folders(&self) -> &[PlaylistFolder] {
        &self.playlist_folders
    }

    pub fn smart_playlists(&self) -> &[SmartPlaylist] {
        &self.smart_playlists
    }

    pub fn last_scan_library_path(&self) -> Option<&Path> {
        self.last_scan_library_path.as_deref()
    }

    pub fn last_scan_summary(&self) -> Option<&LibraryScanSummary> {
        self.last_scan_summary.as_ref()
    }

    pub fn last_library_import_summary(&self) -> Option<&LibraryImportSummary> {
        self.last_library_import_summary.as_ref()
    }

    pub fn last_library_consolidation_summary(&self) -> Option<&LibraryConsolidationSummary> {
        self.last_library_consolidation_summary.as_ref()
    }

    pub fn background_task_status(&self) -> &BackgroundTaskStatus {
        &self.background_task_status
    }

    pub fn request_library_consolidation_cancellation(&self) {
        if let Some(cancellation_requested) = &self.library_consolidation_cancellation {
            cancellation_requested.store(true, Ordering::SeqCst);
        }
    }

    pub fn request_library_scan_cancellation(&self) {
        if let Some(cancellation_requested) = &self.library_scan_cancellation {
            cancellation_requested.store(true, Ordering::SeqCst);
        }
    }

    pub fn request_library_import_cancellation(&self) {
        if let Some(cancellation_requested) = &self.library_import_cancellation {
            cancellation_requested.store(true, Ordering::SeqCst);
        }
    }

    // Dispatches a cancellation request to whichever background task is
    // currently running. Background tasks are mutually exclusive (see
    // `BackgroundTaskStatus::is_running`), so at most one of these
    // tokens is live at any moment. Idempotent: calling this with no
    // task running is a no-op, as is calling it twice while one is
    // winding down. Also pokes the notification observer so the lane
    // can repaint the running notification's label as "Cancelling..."
    // without waiting for the next worker poll tick.
    pub fn request_background_task_cancellation(&self) {
        self.request_library_scan_cancellation();
        self.request_library_import_cancellation();
        self.request_library_consolidation_cancellation();
        // A device sync runs on its own worker (not part of the
        // mutually-exclusive library-task set), so cancel it here too:
        // this is the method the status-bar Cancel button invokes, and
        // the device-sync notification it may be cancelling is
        // cancellable. The flag is reset when the next sync starts, so
        // a stale request can never abort a future sync.
        self.device_sync_scheduler.request_cancellation();
        self.notify_notification_observer();
    }

    // True while a cancellation request has been issued but the
    // background task has not yet reported back with its final status.
    // UI surfaces use this to flip the status label to "Cancelling..."
    // so the user sees their click was received.
    pub fn background_task_cancellation_requested(&self) -> bool {
        fn flag_set(token: Option<&Arc<AtomicBool>>) -> bool {
            token
                .map(|token| token.load(Ordering::SeqCst))
                .unwrap_or(false)
        }
        flag_set(self.library_scan_cancellation.as_ref())
            || flag_set(self.library_import_cancellation.as_ref())
            || flag_set(self.library_consolidation_cancellation.as_ref())
            // The device sync worker is not part of the mutually-exclusive
            // library-task set and tracks its own cancel flag, so consult
            // it directly — otherwise the lane never shows "Cancelling..."
            // after the user cancels a running sync.
            || self.device_sync_scheduler.is_cancelling()
    }

    /// Read-only view onto the notification surface for renderers.
    pub fn notifications(&self) -> &NotificationCenter {
        &self.notifications
    }

    /// Install the observer that the runtime fires after every
    /// notification mutation. The observer must not synchronously
    /// reach back into the runtime — it runs while a mutable borrow
    /// is held — so implementations should defer their work onto the
    /// main loop (e.g. `glib::idle_add_local_once`).
    pub fn set_notification_observer(&mut self, observer: NotificationObserver) {
        self.notification_observer = Some(observer);
    }

    pub fn push_persistent_notification(
        &mut self,
        category: NotificationCategory,
        severity: NotificationSeverity,
        body: String,
        cancellable: bool,
    ) -> NotificationId {
        let id = self
            .notifications
            .push_persistent(category, severity, body, cancellable);
        self.notify_notification_observer();
        id
    }

    pub fn push_ephemeral_notification(
        &mut self,
        category: NotificationCategory,
        severity: NotificationSeverity,
        body: String,
    ) -> NotificationId {
        let id = self.notifications.push_ephemeral(category, severity, body);
        self.notify_notification_observer();
        id
    }

    pub fn dismiss_notification(&mut self, id: NotificationId) {
        self.notifications.dismiss(id);
        self.notify_notification_observer();
    }

    /// Replace the body text of an existing notification in place,
    /// firing the observer so the lane re-renders without animating
    /// through a dismiss+repush.
    pub fn update_notification_body(&mut self, id: NotificationId, body: String) {
        if self.notifications.update_body(id, body) {
            self.notify_notification_observer();
        }
    }

    /// Pop the currently-displayed ephemeral. Called by the widget
    /// when its display timer has elapsed; the widget then renders the
    /// next head (or falls back to the persistent stack).
    pub fn expire_current_ephemeral_notification(&mut self) -> Option<Notification> {
        let expired = self.notifications.expire_current_ephemeral();
        if expired.is_some() {
            self.notify_notification_observer();
        }
        expired
    }

    fn notify_notification_observer(&self) {
        if let Some(observer) = &self.notification_observer {
            observer();
        }
    }

    /// Install the observer fired after every lazy availability flip
    /// (failed-play detection, library-path re-stat). The observer
    /// must not synchronously re-enter the runtime — defer onto the
    /// main loop, same contract as [`Self::set_notification_observer`].
    pub fn set_track_availability_observer(&mut self, observer: TrackAvailabilityObserver) {
        self.track_availability_observer = Some(observer);
    }

    pub(crate) fn notify_track_availability_observer(&self) {
        if let Some(observer) = &self.track_availability_observer {
            observer();
        }
    }

    /// Install the channel sender background workers use to announce
    /// that a single track's persisted state has changed under us
    /// (analysis filled BPM, online lyrics/tags wrote a row, etc.).
    /// The UI shell holds the matching receiver and forwards each id
    /// into [`Self::apply_track_updated`] on the main loop.
    pub fn set_track_updated_sink(&mut self, sink: async_channel::Sender<TrackId>) {
        self.track_updated_sink = Some(sink);
    }

    /// Install the observer fired by [`Self::apply_track_updated`].
    /// The observer must not synchronously re-enter the runtime —
    /// same contract as [`Self::set_track_availability_observer`].
    pub fn set_track_data_observer(&mut self, observer: TrackDataObserver) {
        self.track_data_observer = Some(observer);
    }

    /// Install the observer fired whenever Smart Shuffle's state
    /// changes — a training run starts, completes, or a previously
    /// persisted model is adopted. The Shuffle preferences tab
    /// installs this on open so its captions stay live; it must
    /// pair with [`Self::clear_smart_shuffle_state_observer`] on
    /// close. Same re-entrancy contract as the other observers.
    pub fn set_smart_shuffle_state_observer(&mut self, observer: SmartShuffleStateObserver) {
        self.smart_shuffle_state_observer = Some(observer);
    }

    /// Drop the observer installed by
    /// [`Self::set_smart_shuffle_state_observer`]. Called by the
    /// Shuffle preferences tab when its window closes so the closure
    /// (and any widgets it captures) can be dropped.
    pub fn clear_smart_shuffle_state_observer(&mut self) {
        self.smart_shuffle_state_observer = None;
    }

    fn fire_smart_shuffle_state_observer(&self) {
        if let Some(observer) = &self.smart_shuffle_state_observer {
            observer();
        }
    }

    /// Notify the installed [`TrackDataObserver`] that the persisted
    /// state of a single track changed under it. Callers must have
    /// already refreshed the matching entry in `library_tracks` so the
    /// observer reads the new values when it defers back into the
    /// runtime. Same re-entrancy contract as the other observers: the
    /// closure must not synchronously re-enter the runtime.
    pub(crate) fn fire_track_data_observer(&self, track_id: TrackId) {
        if let Some(observer) = &self.track_data_observer {
            observer(track_id);
        }
    }

    /// Reload the named track from the library store, replace its
    /// entry in `library_tracks` (preserving sort order: the vec is
    /// kept sorted by id and we never reorder), and fire the
    /// `track_data_observer` so visible widgets can repaint just
    /// that row. No-op when the track has vanished from the store
    /// between push and drain — the next library_changed pass will
    /// pick up the deletion.
    pub fn apply_track_updated(&mut self, track_id: TrackId) {
        let Some(store) = self.library_store.as_deref() else {
            return;
        };
        // One keyed SQLite lookup, not a full-library decode. A 10k-track
        // analysis sweep emits one event per persisted per-track update;
        // re-reading and scanning every row here would be ~O(n^2) decodes
        // on the GTK thread.
        let refreshed = match store.track(track_id).ok().flatten() {
            Some(track) => track,
            None => return,
        };
        // Keep the search document current with the reloaded metadata (an
        // online enrichment pass can rewrite title/artist/etc).
        self.search_index.insert(&refreshed);
        if let Some(slot) = self
            .library_tracks
            .iter_mut()
            .find(|track| track.id == track_id)
        {
            *slot = refreshed;
        } else {
            // Track became visible to the store between the original
            // load and now. Insert in id-sort order so the slice
            // contract holds.
            let insertion = self
                .library_tracks
                .binary_search_by_key(&track_id, |track| track.id)
                .unwrap_or_else(|index| index);
            self.library_tracks.insert(insertion, refreshed);
        }
        self.fire_track_data_observer(track_id);
    }

    pub fn playback_state(&self) -> PlaybackState {
        self.playback_service
            .as_deref()
            .map(PlaybackService::state)
            .unwrap_or_default()
    }

    pub fn set_track_ended_callback(&self, callback: TrackEndedCallback) {
        if let Some(service) = self.playback_service.as_deref() {
            service.set_on_track_ended(callback);
        }
    }

    pub fn playback_options(&self) -> PlaybackOptions {
        self.playback_queue.options()
    }

    pub fn playback_queue_current_track_id(&self) -> Option<TrackId> {
        self.playback_queue.current_track_id()
    }

    pub fn playback_queue_next_track_id(&self) -> Option<TrackId> {
        self.playback_queue.next_track_id()
    }

    pub fn now_playing(&self) -> NowPlaying {
        let state = self.playback_state();
        let track = playback::playback_track_id(&state)
            .and_then(|track_id| {
                self.library_tracks
                    .iter()
                    .find(|track| track.id == track_id)
            })
            .cloned();

        NowPlaying {
            track,
            state,
            options: self.playback_queue.options(),
        }
    }

    pub fn read_artwork(&self, path: &Path) -> Option<Vec<u8>> {
        self.metadata_service
            .as_deref()
            .and_then(|service| service.read_artwork(path).ok().flatten())
    }

    pub fn absolute_track_path(&self, track: &Track) -> Option<PathBuf> {
        self.settings
            .library_path()
            .map(|library_path| track.location.absolute_path(library_path))
    }

    /// Persist the playback volume preference. The audio path is updated
    /// separately via `PlaybackCommand::SetVolume` (which the UI dispatches
    /// immediately for responsive feedback) — this method only writes the
    /// user setting so the choice survives a restart.
    pub fn save_playback_volume(&mut self, volume: VolumePercent) -> ApplicationRuntimeResult<()> {
        if self.settings.playback.volume == volume {
            return Ok(());
        }
        self.settings.playback.volume = volume;
        self.persist_settings_with_visible_failure()
    }

    /// Persist the current in-memory `settings` and, on failure, surface
    /// the problem through the NotificationCenter so a normal-operation
    /// save error (debounced volume drag, sidebar state) is never silently
    /// swallowed. The in-memory value already took effect; the user only
    /// needs to know it will not survive a restart. The error is still
    /// returned for callers that want to react.
    ///
    /// This is for *live* saves. The close-time UI-state save has no
    /// visible surface left (the window is tearing down) and is handled
    /// deliberately at its call site with a logged fallback instead.
    fn persist_settings_with_visible_failure(&mut self) -> ApplicationRuntimeResult<()> {
        let save_result = match self.settings_store.as_ref() {
            Some(store) => store.save_settings(self.settings.clone()),
            None => Ok(()),
        };
        if save_result.is_err() {
            self.push_ephemeral_notification(
                NotificationCategory::Settings,
                NotificationSeverity::Warning,
                runtime_error_text(&ApplicationRuntimeError::SettingsSaveFailed).to_owned(),
            );
            return Err(ApplicationRuntimeError::SettingsSaveFailed);
        }
        Ok(())
    }

    /// Result channel the UI shell drains on idle ticks to receive
    /// completed Smart Shuffle index rebuilds.
    pub fn smart_shuffle_rebuild_result_receiver(
        &self,
    ) -> async_channel::Receiver<SmartShuffleRebuildResult> {
        self.smart_shuffle_scheduler.result_receiver()
    }

    pub fn smart_shuffle_is_rebuilding(&self) -> bool {
        self.smart_shuffle_scheduler.is_rebuilding()
    }

    pub fn smart_shuffle_metadata(&self) -> Option<SmartShuffleIndexMetadata> {
        self.smart_shuffle_metadata
    }

    pub fn smart_shuffle_index_is_loaded(&self) -> bool {
        self.smart_shuffle_index.is_some()
    }

    /// Try to load the persisted Smart Shuffle index from the library
    /// store. Called once during runtime setup; silently discards a
    /// blob whose schema version no longer matches the current scorer
    /// so we never feed a stale-shaped index to the picker.
    pub fn load_smart_shuffle_index_from_store(&mut self) -> ApplicationRuntimeResult<()> {
        let Some(store) = self.library_store.as_ref() else {
            return Ok(());
        };
        let stored = store
            .load_smart_shuffle_index()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let Some(stored) = stored else {
            return Ok(());
        };
        if stored.schema_version != SMART_SHUFFLE_INDEX_SCHEMA_VERSION {
            // Stale shape — clear so the next rebuild starts clean.
            let _ = store.clear_smart_shuffle_index();
            return Ok(());
        }
        match SmartShuffleIndex::from_blob(&stored.index_blob) {
            Ok(index) => {
                self.smart_shuffle_metadata = Some(index_metadata(&index));
                self.smart_shuffle_index = Some(index);
            }
            Err(_) => {
                let _ = store.clear_smart_shuffle_index();
            }
        }
        Ok(())
    }

    /// Schedule a fresh Smart Shuffle index rebuild on the background
    /// worker. Returns `false` when the scheduler is already busy or
    /// there is no library to index. The result is delivered through
    /// [`Self::smart_shuffle_rebuild_result_receiver`] and applied via
    /// [`Self::apply_smart_shuffle_rebuild_result`].
    pub fn request_smart_shuffle_rebuild(&mut self) -> bool {
        if self.library_tracks.is_empty() {
            return false;
        }
        let tracks = self.library_tracks.clone();
        // Acoustics are the enhancement layer (§13): the index caches
        // them for the timbral terms and the loudness guard, but Smart
        // Shuffle works without them. A missing store or a load error
        // degrades gracefully to a zero-coverage, metadata-only index
        // rather than blocking the rebuild.
        let acoustics = self
            .library_store
            .as_ref()
            .and_then(|store| store.load_all_acoustics().ok())
            .unwrap_or_default();
        let now = self.clock.now();
        let scheduled = self
            .smart_shuffle_scheduler
            .request_rebuild(tracks, acoustics, now);
        if scheduled {
            self.fire_smart_shuffle_state_observer();
        }
        scheduled
    }

    /// Apply a completed index rebuild: persist the new index's blob
    /// and adopt it in memory. The Smart Shuffle state observer is
    /// fired exactly once on the way out — the scheduler's
    /// `is_rebuilding` flag flipped from true to false, so a
    /// subscribed preferences tab must re-read its state. Rebuilds are
    /// otherwise silent (they happen on a background cadence; a toast
    /// on every daily rebuild would be noise).
    pub fn apply_smart_shuffle_rebuild_result(&mut self, result: SmartShuffleRebuildResult) {
        self.apply_smart_shuffle_rebuild_result_inner(result);
        self.fire_smart_shuffle_state_observer();
    }

    fn apply_smart_shuffle_rebuild_result_inner(&mut self, result: SmartShuffleRebuildResult) {
        let index = result.index;
        let Ok(blob) = index.to_blob() else {
            self.notifications.push_ephemeral(
                NotificationCategory::SmartShuffle,
                NotificationSeverity::Warning,
                "Smart Shuffle: failed to serialise the rebuilt index.".to_owned(),
            );
            return;
        };
        let stored = sustain_library_store::StoredSmartShuffleIndex {
            index_blob: blob,
            schema_version: SMART_SHUFFLE_INDEX_SCHEMA_VERSION,
        };
        if let Some(store) = self.library_store.as_ref()
            && store.save_smart_shuffle_index(&stored).is_err()
        {
            self.notifications.push_ephemeral(
                NotificationCategory::SmartShuffle,
                NotificationSeverity::Warning,
                "Smart Shuffle: failed to persist the rebuilt index.".to_owned(),
            );
            return;
        }
        self.smart_shuffle_metadata = Some(index_metadata(&index));
        self.smart_shuffle_index = Some(index);
    }

    /// Hook invoked from the shuffle-mode command handler. When the
    /// user switches to Smart and no index exists yet, kick off a
    /// background rebuild so the picker has genre IDF to work with;
    /// until it lands the picker degrades gracefully to near-uniform
    /// picks rather than refusing to play.
    pub(crate) fn on_shuffle_mode_changed(&mut self) {
        let mode = self.playback_queue.options().shuffle_mode;
        if !matches!(mode, ShuffleMode::Smart) {
            return;
        }
        if self.smart_shuffle_index.is_some() {
            return;
        }
        if !self.smart_shuffle_scheduler.is_rebuilding() {
            self.request_smart_shuffle_rebuild();
        }
    }

    /// Mirror the current playback queue's shuffle mode into the persisted
    /// user settings. Called from the shuffle command handler so the choice
    /// survives a restart, the same way [`Self::save_playback_volume`] does.
    pub(crate) fn persist_playback_shuffle_mode(&mut self) -> ApplicationRuntimeResult<()> {
        let shuffle_mode = self.playback_queue.options().shuffle_mode;
        if self.settings.playback.shuffle_mode == shuffle_mode {
            return Ok(());
        }
        self.settings.playback.shuffle_mode = shuffle_mode;
        if let Some(store) = self.settings_store.as_ref() {
            store
                .save_settings(self.settings.clone())
                .map_err(|_| ApplicationRuntimeError::SettingsSaveFailed)?;
        }
        Ok(())
    }

    pub fn save_ui_settings(&mut self, ui: UiSettings) -> ApplicationRuntimeResult<()> {
        if self.settings.ui == ui {
            return Ok(());
        }
        self.settings.ui = ui;
        if let Some(store) = self.settings_store.as_ref() {
            store
                .save_settings(self.settings.clone())
                .map_err(|_| ApplicationRuntimeError::SettingsSaveFailed)?;
        }
        Ok(())
    }

    pub fn load_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
    ) -> ApplicationRuntimeResult<Option<TrackColumnLayout>> {
        let Some(store) = self.library_store.as_deref() else {
            return Ok(None);
        };
        store
            .load_track_column_layout(scope)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
    }

    pub fn save_track_column_layout(
        &self,
        scope: TrackColumnLayoutScope,
        layout: &TrackColumnLayout,
    ) -> ApplicationRuntimeResult<()> {
        let Some(store) = self.library_store.as_deref() else {
            return Err(ApplicationRuntimeError::LibraryStoreFailed);
        };
        store
            .save_track_column_layout(scope, layout)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
    }

    pub fn smart_playlist_matching_tracks(
        &self,
        smart_playlist_id: SmartPlaylistId,
    ) -> Vec<&Track> {
        let Some(smart_playlist) = self
            .smart_playlists
            .iter()
            .find(|smart_playlist| smart_playlist.id == smart_playlist_id)
        else {
            return Vec::new();
        };
        matching_tracks(
            &self.library_tracks,
            &smart_playlist.rules,
            self.clock.now(),
        )
    }

    /// Where a single track stands relative to a smart playlist after
    /// a background mutation: included, excluded, or
    /// "indeterminable without a full re-evaluation". The third case
    /// covers limit-based smart playlists (Top-N), where updating
    /// track X can evict track Y from the visible set — partial
    /// inspection of just track X is not enough.
    ///
    /// The UI shell uses this to decide whether a track update can
    /// be applied as an in-place row refresh (preserves scroll and
    /// selection) or has to fall back to a full table rebuild.
    /// Calling this with a non-existent smart-playlist id returns
    /// [`SmartPlaylistTrackStatus::Excluded`] — the track is not in
    /// the (empty) view.
    pub fn smart_playlist_track_status(
        &self,
        smart_playlist_id: SmartPlaylistId,
        track_id: TrackId,
    ) -> SmartPlaylistTrackStatus {
        let Some(smart_playlist) = self
            .smart_playlists
            .iter()
            .find(|smart_playlist| smart_playlist.id == smart_playlist_id)
        else {
            return SmartPlaylistTrackStatus::Excluded;
        };
        if smart_playlist.rules.limit.is_some() {
            // Limit-based: a per-track check cannot capture the
            // eviction effect, so admit we don't know and let the
            // caller rebuild.
            return SmartPlaylistTrackStatus::RequiresFullRebuild;
        }
        let Some(track) = self
            .library_tracks
            .iter()
            .find(|track| track.id == track_id)
        else {
            return SmartPlaylistTrackStatus::Excluded;
        };
        if track_matches_rule_set(track, &smart_playlist.rules, self.clock.now()) {
            SmartPlaylistTrackStatus::Included
        } else {
            SmartPlaylistTrackStatus::Excluded
        }
    }

    /// Resolve the track IDs of a playlist sidebar entry. Regular
    /// playlists return their persisted entries; smart playlists
    /// return the current rule-evaluated track set. Returns `None`
    /// when the item is a folder, or the supplied id is unknown.
    /// An empty `Vec` is a valid return value for a known-but-empty
    /// playlist.
    pub fn playlist_item_track_ids(&self, item: PlaylistItem) -> Option<Vec<TrackId>> {
        match item {
            PlaylistItem::Playlist(id) => self
                .playlists
                .iter()
                .find(|playlist| playlist.id == id)
                .map(|playlist| {
                    playlist
                        .entries
                        .iter()
                        .map(|entry| entry.track_id)
                        .collect()
                }),
            PlaylistItem::SmartPlaylist(id) => {
                let exists = self
                    .smart_playlists
                    .iter()
                    .any(|smart_playlist| smart_playlist.id == id);
                if !exists {
                    return None;
                }
                Some(
                    self.smart_playlist_matching_tracks(id)
                        .into_iter()
                        .map(|track| track.id)
                        .collect(),
                )
            }
            PlaylistItem::Folder(_) => None,
        }
    }

    /// Request an analysis run for the tracks resolved from a sidebar
    /// playlist entry. Folders, unknown ids, and empty playlists all
    /// short-circuit to [`RunDecision::TargetEmpty`]; otherwise the
    /// track set is forwarded to [`Self::request_tracks_analysis_run`].
    pub fn request_playlist_analysis_run(
        &mut self,
        item: PlaylistItem,
        request: AnalysisRunRequest,
    ) -> RunDecision {
        let Some(track_ids) = self.playlist_item_track_ids(item) else {
            return RunDecision::TargetEmpty;
        };
        self.request_tracks_analysis_run(track_ids, request)
    }

    /// Request an online-retrieval run for the tracks resolved from a
    /// sidebar playlist entry. Symmetric to
    /// [`Self::request_playlist_analysis_run`] but targets the online
    /// scheduler.
    pub fn request_playlist_online_run(
        &mut self,
        item: PlaylistItem,
        request: OnlineRunRequest,
    ) -> RunDecision {
        let Some(track_ids) = self.playlist_item_track_ids(item) else {
            return RunDecision::TargetEmpty;
        };
        self.request_tracks_online_run(track_ids, request)
    }

    /// Request an analysis run for an explicit set of track ids.
    ///
    /// Decision tree:
    ///   * `Single(capability)` with the matching global toggle on
    ///     -> [`RunDecision::DeniedBackgroundEnabled`] (the background
    ///     sweep is already going to process every track that needs
    ///     this capability, the per-set trigger would be redundant).
    ///   * Empty track set -> [`RunDecision::TargetEmpty`].
    ///   * Library store not installed -> [`RunDecision::SchedulerUnavailable`]
    ///     (we cannot filter, so we refuse to dispatch).
    ///   * Track set non-empty but the filter prunes every track
    ///     (all requested capabilities are already cached) ->
    ///     [`RunDecision::AlreadyComplete`].
    ///   * Scheduler not started -> [`RunDecision::SchedulerUnavailable`].
    ///   * Otherwise -> [`RunDecision::Accepted`] and the filtered
    ///     subset is dispatched.
    ///
    /// `All` always submits the full BPM+key+audio mask regardless
    /// of which global toggles are on; the filter still applies so
    /// re-running `All` on a fully-analyzed playlist is a no-op.
    pub fn request_tracks_analysis_run(
        &mut self,
        track_ids: Vec<TrackId>,
        request: AnalysisRunRequest,
    ) -> RunDecision {
        if let AnalysisRunRequest::Single(capability) = request {
            let global_on = match capability {
                AnalysisCapability::Bpm => self.settings.analysis.bpm,
                AnalysisCapability::Key => self.settings.analysis.key,
                AnalysisCapability::Audio => self.settings.analysis.audio,
            };
            if global_on {
                self.push_ephemeral_notification(
                    NotificationCategory::AnalysisBackground,
                    NotificationSeverity::Info,
                    format!(
                        "Background {} is enabled. These tracks are already queued by the global sweep.",
                        capability.label()
                    ),
                );
                return RunDecision::DeniedBackgroundEnabled;
            }
        }
        if track_ids.is_empty() {
            return RunDecision::TargetEmpty;
        }
        let capabilities = request.capabilities();
        let Some(library_store) = self.library_store.clone() else {
            return RunDecision::SchedulerUnavailable;
        };
        let original_count = track_ids.len();
        let filtered = match library_store.filter_tracks_needing_analysis(
            &track_ids,
            capabilities,
            sustain_analysis::ANALYZER_VERSION,
        ) {
            Ok(filtered) => filtered,
            Err(_) => return RunDecision::SchedulerUnavailable,
        };
        if filtered.is_empty() {
            self.push_ephemeral_notification(
                NotificationCategory::AnalysisBackground,
                NotificationSeverity::Info,
                already_complete_text(original_count, request.label()),
            );
            return RunDecision::AlreadyComplete;
        }
        let Some(scheduler) = self.analysis_scheduler.as_ref() else {
            return RunDecision::SchedulerUnavailable;
        };
        let count = filtered.len();
        scheduler.request_explicit_run(filtered, capabilities);
        self.push_ephemeral_notification(
            NotificationCategory::AnalysisBackground,
            NotificationSeverity::Info,
            queued_text(count, request.label()),
        );
        RunDecision::Accepted
    }

    /// Request an online retrieval run for an explicit set of track
    /// ids. Unlike [`Self::request_tracks_analysis_run`], this is a
    /// *force* path: it ignores the `*_attempted_at_unix` stamps so a
    /// manual click re-contacts tracks that previously came back empty,
    /// and it fires even when the matching background toggle is on
    /// (the user asked for it now). It is safe to skip the pre-filter
    /// because the online scheduler's per-track guard is missing-only —
    /// tracks with stored lyrics / embedded artwork / an existing tag
    /// are skipped there, and tag fills never overwrite — so only the
    /// previously-empty tracks are actually contacted (see issue #61
    /// and the `online_scheduler` module header).
    pub fn request_tracks_online_run(
        &mut self,
        track_ids: Vec<TrackId>,
        request: OnlineRunRequest,
    ) -> RunDecision {
        if track_ids.is_empty() {
            return RunDecision::TargetEmpty;
        }
        let capabilities = request.capabilities();
        let Some(scheduler) = self.online_scheduler.as_ref() else {
            return RunDecision::SchedulerUnavailable;
        };
        let count = track_ids.len();
        scheduler.request_explicit_run(track_ids, capabilities);
        self.push_ephemeral_notification(
            NotificationCategory::OnlineBackground,
            NotificationSeverity::Info,
            queued_text(count, request.label()),
        );
        RunDecision::Accepted
    }
}

fn queued_text(count: usize, label: &str) -> String {
    format!(
        "Queued {count} {noun} for {label}.",
        noun = if count == 1 { "track" } else { "tracks" },
    )
}

fn already_complete_text(count: usize, label: &str) -> String {
    if count == 1 {
        format!("That track already has {label} — nothing to queue.")
    } else {
        format!("All {count} tracks already have {label} — nothing to queue.")
    }
}

/// Mirror the live index's bookkeeping into the cached metadata the
/// Preferences caption reads.
fn index_metadata(index: &SmartShuffleIndex) -> SmartShuffleIndexMetadata {
    SmartShuffleIndexMetadata {
        indexed_track_count: index.indexed_track_count(),
        analysis_coverage: index.analysis_coverage(),
        built_at: std::time::UNIX_EPOCH
            + std::time::Duration::from_secs(index.built_at_unix().max(0) as u64),
    }
}

/// Outcome of [`ApplicationRuntime::smart_playlist_track_status`].
/// Drives the UI shell's decision between an in-place row refresh
/// and a full table rebuild after a background track mutation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistTrackStatus {
    /// The track currently matches the smart playlist's rules.
    Included,
    /// The track does not match the smart playlist's rules.
    Excluded,
    /// The smart playlist has a limit; partial inspection is not
    /// sufficient because the limit may evict other tracks when this
    /// one's data changes. Callers should fall back to a full
    /// re-evaluation of the playlist.
    RequiresFullRebuild,
}

/// Single-capability selector for an analysis run. The right-click
/// menus expose one per-capability menu entry per variant; each is
/// rendered insensitive when its matching global toggle is on (the
/// background sweep is already going to cover it).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisCapability {
    Bpm,
    Key,
    Audio,
}

impl AnalysisCapability {
    /// Human-readable label for the notification text the runtime
    /// emits when a per-set run is accepted or denied.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bpm => "BPM analysis",
            Self::Key => "key detection",
            Self::Audio => "audio analysis",
        }
    }
}

/// Single-capability selector for an online retrieval run.
/// Counterpart to [`AnalysisCapability`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnlineCapability {
    Lyrics,
    Artwork,
    Tags,
}

impl OnlineCapability {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lyrics => "lyrics retrieval",
            Self::Artwork => "artwork retrieval",
            Self::Tags => "tag enrichment",
        }
    }
}

/// Shape of an analysis-run request submitted by the right-click
/// menus. `Single(cap)` corresponds to a per-capability menu item;
/// `All` corresponds to the bundle entry that submits BPM+Key+
/// Audio in a single dispatch.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnalysisRunRequest {
    Single(AnalysisCapability),
    All,
}

impl AnalysisRunRequest {
    /// Bitmask the scheduler should run for this request.
    pub fn capabilities(self) -> AnalysisCapabilities {
        match self {
            Self::Single(AnalysisCapability::Bpm) => AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            Self::Single(AnalysisCapability::Key) => AnalysisCapabilities {
                bpm: false,
                key: true,
                audio: false,
            },
            Self::Single(AnalysisCapability::Audio) => AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            Self::All => AnalysisCapabilities {
                bpm: true,
                key: true,
                audio: true,
            },
        }
    }

    /// Notification label for the accepted case.
    pub fn label(self) -> &'static str {
        match self {
            Self::Single(capability) => capability.label(),
            Self::All => "analysis",
        }
    }
}

/// Shape of an online-retrieval-run request submitted by the
/// right-click menus. Counterpart to [`AnalysisRunRequest`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnlineRunRequest {
    Single(OnlineCapability),
    All,
}

impl OnlineRunRequest {
    pub fn capabilities(self) -> OnlineCapabilities {
        match self {
            Self::Single(OnlineCapability::Lyrics) => OnlineCapabilities {
                lyrics: true,
                artwork: false,
                tags: false,
            },
            Self::Single(OnlineCapability::Artwork) => OnlineCapabilities {
                lyrics: false,
                artwork: true,
                tags: false,
            },
            Self::Single(OnlineCapability::Tags) => OnlineCapabilities {
                lyrics: false,
                artwork: false,
                tags: true,
            },
            Self::All => OnlineCapabilities {
                lyrics: true,
                artwork: true,
                tags: true,
            },
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Single(capability) => capability.label(),
            Self::All => "online retrieval",
        }
    }
}

/// Outcome of a per-set run request. The runtime always pushes an
/// ephemeral notification matching the decision; the value is
/// returned so callers (UI code, tests) can observe what happened
/// without having to scrape the notification lane.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RunDecision {
    /// Submitted to the matching scheduler. The user will see a
    /// "Queued N tracks..." notification followed by the regular
    /// background-progress notification while the work runs.
    Accepted,
    /// The corresponding global setting toggle is on, so the tracks
    /// are already going to be processed by the background sweep.
    /// The per-set trigger is redundant and the request is rejected.
    /// Only [`AnalysisRunRequest::Single`] / [`OnlineRunRequest::Single`]
    /// can be denied this way; the `All` variants always proceed.
    DeniedBackgroundEnabled,
    /// The supplied target resolves to no tracks (folder row, unknown
    /// playlist id, empty playlist, or empty explicit Vec).
    TargetEmpty,
    /// The supplied target had tracks, but every one of them already
    /// has the requested capability cached (BPM/key/waveform for
    /// analysis; tag/artwork/lyrics for online). Nothing is queued.
    /// The user sees a notification distinguishing this from the
    /// `Accepted` path so a no-op click on a fully-analyzed playlist
    /// is visible.
    AlreadyComplete,
    /// The matching scheduler has not been started (e.g. headless
    /// runtime, tests). Nothing to dispatch to.
    SchedulerUnavailable,
}

impl Default for ApplicationRuntime {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
#[allow(clippy::panic, reason = "test failures use panic! to report context")]
#[path = "lib_tests.rs"]
mod tests;
