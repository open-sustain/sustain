// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::RefCell, collections::HashSet, path::PathBuf, rc::Rc};

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
mod multi;

use artwork::{ArtworkPage, set_frame_texture};
use details::DetailsPage;
use file_page::FilePage;
use lyrics::LyricsPage;
pub(crate) use multi::open_multi_track_info_dialog;

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
    ordered_track_ids: Vec<TrackId>,
    start_index: usize,
) {
    let Some(track_id) = ordered_track_ids.get(start_index).copied() else {
        return;
    };
    let Some(initial) = load_track(runtime, track_id) else {
        return;
    };
    let cursor = Rc::new(RefCell::new(DialogCursor::new(
        ordered_track_ids,
        start_index,
        initial.track.clone(),
    )));

    let window = gtk::Window::builder()
        .title(format!("Get Info - {}", track_display_name(&initial.track)))
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(DIALOG_WIDTH)
        .build();
    window.add_css_class("track-info-window");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 0);
    outer.set_margin_bottom(16);

    let header = build_header(&initial.track);
    outer.append(&header.widget);

    let stack = gtk::Stack::new();
    stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    stack.set_transition_duration(120);
    stack.set_hexpand(true);
    stack.set_vhomogeneous(false);
    stack.set_margin_top(12);
    stack.set_margin_start(DIALOG_SIDE_MARGIN);
    stack.set_margin_end(DIALOG_SIDE_MARGIN);

    let details = DetailsPage::new(
        &initial.track.metadata,
        initial.track.rating,
        &initial.track.statistics,
        runtime.borrow().distinct_genres(),
    );
    stack.add_titled(&details.widget, Some("details"), "Details");

    let artwork = ArtworkPage::new(
        parent,
        command_controller,
        library_changed_holder,
        initial.track.id,
        header.cover_frame.clone(),
        artwork_loader,
        initial.artwork_source.clone(),
        initial.has_embedded_artwork,
    );
    stack.add_titled(&artwork.widget, Some("artwork"), "Artwork");

    let lyrics = LyricsPage::new(&initial.track.metadata);
    stack.add_titled(&lyrics.widget, Some("lyrics"), "Lyrics");

    let file_page = FilePage::new(&initial.track, initial.absolute_path.as_deref());
    stack.add_titled(&file_page.widget, Some("file"), "File");

    let switcher = gtk::StackSwitcher::new();
    switcher.set_stack(Some(&stack));
    switcher.set_halign(gtk::Align::Center);
    switcher.set_margin_top(HEADER_GAP_BELOW);
    switcher.set_margin_start(DIALOG_SIDE_MARGIN);
    switcher.set_margin_end(DIALOG_SIDE_MARGIN);
    outer.append(&switcher);
    outer.append(&stack);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::Fill);
    buttons.set_margin_top(14);
    buttons.set_margin_start(DIALOG_SIDE_MARGIN);
    buttons.set_margin_end(DIALOG_SIDE_MARGIN);
    let previous = navigation_button("go-previous-symbolic", "Previous track (Ctrl+[)");
    let next = navigation_button("go-next-symbolic", "Next track (Ctrl+])");
    buttons.append(&previous);
    buttons.append(&next);
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_hexpand(true);
    buttons.append(&spacer);
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

    let pages = Rc::new(DialogPages {
        header,
        details,
        artwork,
        lyrics,
        file: file_page,
    });
    refresh_navigation_buttons(runtime, &cursor, &previous, &next);

    let navigate = {
        let window = window.clone();
        let runtime = runtime.clone();
        let cursor = cursor.clone();
        let pages = pages.clone();
        let command_controller = command_controller.clone();
        let library_changed_holder = library_changed_holder.clone();
        let track_row_changed_holder = track_row_changed_holder.clone();
        let previous = previous.clone();
        let next = next.clone();
        Rc::new(move |direction| {
            if !commit_current(
                &cursor,
                &pages,
                &command_controller,
                &library_changed_holder,
                &track_row_changed_holder,
            ) {
                return;
            }
            let Some(index) = find_valid_index(&runtime, &cursor.borrow(), direction) else {
                refresh_navigation_buttons(&runtime, &cursor, &previous, &next);
                return;
            };
            let track_id = cursor.borrow().ordered_ids[index];
            let Some(loaded) = load_track(&runtime, track_id) else {
                return;
            };
            reload_dialog(&window, &cursor, &pages, index, loaded);
            refresh_navigation_buttons(&runtime, &cursor, &previous, &next);
        })
    };
    let navigate_previous = navigate.clone();
    previous.connect_clicked(move |_| navigate_previous(Direction::Previous));
    next.connect_clicked(move |_| navigate(Direction::Next));

    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    let previous_for_key = previous.clone();
    let next_for_key = next.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else if state.contains(gdk::ModifierType::CONTROL_MASK) && key == gdk::Key::bracketleft {
            if previous_for_key.is_sensitive() {
                previous_for_key.emit_clicked();
            }
            glib::Propagation::Stop
        } else if state.contains(gdk::ModifierType::CONTROL_MASK) && key == gdk::Key::bracketright {
            if next_for_key.is_sensitive() {
                next_for_key.emit_clicked();
            }
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
    let cursor_for_ok = cursor.clone();
    let pages_for_ok = pages.clone();
    ok.connect_clicked(move |_| {
        if commit_current(
            &cursor_for_ok,
            &pages_for_ok,
            &command_controller,
            &library_changed_holder,
            &track_row_changed_holder,
        ) {
            window_for_ok.close();
        }
    });

    window.present();
}

struct DialogCursor {
    ordered_ids: Vec<TrackId>,
    index: usize,
    current_track: Track,
    baseline_metadata: sustain_app_runtime::TrackMetadata,
    baseline_rating: sustain_app_runtime::Rating,
    baseline_play_count: u64,
}

impl DialogCursor {
    fn new(ordered_ids: Vec<TrackId>, index: usize, track: Track) -> Self {
        let mut cursor = Self {
            ordered_ids,
            index,
            current_track: track.clone(),
            baseline_metadata: track.metadata.clone(),
            baseline_rating: track.rating,
            baseline_play_count: track.statistics.play_count,
        };
        cursor.replace_current(index, track);
        cursor
    }

    fn replace_current(&mut self, index: usize, track: Track) {
        self.index = index;
        self.baseline_metadata = track.metadata.clone();
        self.baseline_rating = track.rating;
        self.baseline_play_count = track.statistics.play_count;
        self.current_track = track;
    }
}

struct LoadedTrack {
    track: Track,
    absolute_path: Option<PathBuf>,
    artwork_source: Option<ArtworkSource>,
    has_embedded_artwork: bool,
}

struct DialogPages {
    header: Header,
    details: DetailsPage,
    artwork: ArtworkPage,
    lyrics: LyricsPage,
    file: FilePage,
}

#[derive(Clone, Copy)]
enum Direction {
    Previous,
    Next,
}

fn load_track(runtime: &SharedRuntime, track_id: TrackId) -> Option<LoadedTrack> {
    let runtime = runtime.borrow();
    let track = runtime
        .library_tracks()
        .iter()
        .find(|track| track.id == track_id)?
        .clone();
    let absolute_path = runtime.absolute_track_path(&track);
    let artwork_source = absolute_path.as_ref().map(|absolute| {
        ArtworkSource::embedded_track(track.location.path().to_path_buf(), absolute.clone())
    });
    let has_embedded_artwork = track.has_embedded_artwork == Some(true);
    Some(LoadedTrack {
        track,
        absolute_path,
        artwork_source,
        has_embedded_artwork,
    })
}

fn reload_dialog(
    window: &gtk::Window,
    cursor: &Rc<RefCell<DialogCursor>>,
    pages: &DialogPages,
    index: usize,
    loaded: LoadedTrack,
) {
    cursor
        .borrow_mut()
        .replace_current(index, loaded.track.clone());
    window.set_title(Some(&format!(
        "Get Info - {}",
        track_display_name(&loaded.track)
    )));
    pages.header.reload(&loaded.track);
    pages.details.reload(
        &loaded.track.metadata,
        loaded.track.rating,
        &loaded.track.statistics,
    );
    pages.artwork.reload(
        loaded.track.id,
        loaded.artwork_source,
        loaded.has_embedded_artwork,
    );
    pages.lyrics.reload(&loaded.track.metadata);
    pages
        .file
        .reload(&loaded.track, loaded.absolute_path.as_deref());
}

fn commit_current(
    cursor: &Rc<RefCell<DialogCursor>>,
    pages: &DialogPages,
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
    track_row_changed_holder: &TrackRowChangedHolder,
) -> bool {
    let cursor = cursor.borrow();
    let track_id = cursor.current_track.id;
    let mut change = pages.details.metadata_diff(&cursor.baseline_metadata);
    change.lyrics = pages.lyrics.lyrics_diff(&cursor.baseline_metadata);
    let new_rating = pages.details.current_rating();
    let reset_clicked = pages.details.play_count_reset_requested();

    let mut any_succeeded = false;
    let mut any_failed = false;
    if change != MetadataChange::default() {
        match command_controller.dispatch(ApplicationCommand::UpdateMetadata {
            track_id,
            change: Box::new(change),
        }) {
            Ok(()) => any_succeeded = true,
            Err(_) => any_failed = true,
        }
    }
    if new_rating != cursor.baseline_rating {
        match command_controller.dispatch(ApplicationCommand::SetRating {
            track_id,
            rating: new_rating,
        }) {
            Ok(()) => any_succeeded = true,
            Err(_) => any_failed = true,
        }
    }
    if reset_clicked && cursor.baseline_play_count > 0 {
        match command_controller.dispatch(ApplicationCommand::ResetPlayCount { track_id }) {
            Ok(()) => any_succeeded = true,
            Err(_) => any_failed = true,
        }
    }
    drop(cursor);

    if any_succeeded {
        if let Some(callback) = track_row_changed_holder.borrow().as_ref() {
            callback(track_id);
        } else if let Some(callback) = library_changed_holder.borrow().as_ref() {
            callback();
        }
    }
    !any_failed
}

fn refresh_navigation_buttons(
    runtime: &SharedRuntime,
    cursor: &Rc<RefCell<DialogCursor>>,
    previous: &gtk::Button,
    next: &gtk::Button,
) {
    let cursor = cursor.borrow();
    previous.set_sensitive(find_valid_index(runtime, &cursor, Direction::Previous).is_some());
    next.set_sensitive(find_valid_index(runtime, &cursor, Direction::Next).is_some());
}

fn find_valid_index(
    runtime: &SharedRuntime,
    cursor: &DialogCursor,
    direction: Direction,
) -> Option<usize> {
    let runtime = runtime.borrow();
    let current_ids: HashSet<TrackId> = runtime
        .library_tracks()
        .iter()
        .map(|track| track.id)
        .collect();
    find_valid_index_by(cursor, direction, |track_id| {
        current_ids.contains(&track_id)
    })
}

fn find_valid_index_by(
    cursor: &DialogCursor,
    direction: Direction,
    contains: impl Fn(TrackId) -> bool,
) -> Option<usize> {
    let mut index = cursor.index;
    loop {
        index = match direction {
            Direction::Previous => index.checked_sub(1)?,
            Direction::Next => index
                .checked_add(1)
                .filter(|index| *index < cursor.ordered_ids.len())?,
        };
        let track_id = cursor.ordered_ids[index];
        if contains(track_id) {
            return Some(index);
        }
    }
}

fn navigation_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon_name);
    button.set_tooltip_text(Some(tooltip));
    button
}

fn track_display_name(track: &Track) -> String {
    track
        .metadata
        .title
        .as_deref()
        .filter(|title| !title.trim().is_empty())
        .map(str::to_owned)
        .or_else(|| {
            track
                .location
                .path()
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "Untitled".to_owned())
}

struct Header {
    widget: gtk::Box,
    cover_frame: gtk::Frame,
    title: gtk::Label,
    artist: gtk::Label,
    album: gtk::Label,
}

impl Header {
    fn reload(&self, track: &Track) {
        self.title
            .set_text(track.metadata.title.as_deref().unwrap_or("Untitled"));
        self.artist
            .set_text(track.metadata.artist.as_deref().unwrap_or("Unknown Artist"));
        self.album
            .set_text(track.metadata.album.as_deref().unwrap_or_default());
        self.album.set_visible(track.metadata.album.is_some());
    }
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

    let album = gtk::Label::new(track.metadata.album.as_deref());
    album.add_css_class("track-info-subtitle");
    album.set_xalign(0.0);
    album.set_ellipsize(gtk::pango::EllipsizeMode::End);
    album.set_visible(track.metadata.album.is_some());
    info.append(&album);

    row.append(&info);
    Header {
        widget: row,
        cover_frame,
        title,
        artist,
        album,
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use sustain_app_runtime::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::{DialogCursor, Direction, find_valid_index_by};

    #[test]
    fn advancing_cursor_re_snapshots_the_new_track_baseline() {
        let first = track(1, "First", 2, 1);
        let second = track(2, "Second", 4, 3);
        let mut cursor = DialogCursor::new(vec![first.id, second.id], 0, first);

        cursor.replace_current(1, second.clone());

        assert_eq!(cursor.index, 1);
        assert_eq!(cursor.current_track.id, second.id);
        assert_eq!(cursor.baseline_metadata, second.metadata);
        assert_eq!(cursor.baseline_rating, second.rating);
        assert_eq!(cursor.baseline_play_count, 4);
    }

    #[test]
    fn navigation_skips_tracks_removed_after_dialog_open() {
        let first = track(1, "First", 0, 0);
        let removed = track(2, "Removed", 0, 0);
        let third = track(3, "Third", 0, 0);
        let cursor = DialogCursor::new(vec![first.id, removed.id, third.id], 0, first);

        let target = find_valid_index_by(&cursor, Direction::Next, |track_id| track_id == third.id);

        assert_eq!(target, Some(2));
    }

    fn track(id: i64, title: &str, play_count: u64, rating: u8) -> Track {
        let metadata = TrackMetadata {
            title: Some(title.to_owned()),
            ..TrackMetadata::default()
        };
        Track {
            id: TrackId::new(id).expect("positive track id"),
            location: TrackLocation::available(
                TrackRelativePath::new(PathBuf::from(format!("{title}.flac")))
                    .expect("valid relative path"),
            ),
            metadata,
            rating: Rating::new(rating).expect("rating in range"),
            statistics: PlayStatistics {
                play_count,
                ..PlayStatistics::default()
            },
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }
}
