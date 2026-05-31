// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{FileDialog, FileFilter, gdk, gio, glib};
use sustain_app_runtime::{ApplicationCommand, TrackId};
use sustain_artwork::read_artwork_file;

use super::{ARTWORK_PREVIEW_SIZE, COVER_THUMB_SIZE, LibraryChangedHolder};
use crate::command_controller::SharedCommandController;

type ArtworkRefreshCallback = Rc<dyn Fn(Option<&[u8]>)>;

pub(super) struct ArtworkPage {
    pub(super) widget: gtk::Box,
}

impl ArtworkPage {
    pub(super) fn new(
        parent_window: &gtk::Window,
        command_controller: &SharedCommandController,
        library_changed_holder: &LibraryChangedHolder,
        track_id: TrackId,
        header_cover: gtk::Frame,
        initial_bytes: Option<&[u8]>,
    ) -> Self {
        let page = gtk::Box::new(gtk::Orientation::Vertical, 6);
        page.add_css_class("track-info-artwork");
        page.set_margin_top(10);
        page.set_halign(gtk::Align::Center);

        let frame = gtk::Frame::new(None);
        frame.add_css_class("track-info-artwork-frame");
        frame.set_size_request(ARTWORK_PREVIEW_SIZE, ARTWORK_PREVIEW_SIZE);
        page.append(&frame);

        let note = gtk::Label::new(None);
        note.add_css_class("dim-label");
        note.set_margin_top(4);
        page.append(&note);

        let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        buttons.set_halign(gtk::Align::Center);
        buttons.set_margin_top(12);
        let add_button = gtk::Button::with_label("Add Artwork\u{2026}");
        let remove_button = gtk::Button::with_label("Remove Artwork");
        remove_button.add_css_class("destructive-action");
        buttons.append(&add_button);
        buttons.append(&remove_button);
        page.append(&buttons);

        let refresh: ArtworkRefreshCallback = {
            let frame = frame.clone();
            let header_cover = header_cover.clone();
            let note = note.clone();
            let remove_button = remove_button.clone();
            Rc::new(move |bytes: Option<&[u8]>| {
                update_artwork_frame(&frame, bytes, ARTWORK_PREVIEW_SIZE);
                update_artwork_frame(&header_cover, bytes, COVER_THUMB_SIZE);
                note.set_text(if bytes.is_some() {
                    "Artwork is embedded in the audio file."
                } else {
                    "This track has no embedded artwork."
                });
                remove_button.set_sensitive(bytes.is_some());
            })
        };
        refresh(initial_bytes);

        {
            let parent_window = parent_window.clone();
            let command_controller = command_controller.clone();
            let library_changed_holder = library_changed_holder.clone();
            let refresh = refresh.clone();
            add_button.connect_clicked(move |_| {
                open_artwork_picker(
                    &parent_window,
                    command_controller.clone(),
                    library_changed_holder.clone(),
                    track_id,
                    refresh.clone(),
                );
            });
        }

        {
            let command_controller = command_controller.clone();
            let library_changed_holder = library_changed_holder.clone();
            let refresh = refresh.clone();
            remove_button.connect_clicked(move |_| {
                if command_controller.dispatch_succeeded(ApplicationCommand::SetArtwork {
                    track_id,
                    artwork: None,
                }) {
                    refresh(None);
                    if let Some(callback) = library_changed_holder.borrow().as_ref() {
                        callback();
                    }
                }
            });
        }

        Self { widget: page }
    }
}

pub(super) fn update_artwork_frame(frame: &gtk::Frame, bytes: Option<&[u8]>, size: i32) {
    frame.set_child(None::<&gtk::Widget>);
    if let Some(texture) = bytes.and_then(|bytes| artwork_texture_from_slice(bytes, size)) {
        let image = gtk::Image::from_paintable(Some(&texture));
        image.set_pixel_size(size);
        frame.set_child(Some(&image));
    } else {
        let placeholder = gtk::Image::from_icon_name("image-missing-symbolic");
        let icon_size = if size > 100 { size / 3 } else { size / 2 };
        placeholder.set_pixel_size(icon_size.max(16));
        frame.set_child(Some(&placeholder));
    }
}

fn artwork_texture_from_slice(bytes: &[u8], size: i32) -> Option<gdk::Texture> {
    sustain_artwork::validate_encoded_artwork(bytes).ok()?;
    let bytes = glib::Bytes::from_owned(bytes.to_vec());
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    let pixbuf = gtk::gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        size,
        size,
        true,
        None::<&gio::Cancellable>,
    )
    .ok()?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

fn open_artwork_picker(
    parent: &gtk::Window,
    command_controller: SharedCommandController,
    library_changed_holder: LibraryChangedHolder,
    track_id: TrackId,
    refresh: ArtworkRefreshCallback,
) {
    let dialog = FileDialog::builder()
        .title("Choose Artwork")
        .modal(true)
        .build();

    let filter = FileFilter::new();
    filter.set_name(Some("Images"));
    filter.add_mime_type("image/png");
    filter.add_mime_type("image/jpeg");
    filter.add_mime_type("image/webp");
    filter.add_mime_type("image/gif");
    let filters = gio::ListStore::new::<FileFilter>();
    filters.append(&filter);
    dialog.set_filters(Some(&filters));
    dialog.set_default_filter(Some(&filter));

    dialog.open(Some(parent), None::<&gio::Cancellable>, move |result| {
        let Ok(file) = result else {
            return;
        };
        let Some(path) = file.path() else {
            return;
        };
        let bytes = match read_artwork_file(&path) {
            Ok(bytes) => bytes,
            Err(error) => {
                eprintln!(
                    "sustain: failed to read artwork file {}: {error}",
                    path.display()
                );
                command_controller.report_artwork_selection_error(&error);
                return;
            }
        };
        if command_controller.dispatch_succeeded(ApplicationCommand::SetArtwork {
            track_id,
            artwork: Some(bytes.clone()),
        }) {
            refresh(Some(&bytes));
            if let Some(callback) = library_changed_holder.borrow().as_ref() {
                callback();
            }
        }
    });
}
