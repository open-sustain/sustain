// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use sustain_domain::{ApplicationCommand, LibraryManagementMode};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity,
    file_presence::{FilePresence, probe_file_presence},
    notifications,
};

impl ApplicationRuntime {
    pub fn handle_command(&mut self, command: ApplicationCommand) -> ApplicationRuntimeResult<()> {
        let allowed_while_hydrating = match &command {
            ApplicationCommand::Playback(sustain_domain::PlaybackCommand::SetVolume(_)) => true,
            ApplicationCommand::UpdateSettings(settings) => {
                settings.library == self.settings.library
            }
            _ => false,
        };
        if self.library_hydration_state() != crate::LibraryHydrationState::Ready
            && !allowed_while_hydrating
        {
            self.ensure_library_hydrated()?;
        }
        match command {
            ApplicationCommand::Playback(command) => {
                self.handle_playback_command(command)?;
            }
            ApplicationCommand::UpdateSettings(settings) => {
                // Enforce the `audio ⇒ bpm ∧ key` invariant at the single
                // command chokepoint so the persisted file, the in-memory
                // state, and the background scheduler all agree: audio
                // analysis yields all three off one decode.
                let settings = {
                    let mut settings = settings;
                    settings.analysis = settings.analysis.normalized();
                    settings
                };
                if settings.library != self.settings.library {
                    self.ensure_library_hydrated()?;
                }
                if (self.background_task_status.is_running()
                    || self.has_pending_managed_metadata_retarget())
                    && settings.library != self.settings.library
                {
                    // The only narrow exception is the management-mode
                    // flip from managed → unmanaged DURING an active
                    // consolidation, same library path: the user is
                    // explicitly aborting the organization job they
                    // just started. Every other library change — and
                    // in particular any `library.path` change — is
                    // rejected outright while a background task or
                    // serialized retarget is still moving files.
                    let cancellation_allowed = self
                        .background_task_status
                        .is_library_consolidation_running()
                        && self.settings.library.path == settings.library.path
                        && self.settings.library.management_mode
                            == LibraryManagementMode::CopyAddedFilesIntoLibrary
                        && settings.library.management_mode
                            == LibraryManagementMode::ReferenceFilesInPlace;

                    if cancellation_allowed {
                        self.request_library_consolidation_cancellation();
                    } else {
                        return Err(ApplicationRuntimeError::BackgroundTaskRunning);
                    }
                }
                let organized_library_contract_changed = settings.library.management_mode
                    == LibraryManagementMode::CopyAddedFilesIntoLibrary
                    && (self.settings.library.management_mode
                        != LibraryManagementMode::CopyAddedFilesIntoLibrary
                        || settings.library.path != self.settings.library.path);
                if organized_library_contract_changed {
                    let library_path = settings
                        .library_path()
                        .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
                        .to_path_buf();
                    self.ensure_managed_library_filesystem_supported_at(&library_path)?;
                }
                let previous_library_path = self.settings.library.path.clone();
                let previous_analysis = self.settings.analysis;
                let previous_online = self.settings.online;
                let previous_resource_usage = self.settings.background_jobs.resource_usage;
                if let Some(settings_store) = &self.settings_store {
                    settings_store
                        .save_settings(settings.clone())
                        .map_err(|_| ApplicationRuntimeError::SettingsSaveFailed)?;
                }
                self.settings = settings;
                // Settings changes that do NOT alter `library.path`
                // never stat tracks — toggling the management-mode
                // checkbox in Preferences must not freeze the UI on a
                // 10k library. A library path change is the exception:
                // it is structural reconciliation (semantically closer
                // to a scan than to a preference toggle), the user
                // just typed/picked the new root, and the cost is
                // bounded by the library size.
                let new_library_path = self.settings.library.path.clone();
                if let (Some(previous), Some(new)) =
                    (previous_library_path.as_ref(), new_library_path.as_ref())
                    && previous != new
                {
                    self.reconcile_track_availability_after_library_path_change(new.clone())?;
                }
                // Propagate analysis-tickbox changes to the background
                // scheduler so toggling a capability off stops the
                // worker between tracks (matching the managed-library
                // cancellation precedent). Library-path changes also
                // propagate so the worker resolves paths against the
                // new root.
                if self.settings.analysis != previous_analysis
                    && let Some(scheduler) = self.analysis_scheduler()
                {
                    scheduler.update_settings(self.settings.analysis);
                }
                if self.settings.library.path != previous_library_path
                    && let Some(scheduler) = self.analysis_scheduler()
                {
                    scheduler.set_library_path(self.settings.library.path.clone());
                }
                if self.settings.online != previous_online
                    && let Some(scheduler) = self.online_scheduler()
                {
                    scheduler.update_settings(self.settings.online);
                }
                if self.settings.library.path != previous_library_path
                    && let Some(scheduler) = self.online_scheduler()
                {
                    scheduler.set_library_path(self.settings.library.path.clone());
                }
                if self.settings.library.path != previous_library_path
                    && let Some(writer) = self.metadata_writer()
                {
                    writer.set_library_path(self.settings.library.path.clone());
                }
                // Resource-usage flips trigger a teardown + respawn of
                // the analysis worker pool at the new size + priority.
                if self.settings.background_jobs.resource_usage != previous_resource_usage
                    && let Some(scheduler) = self.analysis_scheduler()
                {
                    scheduler.update_resource_usage(self.settings.background_jobs.resource_usage);
                }
            }
            ApplicationCommand::ScanLibrary { library_path } => {
                self.scan_library(library_path)?;
            }
            ApplicationCommand::RemoveTrackFromLibrary { track_id } => {
                self.remove_track_from_library(track_id)?;
            }
            ApplicationCommand::MoveTrackToTrash { track_id } => {
                self.move_track_to_trash(track_id)?;
            }
            ApplicationCommand::SetRating { track_id, rating } => {
                self.set_rating(track_id, rating)?;
            }
            ApplicationCommand::CreatePlaylist {
                name,
                parent_folder_id,
            } => {
                self.create_playlist(name, parent_folder_id)?;
            }
            ApplicationCommand::RenamePlaylist { playlist_id, name } => {
                self.rename_playlist(playlist_id, name)?;
            }
            ApplicationCommand::DeletePlaylist { playlist_id } => {
                self.delete_playlist(playlist_id)?;
            }
            ApplicationCommand::AddTracksToPlaylist {
                playlist_id,
                track_ids,
            } => {
                self.add_tracks_to_playlist(playlist_id, track_ids)?;
            }
            ApplicationCommand::RemoveTracksFromPlaylist {
                playlist_id,
                track_ids,
            } => {
                self.remove_tracks_from_playlist(playlist_id, track_ids)?;
            }
            ApplicationCommand::MovePlaylistEntries {
                playlist_id,
                track_ids,
                new_position,
            } => {
                self.move_playlist_entries(playlist_id, track_ids, new_position)?;
            }
            ApplicationCommand::CreatePlaylistFolder {
                name,
                parent_folder_id,
            } => {
                self.create_playlist_folder(name, parent_folder_id)?;
            }
            ApplicationCommand::RenamePlaylistFolder { folder_id, name } => {
                self.rename_playlist_folder(folder_id, name)?;
            }
            ApplicationCommand::DeletePlaylistFolder { folder_id } => {
                self.delete_playlist_folder(folder_id)?;
            }
            ApplicationCommand::CreateSmartPlaylist {
                name,
                parent_folder_id,
                rules,
            } => {
                self.create_smart_playlist(name, parent_folder_id, rules)?;
            }
            ApplicationCommand::UpdateSmartPlaylist {
                smart_playlist_id,
                name,
                rules,
            } => {
                self.update_smart_playlist(smart_playlist_id, name, rules)?;
            }
            ApplicationCommand::DeleteSmartPlaylist { smart_playlist_id } => {
                self.delete_smart_playlist(smart_playlist_id)?;
            }
            ApplicationCommand::MovePlaylistItem {
                item,
                target_parent_folder_id,
                position,
            } => {
                self.move_playlist_item(item, target_parent_folder_id, position)?;
            }
            ApplicationCommand::UpdateMetadata { track_id, change } => {
                self.update_metadata(track_id, *change)?;
            }
            ApplicationCommand::ResetPlayCount { track_id } => {
                self.reset_play_count(track_id)?;
            }
            ApplicationCommand::SetArtwork { track_id, artwork } => {
                self.set_artwork(track_id, artwork)?;
            }
            ApplicationCommand::FetchArtwork { track_id } => {
                self.fetch_artwork(track_id)?;
            }
            ApplicationCommand::AddExternalLibraryItems { paths } => {
                self.add_external_library_items(paths)?;
            }
            ApplicationCommand::SetDeviceLayout { device_id, layout } => {
                self.set_device_layout(device_id, layout)?;
            }
            ApplicationCommand::SetDeviceSubPath {
                device_id,
                sub_path,
            } => {
                self.set_device_sub_path(device_id, sub_path)?;
            }
            ApplicationCommand::SetDeviceFilesPerFolderCap { device_id, cap } => {
                self.set_device_files_per_folder_cap(device_id, cap)?;
            }
            ApplicationCommand::SetDeviceSelection {
                device_id,
                selection,
            } => {
                self.set_device_selection(device_id, selection)?;
            }
            ApplicationCommand::RenameDevice { device_id, label } => {
                self.rename_device(device_id, label)?;
            }
            ApplicationCommand::ForgetDevice { device_id } => {
                self.forget_device(device_id)?;
            }
            ApplicationCommand::SyncDevice {
                device_id,
                remove_stale,
            } => {
                self.start_device_sync(device_id, remove_stale)?;
            }
            ApplicationCommand::AnalyzeDeviceTracks { device_id } => {
                self.analyze_device_tracks(device_id)?;
            }
        }

        Ok(())
    }

    /// Re-stat every persisted track against `new_library_path` and
    /// flush the resulting availability flags to SQLite, then surface
    /// the outcome as an ephemeral notification. Called once per
    /// accepted library-path change (never on no-op updates, never on
    /// management-mode toggles), so the user gets an immediate, honest
    /// picture of what is reachable under the new root instead of
    /// having to wait until they click a track to discover it is gone.
    fn reconcile_track_availability_after_library_path_change(
        &mut self,
        new_library_path: std::path::PathBuf,
    ) -> ApplicationRuntimeResult<()> {
        self.reconcile_track_availability_after_library_path_change_with(
            new_library_path,
            probe_file_presence,
        )
    }

    pub(super) fn reconcile_track_availability_after_library_path_change_with(
        &mut self,
        new_library_path: std::path::PathBuf,
        probe: impl Fn(&std::path::Path) -> FilePresence,
    ) -> ApplicationRuntimeResult<()> {
        let total = self.library_tracks.len();
        let mut changed = Vec::new();
        let mut newly_missing = 0usize;
        let mut unresolved = 0usize;
        let mut reconciled = Vec::with_capacity(total);
        for mut track in std::mem::take(&mut self.library_tracks) {
            let was_missing = track.location.is_missing();
            let availability = match probe(&track.location.absolute_path(&new_library_path)) {
                FilePresence::Present => Some(sustain_domain::TrackAvailability::Available),
                FilePresence::Absent => Some(sustain_domain::TrackAvailability::Missing),
                FilePresence::ProbeFailed => {
                    unresolved += 1;
                    None
                }
            };
            if let Some(availability) = availability {
                track.location = track.location.with_availability(availability);
            }
            let now_missing = track.location.is_missing();
            if was_missing != now_missing {
                changed.push((track.id, track.location.clone()));
                if now_missing {
                    newly_missing += 1;
                }
            }
            reconciled.push(track);
        }
        self.library_tracks = reconciled;
        self.rebuild_search_index();

        if !changed.is_empty()
            && let Some(store) = self.library_store.as_ref()
        {
            store
                .update_track_locations(&changed)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }

        if !changed.is_empty() {
            self.refresh_playback_queue_track_ids();
            self.notify_track_availability_observer();
        }

        let severity = if newly_missing > 0 || unresolved > 0 {
            NotificationSeverity::Warning
        } else {
            NotificationSeverity::Info
        };
        self.push_ephemeral_notification(
            NotificationCategory::LibraryScan,
            severity,
            notifications::library_path_change_outcome_text(newly_missing, unresolved, total),
        );

        Ok(())
    }
}
