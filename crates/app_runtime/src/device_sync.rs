// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Runtime glue for device sync (issues #23 / #24).
//!
//! The durable format crates ([`sustain_device_sync`] and the
//! `sustain-pioneer` crate it builds on) do the work; this module
//! connects them to the runtime's state: it
//! discovers connected devices, resolves a device's saved playlist
//! selection (smart playlists re-evaluated every time) into the engine's
//! neutral inputs, drives the background sync scheduler, and reports
//! progress through the [`crate::NotificationCenter`].

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use sustain_device_sync::{
    ConnectedDevice, PreparedPioneerAssets, PreparedSyncRequest, SourceSnapshot, SyncInputPlaylist,
    SyncInputTrack, SyncOutcome, SyncProgress, SyncRequest, SyncStage, capacity, engine,
    resolve_source_fingerprint, source_file_stat,
};
use sustain_domain::{
    DeviceLayout, DeviceRelativePath, FilesPerFolderCap, MusicalKey, Playlist, PlaylistItem,
    SmartPlaylist, SyncDevice, SyncDeviceId, Track, TrackId, matching_tracks,
};
use sustain_i18n::{gettext, tr_format};
use sustain_library_store::{AnalysisCapabilities, LibraryStore};
use sustain_metadata::MetadataService;

use crate::device_plan_scheduler::{
    DeviceMountIdentity, DevicePlanGeneration, DevicePlanResult, DevicePlanSnapshot,
};
use crate::device_sync_scheduler::{DeviceSyncEvent, DeviceSyncStartOutcome};
use crate::{
    AnalysisRunRequest, ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult,
    NotificationCategory, NotificationSeverity, notifications,
};

/// Per-device analysis coverage for the ticked playlists, shown in the
/// Pioneer export panel. `analyzable` is how many tracks an analysis run
/// would still touch (distinguishes "not yet attempted" from "attempted,
/// no confident result": a track counted in `missing_bpm` but not in
/// `analyzable` was already attempted and produced nothing).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceAnalysisReadiness {
    pub total: usize,
    pub missing_bpm: usize,
    pub missing_key: usize,
    pub missing_waveform: usize,
    pub analyzable: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceSyncPlanState {
    Pending,
    Ready(DevicePlanSnapshot),
    Unavailable,
}

pub(crate) struct DevicePlanCache {
    generation: DevicePlanGeneration,
    mount: DeviceMountIdentity,
    state: DeviceSyncPlanState,
}

impl ApplicationRuntime {
    /// Enumerate currently-connected devices, resolved against the saved
    /// device configuration. Performs filesystem probing — call lazily
    /// (never during the cold-start window).
    pub fn connected_devices(&self) -> Vec<ConnectedDevice> {
        let known = self
            .library_store
            .as_ref()
            .and_then(|store| store.sync_devices().ok())
            .unwrap_or_default();
        sustain_device_sync::discover(&known)
    }

    /// The saved configuration for a device, if Sustain has it.
    pub fn device_config(&self, id: &SyncDeviceId) -> Option<SyncDevice> {
        self.library_store
            .as_ref()
            .and_then(|store| store.sync_device(id).ok().flatten())
    }

    /// The saved ticked-playlist selection for a device.
    pub fn device_selection(&self, id: &SyncDeviceId) -> Vec<PlaylistItem> {
        self.library_store
            .as_ref()
            .and_then(|store| store.device_selection(id).ok())
            .unwrap_or_default()
    }

    /// The deduplicated set of library tracks the device's ticked
    /// playlists resolve to, in first-seen order — a track in several
    /// selected playlists counts once. Smart playlists are evaluated
    /// live. Drives the status-bar track/duration/size summary while the
    /// device view is shown.
    pub fn device_selected_tracks(&self, id: &SyncDeviceId) -> Vec<Track> {
        let by_id: HashMap<_, _> = self.library_tracks.iter().map(|t| (t.id, t)).collect();
        let mut seen = HashSet::new();
        let mut tracks = Vec::new();
        for item in self.device_selection(id) {
            let Some(track_ids) = self.playlist_item_track_ids(item) else {
                continue;
            };
            for tid in track_ids {
                if seen.insert(tid)
                    && let Some(track) = by_id.get(&tid)
                {
                    tracks.push((*track).clone());
                }
            }
        }
        tracks
    }

    /// True while a device sync is running on the background worker.
    pub fn device_sync_in_progress(&self) -> bool {
        self.device_sync_scheduler.is_syncing()
    }

    /// Ask the in-flight device sync to stop at the next file boundary.
    pub fn request_device_sync_cancellation(&self) {
        self.device_sync_scheduler.request_cancellation();
        self.notify_notification_observer();
    }

    /// Cancel and join an in-flight sync before runtime teardown returns.
    pub fn shutdown_device_sync_scheduler(&mut self) {
        if let Some(id) = self.device_sync_notification_id.take() {
            self.dismiss_notification(id);
        }
        self.device_sync_scheduler.shutdown();
    }

    /// Event channel the UI shell drains on idle, feeding each event back
    /// into [`Self::apply_device_sync_event`].
    pub fn device_sync_event_receiver(&self) -> async_channel::Receiver<DeviceSyncEvent> {
        self.device_sync_scheduler.event_receiver()
    }

    pub fn device_plan_result_receiver(&self) -> async_channel::Receiver<DevicePlanResult> {
        self.device_plan_scheduler.result_receiver()
    }

    pub fn shutdown_device_plan_scheduler(&mut self) {
        self.device_plan_scheduler.shutdown();
        self.device_plan_cache = None;
    }

    /// Ensure a saved-config row exists for a connected device, creating
    /// one with sensible defaults (and refreshing its volume id) if not.
    /// The UI calls this when a device panel opens, so subsequent
    /// configuration commands have a row to update.
    pub fn ensure_device_config(
        &self,
        connected: &ConnectedDevice,
    ) -> ApplicationRuntimeResult<SyncDevice> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        if let Some(mut existing) = store
            .sync_device(&connected.id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        {
            // Keep the volume id fresh for marker-loss fallback recognition.
            if existing.volume_id != connected.volume_id && connected.volume_id.is_some() {
                existing.volume_id = connected.volume_id.clone();
                store
                    .save_sync_device(&existing)
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
            }
            return Ok(existing);
        }
        let device = SyncDevice {
            id: connected.id.clone(),
            label: connected.label.clone(),
            kind: connected.kind,
            layout: DeviceLayout::M3u,
            sub_path: DeviceRelativePath::new(connected.kind.default_sub_path())
                .expect("static device default sub-path is safe"),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: connected.volume_id.clone(),
        };
        store
            .save_sync_device(&device)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        Ok(device)
    }

    // --- Configuration command handlers ---

    pub(crate) fn set_device_layout(
        &mut self,
        id: SyncDeviceId,
        layout: DeviceLayout,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.layout = layout;
        self.persist_device(&device)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    pub(crate) fn set_device_sub_path(
        &mut self,
        id: SyncDeviceId,
        sub_path: DeviceRelativePath,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.sub_path = sub_path;
        self.persist_device(&device)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    pub(crate) fn set_device_files_per_folder_cap(
        &mut self,
        id: SyncDeviceId,
        cap: FilesPerFolderCap,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.files_per_folder_cap = cap;
        self.persist_device(&device)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    pub(crate) fn rename_device(
        &mut self,
        id: SyncDeviceId,
        label: String,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.label = label;
        self.persist_device(&device)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    pub(crate) fn set_device_selection(
        &mut self,
        id: SyncDeviceId,
        selection: Vec<PlaylistItem>,
    ) -> ApplicationRuntimeResult<()> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        store
            .save_device_selection(&id, &selection)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    pub(crate) fn forget_device(&mut self, id: SyncDeviceId) -> ApplicationRuntimeResult<()> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        store
            .delete_sync_device(&id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        self.invalidate_device_sync_plan();
        Ok(())
    }

    fn device_config_or_default(&self, id: &SyncDeviceId) -> SyncDevice {
        self.device_config(id).unwrap_or_else(|| SyncDevice {
            id: id.clone(),
            label: "Device".to_owned(),
            kind: sustain_domain::DeviceKind::UsbDrive,
            layout: DeviceLayout::M3u,
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        })
    }

    fn persist_device(&self, device: &SyncDevice) -> ApplicationRuntimeResult<()> {
        self.library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?
            .save_sync_device(device)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
    }

    // --- Analysis readiness (Pioneer panel) ---

    /// Analysis coverage over the tracks in a device's ticked playlists.
    pub fn device_analysis_readiness(&self, id: &SyncDeviceId) -> DeviceAnalysisReadiness {
        let track_ids = self.device_selection_track_ids(id);
        let total = track_ids.len();
        let mut readiness = DeviceAnalysisReadiness {
            total,
            ..Default::default()
        };
        let by_id: HashMap<_, _> = self.library_tracks.iter().map(|t| (t.id, t)).collect();
        for tid in &track_ids {
            if let Some(track) = by_id.get(tid) {
                if track.metadata.bpm.is_none() {
                    readiness.missing_bpm += 1;
                }
                if track.metadata.key.is_none() {
                    readiness.missing_key += 1;
                }
            }
        }
        if let Some(store) = self.library_store.as_ref() {
            let version = sustain_analysis::ANALYZER_VERSION;
            let audio_only = AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            };
            readiness.missing_waveform = store
                .filter_tracks_needing_analysis(&track_ids, audio_only, version)
                .map(|v| v.len())
                .unwrap_or(0);
            readiness.analyzable = store
                .filter_tracks_needing_analysis(&track_ids, AnalysisCapabilities::all(), version)
                .map(|v| v.len())
                .unwrap_or(0);
        }
        readiness
    }

    pub(crate) fn analyze_device_tracks(
        &mut self,
        id: SyncDeviceId,
    ) -> ApplicationRuntimeResult<()> {
        let track_ids = self.device_selection_track_ids(&id);
        if track_ids.is_empty() {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Info,
                gettext("This device's selection is empty."),
            );
            return Ok(());
        }
        // Reuse the shared analysis pipeline; it pushes its own
        // queued/already-complete notifications.
        self.request_tracks_analysis_run(track_ids, AnalysisRunRequest::All);
        Ok(())
    }

    // --- Plan + sync ---

    pub fn request_device_sync_plan(&mut self, connected: &ConnectedDevice) {
        let generation = self.next_device_plan_generation();
        let mount = mount_identity(connected);
        self.device_plan_cache = Some(DevicePlanCache {
            generation,
            mount: mount.clone(),
            state: DeviceSyncPlanState::Pending,
        });

        let Some(store) = self.library_store.clone() else {
            self.mark_device_plan_unavailable(generation, &mount);
            return;
        };
        let preparation = DevicePlanPreparation {
            mount: mount.clone(),
            store,
            library_path: self.settings.library.path.clone(),
            now: self.clock.now(),
            export_date: unix_to_ymd(self.clock_unix_secs()),
        };
        if self
            .device_plan_scheduler
            .request_plan(
                generation,
                mount.clone(),
                Box::new(move |cancelled| preparation.run(cancelled)),
            )
            .is_err()
        {
            self.mark_device_plan_unavailable(generation, &mount);
        }
    }

    pub fn invalidate_device_sync_plan(&mut self) {
        let generation = self.next_device_plan_generation();
        self.device_plan_scheduler.cancel_before(generation);
        self.device_plan_cache = None;
    }

    pub fn device_sync_plan_state(&self, connected: &ConnectedDevice) -> DeviceSyncPlanState {
        let mount = mount_identity(connected);
        self.device_plan_cache
            .as_ref()
            .filter(|cache| cache.mount == mount)
            .map(|cache| cache.state.clone())
            .unwrap_or(DeviceSyncPlanState::Unavailable)
    }

    pub fn apply_device_plan_result(&mut self, result: DevicePlanResult) -> bool {
        let Some(cache) = self.device_plan_cache.as_mut() else {
            return false;
        };
        if cache.generation != result.generation || cache.mount != result.mount {
            return false;
        }
        cache.state = match result.result {
            Ok(snapshot) => DeviceSyncPlanState::Ready(snapshot),
            Err(_) => DeviceSyncPlanState::Unavailable,
        };
        true
    }

    fn next_device_plan_generation(&mut self) -> DevicePlanGeneration {
        self.next_device_plan_generation = self
            .next_device_plan_generation
            .checked_add(1)
            .expect("device-plan generation space exhausted");
        DevicePlanGeneration::new(self.next_device_plan_generation)
    }

    fn mark_device_plan_unavailable(
        &mut self,
        generation: DevicePlanGeneration,
        mount: &DeviceMountIdentity,
    ) {
        if let Some(cache) = self.device_plan_cache.as_mut()
            && cache.generation == generation
            && cache.mount == *mount
        {
            cache.state = DeviceSyncPlanState::Unavailable;
        }
    }

    pub(crate) fn start_device_sync(
        &mut self,
        id: SyncDeviceId,
        remove_stale: bool,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_library_hydrated()?;
        if self.device_sync_scheduler.is_syncing() {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Info,
                gettext("A device sync is already running."),
            );
            return Ok(());
        }
        let Some(connected) = self.connected_devices().into_iter().find(|d| d.id == id) else {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Warning,
                gettext("That device is no longer connected."),
            );
            return Ok(());
        };
        let device = self.ensure_device_config(&connected)?;
        if self.device_selection(&id).is_empty() {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Info,
                gettext("Pick at least one playlist to sync to this device."),
            );
            return Ok(());
        }
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let preparation = DeviceSyncPreparation {
            mount: mount_identity(&connected),
            store,
            metadata_service,
            library_path: self.settings.library.path.clone(),
            now: self.clock.now(),
            export_date: unix_to_ymd(self.clock_unix_secs()),
            remove_stale,
        };
        match self
            .device_sync_scheduler
            .start(id, move |progress, cancel| {
                preparation.run(progress, cancel)
            }) {
            DeviceSyncStartOutcome::Started(_) => {
                let notification = self.push_persistent_notification(
                    NotificationCategory::DeviceSync,
                    NotificationSeverity::Info,
                    notifications::device_sync_running_text(&device.label),
                    true,
                );
                self.device_sync_notification_id = Some(notification);
            }
            DeviceSyncStartOutcome::AlreadyRunning => {
                self.push_ephemeral_notification(
                    NotificationCategory::DeviceSync,
                    NotificationSeverity::Info,
                    gettext("A device sync is already running."),
                );
            }
            DeviceSyncStartOutcome::SpawnFailed(detail) => {
                self.push_ephemeral_notification(
                    NotificationCategory::DeviceSync,
                    NotificationSeverity::Error,
                    tr_format!(
                        gettext("Device sync failed to start: {detail}"),
                        detail = detail
                    ),
                );
            }
        }
        Ok(())
    }

    /// Apply a sync event drained from the worker channel: update the
    /// progress notification, or on completion persist the manifest and
    /// publish the outcome.
    pub fn apply_device_sync_event(&mut self, event: DeviceSyncEvent) {
        match event {
            DeviceSyncEvent::Progress { run_id, progress } => {
                if !self.device_sync_scheduler.is_active_run(run_id) {
                    return;
                }
                if let Some(id) = self.device_sync_notification_id {
                    self.update_notification_body(
                        id,
                        notifications::device_sync_progress_text(progress),
                    );
                }
            }
            DeviceSyncEvent::Finished { run_id, completion } => {
                if !self.device_sync_scheduler.is_active_run(run_id) {
                    return;
                }
                if let Some(id) = self.device_sync_notification_id.take() {
                    self.dismiss_notification(id);
                }
                match completion.result {
                    Ok(outcome) => {
                        let manifest_saved = !outcome.manifest_is_authoritative
                            || self.library_store.as_ref().is_some_and(|store| {
                                store
                                    .save_device_manifest(&completion.device_id, &outcome.manifest)
                                    .is_ok()
                            });
                        if !manifest_saved {
                            self.push_ephemeral_notification(
                                NotificationCategory::DeviceSync,
                                NotificationSeverity::Error,
                                gettext(
                                    "Device sync failed: could not save the on-device manifest.",
                                ),
                            );
                        } else {
                            let severity = if outcome.cancelled {
                                NotificationSeverity::Warning
                            } else {
                                NotificationSeverity::Info
                            };
                            self.push_ephemeral_notification(
                                NotificationCategory::DeviceSync,
                                severity,
                                notifications::device_sync_outcome_text(&outcome),
                            );
                        }
                    }
                    Err(message) => {
                        self.push_ephemeral_notification(
                            NotificationCategory::DeviceSync,
                            NotificationSeverity::Error,
                            tr_format!(gettext("Device sync failed: {message}"), message = message),
                        );
                    }
                }
                self.device_sync_scheduler.acknowledge_completion(run_id);
            }
        }
    }

    // --- Resolution helpers ---

    /// Distinct track ids across a device's ticked playlists (available
    /// tracks only), order-preserving.
    fn device_selection_track_ids(&self, id: &SyncDeviceId) -> Vec<sustain_domain::TrackId> {
        let mut seen = std::collections::HashSet::new();
        let mut ids = Vec::new();
        for item in self.device_selection(id) {
            let Some(track_ids) = self.playlist_item_track_ids(item) else {
                continue;
            };
            for tid in track_ids {
                if seen.insert(tid) {
                    ids.push(tid);
                }
            }
        }
        ids
    }

    fn clock_unix_secs(&self) -> i64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

fn mount_identity(connected: &ConnectedDevice) -> DeviceMountIdentity {
    DeviceMountIdentity {
        device_id: connected.id.clone(),
        mount_path: connected.mount_path.clone(),
        volume_id: connected.volume_id.clone(),
    }
}

struct DevicePlanPreparation {
    mount: DeviceMountIdentity,
    store: Arc<dyn LibraryStore>,
    library_path: Option<PathBuf>,
    now: SystemTime,
    export_date: String,
}

struct DeviceSyncPreparation {
    mount: DeviceMountIdentity,
    store: Arc<dyn LibraryStore>,
    metadata_service: Arc<dyn MetadataService>,
    library_path: Option<PathBuf>,
    now: SystemTime,
    export_date: String,
    remove_stale: bool,
}

impl DeviceSyncPreparation {
    fn run(
        self,
        progress: &mut dyn FnMut(SyncProgress),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, String> {
        let request = match build_worker_sync_request(
            self.store.as_ref(),
            &self.mount,
            self.library_path.as_deref(),
            self.now,
            self.export_date,
            self.remove_stale,
            cancelled,
        ) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(cancelled_outcome()),
            Err(error) => return Err(error),
        };
        if request.tracks.is_empty() {
            return Err(sustain_device_sync::SyncError::Empty.to_string());
        }
        let prepared = match prepare_sync_request(
            request,
            self.store.as_ref(),
            self.metadata_service.as_ref(),
            progress,
            cancelled,
        ) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(cancelled_outcome()),
            Err(error) => return Err(error.to_string()),
        };
        engine::sync(&prepared, progress, cancelled).map_err(|error| error.to_string())
    }
}

fn cancelled_outcome() -> SyncOutcome {
    SyncOutcome {
        cancelled: true,
        ..SyncOutcome::default()
    }
}

impl DevicePlanPreparation {
    fn run(self, cancelled: &dyn Fn() -> bool) -> Option<Result<DevicePlanSnapshot, String>> {
        if cancelled() {
            return None;
        }
        let capacity = match capacity(&self.mount.mount_path) {
            Ok(capacity) => capacity,
            Err(error) => return Some(Err(format!("device capacity probe failed: {error}"))),
        };
        if cancelled() {
            return None;
        }
        let request = match build_worker_sync_request(
            self.store.as_ref(),
            &self.mount,
            self.library_path.as_deref(),
            self.now,
            self.export_date,
            false,
            cancelled,
        ) {
            Ok(Some(request)) => request,
            Ok(None) => return None,
            Err(error) => return Some(Err(error)),
        };
        if cancelled() {
            return None;
        }
        let plan = if request.tracks.is_empty() {
            None
        } else {
            match engine::plan(&request) {
                Ok(plan) => Some(plan),
                Err(error) => return Some(Err(error.to_string())),
            }
        };
        Some(Ok(DevicePlanSnapshot { plan, capacity }))
    }
}

fn build_worker_sync_request(
    store: &dyn LibraryStore,
    mount: &DeviceMountIdentity,
    library_path: Option<&std::path::Path>,
    now: SystemTime,
    export_date: String,
    remove_stale: bool,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<SyncRequest>, String> {
    let device = store
        .sync_device(&mount.device_id)
        .map_err(|error| format!("could not load device configuration: {error:?}"))?
        .ok_or_else(|| "device configuration is unavailable".to_owned())?;
    let selection = store
        .device_selection(&mount.device_id)
        .map_err(|error| format!("could not load device selection: {error:?}"))?;
    if selection.is_empty() {
        return Ok(Some(SyncRequest {
            device,
            mount_path: mount.mount_path.clone(),
            tracks: Vec::new(),
            playlists: Vec::new(),
            previous_manifest: Vec::new(),
            remove_stale,
            export_date,
        }));
    }
    let library_tracks = store
        .tracks()
        .map_err(|error| format!("could not load library tracks: {error:?}"))?;
    let playlists = store
        .playlists()
        .map_err(|error| format!("could not load playlists: {error:?}"))?;
    let smart_playlists = store
        .smart_playlists()
        .map_err(|error| format!("could not load smart playlists: {error:?}"))?;
    let previous_manifest = store
        .device_manifest(&mount.device_id)
        .map_err(|error| format!("could not load device manifest: {error:?}"))?;
    if cancelled() {
        return Ok(None);
    }

    let by_id: HashMap<_, _> = library_tracks
        .iter()
        .map(|track| (track.id, track))
        .collect();
    let mut index_of: HashMap<TrackId, usize> = HashMap::new();
    let mut tracks = Vec::new();
    let mut resolved_playlists = Vec::new();
    for item in selection {
        if cancelled() {
            return Ok(None);
        }
        let Some(track_ids) =
            snapshot_playlist_track_ids(item, &library_tracks, &playlists, &smart_playlists, now)
        else {
            continue;
        };
        let name = snapshot_playlist_name(item, &playlists, &smart_playlists)
            .unwrap_or_else(|| "Playlist".to_owned());
        let mut indices = Vec::with_capacity(track_ids.len());
        for track_id in track_ids {
            if cancelled() {
                return Ok(None);
            }
            let index = match index_of.get(&track_id) {
                Some(&index) => Some(index),
                None => {
                    let Some(track) = by_id.get(&track_id) else {
                        continue;
                    };
                    if track.location.is_missing() {
                        continue;
                    }
                    let Some(library_path) = library_path else {
                        continue;
                    };
                    let source_path = track.location.absolute_path(library_path);
                    let Some(source) = source_snapshot(store, track.id, &source_path)
                        .map_err(|error| format!("could not inspect source track: {error:?}"))?
                    else {
                        continue;
                    };
                    let index = tracks.len();
                    tracks.push(sync_input_track(track, source_path, source));
                    index_of.insert(track_id, index);
                    Some(index)
                }
            };
            if let Some(index) = index {
                indices.push(index);
            }
        }
        resolved_playlists.push(SyncInputPlaylist {
            name,
            track_indices: indices,
        });
    }
    Ok(Some(SyncRequest {
        device,
        mount_path: mount.mount_path.clone(),
        tracks,
        playlists: resolved_playlists,
        previous_manifest,
        remove_stale,
        export_date,
    }))
}

fn prepare_sync_request(
    mut request: SyncRequest,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    progress: &mut dyn FnMut(SyncProgress),
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<PreparedSyncRequest>, sustain_device_sync::SyncError> {
    let mut pioneer_assets =
        (request.device.layout == DeviceLayout::Pioneer).then(PreparedPioneerAssets::new);
    let total = request.tracks.len();
    for (index, track) in request.tracks.iter_mut().enumerate() {
        if cancelled() {
            return Ok(None);
        }
        let cached = store
            .source_fingerprint(track.track_id)
            .map_err(|error| sustain_device_sync::SyncError::Preparation(format!("{error:?}")))?;
        let fingerprint = resolve_source_fingerprint(&track.source_path, cached.as_ref())
            .map_err(|error| sustain_device_sync::SyncError::io(&track.source_path, error))?;
        store
            .save_source_fingerprint(track.track_id, &fingerprint)
            .map_err(|error| sustain_device_sync::SyncError::Preparation(format!("{error:?}")))?;
        track.source = SourceSnapshot::resolved(fingerprint);

        if let Some(assets) = pioneer_assets.as_mut() {
            let waveform = store.load_waveform(track.track_id).map_err(|error| {
                sustain_device_sync::SyncError::Preparation(format!("{error:?}"))
            })?;
            if cancelled() {
                return Ok(None);
            }
            let cover_art = metadata_service
                .read_artwork(&track.source_path)
                .ok()
                .flatten();
            if cancelled() {
                return Ok(None);
            }
            let (waveform_preview, waveform_detail) = match waveform {
                Some(waveform) => (Some(waveform.preview), Some(waveform.detail)),
                None => (None, None),
            };
            assets.push_track(waveform_preview, waveform_detail, cover_art.as_deref());
        }
        progress(SyncProgress {
            stage: SyncStage::Preparing,
            completed: index + 1,
            total,
        });
    }
    PreparedSyncRequest::new(request, pioneer_assets).map(Some)
}

fn snapshot_playlist_track_ids(
    item: PlaylistItem,
    tracks: &[Track],
    playlists: &[Playlist],
    smart_playlists: &[SmartPlaylist],
    now: SystemTime,
) -> Option<Vec<TrackId>> {
    match item {
        PlaylistItem::Playlist(id) => {
            playlists
                .iter()
                .find(|playlist| playlist.id == id)
                .map(|playlist| {
                    playlist
                        .entries
                        .iter()
                        .map(|entry| entry.track_id)
                        .collect()
                })
        }
        PlaylistItem::SmartPlaylist(id) => smart_playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| {
                matching_tracks(tracks, &playlist.rules, now)
                    .into_iter()
                    .map(|track| track.id)
                    .collect()
            }),
        PlaylistItem::Folder(_) => None,
    }
}

fn snapshot_playlist_name(
    item: PlaylistItem,
    playlists: &[Playlist],
    smart_playlists: &[SmartPlaylist],
) -> Option<String> {
    match item {
        PlaylistItem::Playlist(id) => playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::SmartPlaylist(id) => smart_playlists
            .iter()
            .find(|playlist| playlist.id == id)
            .map(|playlist| playlist.name.clone()),
        PlaylistItem::Folder(_) => None,
    }
}

fn sync_input_track(track: &Track, source_path: PathBuf, source: SourceSnapshot) -> SyncInputTrack {
    let metadata = &track.metadata;
    let extension = source_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    SyncInputTrack {
        track_id: track.id,
        source_path,
        title: metadata.title.clone().unwrap_or_default(),
        artist: metadata.artist.clone().unwrap_or_default(),
        album: metadata.album.clone().unwrap_or_default(),
        genre: metadata.genre.clone(),
        track_number: metadata.track_number,
        year: metadata.year.map(|y| y.max(0) as u32),
        duration_ms: metadata.duration.map(|d| d.as_millis() as u32).unwrap_or(0),
        rating: track.rating.stars(),
        bpm: metadata.bpm.map(|b| b as f32),
        key: metadata
            .key
            .as_deref()
            .and_then(MusicalKey::from_short_code),
        bitrate_kbps: metadata.bitrate_kbps,
        sample_rate_hz: metadata.sample_rate_hz.unwrap_or(44_100),
        bit_depth: 16,
        source,
        date_added: track.statistics.date_added_at.map(|t| {
            unix_to_ymd(
                t.duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs() as i64)
                    .unwrap_or(0),
            )
        }),
        extension,
    }
}

/// Observe a source for the request. `Ok(None)` means the file could not be
/// stat'd — it vanished after the library marked it available, so the caller
/// skips it exactly like an `is_missing()` track rather than failing the whole
/// plan/sync. A store error is a genuine infrastructure failure and aborts.
fn source_snapshot(
    store: &dyn sustain_library_store::LibraryStore,
    track_id: sustain_domain::TrackId,
    source_path: &std::path::Path,
) -> ApplicationRuntimeResult<Option<SourceSnapshot>> {
    let Ok(stat) = source_file_stat(source_path) else {
        return Ok(None);
    };
    let cached = store
        .source_fingerprint(track_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(Some(match cached {
        Some(fingerprint) if fingerprint.stat == stat => SourceSnapshot::resolved(fingerprint),
        _ => SourceSnapshot::provisional(stat),
    }))
}

/// Format a Unix timestamp (seconds) as `YYYY-MM-DD` in UTC, without a
/// date-library dependency (Howard Hinnant's civil-from-days algorithm).
fn unix_to_ymd(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let z = days + 719_468;
    let era = (if z >= 0 { z } else { z - 146_096 }) / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 { year + 1 } else { year };
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use sustain_device_sync::{
        DeviceCapacity, SourceSnapshot, SyncInputTrack, SyncProgress, SyncRequest, SyncStage,
        source_file_stat,
    };
    use sustain_domain::{
        DeviceKind, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, MetadataChange, Rating,
        SyncDevice, SyncDeviceId, TrackId, TrackMetadata,
    };
    use sustain_library_store::InMemoryLibraryStore;
    use sustain_metadata::{InitialTags, MetadataResult, MetadataService};

    use super::{
        ConnectedDevice, DeviceMountIdentity, DevicePlanResult, DevicePlanSnapshot,
        DeviceSyncPlanState, prepare_sync_request, unix_to_ymd,
    };
    use crate::ApplicationRuntime;

    fn connected(id: &str, mount_path: &str) -> ConnectedDevice {
        ConnectedDevice {
            id: SyncDeviceId::new(id).expect("device id"),
            kind: DeviceKind::UsbDrive,
            mount_path: mount_path.into(),
            volume_id: Some(format!("{id}-volume")),
            label: id.to_owned(),
            is_known: true,
            has_marker: true,
        }
    }

    #[derive(Default)]
    struct CountingMetadataService {
        artwork_reads: AtomicUsize,
    }

    impl CountingMetadataService {
        fn artwork_reads(&self) -> usize {
            self.artwork_reads.load(Ordering::Acquire)
        }
    }

    impl MetadataService for CountingMetadataService {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(InitialTags {
                metadata: TrackMetadata::default(),
                rating: Rating::unrated(),
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
            self.artwork_reads.fetch_add(1, Ordering::AcqRel);
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    fn pioneer_request(source_paths: &[&Path]) -> SyncRequest {
        SyncRequest {
            device: SyncDevice {
                id: SyncDeviceId::new("pioneer-device").expect("device id"),
                label: "Pioneer".to_owned(),
                kind: DeviceKind::UsbDrive,
                layout: DeviceLayout::Pioneer,
                sub_path: DeviceRelativePath::root(),
                files_per_folder_cap: FilesPerFolderCap::Unlimited,
                volume_id: None,
            },
            mount_path: "/mnt/device".into(),
            tracks: source_paths
                .iter()
                .enumerate()
                .map(|(index, path)| SyncInputTrack {
                    track_id: TrackId::new(index as i64 + 1).expect("track id"),
                    source_path: (*path).to_path_buf(),
                    title: format!("Track {}", index + 1),
                    artist: "Artist".to_owned(),
                    album: "Album".to_owned(),
                    genre: None,
                    track_number: None,
                    year: None,
                    duration_ms: 1_000,
                    rating: 0,
                    bpm: None,
                    key: None,
                    bitrate_kbps: None,
                    sample_rate_hz: 44_100,
                    bit_depth: 16,
                    source: SourceSnapshot::provisional(
                        source_file_stat(path).expect("source stat"),
                    ),
                    date_added: None,
                    extension: "mp3".to_owned(),
                })
                .collect(),
            playlists: Vec::new(),
            previous_manifest: Vec::new(),
            remove_stale: false,
            export_date: "2026-06-01".to_owned(),
        }
    }

    #[test]
    fn formats_known_dates() {
        assert_eq!(unix_to_ymd(0), "1970-01-01");
        assert_eq!(unix_to_ymd(1_700_000_000), "2023-11-14");
        // 2026-05-29 00:00:00 UTC
        assert_eq!(unix_to_ymd(1_780_012_800), "2026-05-29");
    }

    #[test]
    fn runtime_discards_stale_plan_generation() {
        let mut runtime = ApplicationRuntime::new();
        let first = connected("first", "/mnt/first");
        runtime.request_device_sync_plan(&first);
        let generation = runtime
            .device_plan_cache
            .as_ref()
            .expect("first cache")
            .generation;
        let mount = DeviceMountIdentity {
            device_id: first.id.clone(),
            mount_path: first.mount_path.clone(),
            volume_id: first.volume_id.clone(),
        };
        let second = connected("second", "/mnt/second");
        runtime.request_device_sync_plan(&second);

        assert!(!runtime.apply_device_plan_result(DevicePlanResult {
            generation,
            mount,
            result: Ok(DevicePlanSnapshot {
                plan: None,
                capacity: DeviceCapacity::default(),
            }),
        }));
        assert_eq!(
            runtime.device_sync_plan_state(&second),
            DeviceSyncPlanState::Unavailable
        );
    }

    #[test]
    fn pioneer_preparation_reads_assets_and_reports_progress() {
        let sources = tempfile::tempdir().expect("source directory");
        let first = sources.path().join("first.mp3");
        let second = sources.path().join("second.mp3");
        std::fs::write(&first, b"first source").expect("write first source");
        std::fs::write(&second, b"second source").expect("write second source");
        let request = pioneer_request(&[&first, &second]);
        let store = InMemoryLibraryStore::new();
        let metadata = CountingMetadataService::default();
        let mut ticks = Vec::new();

        let prepared = prepare_sync_request(
            request,
            &store,
            &metadata,
            &mut |tick| ticks.push(tick),
            &|| false,
        )
        .expect("preparation succeeds")
        .expect("preparation is not cancelled");

        assert_eq!(metadata.artwork_reads(), 2);
        assert_eq!(
            ticks,
            vec![
                SyncProgress {
                    stage: SyncStage::Preparing,
                    completed: 1,
                    total: 2,
                },
                SyncProgress {
                    stage: SyncStage::Preparing,
                    completed: 2,
                    total: 2,
                },
            ]
        );
        assert!(
            prepared
                .tracks
                .iter()
                .all(|track| track.source.content_hash.is_some())
        );
    }

    #[test]
    fn pioneer_preparation_observes_cancellation_after_artwork_read() {
        let sources = tempfile::tempdir().expect("source directory");
        let first = sources.path().join("first.mp3");
        let second = sources.path().join("second.mp3");
        std::fs::write(&first, b"first source").expect("write first source");
        std::fs::write(&second, b"second source").expect("write second source");
        let request = pioneer_request(&[&first, &second]);
        let store = InMemoryLibraryStore::new();
        let metadata = CountingMetadataService::default();
        let mut ticks = Vec::new();

        let prepared = prepare_sync_request(
            request,
            &store,
            &metadata,
            &mut |tick| ticks.push(tick),
            &|| metadata.artwork_reads() >= 1,
        )
        .expect("cancellation is not an error");

        assert!(prepared.is_none());
        assert_eq!(metadata.artwork_reads(), 1);
        assert!(ticks.is_empty());
    }
}
