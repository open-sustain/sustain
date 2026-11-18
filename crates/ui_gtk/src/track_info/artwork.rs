// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    io,
    path::PathBuf,
    rc::Rc,
};

use gtk::prelude::*;
use gtk::{FileDialog, FileFilter, gdk, gio, glib};
use sustain_app_runtime::{ApplicationCommand, TrackId};
use sustain_artwork::{
    ArtworkReadError, MAX_ENCODED_ARTWORK_BYTES, read_artwork_file, validate_encoded_artwork,
};
use sustain_i18n::gettext;

use super::{ARTWORK_PREVIEW_SIZE, COVER_THUMB_SIZE};
use crate::artwork_loader::{ArtworkLoader, ArtworkSource};
use crate::command_controller::SharedCommandController;
use crate::{ArtworkChangedHolder, PlaybackChangedCallback};

/// Updates the preview frame, the header thumbnail, the explanatory
/// note, and the Remove button from a (possibly absent) decoded texture
/// plus whether the track actually carries artwork. The texture is
/// always decoded off the GTK thread before this runs (#107).
type ArtworkRefreshCallback = Rc<dyn Fn(Option<gdk::Texture>, bool)>;

#[derive(Clone)]
struct ArtworkChangePublisher {
    artwork_changed_holder: ArtworkChangedHolder,
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
            match artwork {
                Some(bytes) => self.artwork_loader.prime(source, bytes),
                None => self.artwork_loader.prime_missing(source),
            }
        }
        if let Some(callback) = self.artwork_changed_holder.borrow().as_ref() {
            callback(&[track_id]);
        }
        (self.playback_changed)();
    }
}

pub(super) struct ArtworkPage {
    pub(super) widget: gtk::Box,
    editor: Rc<ArtworkEditor>,
    load_generation: Rc<Cell<u64>>,
    artwork_loader: ArtworkLoader,
}

impl ArtworkPage {
    #[allow(clippy::too_many_arguments)]
    pub(super) fn new(
        parent_window: &gtk::Window,
        command_controller: &SharedCommandController,
        artwork_changed_holder: &ArtworkChangedHolder,
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
        let add_button = gtk::Button::with_label(&gettext("Add Artwork\u{2026}"));
        let paste_button = gtk::Button::with_label(&gettext("Paste Artwork"));
        paste_button.set_tooltip_text(Some(&gettext(
            "Use the image, or the image file, on the clipboard (Ctrl+V)",
        )));
        let remove_button = gtk::Button::with_label(&gettext("Remove Artwork"));
        remove_button.add_css_class("destructive-action");
        buttons.append(&add_button);
        buttons.append(&paste_button);
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
                note.set_text(&if has_artwork {
                    gettext("Artwork is embedded in the audio file.")
                } else {
                    gettext("This track has no embedded artwork.")
                });
                remove_button.set_sensitive(has_artwork);
            })
        };

        let editor = Rc::new(ArtworkEditor {
            parent_window: parent_window.clone(),
            command_controller: command_controller.clone(),
            publisher: ArtworkChangePublisher {
                artwork_changed_holder: artwork_changed_holder.clone(),
                playback_changed,
                artwork_loader: artwork_loader.clone(),
            },
            refresh,
            current_track_id: Cell::new(track_id),
            current_artwork_source: RefCell::new(None),
        });
        {
            let editor = editor.clone();
            add_button.connect_clicked(move |_| editor.choose());
        }
        {
            let editor = editor.clone();
            paste_button.connect_clicked(move |button| editor.paste(button.clipboard()));
        }
        {
            let editor = editor.clone();
            remove_button.connect_clicked(move |_| editor.remove());
        }

        let page = Self {
            widget: page,
            editor,
            load_generation: Rc::new(Cell::new(0)),
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
        self.editor.current_track_id.set(track_id);
        self.editor
            .current_artwork_source
            .replace(artwork_source.clone());
        let generation = self.load_generation.get().wrapping_add(1);
        self.load_generation.set(generation);
        (self.editor.refresh)(None, has_embedded_artwork);
        if let Some(source) = artwork_source {
            let refresh = self.editor.refresh.clone();
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

    /// Set the shown track's artwork from the clipboard, as the Paste
    /// Artwork button does.
    pub(super) fn paste(&self) {
        self.editor.paste(self.widget.clipboard());
    }
}

/// The artwork edits — choose a file, paste, remove — applied to the
/// track the page shows when the edit starts.
struct ArtworkEditor {
    parent_window: gtk::Window,
    command_controller: SharedCommandController,
    publisher: ArtworkChangePublisher,
    refresh: ArtworkRefreshCallback,
    current_track_id: Cell<TrackId>,
    current_artwork_source: RefCell<Option<ArtworkSource>>,
}

/// Where new artwork comes from: an image file, or encoded image bytes
/// already in memory (pasted).
enum ArtworkInput {
    File(PathBuf),
    Bytes(Vec<u8>),
}

/// The track an artwork edit targets, captured when the edit starts so a
/// slow read cannot land on the track navigated to in the meantime.
struct ArtworkTarget {
    track_id: TrackId,
    artwork_source: Option<ArtworkSource>,
}

impl ArtworkEditor {
    fn target(&self) -> ArtworkTarget {
        ArtworkTarget {
            track_id: self.current_track_id.get(),
            artwork_source: self.current_artwork_source.borrow().clone(),
        }
    }

    fn choose(self: &Rc<Self>) {
        let dialog = FileDialog::builder()
            .title(gettext("Choose Artwork"))
            .modal(true)
            .build();

        let filter = FileFilter::new();
        filter.set_name(Some(&gettext("Images")));
        for mime_type in ARTWORK_MIME_TYPES {
            filter.add_mime_type(mime_type);
        }
        let filters = gio::ListStore::new::<FileFilter>();
        filters.append(&filter);
        dialog.set_filters(Some(&filters));
        dialog.set_default_filter(Some(&filter));

        let target = self.target();
        let editor = self.clone();
        dialog.open(
            Some(&self.parent_window),
            None::<&gio::Cancellable>,
            move |result| {
                let Ok(file) = result else {
                    return;
                };
                let Some(path) = file.path() else {
                    return;
                };
                editor.load(target, ArtworkInput::File(path));
            },
        );
    }

    /// Take the artwork from `clipboard`: an image copied from another
    /// application, or an image file copied in a file manager.
    fn paste(self: &Rc<Self>, clipboard: gdk::Clipboard) {
        let target = self.target();
        let editor = self.clone();
        glib::MainContext::default().spawn_local(async move {
            match read_clipboard_artwork(&clipboard).await {
                Ok(Some(input)) => editor.load(target, input),
                Ok(None) => editor
                    .command_controller
                    .report_command_warning(gettext("The clipboard holds no image.")),
                Err(error) => editor
                    .command_controller
                    .report_artwork_selection_error(&error),
            }
        });
    }

    fn remove(&self) {
        let target = self.target();
        if self
            .command_controller
            .dispatch_succeeded(ApplicationCommand::SetArtwork {
                track_id: target.track_id,
                artwork: None,
            })
        {
            self.publisher
                .publish(target.track_id, target.artwork_source, None);
            (self.refresh)(None, false);
        }
    }

    /// Read and decode `input` off the GTK thread, then apply it to
    /// `target` on the main loop. A large image or a slow mount can make
    /// both steps take a while; running them inline would freeze the
    /// dialog (#107).
    fn load(self: &Rc<Self>, target: ArtworkTarget, input: ArtworkInput) {
        let (sender, receiver) = async_channel::bounded::<LoadedArtwork>(1);
        std::thread::Builder::new()
            .name("sustain-artwork-load".to_owned())
            .spawn(move || {
                let bytes = match input {
                    ArtworkInput::File(path) => read_artwork_file(&path),
                    ArtworkInput::Bytes(bytes) => validate_encoded_artwork(&bytes)
                        .map(|_| bytes)
                        .map_err(ArtworkReadError::from),
                };
                let outcome = match bytes {
                    Ok(bytes) => {
                        let texture = texture_from_bytes(&bytes, ARTWORK_PREVIEW_SIZE);
                        LoadedArtwork::Ready { bytes, texture }
                    }
                    Err(error) => LoadedArtwork::Failed(error),
                };
                // The receiver lives on the main loop until it gets this
                // outcome; a send failure just means it closed first.
                let _ = sender.send_blocking(outcome);
            })
            .expect("spawn artwork load worker thread");

        let editor = self.clone();
        glib::MainContext::default().spawn_local(async move {
            let Ok(outcome) = receiver.recv().await else {
                return;
            };
            editor.apply(target, outcome);
        });
    }

    fn apply(&self, target: ArtworkTarget, outcome: LoadedArtwork) {
        match outcome {
            LoadedArtwork::Ready { bytes, texture } => {
                let cache_bytes = bytes.clone();
                if self
                    .command_controller
                    .dispatch_succeeded(ApplicationCommand::SetArtwork {
                        track_id: target.track_id,
                        artwork: Some(bytes),
                    })
                {
                    self.publisher.publish(
                        target.track_id,
                        target.artwork_source,
                        Some(cache_bytes),
                    );
                    if self.current_track_id.get() == target.track_id {
                        (self.refresh)(texture, true);
                    }
                }
            }
            LoadedArtwork::Failed(error) => {
                eprintln!("sustain: failed to read new artwork: {error}");
                self.command_controller
                    .report_artwork_selection_error(&error);
            }
        }
    }
}

/// The image formats artwork is accepted in, as MIME types: the picker's
/// filter and the clipboard formats a paste asks for.
const ARTWORK_MIME_TYPES: [&str; 4] = ["image/png", "image/jpeg", "image/webp", "image/gif"];

/// Bytes read from the clipboard per step.
const CLIPBOARD_READ_CHUNK: usize = 64 * 1024;

/// The artwork `clipboard` offers: encoded image bytes when it holds an
/// image, else the first file when it holds files, else `None`.
#[allow(
    clippy::future_not_send,
    reason = "GTK clipboard objects are main-thread only; it runs on the main loop via spawn_local"
)]
async fn read_clipboard_artwork(
    clipboard: &gdk::Clipboard,
) -> Result<Option<ArtworkInput>, ArtworkReadError> {
    // Another application's clipboard advertises MIME types only; the
    // union adds the types GDK can deserialize from them, such as the file
    // list a file manager's `text/uri-list` carries.
    let formats = clipboard.formats().union_deserialize_types();
    if ARTWORK_MIME_TYPES
        .iter()
        .any(|mime_type| formats.contain_mime_type(mime_type))
    {
        let (stream, _mime_type) = clipboard
            .read_future(&ARTWORK_MIME_TYPES, glib::Priority::DEFAULT)
            .await
            .map_err(io::Error::other)?;
        return Ok(Some(ArtworkInput::Bytes(read_capped(&stream).await?)));
    }
    if formats.contains_type(gdk::FileList::static_type()) {
        let value = clipboard
            .read_value_future(gdk::FileList::static_type(), glib::Priority::DEFAULT)
            .await
            .map_err(io::Error::other)?;
        let path = value
            .get::<gdk::FileList>()
            .ok()
            .and_then(|files| files.files().into_iter().find_map(|file| file.path()));
        return Ok(path.map(ArtworkInput::File));
    }
    Ok(None)
}

/// Read `stream` to its end, stopping just past the artwork size limit so
/// an oversized payload is rejected by the policy without being buffered
/// whole.
#[allow(
    clippy::future_not_send,
    reason = "GIO streams are main-thread objects here; it runs on the main loop via spawn_local"
)]
async fn read_capped(stream: &gio::InputStream) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    while bytes.len() <= MAX_ENCODED_ARTWORK_BYTES {
        let chunk = stream
            .read_bytes_future(CLIPBOARD_READ_CHUNK, glib::Priority::DEFAULT)
            .await
            .map_err(io::Error::other)?;
        if chunk.is_empty() {
            break;
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

/// Place a pre-decoded texture (or the missing-artwork placeholder) into
/// `frame` at `size`. Does no decoding — the texture is produced off the
/// GTK thread by the shared loader or the artwork load worker.
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

/// Outcome of the off-thread read+decode of new artwork, chosen or pasted.
enum LoadedArtwork {
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

#[cfg(test)]
mod tests {
    use super::{MAX_ENCODED_ARTWORK_BYTES, read_capped, set_frame_texture, texture_from_bytes};
    use crate::test_support::with_gtk;
    use gtk::prelude::*;
    use gtk::{gio, glib};

    fn read_capped_blocking(payload: Vec<u8>) -> Vec<u8> {
        let stream = gio::MemoryInputStream::from_bytes(&glib::Bytes::from_owned(payload));
        glib::MainContext::new()
            .block_on(read_capped(stream.upcast_ref()))
            .expect("memory stream reads")
    }

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
        with_gtk(|| {
            assert!(texture_from_bytes(b"definitely not an image", 320).is_none());
            assert!(texture_from_bytes(VALID_PNG, 320).is_some());
        });
    }

    #[test]
    fn frame_shows_placeholder_without_texture_and_image_with_one() {
        with_gtk(|| {
            let frame = gtk::Frame::new(None);

            let shown_image = |frame: &gtk::Frame| {
                frame
                    .child()
                    .expect("frame child present")
                    .downcast::<gtk::Image>()
                    .expect("the frame shows an image")
            };

            set_frame_texture(&frame, None, 96);
            let placeholder = shown_image(&frame);
            assert_eq!(placeholder.storage_type(), gtk::ImageType::IconName);
            assert_eq!(
                placeholder.icon_name().as_deref(),
                Some("image-missing-symbolic"),
                "absent artwork renders the missing-image placeholder"
            );

            let texture = texture_from_bytes(VALID_PNG, 96).expect("valid png decodes");
            set_frame_texture(&frame, Some(&texture), 96);
            let cover = shown_image(&frame);
            assert_eq!(cover.storage_type(), gtk::ImageType::Paintable);
            assert_eq!(
                cover.paintable(),
                Some(texture.upcast::<gtk::gdk::Paintable>()),
                "present artwork renders the supplied texture"
            );
        });
    }

    #[test]
    fn clipboard_read_takes_whole_payloads_and_stops_past_the_size_limit() {
        let small = vec![7; 200_000];
        assert_eq!(read_capped_blocking(small.clone()), small);

        let oversized = read_capped_blocking(vec![0; MAX_ENCODED_ARTWORK_BYTES * 2]);
        assert!(oversized.len() > MAX_ENCODED_ARTWORK_BYTES);
        assert!(oversized.len() < MAX_ENCODED_ARTWORK_BYTES * 2);
    }
}
