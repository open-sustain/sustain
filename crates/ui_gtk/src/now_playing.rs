// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use gtk::prelude::*;
use gtk::{gdk, glib};

use super::{
    APP_ID, NOW_PLAYING_ICON_SIZE, NOW_PLAYING_SIDE_WIDTH, SharedRuntime, TITLEBAR_HEIGHT,
    artwork_color::ArtworkPalette,
    artwork_loader::{ArtworkLoader, ArtworkSource, DecodedArtwork},
    command_controller::SharedCommandController,
    shuffle_icon::ShuffleShineIcon,
};
use model::{
    artist_album_text, playback_position, progress_fraction, remaining_time_text, time_text,
    track_title,
};
use progress_hit_area::ProgressHitArea;
use sustain_app_runtime::{
    ApplicationCommand, NotificationCategory, NotificationId, NotificationSeverity, NowPlaying,
    PlaybackCommand, ShuffleMode, Track, TrackId,
};

mod lyrics_overlay;
mod marquee;
mod model;
mod progress_hit_area;

use lyrics_overlay::LyricsOverlayFace;
use marquee::{MarqueeLabel, MarqueeOverflow};

/// CSS class added to the artwork box while a dominant-color background
/// is active. Defining it as a sibling of `now-playing-artwork` (rather
/// than overriding that class directly) keeps the default neutral tint
/// — applied via the static stylesheet — intact for the no-artwork
/// state without any extra removal step.
const ARTWORK_DOMINANT_COLOR_CLASS: &str = "now-playing-artwork-dominant";

/// CSS class added to the artwork box while it sits in the missing
/// state and is therefore clickable. Lets the stylesheet shift the
/// cursor and apply a hover tint without the now-playing module
/// reaching for runtime cursor APIs.
const ARTWORK_CLICKABLE_CLASS: &str = "now-playing-artwork-clickable";

const ARTWORK_INNER_STACK_PRESENT: &str = "present";
const ARTWORK_INNER_STACK_MISSING: &str = "missing";
const ARTWORK_INNER_STACK_FETCHING: &str = "fetching";

/// Icon shown in the inner stack's "missing" page. Standard freedesktop
/// symbolic icon name; falls back gracefully on systems with a
/// different theme.
const ARTWORK_MISSING_ICON_NAME: &str = "image-missing-symbolic";

const ARTWORK_MISSING_TOOLTIP: &str = "Fetch artwork";
const ARTWORK_PRESENT_TOOLTIP: &str = "Zoom artwork";

#[derive(Clone)]
pub(crate) struct NowPlayingView {
    runtime: SharedRuntime,
    parent_window: glib::WeakRef<gtk::ApplicationWindow>,
    area: gtk::Box,
    stack: gtk::Stack,
    title: MarqueeLabel,
    artist_album: MarqueeLabel,
    elapsed: gtk::Label,
    remaining: gtk::Label,
    hit_area: ProgressHitArea,
    shuffle_icon: ShuffleShineIcon,
    shuffle_button: gtk::Button,
    repeat_icon: gtk::Image,
    repeat_button: gtk::Button,
    artwork_box: gtk::Overlay,
    /// Inner stack of three pages — `present` (the artwork itself),
    /// `missing` (the click-to-fetch icon), `fetching` (the spinner).
    /// Switching pages keeps the tile geometry stable even while the
    /// content swaps in and out.
    artwork_inner_stack: gtk::Stack,
    artwork_image: gtk::Image,
    artwork_spinner: gtk::Spinner,
    lyrics_chip: gtk::Button,
    artwork_loader: ArtworkLoader,
    /// Last absolute path passed to the artwork loader, used to avoid
    /// re-issuing a request when `refresh()` runs on the same track
    /// (the playback poll triggers `refresh()` every second).
    artwork_path: Rc<RefCell<Option<PathBuf>>>,
    prefetched_artwork_path: Rc<RefCell<Option<PathBuf>>>,
    /// Monotonic counter bumped on every track change. Callbacks queued
    /// against the loader capture the value they were issued with and
    /// drop themselves if the track has changed since, so a slow decode
    /// for a previous track never paints over the current one.
    artwork_generation: Rc<Cell<u64>>,
    /// CSS provider that carries the dominant-color rule for the
    /// artwork box. Rewritten in place as palettes resolve; installed
    /// once on the display at construction and stays for the window's
    /// lifetime.
    artwork_color_provider: gtk::CssProvider,
    /// Track for which a remote artwork fetch is currently in flight.
    /// Used to show the spinner on the right tile (even across track
    /// switches mid-fetch) and to make additional clicks during the
    /// fetch idempotent.
    pending_fetch_track_id: Rc<Cell<Option<TrackId>>>,
    /// Id of the persistent notification that mirrors the in-flight
    /// fetch in the status bar's notification lane. Stored here so
    /// `notify_artwork_fetch_complete` can dismiss the exact entry it
    /// owns once the result arrives.
    pending_fetch_notification_id: Rc<Cell<Option<NotificationId>>>,
    duration: Rc<Cell<Duration>>,
}

const EMPTY_STACK_NAME: &str = "no-track";
const LOADED_STACK_NAME: &str = "loaded";
const EMPTY_STATE_ICON_SIZE: i32 = 48;
/// Per-line scroll speeds in pixels per frame. The title and the
/// artist/album line scroll at deliberately different rates (issue #116)
/// so two overflowing lines never crawl in lockstep — that lockstep reads
/// as one rigid block sliding, whereas independent rates give the pair a
/// looser, layered motion. The title (the dominant line) keeps the
/// original pace; the secondary line trails slightly slower.
const MARQUEE_SPEED_TITLE: f64 = 0.75;
const MARQUEE_SPEED_ARTIST: f64 = 0.55;
const MARQUEE_VIEWPORT_WIDTH: i32 = 400;
/// Reduced title-viewport cap used while the LCD chips share the title's
/// line. The chips sit inline immediately after the title, so the title
/// gives up width to keep the combined row from overflowing into the
/// shuffle/repeat side controls; longer titles scroll within this width.
/// Restored to [`MARQUEE_VIEWPORT_WIDTH`] when the track has no chips.
const MARQUEE_TITLE_CHIP_WIDTH: i32 = 240;
/// Gap, in pixels, between the title and the inline LCD chip strip.
const MARQUEE_CHIP_GAP: i32 = 6;

impl NowPlayingView {
    pub(crate) fn new(
        parent_window: &gtk::ApplicationWindow,
        runtime: SharedRuntime,
        command_controller: SharedCommandController,
        artwork_loader: ArtworkLoader,
    ) -> Self {
        let area = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        area.add_css_class("now-playing-area");
        area.set_size_request(super::NOW_PLAYING_WIDTH, TITLEBAR_HEIGHT);
        area.set_hexpand(false);
        area.set_halign(gtk::Align::Center);
        area.set_margin_start(super::NOW_PLAYING_HORIZONTAL_MARGIN);
        area.set_margin_end(super::NOW_PLAYING_HORIZONTAL_MARGIN);
        area.set_valign(gtk::Align::Fill);

        let artwork_box = gtk::Overlay::new();
        artwork_box.add_css_class("now-playing-artwork");
        artwork_box.set_size_request(TITLEBAR_HEIGHT, TITLEBAR_HEIGHT);
        artwork_box.set_overflow(gtk::Overflow::Hidden);
        // Pin the tile to its square `size_request`: never claim extra
        // space on either axis (so the horizontal `loaded_view` doesn't
        // widen it past the square) and centre it vertically rather than
        // filling the bar's height. Same square enforcement the queue
        // rows use.
        artwork_box.set_hexpand(false);
        artwork_box.set_vexpand(false);
        artwork_box.set_valign(gtk::Align::Center);

        let artwork_image = gtk::Image::new();
        artwork_image.set_pixel_size(TITLEBAR_HEIGHT);
        // Pin the image to a square so a non-square cover letterboxes
        // inside the tile instead of stretching the container's aspect —
        // the same square enforcement the queue rows use.
        artwork_image.set_size_request(TITLEBAR_HEIGHT, TITLEBAR_HEIGHT);
        artwork_image.set_hexpand(false);
        artwork_image.set_vexpand(false);
        artwork_image.set_halign(gtk::Align::Fill);
        artwork_image.set_valign(gtk::Align::Fill);

        let artwork_missing_icon = gtk::Image::from_icon_name(ARTWORK_MISSING_ICON_NAME);
        artwork_missing_icon.add_css_class("now-playing-artwork-missing-icon");
        artwork_missing_icon.set_pixel_size(TITLEBAR_HEIGHT / 2);
        artwork_missing_icon.set_halign(gtk::Align::Center);
        artwork_missing_icon.set_valign(gtk::Align::Center);

        let artwork_spinner = gtk::Spinner::new();
        artwork_spinner.add_css_class("now-playing-artwork-spinner");
        artwork_spinner.set_halign(gtk::Align::Center);
        artwork_spinner.set_valign(gtk::Align::Center);

        let artwork_inner_stack = gtk::Stack::new();
        // Fill the square tile via alignment, never via expand — an
        // expanding child would propagate up and stretch the tile wider
        // than tall inside the horizontal now-playing row.
        artwork_inner_stack.set_hexpand(false);
        artwork_inner_stack.set_vexpand(false);
        artwork_inner_stack.add_named(&artwork_image, Some(ARTWORK_INNER_STACK_PRESENT));
        artwork_inner_stack.add_named(&artwork_missing_icon, Some(ARTWORK_INNER_STACK_MISSING));
        artwork_inner_stack.add_named(&artwork_spinner, Some(ARTWORK_INNER_STACK_FETCHING));
        artwork_inner_stack.set_visible_child_name(ARTWORK_INNER_STACK_MISSING);
        artwork_box.set_child(Some(&artwork_inner_stack));

        let artwork_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let prefetched_artwork_path: Rc<RefCell<Option<PathBuf>>> = Rc::new(RefCell::new(None));
        let artwork_color_provider = install_artwork_color_provider();
        let pending_fetch_track_id: Rc<Cell<Option<TrackId>>> = Rc::new(Cell::new(None));
        let pending_fetch_notification_id: Rc<Cell<Option<NotificationId>>> =
            Rc::new(Cell::new(None));

        let details = gtk::Box::new(gtk::Orientation::Vertical, 0);
        details.set_hexpand(true);
        details.set_vexpand(true);

        let marquee_paused = Rc::new(Cell::new(false));
        let title = MarqueeLabel::new(
            "now-playing-title",
            marquee_paused.clone(),
            MARQUEE_SPEED_TITLE,
            MARQUEE_VIEWPORT_WIDTH,
        );
        let artist_album = MarqueeLabel::new(
            "now-playing-artist",
            marquee_paused.clone(),
            MARQUEE_SPEED_ARTIST,
            MARQUEE_VIEWPORT_WIDTH,
        );

        // Vintage-LCD readout chip that sits inline after the title. This
        // is where the lyrics affordance lives now — moved off the artwork
        // cover, which was cramped and visually noisy. Shown only when the
        // track has lyrics; while shown, the title truncates instead of
        // scrolling so the chip stays pinned to its trailing edge.
        let lyrics_chip = gtk::Button::with_label("Lyrics");
        lyrics_chip.add_css_class("now-playing-lcd-chip");
        lyrics_chip.set_tooltip_text(Some("Show lyrics"));
        lyrics_chip.set_cursor_from_name(Some("pointer"));
        lyrics_chip.set_valign(gtk::Align::Center);
        lyrics_chip.set_visible(false);

        let metadata = metadata_box(&title, &artist_album, &lyrics_chip);

        let elapsed = time_label();
        let remaining = time_label();

        let shuffle_icon =
            ShuffleShineIcon::new("media-playlist-shuffle-symbolic", NOW_PLAYING_ICON_SIZE);
        let (shuffle_widget, shuffle_button) =
            side_status_chrome(&shuffle_icon, "Shuffle", &elapsed);

        let repeat_icon = gtk::Image::from_icon_name("media-playlist-repeat-symbolic");
        repeat_icon.add_css_class("now-playing-side-icon");
        repeat_icon.set_pixel_size(NOW_PLAYING_ICON_SIZE);
        repeat_icon.set_halign(gtk::Align::Center);
        let (repeat_widget, repeat_button) = side_status_chrome(&repeat_icon, "Repeat", &remaining);

        let detail_content = gtk::CenterBox::new();
        detail_content.set_hexpand(true);
        detail_content.set_vexpand(true);
        detail_content.set_valign(gtk::Align::Fill);
        detail_content.set_start_widget(Some(&shuffle_widget));
        detail_content.set_center_widget(Some(&metadata));
        detail_content.set_end_widget(Some(&repeat_widget));

        let duration = Rc::new(Cell::new(Duration::ZERO));
        let hit_area = ProgressHitArea::new(command_controller.clone(), duration.clone());

        details.append(&detail_content);
        details.append(hit_area.widget());
        hit_area.install_hover_visibility_on(&area);

        let loaded_view = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        loaded_view.set_hexpand(true);
        loaded_view.set_vexpand(true);
        loaded_view.append(&artwork_box);
        loaded_view.append(&details);

        let empty_view = empty_state_view();

        let stack = gtk::Stack::new();
        stack.set_hexpand(true);
        stack.set_vexpand(true);
        stack.set_hhomogeneous(true);
        stack.set_vhomogeneous(true);
        stack.add_named(&empty_view, Some(EMPTY_STACK_NAME));
        stack.add_named(&loaded_view, Some(LOADED_STACK_NAME));
        stack.set_visible_child_name(EMPTY_STACK_NAME);
        area.append(&stack);

        install_hover_pause(&area, &title, &artist_album, marquee_paused);

        let view = Self {
            runtime: runtime.clone(),
            parent_window: parent_window.downgrade(),
            area,
            stack,
            title,
            artist_album,
            elapsed,
            remaining,
            hit_area,
            shuffle_icon,
            shuffle_button,
            repeat_icon,
            repeat_button,
            artwork_box,
            artwork_inner_stack,
            artwork_image,
            artwork_spinner,
            lyrics_chip,
            artwork_loader,
            artwork_path,
            prefetched_artwork_path,
            artwork_generation: Rc::new(Cell::new(0)),
            artwork_color_provider,
            pending_fetch_track_id,
            pending_fetch_notification_id,
            duration,
        };
        install_playback_option_controls(&view, command_controller.clone());
        install_artwork_click_handler(&view, command_controller);
        install_lyrics_chip_handler(&view);
        view.refresh(&runtime.borrow().now_playing());
        install_refresh_timer(&view, runtime);
        view
    }

    /// Called by the result consumer when a remote artwork fetch
    /// finishes. Clears the pending-fetch state if this is the track
    /// we were waiting on, and resets the tracked artwork path so
    /// the next refresh re-evaluates from the (now-primed or
    /// invalidated) cache instead of short-circuiting on
    /// "same track, same path". Without that reset the freshly
    /// primed cache entry would never be drawn until something else
    /// caused a track-change refresh.
    pub(crate) fn notify_artwork_fetch_complete(&self, track_id: TrackId) {
        if self.pending_fetch_track_id.get() == Some(track_id) {
            self.pending_fetch_track_id.set(None);
        }
        if let Some(id) = self.pending_fetch_notification_id.take() {
            self.runtime.borrow_mut().dismiss_notification(id);
        }
        if self.runtime.borrow().playback_queue_current_track_id() == Some(track_id) {
            *self.artwork_path.borrow_mut() = None;
        }
    }

    pub(crate) fn widget(&self) -> gtk::Box {
        self.area.clone()
    }

    fn sync_artwork(&self, track: Option<&Track>) {
        let new_source = track.and_then(|track| self.artwork_source(track));
        let new_path = new_source.as_ref().map(absolute_path_of);
        let track_id = track.map(|track| track.id);
        let same_track = *self.artwork_path.borrow() == new_path;
        if same_track {
            // Geometry hasn't changed — still re-apply the visible-state
            // because a fetch result may have arrived (cache primed
            // by the result consumer) or completed (pending cleared)
            // without changing the underlying source path.
            self.apply_artwork_state(track_id, new_source.as_ref());
            return;
        }
        *self.artwork_path.borrow_mut() = new_path;

        // Bump the per-track generation so any callback still in flight
        // for the previous track no-ops when it lands. Snapshot the new
        // value into each closure below; without the snapshot the
        // callback would read whatever generation happened to be
        // current at delivery time and apply unconditionally.
        let generation_snapshot = self.artwork_generation.get().wrapping_add(1);
        self.artwork_generation.set(generation_snapshot);

        let Some(source) = new_source else {
            self.apply_decoded_artwork(&DecodedArtwork::default());
            self.apply_artwork_state(track_id, None);
            return;
        };

        // Warm the larger detail cover off the main thread. The tile path
        // below only resolves the small texture, but clicking the tile opens
        // the lyrics/artwork overlay, which wants the full-resolution detail.
        // Loading it now means that click is instant and crisp instead of
        // falling back to the upscaled tile; the detail cache is bounded, so
        // this pins at most the current track's cover.
        self.artwork_loader
            .request_detail(source.clone(), Box::new(|_| {}));

        // Synchronous cache hit (in-memory) — apply immediately to
        // avoid a one-tick gap where the previous artwork's color
        // would still be visible. Cold cache requests fall through to
        // the async loader.
        if let Some(decoded) = self.artwork_loader.cached(&source) {
            self.apply_decoded_artwork(&decoded);
            self.apply_artwork_state(track_id, Some(&source));
            return;
        }

        // Show the neutral placeholder while the worker decodes, so a
        // stale dominant color from the previous track doesn't linger
        // in the gap before the new palette arrives.
        self.apply_decoded_artwork(&DecodedArtwork::default());
        self.apply_artwork_state(track_id, Some(&source));

        let view = self.clone();
        let source_for_callback = source.clone();
        let track_id_for_callback = track_id;
        self.artwork_loader.request(
            source,
            Box::new(move |decoded| {
                if view.artwork_generation.get() != generation_snapshot {
                    return;
                }
                view.apply_decoded_artwork(&decoded);
                view.apply_artwork_state(track_id_for_callback, Some(&source_for_callback));
            }),
        );
    }

    /// Pick the inner stack page and clickable affordance based on
    /// what we currently know about the track's artwork. Called both
    /// after the synchronous cache check and after the async loader
    /// callback lands.
    fn apply_artwork_state(&self, track_id: Option<TrackId>, source: Option<&ArtworkSource>) {
        let pending = self.pending_fetch_track_id.get();
        let is_fetching = match (track_id, pending) {
            (Some(track_id), Some(pending)) => track_id == pending,
            _ => false,
        };
        if is_fetching {
            self.set_artwork_inner_page(ARTWORK_INNER_STACK_FETCHING);
            return;
        }
        let has_artwork = source
            .and_then(|source| self.artwork_loader.cached(source))
            .and_then(|decoded| {
                decoded
                    .tile_texture
                    .as_ref()
                    .or(decoded.detail_texture.as_ref())
                    .map(|_| ())
            })
            .is_some();
        if has_artwork {
            self.set_artwork_inner_page(ARTWORK_INNER_STACK_PRESENT);
        } else {
            self.set_artwork_inner_page(ARTWORK_INNER_STACK_MISSING);
        }
    }

    fn set_artwork_inner_page(&self, name: &'static str) {
        self.artwork_inner_stack.set_visible_child_name(name);
        self.artwork_spinner
            .set_spinning(name == ARTWORK_INNER_STACK_FETCHING);
        if name == ARTWORK_INNER_STACK_MISSING || name == ARTWORK_INNER_STACK_PRESENT {
            self.artwork_box.add_css_class(ARTWORK_CLICKABLE_CLASS);
            let tooltip = if name == ARTWORK_INNER_STACK_MISSING {
                ARTWORK_MISSING_TOOLTIP
            } else {
                ARTWORK_PRESENT_TOOLTIP
            };
            self.artwork_box.set_tooltip_text(Some(tooltip));
            // GTK4's CSS `cursor` property is honoured inconsistently
            // across distributions, so set the cursor on the widget
            // directly. Falls back to the parent's cursor when the
            // named cursor isn't available on the active theme.
            let cursor = gdk::Cursor::from_name("pointer", None);
            self.artwork_box.set_cursor(cursor.as_ref());
        } else {
            self.artwork_box.remove_css_class(ARTWORK_CLICKABLE_CLASS);
            self.artwork_box.set_tooltip_text(None);
            self.artwork_box.set_cursor(None);
        }
    }

    /// Click handler entry point. Returns true if a fetch was
    /// dispatched (so the caller knows to refresh into the spinner
    /// state); false if the click was ignored (no current track, no
    /// fetch path, or another fetch already in flight for this
    /// track).
    fn handle_artwork_click(&self, command_controller: &SharedCommandController) -> bool {
        match self.artwork_inner_stack.visible_child_name().as_deref() {
            Some(ARTWORK_INNER_STACK_PRESENT) => {
                return self.open_artwork_lyrics_overlay(LyricsOverlayFace::Artwork);
            }
            Some(ARTWORK_INNER_STACK_MISSING) => {}
            _ => return false,
        }
        let Some(track_id) = self.runtime.borrow().playback_queue_current_track_id() else {
            return false;
        };
        // PLAN: "The click is idempotent: further clicks while a fetch
        // is in flight do nothing." We additionally treat a click as
        // idempotent if any other track is being fetched for, because
        // the worker is single-slot and we don't want to enqueue
        // background work the user has no surface for.
        if self.pending_fetch_track_id.get().is_some() {
            return false;
        }
        if !command_controller.dispatch_succeeded(ApplicationCommand::FetchArtwork { track_id }) {
            // Dispatch already surfaced the error through the
            // notification lane (e.g. ArtworkFetchingUnavailable).
            // Leave the tile in the missing state so the user can
            // retry once whatever blocked the call is resolved.
            return false;
        }
        self.pending_fetch_track_id.set(Some(track_id));
        self.set_artwork_inner_page(ARTWORK_INNER_STACK_FETCHING);
        // Mirror the tile-local spinner with a persistent notification
        // so an in-flight fetch is noticeable even when the user
        // isn't looking at the artwork tile. The fetch worker is
        // short-lived but uncancellable from here.
        let notification_id = self.runtime.borrow_mut().push_persistent_notification(
            NotificationCategory::ArtworkFetch,
            NotificationSeverity::Info,
            "Fetching artwork…".to_owned(),
            false,
        );
        self.pending_fetch_notification_id
            .set(Some(notification_id));
        true
    }

    fn open_artwork_lyrics_overlay(&self, initial_face: LyricsOverlayFace) -> bool {
        let Some(parent) = self.parent_window.upgrade() else {
            return false;
        };
        let Some(track) = self.runtime.borrow().now_playing().track else {
            return false;
        };
        if initial_face == LyricsOverlayFace::Lyrics && !track.has_lyrics() {
            return false;
        }

        // Prefer the prewarmed detail cover; fall back to the tile only for
        // the brief window before the detail decode lands, so the overlay
        // still opens (a touch softer) rather than refusing to.
        let decoded = self.artwork_source(&track).as_ref().and_then(|source| {
            self.artwork_loader
                .cached_detail(source)
                .or_else(|| self.artwork_loader.cached(source))
        });
        let texture = decoded.as_ref().and_then(|decoded| {
            decoded
                .detail_texture
                .as_ref()
                .or(decoded.tile_texture.as_ref())
        });
        if initial_face == LyricsOverlayFace::Artwork && texture.is_none() {
            return false;
        }

        let synced_lyrics = if track.has_lyrics() {
            match self.runtime.borrow().load_synced_lyrics(track.id) {
                Ok(lyrics) => lyrics,
                Err(error) => {
                    eprintln!("sustain: could not load synced lyrics for overlay: {error:?}");
                    None
                }
            }
        } else {
            None
        };
        let lyrics_text = synced_lyrics
            .as_ref()
            .map(synced_lyrics_text)
            .filter(|text| !text.trim().is_empty())
            .or_else(|| {
                track
                    .metadata
                    .lyrics
                    .clone()
                    .filter(|lyrics| !lyrics.trim().is_empty())
            });
        lyrics_overlay::open(
            &parent,
            texture,
            decoded.as_ref().and_then(|decoded| decoded.palette),
            lyrics_text.as_deref(),
            initial_face,
        );
        true
    }

    fn artwork_source(&self, track: &Track) -> Option<ArtworkSource> {
        let runtime = self.runtime.borrow();
        let absolute = runtime.absolute_track_path(track)?;
        // The disk cache is keyed by the library-relative path so it
        // survives library-root moves; the worker also needs the
        // absolute path to actually read the file. Albums uses the
        // same convention, so both views hit the same cache row.
        let cache_path = track.location.path().to_path_buf();
        Some(ArtworkSource::embedded_track(cache_path, absolute))
    }

    fn prefetch_next_artwork(&self) {
        let next_track = {
            let runtime = self.runtime.borrow();
            let Some(next_track_id) = runtime.playback_queue_next_track_id() else {
                return;
            };
            runtime
                .library_tracks()
                .iter()
                .find(|track| track.id == next_track_id)
                .cloned()
        };
        let Some(next_track) = next_track else {
            return;
        };
        let Some(source) = self.artwork_source(&next_track) else {
            return;
        };
        let path = absolute_path_of(&source);
        if *self.prefetched_artwork_path.borrow() == Some(path.clone()) {
            return;
        }
        *self.prefetched_artwork_path.borrow_mut() = Some(path);
        if self.artwork_loader.cached(&source).is_none() {
            self.artwork_loader.request(source, Box::new(|_| {}));
        }
    }

    fn apply_decoded_artwork(&self, decoded: &DecodedArtwork) {
        // Page selection is decided by `apply_artwork_state`; this
        // method only loads the texture (or clears it). Keeping the
        // two concerns separate avoids flicker when a callback lands
        // after a fetch result has primed the cache.
        let texture = decoded
            .tile_texture
            .as_ref()
            .or(decoded.detail_texture.as_ref());
        match texture {
            Some(texture) => {
                self.artwork_image.set_paintable(Some(texture));
            }
            None => {
                self.artwork_image.set_paintable(None::<&gdk::Paintable>);
            }
        }
        self.apply_dominant_color(decoded.palette);
    }

    fn apply_dominant_color(&self, palette: Option<ArtworkPalette>) {
        match palette {
            Some(palette) => {
                // Rewriting the provider's CSS is preferable to swapping
                // multiple per-color classes: GTK reapplies styles to
                // every widget that matches the class, so a single
                // load_from_string is one re-style pass rather than
                // several class-list mutations.
                self.artwork_color_provider
                    .load_from_string(&artwork_dominant_color_css(palette));
                self.artwork_box.add_css_class(ARTWORK_DOMINANT_COLOR_CLASS);
            }
            None => {
                self.artwork_box
                    .remove_css_class(ARTWORK_DOMINANT_COLOR_CLASS);
            }
        }
    }

    /// Refresh the inline Lyrics chip for the current track. The chip is
    /// shown only when the track carries lyrics. While it is shown the
    /// title gives up width and truncates (holding still, fading its right
    /// edge) instead of scrolling, so the chip stays pinned to the title's
    /// trailing edge; without lyrics the title reclaims the full viewport
    /// and scrolls as usual.
    fn update_lyrics_chip(&self, track: &Track) {
        let has_lyrics = track.has_lyrics();
        self.lyrics_chip.set_visible(has_lyrics);
        if has_lyrics {
            self.title.set_max_width(MARQUEE_TITLE_CHIP_WIDTH);
            self.title.set_overflow_behavior(MarqueeOverflow::Truncate);
        } else {
            self.title.set_max_width(MARQUEE_VIEWPORT_WIDTH);
            self.title.set_overflow_behavior(MarqueeOverflow::Scroll);
        }
    }

    pub(crate) fn refresh(&self, now_playing: &NowPlaying) {
        self.sync_artwork(now_playing.track.as_ref());

        let Some(track) = &now_playing.track else {
            self.stack.set_visible_child_name(EMPTY_STACK_NAME);
            self.title.set_text("");
            self.artist_album.set_text("");
            self.elapsed.set_text("");
            self.remaining.set_text("");
            self.hit_area.set_position(0.0, false);
            self.duration.set(Duration::ZERO);
            self.lyrics_chip.set_visible(false);
            sync_shuffle_icon(&self.shuffle_icon, now_playing.options.shuffle_mode);
            sync_playback_option_icon(&self.repeat_icon, now_playing.options.repeat_enabled());
            return;
        };

        self.stack.set_visible_child_name(LOADED_STACK_NAME);
        self.update_lyrics_chip(track);

        let duration = track.metadata.duration.unwrap_or_default();
        self.duration.set(duration);
        let position = playback_position(&now_playing.state).unwrap_or_default();
        self.title.set_text(&track_title(track));
        self.artist_album
            .set_text(&artist_album_text(&track.metadata));
        self.elapsed.set_text(&time_text(position));
        self.remaining
            .set_text(&remaining_time_text(position, duration));
        self.hit_area
            .set_position(progress_fraction(position, duration), true);
        sync_shuffle_icon(&self.shuffle_icon, now_playing.options.shuffle_mode);
        sync_playback_option_icon(&self.repeat_icon, now_playing.options.repeat_enabled());
        self.prefetch_next_artwork();
    }
}

fn synced_lyrics_text(lyrics: &sustain_app_runtime::SyncedLyrics) -> String {
    lyrics
        .lines
        .iter()
        .map(|line| line.text.as_str())
        .collect::<Vec<_>>()
        .join("\n")
}

fn install_artwork_color_provider() -> gtk::CssProvider {
    let provider = gtk::CssProvider::new();
    // STYLE_PROVIDER_PRIORITY_APPLICATION + 2 sits one notch above the
    // accent-color provider (+1) and three notches above the static
    // app stylesheet, so the dominant background overrides the neutral
    // tint from app.css. The provider lives for the window's lifetime
    // — there is no removal step because the widget it styles is
    // never re-parented or destroyed before app shutdown.
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION + 2,
        );
    }
    provider
}

fn artwork_dominant_color_css(palette: ArtworkPalette) -> String {
    let background = palette.background_css();
    format!(".now-playing-artwork-dominant {{ background-color: {background}; }}",)
}

fn absolute_path_of(source: &ArtworkSource) -> PathBuf {
    match source {
        ArtworkSource::EmbeddedTrack { file_path, .. } => file_path.clone(),
    }
}

fn install_playback_option_controls(
    view: &NowPlayingView,
    command_controller: SharedCommandController,
) {
    let command_controller_for_shuffle = command_controller.clone();
    let view_for_shuffle = view.clone();
    view.shuffle_button.connect_clicked(move |_| {
        if command_controller_for_shuffle.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::CycleShuffleMode,
        )) {
            view_for_shuffle.refresh(
                &command_controller_for_shuffle
                    .runtime()
                    .borrow()
                    .now_playing(),
            );
        }
    });

    let command_controller_for_repeat = command_controller;
    let view_for_repeat = view.clone();
    view.repeat_button.connect_clicked(move |_| {
        if command_controller_for_repeat
            .dispatch_succeeded(ApplicationCommand::Playback(PlaybackCommand::ToggleRepeat))
        {
            view_for_repeat.refresh(
                &command_controller_for_repeat
                    .runtime()
                    .borrow()
                    .now_playing(),
            );
        }
    });
}

fn install_artwork_click_handler(
    view: &NowPlayingView,
    command_controller: SharedCommandController,
) {
    let click = gtk::GestureClick::new();
    click.set_button(gtk::gdk::BUTTON_PRIMARY);
    let view_for_click = view.clone();
    click.connect_released(move |gesture, _n_press, _x, _y| {
        // Consume the click so it does not propagate to ancestors
        // (e.g. the titlebar drag-to-move handler). Without this,
        // clicking the artwork could also start a window drag in
        // some compositors.
        gesture.set_state(gtk::EventSequenceState::Claimed);
        let _ = view_for_click.handle_artwork_click(&command_controller);
    });
    view.artwork_inner_stack.add_controller(click);
}

fn install_lyrics_chip_handler(view: &NowPlayingView) {
    let view = view.clone();
    view.lyrics_chip.clone().connect_clicked(move |_| {
        let _ = view.open_artwork_lyrics_overlay(LyricsOverlayFace::Lyrics);
    });
}

fn install_refresh_timer(view: &NowPlayingView, runtime: SharedRuntime) {
    let view = view.clone();
    // The 1 Hz cadence here is doing double duty: it drives the
    // now-playing UI refresh (seek bar, time labels, MPRIS-adjacent
    // state) AND it is the heartbeat that lets the runtime accumulate
    // listened time toward the play threshold. Runtime accounting reads its
    // own monotonic clock, so a delayed/coalesced callback still records the
    // real interval. The two run together so that an attempt to disable one
    // (e.g. by detaching the now-playing panel) does not silently break
    // play-count tracking.
    glib::timeout_add_seconds_local(1, move || {
        let now_playing = {
            let mut runtime = runtime.borrow_mut();
            let _ = runtime.on_playback_tick();
            runtime.now_playing()
        };
        view.refresh(&now_playing);
        glib::ControlFlow::Continue
    });
}

fn metadata_box(
    title: &MarqueeLabel,
    artist_album: &MarqueeLabel,
    lyrics_chip: &gtk::Button,
) -> gtk::Box {
    let metadata = gtk::Box::new(gtk::Orientation::Vertical, 0);
    metadata.set_halign(gtk::Align::Center);
    metadata.set_valign(gtk::Align::Center);
    metadata.set_hexpand(true);

    // Title line with the Lyrics chip inline, immediately after the title.
    // The pair is centred as one unit; because the title marquee hugs its
    // text, the chip sits right at the title's trailing edge rather than
    // floating off at a fixed offset.
    let title_row = gtk::Box::new(gtk::Orientation::Horizontal, MARQUEE_CHIP_GAP);
    title_row.set_halign(gtk::Align::Center);
    title_row.set_valign(gtk::Align::Center);
    title_row.append(&title.widget());
    title_row.append(lyrics_chip);

    metadata.append(&title_row);
    metadata.append(&artist_album.widget());
    metadata
}

fn empty_state_view() -> gtk::Box {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
    container.set_hexpand(true);
    container.set_vexpand(true);

    let icon = gtk::Image::from_icon_name(APP_ID);
    icon.add_css_class("now-playing-empty-icon");
    icon.set_pixel_size(EMPTY_STATE_ICON_SIZE);
    icon.set_halign(gtk::Align::Center);
    icon.set_valign(gtk::Align::Center);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    container.append(&icon);
    container
}

fn install_hover_pause(
    area: &gtk::Box,
    title: &MarqueeLabel,
    artist_album: &MarqueeLabel,
    marquee_paused: Rc<Cell<bool>>,
) {
    let motion = gtk::EventControllerMotion::new();
    let title_for_enter = title.clone();
    let artist_album_for_enter = artist_album.clone();
    let marquee_paused_for_enter = marquee_paused.clone();
    motion.connect_enter(move |_motion, _x, _y| {
        marquee_paused_for_enter.set(true);
        title_for_enter.reset_to_start();
        artist_album_for_enter.reset_to_start();
    });

    motion.connect_leave(move |_motion| {
        marquee_paused.set(false);
    });
    area.add_controller(motion);
}

/// Build the shared chrome for a now-playing side control — a centred,
/// fixed-width column with a flat circular button above its time label —
/// around an already-built icon widget. The shuffle control passes a
/// [`ShuffleShineIcon`]; the repeat control passes a plain `gtk::Image`.
fn side_status_chrome(
    icon: &impl IsA<gtk::Widget>,
    tooltip: &str,
    time: &gtk::Label,
) -> (gtk::Box, gtk::Button) {
    let status = gtk::Box::new(gtk::Orientation::Vertical, 2);
    status.set_width_request(NOW_PLAYING_SIDE_WIDTH);
    status.set_halign(gtk::Align::Center);
    status.set_valign(gtk::Align::Center);

    let button = gtk::Button::new();
    button.add_css_class("now-playing-side-button");
    button.set_tooltip_text(Some(tooltip));
    button.set_halign(gtk::Align::Center);
    button.set_valign(gtk::Align::Center);
    button.set_child(Some(icon));

    status.append(&button);
    status.append(time);

    (status, button)
}

fn sync_playback_option_icon(icon: &gtk::Image, enabled: bool) {
    if enabled {
        icon.add_css_class("now-playing-side-icon-active");
    } else {
        icon.remove_css_class("now-playing-side-icon-active");
    }
}

/// Tri-state visual sync for the shuffle icon. Off is the muted glyph,
/// Pure is the solid accent glyph (sharing the active class with
/// Repeat), and Smart adds the periodic white reflection sweep owned by
/// [`ShuffleShineIcon`].
fn sync_shuffle_icon(icon: &ShuffleShineIcon, mode: ShuffleMode) {
    match mode {
        ShuffleMode::Off => icon.set_mode(false, false),
        ShuffleMode::Pure => icon.set_mode(true, false),
        ShuffleMode::Smart => icon.set_mode(true, true),
    }
}

fn time_label() -> gtk::Label {
    let label = gtk::Label::new(None);
    label.add_css_class("now-playing-time");
    label.set_halign(gtk::Align::Center);
    label.set_xalign(0.5);
    label
}

#[cfg(test)]
mod tests {
    use super::synced_lyrics_text;

    #[test]
    fn synced_lyrics_are_rendered_as_readable_plain_lines() {
        let lyrics =
            sustain_app_runtime::SyncedLyrics::parse_lrc("[00:01.50]Hello\n[00:03.00]World")
                .expect("parse synced lyrics");

        assert_eq!(synced_lyrics_text(&lyrics), "Hello\nWorld");
    }
}
