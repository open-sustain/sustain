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
use std::time::UNIX_EPOCH;

use sustain_device_mtp::MtpTransport;
use sustain_device_sync::{
    ConnectedDevice, DeviceRoot, DeviceTarget, DeviceTransport, JournalError, ManifestJournal,
    PioneerTrackAnalysis, PreparedPioneerAssets, PreparedSyncRequest, SourceSnapshot, SyncFailure,
    SyncInputPlaylist, SyncInputTrack, SyncOutcome, SyncProgress, SyncRequest, SyncStage, engine,
    resolve_source_fingerprint, source_file_stat,
};
use sustain_domain::{
    Clock, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, FrozenClock, PioneerExportRecord,
    Playlist, PlaylistItem, SmartPlaylist, SyncDevice, SyncDeviceId, SyncManifestEntry, Track,
    TrackId, matching_tracks,
};
use sustain_i18n::{gettext, tr_format};
use sustain_library_store::{AnalysisCapabilities, LibraryStore};
use sustain_metadata::MetadataService;

use crate::device_plan_scheduler::{
    DeviceMountIdentity, DevicePlanGeneration, DevicePlanResult, DevicePlanSnapshot,
};
use crate::device_sync_scheduler::{DeviceSyncEvent, DeviceSyncFailure, DeviceSyncStartOutcome};
use crate::{
    AnalysisRunRequest, ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult,
    CollationKey, NotificationCategory, NotificationSeverity, notifications, track_artist_name,
};

/// Per-device analysis coverage for the ticked playlists/artists, shown in
/// the Pioneer export panel. `analyzable` is how many tracks an analysis run
/// would still touch (distinguishes "not yet attempted" from "attempted,
/// no confident result": a track counted in `missing_bpm` but not in
/// `analyzable` was already attempted and produced nothing).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceAnalysisReadiness {
    pub total: usize,
    pub missing_bpm: usize,
    pub missing_key: usize,
    pub missing_waveform: usize,
    /// Tracks that will reach the player without a beat grid: none was
    /// measured, or the library's BPM no longer agrees with it.
    pub missing_beat_grid: usize,
    pub analyzable: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceSyncPlanState {
    Pending,
    Ready(DevicePlanSnapshot),
    Unavailable,
}

/// The outcome of one asynchronous MTP discovery probe, sent from the
/// worker thread back to the runtime. `generation` lets the runtime drop a
/// result that a newer refresh has already superseded.
#[derive(Debug)]
pub struct MtpDiscoveryResult {
    generation: u64,
    devices: Vec<ConnectedDevice>,
}

pub(crate) struct DevicePlanCache {
    generation: DevicePlanGeneration,
    mount: DeviceMountIdentity,
    state: DeviceSyncPlanState,
}

impl ApplicationRuntime {
    /// Enumerate currently-connected devices, resolved against the saved
    /// device configuration: block-mounted drives probed inline (cheap,
    /// reads `/proc/mounts`) followed by the cached Android-over-MTP
    /// devices. The MTP probe is slow, so it never runs here — it is
    /// refreshed asynchronously by [`Self::refresh_mtp_devices`] and its
    /// result merged in from the cache. Call lazily (never during the
    /// cold-start window).
    pub fn connected_devices(&self) -> Vec<ConnectedDevice> {
        let known = self.known_sync_devices();
        let mut devices = sustain_device_sync::discover(&known);
        devices.extend(self.mtp_devices.iter().cloned());
        devices
    }

    /// The saved device configuration, used to recognise connected devices.
    fn known_sync_devices(&self) -> Vec<SyncDevice> {
        self.library_store
            .as_ref()
            .and_then(|store| store.sync_devices().ok())
            .unwrap_or_default()
    }

    /// Refresh the cache of connected Android-over-MTP devices.
    ///
    /// Cheap on the main thread: it reads gio's volume monitor for
    /// candidate volumes (no device I/O) and hands the slow per-candidate
    /// storage + identity-marker probe to a one-shot worker thread. The
    /// worker's result arrives on [`Self::mtp_discovery_receiver`] and is
    /// applied by [`Self::apply_mtp_discovery`]; until then the previous
    /// cache stays in effect. When no candidate is present (the phone was
    /// unplugged) the cache is cleared synchronously so the device list
    /// updates immediately, with no worker spawned.
    pub fn refresh_mtp_devices(&mut self) {
        self.mtp_discovery_generation = self
            .mtp_discovery_generation
            .checked_add(1)
            .expect("mtp-discovery generation space exhausted");
        let generation = self.mtp_discovery_generation;

        let candidates = sustain_device_mtp::candidates();
        if candidates.is_empty() {
            self.mtp_devices.clear();
            return;
        }

        let known = self.known_sync_devices();
        let sink = self.mtp_discovery_sink.clone();
        // A one-shot detached worker: gio objects are `!Send`, so the probe
        // reconstructs them on this thread from the plain candidate data and
        // never shares them with the main loop. The closure captures only
        // owned, `Send` data (candidates, known config, the channel sender),
        // so it is safe to outlive this call. Spawn failure (resource
        // exhaustion) simply leaves the existing cache in place.
        let spawn = std::thread::Builder::new()
            .name("sustain-mtp-discovery".to_owned())
            .spawn(move || {
                let devices = candidates
                    .iter()
                    .filter_map(|candidate| sustain_device_mtp::resolve(candidate, &known))
                    .collect();
                // The receiver lives as long as the runtime; a send error
                // only means the app is shutting down, so dropping the
                // result is the right thing to do.
                let _ = sink.send_blocking(MtpDiscoveryResult {
                    generation,
                    devices,
                });
            });
        if let Err(error) = spawn {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Error,
                tr_format!(
                    gettext("Could not scan for Android devices: {error}"),
                    error = error
                ),
            );
        }
    }

    /// The channel the UI shell drains on idle, feeding each MTP discovery
    /// result back into [`Self::apply_mtp_discovery`].
    pub fn mtp_discovery_receiver(&self) -> async_channel::Receiver<MtpDiscoveryResult> {
        self.mtp_discovery_source.clone()
    }

    /// Apply an MTP discovery result from the worker. Stale results (from a
    /// probe a newer refresh has superseded) are dropped. Returns `true`
    /// when the cache changed and the device list should be re-rendered.
    pub fn apply_mtp_discovery(&mut self, result: MtpDiscoveryResult) -> bool {
        if result.generation != self.mtp_discovery_generation {
            return false;
        }
        if self.mtp_devices == result.devices {
            return false;
        }
        self.mtp_devices = result.devices;
        true
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

    /// The saved ticked-artist selection for a device.
    pub fn device_artist_selection(&self, id: &SyncDeviceId) -> Vec<String> {
        self.library_store
            .as_ref()
            .and_then(|store| store.device_artist_selection(id).ok())
            .unwrap_or_default()
    }

    /// The deduplicated set of library tracks the device's ticked
    /// playlists and artists resolve to, in first-seen order — a track in
    /// several selected sources counts once. Smart playlists and artist
    /// names are evaluated live. Drives the status-bar track/duration/size
    /// summary while the device view is shown.
    pub fn device_selected_tracks(&self, id: &SyncDeviceId) -> Vec<Track> {
        let mut seen = HashSet::new();
        let mut tracks = Vec::new();
        for item in self.device_selection(id) {
            let Some(track_ids) = self.playlist_item_track_ids(item) else {
                continue;
            };
            for tid in track_ids {
                if seen.insert(tid)
                    && let Some(track) = self.library_track(tid)
                {
                    tracks.push(track.clone());
                }
            }
        }
        let by_artist = ArtistTracks::of(&self.library_tracks);
        for artist in self.device_artist_selection(id) {
            for &tid in by_artist.by(&artist) {
                if seen.insert(tid)
                    && let Some(track) = self.library_track(tid)
                {
                    tracks.push(track.clone());
                }
            }
        }
        tracks
    }

    /// True while a device sync is running on the background worker.
    pub fn device_sync_in_progress(&self) -> bool {
        self.device_sync_scheduler.is_syncing()
    }

    /// The device a running sync is writing to, if one runs.
    pub fn syncing_device(&self) -> Option<&SyncDeviceId> {
        self.syncing_device.as_ref()
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
        self.syncing_device = None;
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

    pub(crate) fn set_device_artist_selection(
        &mut self,
        id: SyncDeviceId,
        artists: Vec<String>,
    ) -> ApplicationRuntimeResult<()> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        store
            .save_device_artist_selection(&id, &artists)
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
        self.push_ephemeral_notification(
            NotificationCategory::DeviceSync,
            NotificationSeverity::Info,
            gettext(
                "Device forgotten. Its saved playlists, artists, and sync history were cleared.",
            ),
        );
        Ok(())
    }

    fn device_config_or_default(&self, id: &SyncDeviceId) -> SyncDevice {
        self.device_config(id).unwrap_or_else(|| SyncDevice {
            id: id.clone(),
            label: gettext("Device"),
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

    /// Analysis coverage over the tracks in a device's ticked playlists/artists.
    pub fn device_analysis_readiness(&self, id: &SyncDeviceId) -> DeviceAnalysisReadiness {
        let track_ids = self.device_selection_track_ids(id);
        let total = track_ids.len();
        let mut readiness = DeviceAnalysisReadiness {
            total,
            ..Default::default()
        };
        let beat_grids = self
            .library_store
            .as_ref()
            .and_then(|store| store.load_beat_grids(&track_ids).ok());
        for tid in &track_ids {
            if let Some(track) = self.library_track(*tid) {
                if track.metadata.bpm.is_none() {
                    readiness.missing_bpm += 1;
                }
                if track.metadata.key.is_none() {
                    readiness.missing_key += 1;
                }
                if let Some(grids) = beat_grids.as_ref()
                    && grids
                        .get(tid)
                        .and_then(|grid| grid.for_library_tempo(track.metadata.bpm))
                        .is_none()
                {
                    readiness.missing_beat_grid += 1;
                }
            }
        }
        if let Some(store) = self.library_store.as_ref() {
            let versions = sustain_analysis::ANALYZER_VERSIONS;
            let audio_only = AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            };
            readiness.missing_waveform = store
                .filter_tracks_needing_analysis(&track_ids, audio_only, versions)
                .map(|v| v.len())
                .unwrap_or(0);
            readiness.analyzable = store
                .filter_tracks_needing_analysis(&track_ids, AnalysisCapabilities::all(), versions)
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
            clock: self.clock.clone(),
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
        if self.device_selection(&id).is_empty() && self.device_artist_selection(&id).is_empty() {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Info,
                gettext("Pick at least one playlist or artist to sync to this device."),
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
            clock: self.clock.clone(),
            export_date: unix_to_ymd(self.clock_unix_secs()),
            remove_stale,
        };
        match self
            .device_sync_scheduler
            .start(move |progress, cancel| preparation.run(progress, cancel))
        {
            DeviceSyncStartOutcome::Started(_) => {
                self.syncing_device = Some(id);
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
    /// progress notification, or on completion publish the outcome.
    pub fn apply_device_sync_event(&mut self, event: DeviceSyncEvent) {
        match event {
            // The runtime marked the device when the run was accepted;
            // the event only tells the shell to show it.
            DeviceSyncEvent::Started { .. } => {}
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
            DeviceSyncEvent::Finished { run_id, result } => {
                if !self.device_sync_scheduler.is_active_run(run_id) {
                    return;
                }
                if let Some(id) = self.device_sync_notification_id.take() {
                    self.dismiss_notification(id);
                }
                self.syncing_device = None;
                // The run changed the device: a plan made before or during
                // it describes a drive that is no longer there.
                self.invalidate_device_sync_plan();
                match result {
                    Ok(outcome) => {
                        let severity = if outcome.cancelled || !outcome.copy_failures.is_empty() {
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
                    Err(failure) => {
                        self.push_ephemeral_notification(
                            NotificationCategory::DeviceSync,
                            NotificationSeverity::Error,
                            tr_format!(
                                gettext("Device sync failed: {detail}"),
                                detail = failure.message
                            ),
                        );
                    }
                }
                self.device_sync_scheduler.acknowledge_completion(run_id);
            }
        }
    }

    // --- Resolution helpers ---

    /// Distinct track ids across a device's ticked playlists/artists
    /// (available tracks only), order-preserving.
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
        let by_artist = ArtistTracks::of(&self.library_tracks);
        for artist in self.device_artist_selection(id) {
            for &tid in by_artist.by(&artist) {
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
        target: connected.target.clone(),
        volume_id: connected.volume_id.clone(),
    }
}

/// Open the transport for a device target. Called on the sync/plan worker
/// thread, so the (thread-affine) gio MTP transport is created and used on
/// the same thread and never crosses back.
fn open_transport(target: &DeviceTarget) -> std::io::Result<Box<dyn DeviceTransport>> {
    match target {
        DeviceTarget::Filesystem { mount_path } => Ok(Box::new(DeviceRoot::open(mount_path)?)),
        DeviceTarget::Mtp(mtp) => Ok(Box::new(MtpTransport::open(mtp))),
    }
}

struct DevicePlanPreparation {
    mount: DeviceMountIdentity,
    store: Arc<dyn LibraryStore>,
    library_path: Option<PathBuf>,
    clock: Arc<dyn Clock>,
    export_date: String,
}

struct DeviceSyncPreparation {
    mount: DeviceMountIdentity,
    store: Arc<dyn LibraryStore>,
    metadata_service: Arc<dyn MetadataService>,
    library_path: Option<PathBuf>,
    clock: Arc<dyn Clock>,
    export_date: String,
    remove_stale: bool,
}

impl DeviceSyncPreparation {
    fn run(
        self,
        progress: &mut dyn FnMut(SyncProgress),
        cancelled: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, DeviceSyncFailure> {
        let request = match build_worker_sync_request(
            self.store.as_ref(),
            &self.mount,
            self.library_path.as_deref(),
            self.clock.as_ref(),
            self.export_date,
            self.remove_stale,
            cancelled,
        ) {
            Ok(Some(request)) => request,
            Ok(None) => return Ok(cancelled_outcome()),
            Err(error) => return Err(DeviceSyncFailure::new(error)),
        };
        if request.tracks.is_empty() {
            return Err(DeviceSyncFailure::new(
                sustain_device_sync::SyncError::Empty.to_string(),
            ));
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
            Err(error) => return Err(DeviceSyncFailure::new(error.to_string())),
        };
        let transport = open_transport(&self.mount.target).map_err(|error| {
            DeviceSyncFailure::new(format!("could not open the device: {error}"))
        })?;
        let device_id = &self.mount.device_id;
        let mut journal = StoreManifestJournal {
            store: self.store.as_ref(),
            device_id,
        };
        let synced = engine::sync(
            transport.as_ref(),
            &prepared,
            &mut journal,
            progress,
            cancelled,
        );
        save_synced_manifest(self.store.as_ref(), device_id, synced)
    }
}

/// Records each change a sync makes to the device in the device's saved
/// manifest and Pioneer export record as it is made, so that a run the
/// process does not survive leaves the next one knowing which files are
/// already there, and which are Sustain's.
struct StoreManifestJournal<'a> {
    store: &'a dyn LibraryStore,
    device_id: &'a SyncDeviceId,
}

impl ManifestJournal for StoreManifestJournal<'_> {
    fn record(&mut self, entry: &SyncManifestEntry) -> Result<(), JournalError> {
        self.store
            .record_device_manifest_entry(self.device_id, entry)
            .map_err(JournalError::from)
    }

    fn forget(&mut self, path: &DeviceRelativePath) -> Result<(), JournalError> {
        self.store
            .forget_device_manifest_entry(self.device_id, path)
            .map_err(JournalError::from)
    }

    /// Made durable before returning: the drive write it vouches for
    /// follows at once, and the drive's write is durable.
    fn record_pioneer_export(&mut self, record: &PioneerExportRecord) -> Result<(), JournalError> {
        self.store
            .save_device_pioneer_export(self.device_id, record)
            .and_then(|()| self.store.flush_durable())
            .map_err(JournalError::from)
    }
}

/// Save the manifest, playlist files and Pioneer export record a sync
/// reports — however it ended, they describe what the device holds — in
/// place of what the journal pieced together.
fn save_synced_manifest(
    store: &dyn LibraryStore,
    device_id: &SyncDeviceId,
    synced: Result<SyncOutcome, Box<SyncFailure>>,
) -> Result<SyncOutcome, DeviceSyncFailure> {
    let save = |manifest: &[SyncManifestEntry],
                playlist_files: &[DeviceRelativePath],
                pioneer_export: &PioneerExportRecord| {
        store
            .save_device_manifest(device_id, manifest)
            .and_then(|()| store.save_device_playlist_files(device_id, playlist_files))
            .and_then(|()| store.save_device_pioneer_export(device_id, pioneer_export))
    };
    match synced {
        Ok(outcome) => match save(
            &outcome.manifest,
            &outcome.playlist_files,
            &outcome.pioneer_export,
        ) {
            Ok(()) => Ok(outcome),
            Err(error) => Err(DeviceSyncFailure::new(format!(
                "could not save the on-device manifest: {error}"
            ))),
        },
        Err(failure) => {
            let message = match failure.error {
                sustain_device_sync::SyncError::Blocked(blocker) => {
                    notifications::sync_blocker_text(blocker)
                }
                ref error => error.to_string(),
            };
            Err(DeviceSyncFailure::new(
                match save(
                    &failure.manifest,
                    &failure.playlist_files,
                    &failure.pioneer_export,
                ) {
                    Ok(()) => message,
                    Err(_) => format!("{message}; could not save the on-device manifest"),
                },
            ))
        }
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
        let transport = match open_transport(&self.mount.target) {
            Ok(transport) => transport,
            Err(error) => return Some(Err(format!("could not open the device: {error}"))),
        };
        let capacity = match transport.capacity() {
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
            self.clock.as_ref(),
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
            match engine::plan(transport.as_ref(), &request) {
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
    clock: &dyn Clock,
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
    let artist_selection = store
        .device_artist_selection(&mount.device_id)
        .map_err(|error| format!("could not load device artist selection: {error:?}"))?;
    if selection.is_empty() && artist_selection.is_empty() {
        return Ok(Some(SyncRequest {
            device,
            tracks: Vec::new(),
            playlists: Vec::new(),
            previous_manifest: Vec::new(),
            previous_playlist_files: Vec::new(),
            previous_pioneer_export: PioneerExportRecord::default(),
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
    let previous_playlist_files = store
        .device_playlist_files(&mount.device_id)
        .map_err(|error| format!("could not load device playlist files: {error:?}"))?;
    let previous_pioneer_export = store
        .device_pioneer_export(&mount.device_id)
        .map_err(|error| format!("could not load device Pioneer export record: {error:?}"))?;
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
    // Every exported smart playlist is evaluated at the same instant, so
    // a track at the edge of a relative-date window cannot land in two
    // playlists that exclude each other, or in neither.
    let clock = FrozenClock::new(clock);
    for item in selection {
        if cancelled() {
            return Ok(None);
        }
        let Some(track_ids) = snapshot_playlist_track_ids(
            item,
            &library_tracks,
            &playlists,
            &smart_playlists,
            &clock,
        ) else {
            continue;
        };
        let name = snapshot_playlist_name(item, &playlists, &smart_playlists)
            .unwrap_or_else(|| gettext("Playlist"));
        let Some(indices) = resolve_sync_track_indices(
            store,
            &by_id,
            &mut index_of,
            &mut tracks,
            library_path,
            track_ids,
            cancelled,
        )?
        else {
            return Ok(None);
        };
        resolved_playlists.push(SyncInputPlaylist {
            name,
            track_indices: indices,
        });
    }
    let by_artist = ArtistTracks::of(&library_tracks);
    for artist in artist_selection {
        if cancelled() {
            return Ok(None);
        }
        let track_ids = by_artist.by(&artist).to_vec();
        if track_ids.is_empty() {
            continue;
        }
        let Some(indices) = resolve_sync_track_indices(
            store,
            &by_id,
            &mut index_of,
            &mut tracks,
            library_path,
            track_ids,
            cancelled,
        )?
        else {
            return Ok(None);
        };
        resolved_playlists.push(SyncInputPlaylist {
            name: artist,
            track_indices: indices,
        });
    }
    Ok(Some(SyncRequest {
        device,
        tracks,
        playlists: resolved_playlists,
        previous_manifest,
        previous_playlist_files,
        previous_pioneer_export,
        remove_stale,
        export_date,
    }))
}

fn resolve_sync_track_indices(
    store: &dyn LibraryStore,
    by_id: &HashMap<TrackId, &Track>,
    index_of: &mut HashMap<TrackId, usize>,
    tracks: &mut Vec<SyncInputTrack>,
    library_path: Option<&std::path::Path>,
    track_ids: Vec<TrackId>,
    cancelled: &dyn Fn() -> bool,
) -> Result<Option<Vec<usize>>, String> {
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
    Ok(Some(indices))
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
            let beat_grid = store.load_beat_grid(track.track_id).map_err(|error| {
                sustain_device_sync::SyncError::Preparation(format!("{error:?}"))
            })?;
            let timeline_delay = sustain_pioneer::timeline::device_delay(&track.source_path)
                .map_err(|error| sustain_device_sync::SyncError::io(&track.source_path, error))?;
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
            assets.push_track(
                PioneerTrackAnalysis {
                    waveform_preview,
                    waveform_detail,
                    beat_grid,
                    timeline_delay,
                },
                cover_art.as_deref(),
            );
        }
        progress(SyncProgress {
            stage: SyncStage::Preparing,
            completed: index + 1,
            total,
        });
    }
    PreparedSyncRequest::new(request, pioneer_assets).map(Some)
}

/// The library's tracks under each artist, however their tags spell the
/// name: the artist checklist lists "Daft Punk" and "DAFT PUNK" as one
/// artist. Built in one pass over the library, so resolving a selection
/// costs the same whether one artist is ticked or all of them.
struct ArtistTracks(HashMap<CollationKey, Vec<TrackId>>);

impl ArtistTracks {
    fn of(tracks: &[Track]) -> Self {
        let mut by_artist = HashMap::<CollationKey, Vec<TrackId>>::new();
        for track in tracks {
            if let Some(artist) = track_artist_name(track) {
                by_artist
                    .entry(CollationKey::new(artist))
                    .or_default()
                    .push(track.id);
            }
        }
        Self(by_artist)
    }

    /// The tracks by `artist`, in library order; none for a blank name.
    fn by(&self, artist: &str) -> &[TrackId] {
        self.0
            .get(&CollationKey::new(artist))
            .map_or(&[], Vec::as_slice)
    }
}

fn snapshot_playlist_track_ids(
    item: PlaylistItem,
    tracks: &[Track],
    playlists: &[Playlist],
    smart_playlists: &[SmartPlaylist],
    clock: &dyn Clock,
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
                matching_tracks(tracks, &playlist.rules, clock)
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
        composer: metadata.composer.clone(),
        comment: metadata.comments.clone(),
        track_number: metadata.track_number,
        disc_number: metadata.disc_number,
        year: metadata.year.map(|y| y.max(0) as u32),
        duration_ms: metadata.duration.map(|d| d.as_millis() as u32).unwrap_or(0),
        rating: track.rating.stars(),
        bpm: metadata.bpm,
        key: metadata.key,
        bitrate_kbps: metadata.bitrate_kbps,
        sample_rate_hz: metadata.sample_rate_hz.unwrap_or(44_100),
        bit_depth: metadata.bit_depth.map_or(16, u16::from),
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
        DeviceCapacity, DeviceTarget, SourceSnapshot, SyncInputTrack, SyncProgress, SyncRequest,
        SyncStage, source_file_stat,
    };
    use sustain_domain::{
        DeviceKind, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, MetadataChange,
        PlayStatistics, Rating, SyncDevice, SyncDeviceId, Track, TrackId, TrackLocation,
        TrackMetadata, TrackRelativePath,
    };
    use sustain_library_store::{LibraryStore, SqliteLibraryStore};
    use sustain_metadata::{InitialTags, MetadataResult, MetadataService};

    use std::collections::BTreeSet;
    use sustain_device_sync::{ManifestJournal, SyncBlocker, SyncError, SyncFailure, SyncOutcome};
    use sustain_domain::SyncManifestEntry;
    use sustain_domain::{PioneerDatabase, PioneerExportRecord};

    use super::{
        ArtistTracks, ConnectedDevice, DeviceMountIdentity, DevicePlanResult, DevicePlanSnapshot,
        DeviceSyncPlanState, StoreManifestJournal, build_worker_sync_request, mount_identity,
        prepare_sync_request, save_synced_manifest, sync_input_track, unix_to_ymd,
    };
    use crate::ApplicationRuntime;

    fn connected(id: &str, mount_path: &str) -> ConnectedDevice {
        ConnectedDevice {
            id: SyncDeviceId::new(id).expect("device id"),
            kind: DeviceKind::UsbDrive,
            target: DeviceTarget::Filesystem {
                mount_path: mount_path.into(),
            },
            volume_id: Some(format!("{id}-volume")),
            label: id.to_owned(),
            is_known: true,
            has_marker: true,
        }
    }

    fn library_track(id: i64, relative_path: &str, artist: &str) -> Track {
        Track {
            id: TrackId::new(id).expect("track id"),
            location: TrackLocation::available(
                TrackRelativePath::new(relative_path).expect("relative path"),
            ),
            metadata: TrackMetadata {
                title: Some(format!("Track {id}")),
                artist: Some(artist.to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
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

    /// A store holding a library track for each track of `request`: a sync's
    /// tracks come from the library.
    fn store_with_tracks_of(request: &SyncRequest) -> SqliteLibraryStore {
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        for track in &request.tracks {
            let id = track.track_id.get();
            store
                .save_track(library_track(id, &format!("{id}.mp3"), &track.artist))
                .expect("save library track");
        }
        store
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
                    composer: None,
                    comment: None,
                    track_number: None,
                    disc_number: None,
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
            previous_playlist_files: Vec::new(),
            previous_pioneer_export: sustain_domain::PioneerExportRecord::default(),
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
    fn sync_tracks_carry_the_library_bit_depth_and_lossy_tracks_carry_sixteen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let path = temp.path().join("track.flac");
        std::fs::write(&path, b"audio").expect("source file");
        let source = || SourceSnapshot::provisional(source_file_stat(&path).expect("stat"));

        let mut hi_res = library_track(1, "track.flac", "Artist");
        hi_res.metadata.bit_depth = Some(24);
        let lossy = library_track(2, "track.flac", "Artist");

        assert_eq!(
            sync_input_track(&hi_res, path.clone(), source()).bit_depth,
            24
        );
        assert_eq!(
            sync_input_track(&lossy, path.clone(), source()).bit_depth,
            16
        );
    }

    #[test]
    fn readiness_counts_tracks_the_player_will_get_without_a_grid() {
        use sustain_domain::{BeatGrid, Bpm, TrackAnalysis, WaveformSegments};
        use sustain_library_store::{AnalysisCapabilities, AnalysisContext};

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        let device = SyncDevice {
            id: SyncDeviceId::new("xdj").expect("device id"),
            label: "USB".to_owned(),
            kind: DeviceKind::UsbDrive,
            layout: DeviceLayout::Pioneer,
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        };
        store.save_sync_device(&device).expect("save device");
        store
            .save_device_artist_selection(&device.id, &["Artist A".to_owned()])
            .expect("save artist selection");
        let tempo = |hundredths| Bpm::from_hundredths(hundredths).expect("tempo");
        let empty = WaveformSegments {
            segment_duration_ms: 0.0,
            segments: Vec::new(),
        };
        // 1: measured, so the grid agrees with the BPM analysis set.
        // 2: measured, but the user typed half the tempo, so the grid no
        // longer describes it. 3: never measured.
        for (id, typed_bpm, measured) in [(1, None, true), (2, Some(6_400), true), (3, None, false)]
        {
            let mut track = library_track(id, &format!("{id}.mp3"), "Artist A");
            track.metadata.bpm = typed_bpm.map(tempo);
            track.metadata.bpm_set_by_user = typed_bpm.is_some();
            store.save_track(track.clone()).expect("save track");
            if measured {
                let analysis = TrackAnalysis {
                    bpm: Some(tempo(12_800)),
                    key: None,
                    beat_grid: Some(
                        BeatGrid::new(tempo(12_800), std::time::Duration::ZERO, 1).expect("grid"),
                    ),
                    waveform_preview: empty.clone(),
                    waveform_detail: empty.clone(),
                    acoustics: None,
                };
                let bpm_only = AnalysisCapabilities {
                    bpm: true,
                    key: false,
                    audio: false,
                };
                store
                    .record_analysis(
                        track.id,
                        &analysis,
                        bpm_only,
                        AnalysisContext {
                            versions: sustain_analysis::ANALYZER_VERSIONS,
                            now_unix: 0,
                        },
                    )
                    .expect("record analysis");
            }
        }
        let runtime = ApplicationRuntime::new()
            .with_library_services(
                std::sync::Arc::new(store),
                std::sync::Arc::new(CountingMetadataService::default()),
            )
            .expect("library services");

        let readiness = runtime.device_analysis_readiness(&device.id);
        assert_eq!(readiness.total, 3);
        assert_eq!(readiness.missing_bpm, 1);
        assert_eq!(readiness.missing_beat_grid, 2);
    }

    #[test]
    fn a_ticked_artist_selects_the_tracks_of_every_spelling_of_the_name() {
        let tracks = [
            library_track(1, "a.mp3", "Daft Punk"),
            library_track(2, "b.mp3", " DAFT PUNK "),
            library_track(3, "c.mp3", "Daft Punk feat. Pharrell"),
            library_track(4, "d.mp3", "Bjo\u{308}rk"),
            library_track(5, "e.mp3", "Bjork"),
            library_track(6, "f.mp3", " "),
        ];
        let by_artist = ArtistTracks::of(&tracks);
        let ids = |artist: &str| -> Vec<i64> {
            by_artist
                .by(artist)
                .iter()
                .map(|track_id| track_id.get())
                .collect()
        };

        assert_eq!(ids("daft punk"), [1, 2]);
        // A composed and a decomposed accent spell one name; a missing
        // accent spells another.
        assert_eq!(ids("Bj\u{f6}rk"), [4]);
        assert_eq!(ids(" "), Vec::<i64>::new());
    }

    #[test]
    fn artist_selection_resolves_to_an_artist_playlist() {
        let temp = tempfile::tempdir().expect("tempdir");
        std::fs::write(temp.path().join("artist-track.mp3"), b"audio").expect("source file");
        std::fs::write(temp.path().join("other-track.mp3"), b"audio").expect("source file");

        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        let device = SyncDevice {
            id: SyncDeviceId::new("artist-device").expect("device id"),
            label: "USB".to_owned(),
            kind: DeviceKind::UsbDrive,
            layout: DeviceLayout::M3u,
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        };
        store.save_sync_device(&device).expect("save device");
        store
            .save_device_artist_selection(&device.id, &["Artist A".to_owned()])
            .expect("save artist selection");
        store
            .save_track(library_track(1, "artist-track.mp3", "Artist A"))
            .expect("save selected track");
        store
            .save_track(library_track(2, "other-track.mp3", "Other Artist"))
            .expect("save unselected track");

        let request = build_worker_sync_request(
            &store,
            &DeviceMountIdentity {
                device_id: device.id,
                target: DeviceTarget::Filesystem {
                    mount_path: "/tmp/device".into(),
                },
                volume_id: None,
            },
            Some(temp.path()),
            &crate::LocalClock,
            "2026-06-14".to_owned(),
            false,
            &|| false,
        )
        .expect("request result")
        .expect("not cancelled");

        assert_eq!(request.tracks.len(), 1);
        assert_eq!(request.playlists.len(), 1);
        assert_eq!(request.playlists[0].name, "Artist A");
        assert_eq!(request.playlists[0].track_indices, vec![0]);
        assert_eq!(
            request.tracks[0].track_id,
            TrackId::new(1).expect("track id")
        );
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
            target: first.target.clone(),
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
        let store = store_with_tracks_of(&request);
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
        let store = store_with_tracks_of(&request);
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

    /// A store knowing a USB drive that holds `a.mp3` and `b.mp3`.
    fn store_with_manifest() -> (SqliteLibraryStore, SyncDeviceId) {
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        let device = SyncDevice {
            id: SyncDeviceId::new("usb").expect("device id"),
            label: "USB".to_owned(),
            kind: DeviceKind::UsbDrive,
            layout: DeviceLayout::Pioneer,
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        };
        store.save_sync_device(&device).expect("save device");
        store
            .save_device_manifest(
                &device.id,
                &[manifest_entry(1, "a.mp3"), manifest_entry(2, "b.mp3")],
            )
            .expect("save manifest");
        (store, device.id)
    }

    fn database(size_bytes: u64) -> PioneerDatabase {
        PioneerDatabase {
            size_bytes,
            sha256: format!("{size_bytes:064x}"),
        }
    }

    fn manifest_entry(track: i64, path: &str) -> SyncManifestEntry {
        SyncManifestEntry {
            track_id: TrackId::new(track).expect("track id"),
            on_device_path: DeviceRelativePath::new(path).expect("device path"),
            fingerprint: format!("fingerprint-{track}"),
            anlz: None,
        }
    }

    #[test]
    fn the_journal_keeps_the_saved_manifest_in_step_with_the_device() {
        let (store, device_id) = store_with_manifest();
        let mut journal = StoreManifestJournal {
            store: &store,
            device_id: &device_id,
        };
        journal
            .record(&manifest_entry(3, "c.mp3"))
            .expect("record a copy");
        journal
            .forget(&DeviceRelativePath::new("a.mp3").expect("device path"))
            .expect("record a removal");

        assert_eq!(
            store.device_manifest(&device_id).expect("load manifest"),
            vec![manifest_entry(2, "b.mp3"), manifest_entry(3, "c.mp3")]
        );
    }

    #[test]
    fn the_journal_makes_the_pioneer_export_it_vouches_for_durable() {
        let (backing, device_id) = store_with_manifest();
        let store = crate::test_store::FaultyStore::new(std::sync::Arc::new(backing));
        let record = PioneerExportRecord {
            databases: BTreeSet::from([database(1), database(2)]),
            artwork: BTreeSet::from([
                DeviceRelativePath::new("PIONEER/Artwork/00001/a1.jpg").expect("device path")
            ]),
        };
        let mut journal = StoreManifestJournal {
            store: &store,
            device_id: &device_id,
        };
        journal
            .record_pioneer_export(&record)
            .expect("record the export");
        assert_eq!(
            store
                .device_pioneer_export(&device_id)
                .expect("load Pioneer export"),
            record
        );
        assert_eq!(store.flush_durable_calls(), 1);

        store.set_fail_flush_durable(true);
        assert!(
            journal.record_pioneer_export(&record).is_err(),
            "a record that may not survive cannot vouch for a drive write"
        );
    }

    #[test]
    fn a_blocked_sync_explains_itself_in_the_user_language() {
        let (store, device_id) = store_with_manifest();
        let failure = SyncFailure {
            error: SyncError::Blocked(SyncBlocker::ForeignPioneerExport),
            manifest: Vec::new(),
            playlist_files: Vec::new(),
            pioneer_export: PioneerExportRecord::default(),
        };
        let failed = save_synced_manifest(&store, &device_id, Err(Box::new(failure)))
            .expect_err("the sync was refused");
        assert_eq!(
            failed.message,
            crate::notifications::sync_blocker_text(SyncBlocker::ForeignPioneerExport)
        );
    }

    #[test]
    fn a_finished_sync_discards_the_plan_made_while_it_ran() {
        let mut runtime = ApplicationRuntime::new();
        let receiver = runtime.device_sync_event_receiver();
        let device = connected("stick", "/mnt/stick");
        runtime.request_device_sync_plan(&device);
        let generation = runtime
            .device_plan_cache
            .as_ref()
            .expect("plan requested")
            .generation;
        assert!(matches!(
            runtime
                .device_sync_scheduler
                .start_test_task(|_progress, _cancel| Ok(SyncOutcome::default())),
            crate::DeviceSyncStartOutcome::Started(_)
        ));
        loop {
            let event = receiver.recv_blocking().expect("sync event");
            let finished = matches!(event, crate::DeviceSyncEvent::Finished { .. });
            runtime.apply_device_sync_event(event);
            if finished {
                break;
            }
        }

        assert!(!runtime.apply_device_plan_result(DevicePlanResult {
            generation,
            mount: mount_identity(&device),
            result: Ok(DevicePlanSnapshot {
                plan: None,
                capacity: DeviceCapacity::default(),
            }),
        }));
        assert_eq!(
            runtime.device_sync_plan_state(&device),
            DeviceSyncPlanState::Unavailable
        );
    }

    #[test]
    fn the_manifest_a_sync_reports_is_saved_however_it_ended() {
        let (store, device_id) = store_with_manifest();
        let playlist = |path: &str| DeviceRelativePath::new(path).expect("device path");
        let outcome = SyncOutcome {
            copied: 1,
            manifest: vec![manifest_entry(3, "c.mp3")],
            playlist_files: vec![playlist("Mix.m3u8")],
            pioneer_export: PioneerExportRecord {
                databases: BTreeSet::from([database(1)]),
                artwork: BTreeSet::from([playlist("PIONEER/Artwork/00001/a1.jpg")]),
            },
            ..SyncOutcome::default()
        };
        let saved = save_synced_manifest(&store, &device_id, Ok(outcome.clone()));
        assert_eq!(saved.expect("the sync succeeded"), outcome);
        assert_eq!(
            store.device_manifest(&device_id).expect("load manifest"),
            vec![manifest_entry(3, "c.mp3")]
        );
        assert_eq!(
            store
                .device_playlist_files(&device_id)
                .expect("load playlist files"),
            vec![playlist("Mix.m3u8")]
        );
        assert_eq!(
            store
                .device_pioneer_export(&device_id)
                .expect("load Pioneer export"),
            outcome.pioneer_export
        );

        let failure = SyncFailure {
            error: SyncError::io("d.mp3", std::io::Error::other("No space left on device")),
            manifest: vec![manifest_entry(4, "d.mp3")],
            playlist_files: vec![playlist("Chill.m3u8")],
            pioneer_export: PioneerExportRecord {
                databases: BTreeSet::from([database(1), database(2)]),
                artwork: BTreeSet::from([playlist("PIONEER/Artwork/00001/a2.jpg")]),
            },
        };
        let failed_export = failure.pioneer_export.clone();
        let failed = save_synced_manifest(&store, &device_id, Err(Box::new(failure)))
            .expect_err("the sync failed");
        assert_eq!(failed.message, "d.mp3: No space left on device");
        assert_eq!(
            store.device_manifest(&device_id).expect("load manifest"),
            vec![manifest_entry(4, "d.mp3")]
        );
        assert_eq!(
            store
                .device_playlist_files(&device_id)
                .expect("load playlist files"),
            vec![playlist("Chill.m3u8")]
        );
        assert_eq!(
            store
                .device_pioneer_export(&device_id)
                .expect("load Pioneer export"),
            failed_export
        );
    }

    #[test]
    fn a_manifest_that_cannot_be_saved_fails_the_sync() {
        let (backing, device_id) = store_with_manifest();
        let store = crate::test_store::FaultyStore::new(std::sync::Arc::new(backing));
        store.set_fail_device_manifest(true);

        let failed = save_synced_manifest(&store, &device_id, Ok(SyncOutcome::default()))
            .expect_err("an unsaved manifest fails the sync");
        assert!(
            failed
                .message
                .starts_with("could not save the on-device manifest"),
            "{}",
            failed.message
        );

        let failure = SyncFailure {
            error: SyncError::Empty,
            manifest: Vec::new(),
            playlist_files: Vec::new(),
            pioneer_export: PioneerExportRecord::default(),
        };
        let failed = save_synced_manifest(&store, &device_id, Err(Box::new(failure)))
            .expect_err("the sync failed");
        assert_eq!(
            failed.message,
            "the selection contains no tracks; could not save the on-device manifest"
        );
        assert_eq!(store.device_manifest_calls(), 2);
    }
}
