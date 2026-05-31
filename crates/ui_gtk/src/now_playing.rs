// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    rc::Rc,
    time::Duration,
};

use gtk::prelude::*;
use gtk::{cairo, gdk, glib};

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

mod model;
mod progress_hit_area;

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

#[derive(Clone)]
pub(crate) struct NowPlayingView {
    runtime: SharedRuntime,
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
    artwork_box: gtk::Box,
    /// Inner stack of three pages — `present` (the artwork itself),
    /// `missing` (the click-to-fetch icon), `fetching` (the spinner).
    /// Switching pages keeps the tile geometry stable even while the
    /// content swaps in and out.
    artwork_inner_stack: gtk::Stack,
    artwork_image: gtk::Image,
    artwork_spinner: gtk::Spinner,
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

#[derive(Clone)]
struct MarqueeLabel {
    root: gtk::Overlay,
    canvas: gtk::DrawingArea,
    draw_model: MarqueeDrawModel,
    x_position: Rc<Cell<f64>>,
    paused: Rc<Cell<bool>>,
}

#[derive(Clone)]
struct MarqueeDrawModel {
    text: Rc<RefCell<String>>,
    text_width: Rc<Cell<f64>>,
    x_position: Rc<Cell<f64>>,
    fade_active: Rc<Cell<bool>>,
    style: MarqueeTextStyle,
}

const EMPTY_STACK_NAME: &str = "no-track";
const LOADED_STACK_NAME: &str = "loaded";
const EMPTY_STATE_ICON_SIZE: i32 = 48;
const MARQUEE_EDGE_FADE_WIDTH: f64 = 28.0;
const MARQUEE_FRAME_MS: u64 = 33;
const MARQUEE_HEIGHT: i32 = 19;
const MARQUEE_LOOP_GAP: f64 = 48.0;
const MARQUEE_SPEED: f64 = 0.75;
const MARQUEE_VIEWPORT_WIDTH: i32 = 400;

impl NowPlayingView {
    pub(crate) fn new(
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

        let artwork_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
        artwork_box.add_css_class("now-playing-artwork");
        artwork_box.set_size_request(TITLEBAR_HEIGHT, TITLEBAR_HEIGHT);
        artwork_box.set_overflow(gtk::Overflow::Hidden);

        let artwork_image = gtk::Image::new();
        artwork_image.set_pixel_size(TITLEBAR_HEIGHT);
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
        artwork_inner_stack.set_hexpand(true);
        artwork_inner_stack.set_vexpand(true);
        artwork_inner_stack.add_named(&artwork_image, Some(ARTWORK_INNER_STACK_PRESENT));
        artwork_inner_stack.add_named(&artwork_missing_icon, Some(ARTWORK_INNER_STACK_MISSING));
        artwork_inner_stack.add_named(&artwork_spinner, Some(ARTWORK_INNER_STACK_FETCHING));
        artwork_inner_stack.set_visible_child_name(ARTWORK_INNER_STACK_MISSING);
        artwork_box.append(&artwork_inner_stack);

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
        let title = MarqueeLabel::new("now-playing-title", marquee_paused.clone());
        let artist_album = MarqueeLabel::new("now-playing-artist", marquee_paused.clone());
        let metadata = metadata_box(&title, &artist_album);

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
        if name == ARTWORK_INNER_STACK_MISSING {
            self.artwork_box.add_css_class(ARTWORK_CLICKABLE_CLASS);
            self.artwork_box
                .set_tooltip_text(Some(ARTWORK_MISSING_TOOLTIP));
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
        // We only act when the missing-state icon is visible. Any
        // other state means there is artwork to display or a fetch
        // is already running — clicks become no-ops in both cases.
        if self.artwork_inner_stack.visible_child_name().as_deref()
            != Some(ARTWORK_INNER_STACK_MISSING)
        {
            return false;
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
            sync_shuffle_icon(&self.shuffle_icon, now_playing.options.shuffle_mode);
            sync_playback_option_icon(&self.repeat_icon, now_playing.options.repeat_enabled());
            return;
        };

        self.stack.set_visible_child_name(LOADED_STACK_NAME);

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
    view.artwork_box.add_controller(click);
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

fn metadata_box(title: &MarqueeLabel, artist_album: &MarqueeLabel) -> gtk::Box {
    let metadata = gtk::Box::new(gtk::Orientation::Vertical, 0);
    metadata.set_halign(gtk::Align::Center);
    metadata.set_valign(gtk::Align::Center);
    metadata.set_hexpand(true);
    metadata.append(&title.widget());
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

impl MarqueeLabel {
    fn new(css_class: &str, paused: Rc<Cell<bool>>) -> Self {
        let width = MARQUEE_VIEWPORT_WIDTH;
        let root = gtk::Overlay::new();
        root.add_css_class("marquee-label");
        root.set_size_request(width, MARQUEE_HEIGHT);
        root.set_hexpand(false);
        root.set_halign(gtk::Align::Center);
        root.set_overflow(gtk::Overflow::Hidden);

        let canvas = gtk::DrawingArea::new();
        canvas.add_css_class(css_class);
        canvas.set_content_width(width);
        canvas.set_content_height(MARQUEE_HEIGHT);
        canvas.set_size_request(width, MARQUEE_HEIGHT);
        canvas.set_hexpand(false);
        canvas.set_halign(gtk::Align::Center);
        canvas.set_overflow(gtk::Overflow::Hidden);

        let text = Rc::new(RefCell::new(String::new()));
        let text_width = Rc::new(Cell::new(0.0));
        let x_position = Rc::new(Cell::new(0.0));
        let fade_active = Rc::new(Cell::new(false));
        let draw_model = MarqueeDrawModel {
            text,
            text_width,
            x_position: x_position.clone(),
            fade_active,
            style: MarqueeTextStyle::from_css_class(css_class),
        };
        install_marquee_draw_func(&canvas, &draw_model);

        root.set_child(Some(&canvas));

        let marquee = Self {
            root,
            canvas,
            draw_model,
            x_position,
            paused,
        };
        marquee.install_animation();
        marquee
    }

    fn widget(&self) -> gtk::Overlay {
        self.root.clone()
    }

    fn set_text(&self, text: &str) {
        if self.draw_model.text.borrow().as_str() == text {
            return;
        }

        self.draw_model.text.replace(text.to_owned());
        self.reset_to_start();
        self.canvas.queue_draw();
    }

    fn reset_to_start(&self) {
        self.x_position.set(0.0);
        self.canvas.queue_draw();
    }

    fn install_animation(&self) {
        let marquee = self.clone();
        glib::timeout_add_local(Duration::from_millis(MARQUEE_FRAME_MS), move || {
            marquee.advance();
            glib::ControlFlow::Continue
        });
    }

    fn advance(&self) {
        let viewport_width = self.canvas.width();
        let text_width = self.draw_model.text_width.get();
        let overflows = viewport_width > 0 && text_width > f64::from(viewport_width) + 1.0;
        let should_scroll = overflows && !self.paused.get();

        self.draw_model.fade_active.set(should_scroll);

        if !should_scroll {
            self.reset_to_start();
            return;
        }

        let mut x_position = self.x_position.get() - MARQUEE_SPEED;
        if x_position <= -text_width - MARQUEE_LOOP_GAP {
            x_position = 0.0;
        }

        self.x_position.set(x_position);
        self.canvas.queue_draw();
    }
}

fn install_marquee_draw_func(canvas: &gtk::DrawingArea, draw_model: &MarqueeDrawModel) {
    let draw_model = draw_model.clone();

    canvas.set_draw_func(move |canvas, context, width, height| {
        draw_marquee_text(canvas, context, width, height, &draw_model);
    });
}

#[derive(Clone, Copy)]
enum MarqueeTextStyle {
    Title,
    Secondary,
}

impl MarqueeTextStyle {
    fn from_css_class(css_class: &str) -> Self {
        if css_class == "now-playing-title" {
            Self::Title
        } else {
            Self::Secondary
        }
    }

    fn font_size(self) -> f64 {
        match self {
            Self::Title => 14.0,
            Self::Secondary => 12.0,
        }
    }

    fn font_weight(self) -> cairo::FontWeight {
        match self {
            Self::Title => cairo::FontWeight::Bold,
            Self::Secondary => cairo::FontWeight::Normal,
        }
    }

    fn alpha(self) -> f64 {
        match self {
            Self::Title => 1.0,
            Self::Secondary => 0.58,
        }
    }
}

fn draw_marquee_text(
    canvas: &gtk::DrawingArea,
    context: &cairo::Context,
    width: i32,
    height: i32,
    draw_model: &MarqueeDrawModel,
) {
    let text = draw_model.text.borrow();
    if text.is_empty() {
        draw_model.text_width.set(0.0);
        return;
    }

    let _result = context.save();
    context.rectangle(0.0, 0.0, f64::from(width), f64::from(height));
    context.clip();
    context.select_font_face(
        "Sans",
        cairo::FontSlant::Normal,
        draw_model.style.font_weight(),
    );
    context.set_font_size(draw_model.style.font_size());
    set_text_source(
        context,
        &canvas.color(),
        draw_model.style.alpha(),
        f64::from(width),
        draw_model.fade_active.get(),
    );

    let Ok(extents) = context.text_extents(&text) else {
        let _result = context.restore();
        return;
    };
    let measured_width = extents.x_advance().max(0.0);
    draw_model.text_width.set(measured_width);

    let x = if measured_width > f64::from(width) + 1.0 {
        draw_model.x_position.get()
    } else {
        (f64::from(width) - measured_width) / 2.0
    };
    let y = (f64::from(height) - extents.height()) / 2.0 - extents.y_bearing();
    draw_text_at(context, &text, x, y);

    if measured_width > f64::from(width) + 1.0 {
        draw_text_at(context, &text, x + measured_width + MARQUEE_LOOP_GAP, y);
    }

    let _result = context.restore();
}

fn set_context_color(context: &cairo::Context, color: &gtk::gdk::RGBA, alpha: f64) {
    context.set_source_rgba(
        f64::from(color.red()),
        f64::from(color.green()),
        f64::from(color.blue()),
        f64::from(color.alpha()) * alpha,
    );
}

fn set_text_source(
    context: &cairo::Context,
    color: &gtk::gdk::RGBA,
    alpha: f64,
    width: f64,
    fade_active: bool,
) {
    if !fade_active || width <= 0.0 {
        set_context_color(context, color, alpha);
        return;
    }

    let gradient = cairo::LinearGradient::new(0.0, 0.0, width, 0.0);
    let red = f64::from(color.red());
    let green = f64::from(color.green());
    let blue = f64::from(color.blue());
    let alpha = f64::from(color.alpha()) * alpha;
    let fade_stop = (MARQUEE_EDGE_FADE_WIDTH / width).clamp(0.0, 0.5);

    gradient.add_color_stop_rgba(0.0, red, green, blue, 0.0);
    gradient.add_color_stop_rgba(fade_stop, red, green, blue, alpha);
    gradient.add_color_stop_rgba(1.0 - fade_stop, red, green, blue, alpha);
    gradient.add_color_stop_rgba(1.0, red, green, blue, 0.0);
    let _result = context.set_source(&gradient);
}

fn draw_text_at(context: &cairo::Context, text: &str, x: f64, y: f64) {
    context.move_to(x, y);
    let _result = context.show_text(text);
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
