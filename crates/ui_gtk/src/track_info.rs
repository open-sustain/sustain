// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{ApplicationCommand, MetadataChange, Track, TrackId};

use super::{
    LibraryChangedHolder, SharedRuntime, TrackRowChangedHolder,
    artwork_loader::{ArtworkLoader, ArtworkSource},
    command_controller::SharedCommandController,
};

mod artwork;
mod details;
mod file_page;
mod form;
mod format;
mod lyrics;

use artwork::{ArtworkPage, set_frame_texture};
use details::DetailsPage;
use file_page::build_file_page;
use lyrics::LyricsPage;

const DIALOG_WIDTH: i32 = 540;
const COVER_THUMB_SIZE: i32 = 96;
const ARTWORK_PREVIEW_SIZE: i32 = 320;
const DIALOG_SIDE_MARGIN: i32 = 18;
const HEADER_GAP_BELOW: i32 = 14;
const NUMBER_ENTRY_WIDTH_CHARS: i32 = 5;
const PAIR_ENTRY_WIDTH_CHARS: i32 = 4;

#[allow(clippy::too_many_arguments)]
pub(crate) fn open_track_info_dialog(
    parent: &gtk::Window,
    runtime: &SharedRuntime,
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
    track_row_changed_holder: &TrackRowChangedHolder,
    artwork_loader: &ArtworkLoader,
    track_id: TrackId,
) {
    let (track, absolute_path) = {
        let runtime_borrow = runtime.borrow();
        let Some(track) = runtime_borrow
            .library_tracks()
            .iter()
            .find(|track| track.id == track_id)
            .cloned()
        else {
            return;
        };
        let absolute_path = runtime_borrow.absolute_track_path(&track);
        (track, absolute_path)
    };

    // Resolve the embedded-artwork source the shared loader reads. The
    // disk cache is keyed by the library-relative path (so it survives
    // library-root moves and shares rows with Albums / now-playing); the
    // worker needs the absolute path to read the file. No artwork bytes
    // are read here — the loader does that off the GTK thread (#107).
    let artwork_source = absolute_path.as_ref().map(|absolute| {
        ArtworkSource::embedded_track(track.location.path().to_path_buf(), absolute.clone())
    });
    let has_embedded_artwork = track.has_embedded_artwork == Some(true);

    let initial_metadata = track.metadata.clone();
    let initial_rating = track.rating;
    let initial_statistics = track.statistics.clone();

    let window = gtk::Window::builder()
        .title("Get Info")
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(DIALOG_WIDTH)
        .build();
    window.add_css_class("track-info-window");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_bottom(16);

    let header = build_header(&track);
    outer.append(&header.widget);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.set_hexpand(true);
    stack.set_vhomogeneous(false);
    stack.set_margin_top(12);
    stack.set_margin_start(DIALOG_SIDE_MARGIN);
    stack.set_margin_end(DIALOG_SIDE_MARGIN);

    let details = DetailsPage::new(&initial_metadata, initial_rating, &initial_statistics);
    stack.add_titled(&details.widget, Some("details"), "Details");

    let artwork = ArtworkPage::new(
        parent,
        command_controller,
        library_changed_holder,
        track_id,
        header.cover_frame.clone(),
        artwork_loader,
        artwork_source,
        has_embedded_artwork,
    );
    stack.add_titled(&artwork.widget, Some("artwork"), "Artwork");

    let lyrics = LyricsPage::new(&initial_metadata);
    stack.add_titled(&lyrics.widget, Some("lyrics"), "Lyrics");

    let file_page = build_file_page(&track, absolute_path.as_deref());
    stack.add_titled(&file_page, Some("file"), "File");

    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_halign(gtk::Align::Center);
    switcher.set_margin_top(HEADER_GAP_BELOW);
    switcher.set_margin_start(DIALOG_SIDE_MARGIN);
    switcher.set_margin_end(DIALOG_SIDE_MARGIN);
    outer.append(&switcher);
    outer.append(&stack);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    buttons.set_margin_top(14);
    buttons.set_margin_start(DIALOG_SIDE_MARGIN);
    buttons.set_margin_end(DIALOG_SIDE_MARGIN);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    outer.append(&buttons);

    window.set_child(Some(&outer));

    // Make OK the dialog's default action so Enter in any single-line
    // field commits the edits (issue #57). The entries opt in via
    // `set_activates_default` in their form factories.
    window.set_default_widget(Some(&ok));

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| {
        window_for_cancel.close();
    });

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    let command_controller = command_controller.clone();
    let library_changed_holder = library_changed_holder.clone();
    let track_row_changed_holder = track_row_changed_holder.clone();
    let window_for_ok = window.clone();
    let details_for_ok = details.clone();
    let lyrics_for_ok = lyrics.clone();
    ok.connect_clicked(move |_| {
        let mut change = details_for_ok.metadata_diff(&initial_metadata);
        change.lyrics = lyrics_for_ok.lyrics_diff(&initial_metadata);
        let new_rating = details_for_ok.current_rating();
        let reset_clicked = details_for_ok.play_count_reset_requested();

        let mut attempted = false;
        let mut any_succeeded = false;
        let mut any_failed = false;
        if change != MetadataChange::default() {
            attempted = true;
            match command_controller.dispatch(ApplicationCommand::UpdateMetadata {
                track_id,
                change: Box::new(change),
            }) {
                Ok(()) => any_succeeded = true,
                Err(_) => any_failed = true,
            }
        }
        if new_rating != initial_rating {
            attempted = true;
            match command_controller.dispatch(ApplicationCommand::SetRating {
                track_id,
                rating: new_rating,
            }) {
                Ok(()) => any_succeeded = true,
                Err(_) => any_failed = true,
            }
        }
        if reset_clicked {
            attempted = true;
            match command_controller.dispatch(ApplicationCommand::ResetPlayCount { track_id }) {
                Ok(()) => any_succeeded = true,
                Err(_) => any_failed = true,
            }
        }
        if any_succeeded {
            if let Some(callback) = track_row_changed_holder.borrow().as_ref() {
                callback(track_id);
            } else if let Some(callback) = library_changed_holder.borrow().as_ref() {
                callback();
            }
        }
        if !attempted || !any_failed {
            window_for_ok.close();
        }
    });

    window.present();
}

struct Header {
    widget: gtk::Box,
    cover_frame: gtk::Frame,
}

fn build_header(track: &Track) -> Header {
    let row = gtk::Box::new(gtk::Orientation::Horizontal, 14);
    row.add_css_class("track-info-header");
    row.set_hexpand(true);

    let cover_frame = gtk::Frame::new(None);
    cover_frame.add_css_class("track-info-cover");
    cover_frame.set_size_request(COVER_THUMB_SIZE, COVER_THUMB_SIZE);
    cover_frame.set_valign(gtk::Align::Center);
    // Starts as a placeholder; the decoded thumbnail is filled in by the
    // Artwork page's shared-loader request once the cover decodes (#107).
    set_frame_texture(&cover_frame, None, COVER_THUMB_SIZE);
    row.append(&cover_frame);

    let info = gtk::Box::new(gtk::Orientation::Vertical, 2);
    info.set_valign(gtk::Align::Center);
    info.set_hexpand(true);

    let title = gtk::Label::new(Some(track.metadata.title.as_deref().unwrap_or("Untitled")));
    title.add_css_class("track-info-title");
    title.set_xalign(0.0);
    title.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&title);

    let artist = gtk::Label::new(Some(
        track.metadata.artist.as_deref().unwrap_or("Unknown Artist"),
    ));
    artist.add_css_class("track-info-subtitle");
    artist.set_xalign(0.0);
    artist.set_ellipsize(gtk::pango::EllipsizeMode::End);
    info.append(&artist);

    if let Some(album) = track.metadata.album.as_deref() {
        let album_label = gtk::Label::new(Some(album));
        album_label.add_css_class("track-info-subtitle");
        album_label.set_xalign(0.0);
        album_label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        info.append(&album_label);
    }

    row.append(&info);
    Header {
        widget: row,
        cover_frame,
    }
}
