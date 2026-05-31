// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::prelude::*;
use gtk::{FileLauncher, gdk, gio};
use sustain_app_runtime::{ApplicationCommand, PlaybackCommand, TrackId};

use super::{
    LibraryChangedHolder, SharedRuntime, ShowAlbumHolder, TrackRowChangedHolder,
    artwork_loader::ArtworkLoader,
    command_controller::SharedCommandController,
    track_context::{TrackActionCallback, TrackActionVisibility},
    track_info::open_track_info_dialog,
};

pub(crate) fn copy_files_callback(
    runtime: &SharedRuntime,
    window: &gtk::Window,
) -> TrackActionCallback {
    let runtime = runtime.clone();
    let window = window.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        let paths = absolute_paths_for_tracks(&runtime, &track_ids);
        if paths.is_empty() {
            return;
        }
        copy_paths_to_clipboard(&window, &paths);
    })
}

pub(crate) fn get_info_callback(
    parent_window: &gtk::Window,
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
    track_row_changed_holder: &TrackRowChangedHolder,
    artwork_loader: &ArtworkLoader,
) -> TrackActionCallback {
    let parent_window = parent_window.clone();
    let runtime = runtime.clone();
    let command_controller = command_controller.clone();
    let library_changed_holder = library_changed_holder.clone();
    let track_row_changed_holder = track_row_changed_holder.clone();
    let artwork_loader = artwork_loader.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        let Some(&track_id) = track_ids.first() else {
            return;
        };
        open_track_info_dialog(
            &parent_window,
            &runtime,
            &command_controller,
            &library_changed_holder,
            &track_row_changed_holder,
            &artwork_loader,
            track_id,
        );
    })
}

pub(crate) fn play_next_callback(
    command_controller: &SharedCommandController,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        if track_ids.is_empty() {
            return;
        }
        let _result = command_controller.dispatch(ApplicationCommand::Playback(
            PlaybackCommand::EnqueueNext(track_ids),
        ));
    })
}

pub(crate) fn add_to_queue_callback(
    command_controller: &SharedCommandController,
) -> TrackActionCallback {
    let command_controller = command_controller.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        if track_ids.is_empty() {
            return;
        }
        let _result = command_controller.dispatch(ApplicationCommand::Playback(
            PlaybackCommand::EnqueueLast(track_ids),
        ));
    })
}

pub(crate) fn playback_has_current_track_visibility(
    runtime: &SharedRuntime,
) -> TrackActionVisibility {
    let runtime = runtime.clone();
    Rc::new(move |_track_ids: &[TrackId]| {
        runtime.borrow().playback_queue_current_track_id().is_some()
    })
}

pub(crate) fn show_album_callback(holder: &ShowAlbumHolder) -> TrackActionCallback {
    let holder = holder.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        let Some(&track_id) = track_ids.first() else {
            return;
        };
        let action = holder.borrow().clone();
        if let Some(action) = action {
            action(track_id);
        }
    })
}

pub(crate) fn track_has_album_visibility(runtime: &SharedRuntime) -> TrackActionVisibility {
    let runtime = runtime.clone();
    Rc::new(move |track_ids: &[TrackId]| {
        let Some(&track_id) = track_ids.first() else {
            return false;
        };
        let runtime_borrow = runtime.borrow();
        runtime_borrow
            .library_tracks()
            .iter()
            .find(|track| track.id == track_id)
            .and_then(|track| track.metadata.album.as_deref())
            .map(|album| !album.trim().is_empty())
            .unwrap_or(false)
    })
}

pub(crate) fn show_in_folder_callback(
    runtime: &SharedRuntime,
    window: &gtk::Window,
) -> TrackActionCallback {
    let runtime = runtime.clone();
    let window = window.clone();
    Rc::new(move |track_ids: Vec<TrackId>| {
        let Some(path) = absolute_paths_for_tracks(&runtime, &track_ids)
            .into_iter()
            .next()
        else {
            return;
        };
        show_path_in_folder(&window, &path);
    })
}

fn absolute_paths_for_tracks(runtime: &SharedRuntime, track_ids: &[TrackId]) -> Vec<PathBuf> {
    let runtime_borrow = runtime.borrow();
    let tracks = runtime_borrow.library_tracks();
    track_ids
        .iter()
        .filter_map(|track_id| {
            tracks
                .iter()
                .find(|track| track.id == *track_id)
                .and_then(|track| runtime_borrow.absolute_track_path(track))
        })
        .collect()
}

fn copy_paths_to_clipboard(window: &gtk::Window, paths: &[PathBuf]) {
    let files: Vec<gio::File> = paths.iter().map(gio::File::for_path).collect();
    let file_list = gdk::FileList::from_array(&files);
    let provider = gdk::ContentProvider::for_value(&file_list.to_value());
    if let Err(error) = window.clipboard().set_content(Some(&provider)) {
        eprintln!("sustain: clipboard set failed: {error}");
    }
}

fn show_path_in_folder(window: &gtk::Window, path: &Path) {
    // @TODO codex: confirm gtk::FileLauncher::open_containing_folder is the
    // right primitive here. Internally it calls org.freedesktop.FileManager1
    // ShowItems over D-Bus and falls back to opening the parent directory if
    // that fails — which is the behaviour we want — but worth a second pair
    // of eyes on the choice vs. calling D-Bus directly.
    let file = gio::File::for_path(path);
    let launcher = FileLauncher::new(Some(&file));
    launcher.open_containing_folder(Some(window), None::<&gio::Cancellable>, |result| {
        if let Err(error) = result {
            eprintln!("sustain: open containing folder failed: {error}");
        }
    });
}
