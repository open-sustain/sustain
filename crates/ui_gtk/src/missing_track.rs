// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::RefCell, collections::HashMap, rc::Rc};

use gtk::prelude::*;
use gtk::{FileDialog, FileFilter, gio};
use sustain_app_runtime::{
    ApplicationCommand, ApplicationRuntimeError, PlaybackCommand, PlaybackQueueRequest, TrackId,
};

use crate::{
    PlaybackChangedCallback, SharedRuntime,
    artwork_loader::{ArtworkLoader, ArtworkSource},
    command_controller::SharedCommandController,
};

pub(crate) type LocateMissingTrackCallback = Rc<dyn Fn(TrackId, PlaybackQueueRequest)>;
pub(crate) type MissingTrackRelocationCompletedCallback = Rc<dyn Fn(TrackId, bool)>;
pub(crate) type PendingLocatedPlaybacks = Rc<RefCell<HashMap<TrackId, PlaybackQueueRequest>>>;

pub(crate) fn play_track_or_offer_locate(
    command_controller: &SharedCommandController,
    track_id: TrackId,
    queue: PlaybackQueueRequest,
    playback_changed: &PlaybackChangedCallback,
    locate_missing_track: &LocateMissingTrackCallback,
) {
    match command_controller.dispatch_unreported(ApplicationCommand::Playback(
        PlaybackCommand::PlayTrack {
            track_id,
            queue: queue.clone(),
        },
    )) {
        Ok(()) => playback_changed(),
        Err(ApplicationRuntimeError::TrackUnavailable)
            if command_controller
                .runtime()
                .borrow()
                .library_track(track_id)
                .is_some_and(|track| track.location.is_missing()) =>
        {
            locate_missing_track(track_id, queue);
        }
        Err(error) => command_controller.report_command_error(&error),
    }
}

pub(crate) fn missing_track_relocation_completed_callback(
    command_controller: &SharedCommandController,
    pending_playbacks: &PendingLocatedPlaybacks,
    playback_changed: PlaybackChangedCallback,
    artwork_loader: &ArtworkLoader,
) -> MissingTrackRelocationCompletedCallback {
    let command_controller = command_controller.clone();
    let pending_playbacks = pending_playbacks.clone();
    let artwork_loader = artwork_loader.clone();
    Rc::new(move |track_id, succeeded| {
        let queue = pending_playbacks.borrow_mut().remove(&track_id);
        if !succeeded {
            return;
        }
        if let Some(source) = artwork_source_for_track(&command_controller.runtime(), track_id) {
            artwork_loader.invalidate(&source);
        }
        let Some(queue) = queue else {
            return;
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::PlayTrack { track_id, queue },
        )) {
            playback_changed();
        }
    })
}

pub(crate) fn locate_missing_track_callback(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    pending_playbacks: &PendingLocatedPlaybacks,
    relocation_completed: &MissingTrackRelocationCompletedCallback,
    artwork_loader: &ArtworkLoader,
) -> LocateMissingTrackCallback {
    let parent = parent.clone();
    let command_controller = command_controller.clone();
    let pending_playbacks = pending_playbacks.clone();
    let relocation_completed = relocation_completed.clone();
    let artwork_loader = artwork_loader.clone();

    Rc::new(move |track_id, queue| {
        pending_playbacks.borrow_mut().insert(track_id, queue);

        let dialog = FileDialog::builder()
            .title("Locate Missing Track")
            .accept_label("Locate")
            .modal(true)
            .build();
        install_audio_filters(&dialog);

        let initial_parent = {
            let runtime = command_controller.runtime();
            let runtime = runtime.borrow();
            runtime
                .library_track(track_id)
                .and_then(|track| runtime.absolute_track_path(track))
                .and_then(|path| path.parent().map(ToOwned::to_owned))
                .filter(|path| path.is_dir())
        };
        if let Some(parent_path) = initial_parent {
            dialog.set_initial_folder(Some(&gio::File::for_path(parent_path)));
        }

        let pending_playbacks_for_open = pending_playbacks.clone();
        let command_controller_for_open = command_controller.clone();
        let relocation_completed_for_open = relocation_completed.clone();
        let artwork_loader_for_open = artwork_loader.clone();
        dialog.open(Some(&parent), None::<&gio::Cancellable>, move |result| {
            let Ok(file) = result else {
                pending_playbacks_for_open.borrow_mut().remove(&track_id);
                return;
            };
            let Some(replacement_path) = file.path() else {
                pending_playbacks_for_open.borrow_mut().remove(&track_id);
                return;
            };
            if let Some(source) =
                artwork_source_for_track(&command_controller_for_open.runtime(), track_id)
            {
                artwork_loader_for_open.invalidate(&source);
            }
            if !command_controller_for_open.dispatch_succeeded(
                ApplicationCommand::RelocateMissingTrack {
                    track_id,
                    replacement_path,
                },
            ) {
                pending_playbacks_for_open.borrow_mut().remove(&track_id);
                return;
            }

            // Without the metadata-writer actor (headless/test builds), the
            // command completes synchronously. Desktop builds keep the row
            // missing until the actor reports completion through its event
            // consumer.
            let relocated = command_controller_for_open
                .runtime()
                .borrow()
                .library_track(track_id)
                .is_some_and(|track| !track.location.is_missing());
            if relocated {
                relocation_completed_for_open(track_id, true);
            }
        });
    })
}

fn artwork_source_for_track(runtime: &SharedRuntime, track_id: TrackId) -> Option<ArtworkSource> {
    let runtime = runtime.borrow();
    let track = runtime.library_track(track_id)?;
    let absolute_path = runtime.absolute_track_path(track)?;
    Some(ArtworkSource::embedded_track(
        track.location.relative_path.to_path_buf(),
        absolute_path,
    ))
}

fn install_audio_filters(dialog: &FileDialog) {
    let filter = FileFilter::new();
    filter.set_name(Some("Audio Files"));
    for pattern in [
        "*.mp3", "*.MP3", "*.ogg", "*.OGG", "*.opus", "*.OPUS", "*.flac", "*.FLAC", "*.m4a",
        "*.M4A", "*.mp4", "*.MP4", "*.wav", "*.WAV", "*.aiff", "*.AIFF",
    ] {
        filter.add_pattern(pattern);
    }
    let filters = gio::ListStore::new::<FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.set_default_filter(Some(&filter));
}
