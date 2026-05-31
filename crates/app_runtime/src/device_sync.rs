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
use std::time::UNIX_EPOCH;

use sustain_device_sync::{
    ConnectedDevice, SourceSnapshot, SyncInputPlaylist, SyncInputTrack, SyncPlan, SyncRequest,
    engine, source_file_stat,
};
use sustain_domain::{
    DeviceLayout, DeviceRelativePath, FilesPerFolderCap, MusicalKey, PlaylistItem, SyncDevice,
    SyncDeviceId, Track,
};
use sustain_library_store::AnalysisCapabilities;

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

/// A connected device's filesystem capacity, read from `statvfs`. Drives
/// the panel's disk-occupation bar (how much of `total_bytes` the ticked
/// playlists would occupy).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DeviceCapacity {
    /// Total size of the device's filesystem in bytes.
    pub total_bytes: u64,
    /// Bytes currently free for an unprivileged writer.
    pub available_bytes: u64,
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

    /// Read a mounted filesystem's total and available capacity. Returns
    /// `None` if the path cannot be `statvfs`'d (e.g. the device was
    /// unplugged between discovery and this call).
    pub fn mount_capacity(&self, mount_path: &std::path::Path) -> Option<DeviceCapacity> {
        crate::mount::capacity(mount_path).map(|(total_bytes, available_bytes)| DeviceCapacity {
            total_bytes,
            available_bytes,
        })
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
        &self,
        id: SyncDeviceId,
        layout: DeviceLayout,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.layout = layout;
        self.persist_device(&device)
    }

    pub(crate) fn set_device_sub_path(
        &self,
        id: SyncDeviceId,
        sub_path: DeviceRelativePath,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.sub_path = sub_path;
        self.persist_device(&device)
    }

    pub(crate) fn set_device_files_per_folder_cap(
        &self,
        id: SyncDeviceId,
        cap: FilesPerFolderCap,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.files_per_folder_cap = cap;
        self.persist_device(&device)
    }

    pub(crate) fn rename_device(
        &self,
        id: SyncDeviceId,
        label: String,
    ) -> ApplicationRuntimeResult<()> {
        let mut device = self.device_config_or_default(&id);
        device.label = label;
        self.persist_device(&device)
    }

    pub(crate) fn set_device_selection(
        &self,
        id: SyncDeviceId,
        selection: Vec<PlaylistItem>,
    ) -> ApplicationRuntimeResult<()> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        store
            .save_device_selection(&id, &selection)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
    }

    pub(crate) fn forget_device(&self, id: SyncDeviceId) -> ApplicationRuntimeResult<()> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        store
            .delete_sync_device(&id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
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
                "This device's selection is empty.".to_owned(),
            );
            return Ok(());
        }
        // Reuse the shared analysis pipeline; it pushes its own
        // queued/already-complete notifications.
        self.request_tracks_analysis_run(track_ids, AnalysisRunRequest::All);
        Ok(())
    }

    // --- Plan + sync ---

    /// Compute what a sync of `id` to its connected mount would do
    /// (copies, updates, removals), without writing. Returns `None` when
    /// the device is not connected, has no library root, or the
    /// selection is empty.
    pub fn device_sync_plan(&self, id: &SyncDeviceId) -> Option<SyncPlan> {
        self.ensure_library_hydrated().ok()?;
        let connected = self.connected_devices().into_iter().find(|d| &d.id == id)?;
        let device = self.device_config(id)?;
        let request = self
            .build_sync_request(&device, connected.mount_path, false, false)
            .ok()?;
        engine::plan(&request).ok()
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
                "A device sync is already running.".to_owned(),
            );
            return Ok(());
        }
        let Some(connected) = self.connected_devices().into_iter().find(|d| d.id == id) else {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Warning,
                "That device is no longer connected.".to_owned(),
            );
            return Ok(());
        };
        let device = self.ensure_device_config(&connected)?;
        let request = self.build_sync_request(&device, connected.mount_path, remove_stale, true)?;
        if request.tracks.is_empty() {
            self.push_ephemeral_notification(
                NotificationCategory::DeviceSync,
                NotificationSeverity::Info,
                "Pick at least one playlist to sync to this device.".to_owned(),
            );
            return Ok(());
        }
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        match self.device_sync_scheduler.start(id, request, library_store) {
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
                    "A device sync is already running.".to_owned(),
                );
            }
            DeviceSyncStartOutcome::SpawnFailed(detail) => {
                self.push_ephemeral_notification(
                    NotificationCategory::DeviceSync,
                    NotificationSeverity::Error,
                    format!("Device sync failed to start: {detail}"),
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
                        let manifest_saved = self.library_store.as_ref().is_some_and(|store| {
                            store
                                .save_device_manifest(&completion.device_id, &outcome.manifest)
                                .is_ok()
                        });
                        if !manifest_saved {
                            self.push_ephemeral_notification(
                                NotificationCategory::DeviceSync,
                                NotificationSeverity::Error,
                                "Device sync failed: could not save the on-device manifest."
                                    .to_owned(),
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
                            format!("Device sync failed: {message}"),
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

    fn build_sync_request(
        &self,
        device: &SyncDevice,
        mount_path: PathBuf,
        remove_stale: bool,
        load_pioneer_assets: bool,
    ) -> ApplicationRuntimeResult<SyncRequest> {
        let store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryStoreFailed)?;
        // Waveforms (from SQLite) and cover art (read from each file) are
        // only consumed when actually writing the Pioneer ANLZ/artwork
        // files; planning (the occupation bar, the diff) never reads
        // them, so skip those per-track loads on that path — it runs on
        // every playlist toggle.
        let want_pioneer_assets = load_pioneer_assets && device.layout == DeviceLayout::Pioneer;
        let by_id: HashMap<_, _> = self.library_tracks.iter().map(|t| (t.id, t)).collect();

        let mut index_of: HashMap<sustain_domain::TrackId, usize> = HashMap::new();
        let mut tracks: Vec<SyncInputTrack> = Vec::new();
        let mut playlists: Vec<SyncInputPlaylist> = Vec::new();

        for item in self.device_selection(&device.id) {
            let Some(track_ids) = self.playlist_item_track_ids(item) else {
                continue;
            };
            let name = self
                .playlist_item_name(item)
                .unwrap_or_else(|| "Playlist".to_owned());
            let mut indices = Vec::with_capacity(track_ids.len());
            for tid in track_ids {
                let resolved = match index_of.get(&tid) {
                    Some(&existing) => Some(existing),
                    None => {
                        let Some(track) = by_id.get(&tid) else {
                            continue;
                        };
                        if track.location.is_missing() {
                            continue;
                        }
                        let Some(source_path) = self.absolute_track_path(track) else {
                            continue;
                        };
                        let preview_detail = if want_pioneer_assets {
                            store.load_waveform(tid).ok().flatten()
                        } else {
                            None
                        };
                        let cover_art = if want_pioneer_assets {
                            self.read_artwork(&source_path)
                        } else {
                            None
                        };
                        let Some(source) = source_snapshot(store.as_ref(), track.id, &source_path)?
                        else {
                            continue;
                        };
                        let input =
                            sync_input_track(track, source_path, source, preview_detail, cover_art);
                        let position = tracks.len();
                        tracks.push(input);
                        index_of.insert(tid, position);
                        Some(position)
                    }
                };
                if let Some(position) = resolved {
                    indices.push(position);
                }
            }
            playlists.push(SyncInputPlaylist {
                name,
                track_indices: indices,
            });
        }

        let previous_manifest = store
            .device_manifest(&device.id)
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let export_date = unix_to_ymd(self.clock_unix_secs());

        Ok(SyncRequest {
            device: device.clone(),
            mount_path,
            tracks,
            playlists,
            previous_manifest,
            remove_stale,
            export_date,
        })
    }

    fn playlist_item_name(&self, item: PlaylistItem) -> Option<String> {
        match item {
            PlaylistItem::Playlist(id) => self
                .playlists
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone()),
            PlaylistItem::SmartPlaylist(id) => self
                .smart_playlists
                .iter()
                .find(|p| p.id == id)
                .map(|p| p.name.clone()),
            PlaylistItem::Folder(_) => None,
        }
    }

    fn clock_unix_secs(&self) -> i64 {
        self.clock
            .now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0)
    }
}

fn sync_input_track(
    track: &Track,
    source_path: PathBuf,
    source: SourceSnapshot,
    waveform: Option<sustain_library_store::StoredWaveform>,
    cover_art: Option<Vec<u8>>,
) -> SyncInputTrack {
    let metadata = &track.metadata;
    let extension = source_path
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .unwrap_or_default();
    let (waveform_preview, waveform_detail) = match waveform {
        Some(stored) => (Some(stored.preview), Some(stored.detail)),
        None => (None, None),
    };
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
        waveform_preview,
        waveform_detail,
        cover_art,
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
    use super::unix_to_ymd;

    #[test]
    fn formats_known_dates() {
        assert_eq!(unix_to_ymd(0), "1970-01-01");
        assert_eq!(unix_to_ymd(1_700_000_000), "2023-11-14");
        // 2026-05-29 00:00:00 UTC
        assert_eq!(unix_to_ymd(1_780_012_800), "2026-05-29");
    }
}
