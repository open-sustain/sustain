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
                if (self.library_task.is_occupied()
                    || self.has_pending_managed_metadata_retarget()
                    || self.has_pending_missing_track_relocation())
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
                        .library_task
                        .status()
                        .is_library_consolidation_running()
                        && self.settings.library.path == settings.library.path
                        && self.settings.library.management_mode
                            == LibraryManagementMode::CopyAddedFilesIntoLibrary
                        && settings.library.management_mode
                            == LibraryManagementMode::ReferenceFilesInPlace;

                    if cancellation_allowed {
                        self.library_task.request_cancellation_of(
                            crate::library_task::LibraryTaskKind::Organize,
                        );
                    } else {
                        return Err(ApplicationRuntimeError::BackgroundTaskRunning);
                    }
                }
                // A new library root must be a folder: tracks are checked
                // against it at once, so a mistyped or unmounted one would
                // mark them all missing. Clearing the root, or keeping one
                // that has since gone away, is not a change to check.
                if settings.library.path != self.settings.library.path
                    && let Some(library_path) = settings.library.path.as_deref()
                {
                    ensure_library_root_is_a_folder(library_path)?;
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
            ApplicationCommand::RemoveTracksFromLibrary { track_ids } => {
                self.remove_tracks_from_library(&track_ids)?;
            }
            ApplicationCommand::MoveTrackToTrash { track_id } => {
                self.move_track_to_trash(track_id)?;
            }
            ApplicationCommand::RelocateMissingTrack {
                track_id,
                replacement_path,
            } => {
                self.relocate_missing_track(track_id, &replacement_path)?;
            }
            ApplicationCommand::ReplaceTrackAudioFromYoutube { track_id, url } => {
                self.request_youtube_audio_replacement(track_id, url)?;
            }
            ApplicationCommand::ConsolidateDuplicateTracks(request) => {
                self.consolidate_duplicate_tracks(request)?;
            }
            ApplicationCommand::ConsolidateVerifiedDuplicates(merges) => {
                self.consolidate_verified_duplicates(merges)?;
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
            ApplicationCommand::SetFetchedArtwork {
                track_id,
                artwork,
                fetched_at,
            } => {
                self.set_fetched_artwork(track_id, artwork, fetched_at)?;
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
            ApplicationCommand::SetDeviceArtistSelection { device_id, artists } => {
                self.set_device_artist_selection(device_id, artists)?;
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
        // The index of each track whose availability flips, beside its new
        // location.
        let mut flipped = Vec::new();
        let mut newly_missing = 0usize;
        let mut unresolved = 0usize;
        for (index, track) in self.library_tracks.iter().enumerate() {
            let availability = match probe(&track.location.absolute_path(&new_library_path)) {
                FilePresence::Present => sustain_domain::TrackAvailability::Available,
                FilePresence::Absent => sustain_domain::TrackAvailability::Missing,
                FilePresence::ProbeFailed => {
                    unresolved += 1;
                    continue;
                }
            };
            let now_missing = availability == sustain_domain::TrackAvailability::Missing;
            if now_missing != track.location.is_missing() {
                if now_missing {
                    newly_missing += 1;
                }
                flipped.push((
                    index,
                    track.location.clone().with_availability(availability),
                ));
            }
        }

        if !flipped.is_empty()
            && let Some(store) = self.library_store.as_ref()
        {
            let changed = flipped
                .iter()
                .map(|(index, location)| (self.library_tracks[*index].id, location.clone()))
                .collect::<Vec<_>>();
            store
                .update_track_locations(&changed)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        }
        // Adopt only what the store committed.
        let changed = !flipped.is_empty();
        for (index, location) in flipped {
            let mut track = self.library_tracks[index].clone();
            track.location = location;
            self.store_library_track(index, track);
        }

        if changed {
            self.refresh_playback_queue_track_ids();
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

/// `Ok` when `path` is a folder Sustain can list and open files in: read
/// and search permission, as the effective user. A missing path, or one
/// that names a file, is not a folder; any other failure to reach it, such
/// as a permission refused on it or on the way to it, leaves it
/// inaccessible.
fn ensure_library_root_is_a_folder(path: &std::path::Path) -> ApplicationRuntimeResult<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => return Err(ApplicationRuntimeError::LibraryPathNotAFolder),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
            ) =>
        {
            return Err(ApplicationRuntimeError::LibraryPathNotAFolder);
        }
        Err(_) => return Err(ApplicationRuntimeError::LibraryPathInaccessible),
    }
    rustix::fs::accessat(
        rustix::fs::CWD,
        path,
        rustix::fs::Access::READ_OK | rustix::fs::Access::EXEC_OK,
        rustix::fs::AtFlags::EACCESS,
    )
    .map_err(|_| ApplicationRuntimeError::LibraryPathInaccessible)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::ensure_library_root_is_a_folder;
    use crate::ApplicationRuntimeError;

    #[test]
    fn a_library_root_is_a_folder_a_missing_or_file_path_or_an_unreadable_one() {
        let root = std::env::temp_dir().join(format!("sustain_root_check_{}", std::process::id()));
        let locked = root.join("locked");
        std::fs::create_dir_all(locked.join("inside")).expect("create folders");
        std::fs::write(root.join("file.flac"), b"audio").expect("write a file");

        assert_eq!(ensure_library_root_is_a_folder(&root), Ok(()));
        assert_eq!(
            ensure_library_root_is_a_folder(&root.join("missing")),
            Err(ApplicationRuntimeError::LibraryPathNotAFolder)
        );
        assert_eq!(
            ensure_library_root_is_a_folder(&root.join("file.flac")),
            Err(ApplicationRuntimeError::LibraryPathNotAFolder)
        );
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000))
            .expect("lock the folder");
        let behind_a_refusal = ensure_library_root_is_a_folder(&locked.join("inside"));
        let refused = ensure_library_root_is_a_folder(&locked);
        // Listable, but its files cannot be opened.
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o400))
            .expect("make the folder listable only");
        let unsearchable = ensure_library_root_is_a_folder(&locked);
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755))
            .expect("unlock the folder");
        std::fs::remove_dir_all(&root).expect("remove test folders");

        // Root reads through any permission; the refusal needs a user.
        if rustix::process::geteuid().is_root() {
            return;
        }
        assert_eq!(
            behind_a_refusal,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
        assert_eq!(
            refused,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
        assert_eq!(
            unsearchable,
            Err(ApplicationRuntimeError::LibraryPathInaccessible)
        );
    }
}
