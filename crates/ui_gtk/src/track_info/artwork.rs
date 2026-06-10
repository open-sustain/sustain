// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{FileDialog, FileFilter, gdk, gio, glib};
use sustain_app_runtime::{ApplicationCommand, TrackId};
use sustain_artwork::{ArtworkReadError, read_artwork_file};

use super::{ARTWORK_PREVIEW_SIZE, COVER_THUMB_SIZE};
use crate::artwork_loader::{ArtworkLoader, ArtworkSource};
use crate::command_controller::SharedCommandController;
use crate::{PlaybackChangedCallback, TrackRowChangedHolder, TrackRowChangedKind};

/// Updates the preview frame, the header thumbnail, the explanatory
/// note, and the Remove button from a (possibly absent) decoded texture
/// plus whether the track actually carries artwork. The texture is
/// always decoded off the GTK thread before this runs (#107).
type ArtworkRefreshCallback = Rc<dyn Fn(Option<gdk::Texture>, bool)>;

#[derive(Clone)]
struct ArtworkChangePublisher {
    track_row_changed_holder: TrackRowChangedHolder,
    playback_changed: PlaybackChangedCallback,
    artwork_loader: ArtworkLoader,
}

impl ArtworkChangePublisher {
    fn publish(
        &self,
        track_id: TrackId,
        artwork_source: Option<ArtworkSource>,
        artwork: Option<Vec<u8>>,
    ) {
        if let Some(source) = artwork_source {
            self.artwork_loader.invalidate(&source);
            match artwork {
                Some(bytes) => self.artwork_loader.prime(source, bytes),
                None => self.artwork_loader.prime_missing(source),
            }
        }
        if let Some(callback) = self.track_row_changed_holder.borrow().as_ref() {
            callback(track_id, TrackRowChangedKind::Artwork);
        }
        (self.playback_changed)();
    }
}

pub(super) struct ArtworkPage {
    pub(super) widget: gtk::Box,
    current_track_id: Rc<Cell<TrackId>>,
    current_artwork_source: Rc<RefCell<Option<ArtworkSource>>>,
    load_generation: Rc<Cell<u64>>,
    refresh: ArtworkRefreshCallback,
    artwork_loader: ArtworkLoader,
}

impl ArtworkPage {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        parent_window: &gtk::Window,
        command_controller: &SharedCommandController,
        track_row_changed_holder: &TrackRowChangedHolder,
        playback_changed: PlaybackChangedCallback,
        track_id: TrackId,
        header_cover: gtk::Frame,
        artwork_loader: &ArtworkLoader,
        artwork_source: Option<ArtworkSource>,
        has_embedded_artwork: bool,
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
            Rc::new(move |texture: Option<gdk::Texture>, has_artwork: bool| {
                set_frame_texture(&frame, texture.as_ref(), ARTWORK_PREVIEW_SIZE);
                set_frame_texture(&header_cover, texture.as_ref(), COVER_THUMB_SIZE);
                note.set_text(if has_artwork {
                    "Artwork is embedded in the audio file."
                } else {
                    "This track has no embedded artwork."
                });
                remove_button.set_sensitive(has_artwork);
            })
        };

        let current_track_id = Rc::new(Cell::new(track_id));
        let current_artwork_source = Rc::new(RefCell::new(None));
        let load_generation = Rc::new(Cell::new(0));
        let artwork_change_publisher = ArtworkChangePublisher {
            track_row_changed_holder: track_row_changed_holder.clone(),
            playback_changed,
            artwork_loader: artwork_loader.clone(),
        };

        {
            let parent_window = parent_window.clone();
            let command_controller = command_controller.clone();
            let artwork_change_publisher = artwork_change_publisher.clone();
            let refresh = refresh.clone();
            let current_track_id = current_track_id.clone();
            let current_artwork_source = current_artwork_source.clone();
            add_button.connect_clicked(move |_| {
                let track_id = current_track_id.get();
                let artwork_source = current_artwork_source.borrow().clone();
                open_artwork_picker(
                    &parent_window,
                    command_controller.clone(),
                    artwork_change_publisher.clone(),
                    track_id,
                    artwork_source,
                    current_track_id.clone(),
                    refresh.clone(),
                );
            });
        }

        {
            let command_controller = command_controller.clone();
            let artwork_change_publisher = artwork_change_publisher.clone();
            let refresh = refresh.clone();
            let current_track_id = current_track_id.clone();
            let current_artwork_source = current_artwork_source.clone();
            remove_button.connect_clicked(move |_| {
                let track_id = current_track_id.get();
                let artwork_source = current_artwork_source.borrow().clone();
                if command_controller.dispatch_succeeded(ApplicationCommand::SetArtwork {
                    track_id,
                    artwork: None,
                }) {
                    artwork_change_publisher.publish(track_id, artwork_source, None);
                    refresh(None, false);
                }
            });
        }

        let page = Self {
            widget: page,
            current_track_id,
            current_artwork_source,
            load_generation,
            refresh,
            artwork_loader: artwork_loader.clone(),
        };
        page.reload(track_id, artwork_source, has_embedded_artwork);
        page
    }

    pub(super) fn reload(
        &self,
        track_id: TrackId,
        artwork_source: Option<ArtworkSource>,
        has_embedded_artwork: bool,
    ) {
        self.current_track_id.set(track_id);
        self.current_artwork_source.replace(artwork_source.clone());
        let generation = self.load_generation.get().wrapping_add(1);
        self.load_generation.set(generation);
        (self.refresh)(None, has_embedded_artwork);
        if let Some(source) = artwork_source {
            let refresh = self.refresh.clone();
            let load_generation = self.load_generation.clone();
            self.artwork_loader.request_detail(
                source,
                Box::new(move |decoded| {
                    if load_generation.get() != generation {
                        return;
                    }
                    let has_artwork = has_embedded_artwork || decoded.detail_texture.is_some();
                    refresh(decoded.detail_texture, has_artwork);
                }),
            );
        }
    }
}

/// Place a pre-decoded texture (or the missing-artwork placeholder) into
/// `frame` at `size`. Does no decoding — the texture is produced off the
/// GTK thread by the shared loader or the picker worker.
pub(super) fn set_frame_texture(frame: &gtk::Frame, texture: Option<&gdk::Texture>, size: i32) {
    frame.set_child(None::<&gtk::Widget>);
    if let Some(texture) = texture {
        let image = gtk::Image::from_paintable(Some(texture));
        image.set_pixel_size(size);
        frame.set_child(Some(&image));
    } else {
        let placeholder = gtk::Image::from_icon_name("image-missing-symbolic");
        let icon_size = if size > 100 { size / 3 } else { size / 2 };
        placeholder.set_pixel_size(icon_size.max(16));
        frame.set_child(Some(&placeholder));
    }
}

/// Decode encoded image bytes into a scaled texture. Runs on a worker
/// thread (pixbuf decode + `gdk::Texture` construction are safe off the
/// main thread — the shared artwork loader relies on the same property).
fn texture_from_bytes(bytes: &[u8], size: i32) -> Option<gdk::Texture> {
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

/// Outcome of the off-thread read+decode of a picker-selected image.
enum PickedArtwork {
    /// Valid bytes (always) plus a decoded preview texture when the
    /// pixbuf decode succeeded. The bytes are still applied even if the
    /// texture is `None` (a format the policy accepts but pixbuf cannot
    /// render) so the file's tag is updated and a placeholder shown.
    Ready {
        bytes: Vec<u8>,
        texture: Option<gdk::Texture>,
    },
    Failed(ArtworkReadError),
}

fn open_artwork_picker(
    parent: &gtk::Window,
    command_controller: SharedCommandController,
    artwork_change_publisher: ArtworkChangePublisher,
    track_id: TrackId,
    artwork_source: Option<ArtworkSource>,
    current_track_id: Rc<Cell<TrackId>>,
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

        // Read and decode the selected image off the GTK thread, then
        // apply the result on the main loop. A large image or a slow
        // mount can make both steps take a while; running them inline
        // would freeze the dialog (#107).
        let (sender, receiver) = async_channel::bounded::<PickedArtwork>(1);
        std::thread::Builder::new()
            .name("sustain-artwork-pick".to_owned())
            .spawn(move || {
                let outcome = match read_artwork_file(&path) {
                    Ok(bytes) => {
                        let texture = texture_from_bytes(&bytes, ARTWORK_PREVIEW_SIZE);
                        PickedArtwork::Ready { bytes, texture }
                    }
                    Err(error) => PickedArtwork::Failed(error),
                };
                // The receiver lives on the main loop for the dialog's
                // lifetime; a send failure just means it closed first.
                let _ = sender.send_blocking(outcome);
            })
            .expect("spawn artwork picker worker thread");

        glib::MainContext::default().spawn_local(async move {
            let Ok(outcome) = receiver.recv().await else {
                return;
            };
            match outcome {
                PickedArtwork::Ready { bytes, texture } => {
                    let cache_bytes = bytes.clone();
                    if command_controller.dispatch_succeeded(ApplicationCommand::SetArtwork {
                        track_id,
                        artwork: Some(bytes),
                    }) {
                        artwork_change_publisher.publish(
                            track_id,
                            artwork_source,
                            Some(cache_bytes),
                        );
                        if current_track_id.get() == track_id {
                            refresh(texture, true);
                        }
                    }
                }
                PickedArtwork::Failed(error) => {
                    eprintln!("sustain: failed to read selected artwork: {error}");
                    command_controller.report_artwork_selection_error(&error);
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::{set_frame_texture, texture_from_bytes};
    use crate::test_support::with_gtk;
    use gtk::prelude::*;

    /// A minimal valid 1×1 PNG, shared with the artwork policy tests.
    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn picker_decode_rejects_unparseable_bytes() {
        // The picker worker decodes off the GTK thread; garbage must
        // degrade to no texture (the file's bytes are still applied and a
        // placeholder shown) rather than producing a bogus image.
        if !with_gtk(|| {
            assert!(texture_from_bytes(b"definitely not an image", 320).is_none());
            assert!(texture_from_bytes(VALID_PNG, 320).is_some());
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }

    #[test]
    fn frame_shows_placeholder_without_texture_and_image_with_one() {
        if !with_gtk(|| {
            let frame = gtk::Frame::new(None);

            set_frame_texture(&frame, None, 96);
            let placeholder = frame.child().expect("placeholder child present");
            assert!(
                placeholder.downcast::<gtk::Image>().is_ok(),
                "absent artwork renders the missing-image placeholder"
            );

            let texture = texture_from_bytes(VALID_PNG, 96).expect("valid png decodes");
            set_frame_texture(&frame, Some(&texture), 96);
            let cover = frame.child().expect("cover child present");
            assert!(
                cover.downcast::<gtk::Image>().is_ok(),
                "present artwork renders an image"
            );
        }) {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
