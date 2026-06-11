// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
};

use gtk::glib;
use gtk::prelude::*;
use sustain_app_runtime::{ApplicationCommand, PlaybackCommand, PlaybackState, VolumePercent};

use super::{
    MEDIA_ICON_SIZE, NOW_PLAYING_HORIZONTAL_MARGIN, PlaybackChangedCallback, SharedRuntime,
    TITLEBAR_CONTROL_HEIGHT, TITLEBAR_HEIGHT, TITLEBAR_LEFT_PADDING, TITLEBAR_RIGHT_PADDING,
    VOLUME_MAGNET_THRESHOLD, VOLUME_WIDTH, command_controller::SharedCommandController,
};

/// Debounce window for persisting the playback volume to TOML.
///
/// `value-changed` on the slider fires once per pixel of mouse motion during
/// a drag. The TOML store rewrites the entire settings file on each save, so
/// we coalesce all rapid changes into a single write 250 ms after the slider
/// stops moving. A pending save is flushed synchronously on window close so a
/// last-second adjustment is never lost.
const VOLUME_SAVE_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(250);

pub(crate) type SearchChangedCallback = Rc<dyn Fn(String)>;
type VolumeSaveCallback = Rc<dyn Fn(VolumePercent)>;
type VolumeSaveCallbackSlot = Rc<RefCell<Option<VolumeSaveCallback>>>;

#[derive(Clone)]
pub(crate) struct Titlebar {
    pub(crate) widget: gtk::WindowHandle,
    pub(crate) play_pause_icon: gtk::Image,
    previous: gtk::Button,
    play_pause: gtk::Button,
    next: gtk::Button,
    volume: gtk::Scale,
    search: gtk::SearchEntry,
    volume_pending_save: Rc<RefCell<Option<glib::SourceId>>>,
    volume_save_callback: VolumeSaveCallbackSlot,
}

pub(crate) fn build_titlebar(now_playing: gtk::Box, initial_volume: VolumePercent) -> Titlebar {
    let topbar = gtk::CenterBox::new();
    topbar.add_css_class("titlebar");
    topbar.set_hexpand(true);
    topbar.set_height_request(TITLEBAR_HEIGHT);

    let previous = media_icon_button("media-skip-backward-symbolic", "Previous");
    let play_pause_icon = gtk::Image::from_icon_name("media-playback-start-symbolic");
    play_pause_icon.set_pixel_size(MEDIA_ICON_SIZE);
    let play_pause = media_icon_button_from_image(&play_pause_icon, "Play/Pause");
    let next = media_icon_button(
        "media-skip-forward-symbolic",
        "Next, right-click to display the queue",
    );
    set_titlebar_control_height(&previous);
    set_titlebar_control_height(&play_pause);
    set_titlebar_control_height(&next);

    let volume = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 1.0, 0.01);
    volume.add_css_class("volume-slider");
    volume.set_value(initial_volume.as_scalar());
    volume.set_width_request(VOLUME_WIDTH);
    volume.set_height_request(TITLEBAR_CONTROL_HEIGHT);
    volume.set_draw_value(false);
    volume.set_tooltip_text(Some("Volume"));

    let low_volume_icon = volume_icon("audio-volume-low-symbolic", "Low volume");
    let high_volume_icon = volume_icon("audio-volume-high-symbolic", "High volume");

    let volume_controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    volume_controls.set_valign(gtk::Align::Center);
    volume_controls.append(&low_volume_icon);
    volume_controls.append(&volume);
    volume_controls.append(&high_volume_icon);

    let search = gtk::SearchEntry::new();
    search.add_css_class("topbar-search");
    search.set_placeholder_text(Some("Search"));
    search.set_width_chars(24);
    search.set_valign(gtk::Align::Center);
    // GTK's default search delay (150ms) fires partway through a typed word
    // for normal typing speeds; 300ms lets a typed word land before the
    // first filter pass without feeling sluggish on a single quick keystroke.
    search.set_search_delay(300);

    let window_controls = gtk::WindowControls::new(gtk::PackType::End);
    window_controls.set_valign(gtk::Align::Center);

    let left_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    left_controls.set_valign(gtk::Align::Center);
    left_controls.set_margin_start(NOW_PLAYING_HORIZONTAL_MARGIN);
    left_controls.set_margin_end(NOW_PLAYING_HORIZONTAL_MARGIN);
    left_controls.append(&previous);
    left_controls.append(&play_pause);
    left_controls.append(&next);
    left_controls.append(&horizontal_spacer(TITLEBAR_LEFT_PADDING / 2));
    left_controls.append(&volume_controls);

    let right_controls = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    right_controls.set_valign(gtk::Align::Center);
    right_controls.append(&search);
    right_controls.append(&horizontal_spacer(TITLEBAR_RIGHT_PADDING));
    right_controls.append(&window_controls);

    topbar.set_start_widget(Some(&left_controls));
    topbar.set_center_widget(Some(&now_playing));
    topbar.set_end_widget(Some(&right_controls));

    let handle = gtk::WindowHandle::new();
    handle.set_child(Some(&topbar));
    Titlebar {
        widget: handle,
        previous,
        play_pause,
        play_pause_icon,
        next,
        volume,
        search,
        volume_pending_save: Rc::new(RefCell::new(None)),
        volume_save_callback: Rc::new(RefCell::new(None)),
    }
}

/// Wires the topbar SearchEntry to the supplied callback. Fires on every
/// keystroke with the trimmed query text (`""` when empty/cleared). The
/// callback runs synchronously: filtering ~10k in-memory tracks is
/// microseconds, so no debounce is needed in the first cut. If real
/// libraries reveal jank we wrap this in a `glib::timeout_add_local_once`
/// the same way [`schedule_volume_save`] does.
pub(crate) fn connect_titlebar_search(titlebar: &Titlebar, callback: SearchChangedCallback) {
    titlebar.search.connect_search_changed(move |entry| {
        callback(entry.text().trim().to_owned());
    });
}

pub(crate) fn connect_titlebar_playback_controls(
    titlebar: &Titlebar,
    runtime: &SharedRuntime,
    command_controller: SharedCommandController,
    playback_changed: PlaybackChangedCallback,
) {
    let command_controller_for_previous = command_controller.clone();
    let playback_changed_for_previous = playback_changed.clone();
    titlebar.previous.connect_clicked(move |_| {
        if command_controller_for_previous.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::PlayPreviousTrack,
        )) {
            playback_changed_for_previous();
        }
    });

    // The Play/Pause button is wired separately in
    // [`connect_titlebar_play_button`] — its behaviour depends on the
    // visible view (cold-starting that view's first track when nothing
    // is loaded), which only exists later in window construction.

    let command_controller_for_next = command_controller.clone();
    let playback_changed_for_next = playback_changed.clone();
    titlebar.next.connect_clicked(move |_| {
        // The Next button is the canonical user-initiated skip surface:
        // it records a skip on the currently playing track (when the
        // play threshold has not yet been reached) before advancing.
        // EOS auto-advance uses PlaybackCommand::PlayNextTrack instead
        // and never increments skip_count.
        if command_controller_for_next.dispatch_succeeded(ApplicationCommand::Playback(
            PlaybackCommand::SkipCurrentTrack,
        )) {
            playback_changed_for_next();
        }
    });

    // The save callback rewrites settings.toml; we wrap it in the debounce
    // scheduler below so a drag fires it once, not hundreds of times.
    let save_volume: Rc<dyn Fn(VolumePercent)> = {
        let runtime = runtime.clone();
        Rc::new(move |volume| {
            let _ = runtime.borrow_mut().save_playback_volume(volume);
        })
    };
    *titlebar.volume_save_callback.borrow_mut() = Some(save_volume);

    let volume_syncing = Rc::new(Cell::new(false));
    let command_controller_for_volume = command_controller.clone();
    let volume_syncing_for_change = volume_syncing.clone();
    let volume_pending_save = titlebar.volume_pending_save.clone();
    let volume_save_callback = titlebar.volume_save_callback.clone();
    titlebar.volume.connect_value_changed(move |volume| {
        if volume_syncing_for_change.get() {
            return;
        }

        let value = magnetized_volume_value(volume.value());
        if (volume.value() - value).abs() > f64::EPSILON {
            volume_syncing_for_change.set(true);
            volume.set_value(value);
            volume_syncing_for_change.set(false);
        }

        let settled_volume = VolumePercent::from_scalar(value);

        // Audio path: dispatch immediately so the speaker reacts to the
        // slider in real time. No persistence here.
        let _result = command_controller_for_volume.dispatch(ApplicationCommand::Playback(
            PlaybackCommand::SetVolume(settled_volume),
        ));

        // Persistence path: replace any pending save with one scheduled for
        // VOLUME_SAVE_DEBOUNCE in the future, so a continuous drag collapses
        // to one TOML write.
        schedule_volume_save(
            settled_volume,
            volume_pending_save.clone(),
            volume_save_callback.clone(),
        );
    });

    // The startup volume value is whatever was persisted (or the default if
    // no settings file exists yet). Push it through the audio path so the
    // playback service is aligned with the slider before the first track
    // plays.
    let initial_volume = runtime.borrow().settings().playback.volume;
    let _result = command_controller.dispatch(ApplicationCommand::Playback(
        PlaybackCommand::SetVolume(initial_volume),
    ));
}

/// Wire the top-bar Play/Pause button.
///
/// Unlike Previous / Next, the Play button is not a fixed transport
/// command: when a track is already loaded it toggles play/pause, but on
/// a cold start (nothing loaded) it begins playback from the currently
/// visible view. That decision needs the content stack, which only
/// exists later in window construction, so the caller supplies a
/// ready-made closure that owns the full "toggle or cold-start, then
/// refresh" behaviour and this function simply binds it to the click.
pub(crate) fn connect_titlebar_play_button(titlebar: &Titlebar, on_play_pause: Rc<dyn Fn()>) {
    titlebar
        .play_pause
        .connect_clicked(move |_| on_play_pause());
}

fn schedule_volume_save(
    volume: VolumePercent,
    pending_save: Rc<RefCell<Option<glib::SourceId>>>,
    save_callback: VolumeSaveCallbackSlot,
) {
    if let Some(previous) = pending_save.borrow_mut().take() {
        previous.remove();
    }
    let pending_save_clear = pending_save.clone();
    let save_callback_for_timer = save_callback.clone();
    let source_id = glib::timeout_add_local_once(VOLUME_SAVE_DEBOUNCE, move || {
        pending_save_clear.borrow_mut().take();
        let Some(callback) = save_callback_for_timer.borrow().as_ref().cloned() else {
            return;
        };
        callback(volume);
    });
    *pending_save.borrow_mut() = Some(source_id);
}

impl Titlebar {
    /// Move keyboard focus to the topbar search field and select whatever
    /// text is currently in it. Pressing the Ctrl+F accelerator should
    /// behave like a standard GNOME find action: a fresh keystroke
    /// replaces any prior query rather than appending to it.
    pub(crate) fn focus_search(&self) {
        self.search.grab_focus();
        self.search.select_region(0, -1);
    }

    pub(crate) fn search_text(&self) -> String {
        self.search.text().trim().to_owned()
    }

    /// The transport Next button. Exposed so the play-queue popover can
    /// parent itself to it and anchor its arrow there — the queue is
    /// hierarchically the "what plays after Next" surface, so it hangs off
    /// the Next control rather than carrying a dedicated button.
    pub(crate) fn next_button(&self) -> gtk::Button {
        self.next.clone()
    }

    pub(crate) fn set_search_text(&self, text: &str) {
        self.search.set_text(text);
    }

    /// Enable or disable the Play/Pause button. Disabled when the library
    /// is empty and nothing is loaded, so a press is never a silent
    /// no-op (issue #60).
    pub(crate) fn set_play_pause_sensitive(&self, sensitive: bool) {
        self.play_pause.set_sensitive(sensitive);
    }

    /// Cancel any pending debounced volume save and run it now. Invoked from
    /// the window-close path so an adjustment made within
    /// [`VOLUME_SAVE_DEBOUNCE`] of shutdown is still persisted.
    pub(crate) fn flush_pending_volume_save(&self) {
        let Some(source_id) = self.volume_pending_save.borrow_mut().take() else {
            return;
        };
        source_id.remove();
        let Some(callback) = self.volume_save_callback.borrow().as_ref().cloned() else {
            return;
        };
        callback(VolumePercent::from_scalar(self.volume.value()));
    }
}

pub(crate) fn sync_play_pause_icon(icon: &gtk::Image, state: &PlaybackState) {
    match state {
        // `Loading` is a track the user just started that is still prerolling;
        // it shows the pause glyph alongside `Playing` so the control reads as
        // "playing, click to pause" the instant playback is requested. The
        // backend reaches `Playing` asynchronously and nothing re-runs this
        // sync between the two, so rendering `Loading` as the start glyph would
        // leave the button stuck on play for the whole track (and flash play
        // for a frame on every skip). This mirrors the MPRIS mapping, which
        // already reports `Loading` as "Playing".
        PlaybackState::Playing { .. } | PlaybackState::Loading { .. } => {
            icon.set_icon_name(Some("media-playback-pause-symbolic"));
        }
        PlaybackState::Paused { .. } | PlaybackState::Stopped => {
            icon.set_icon_name(Some("media-playback-start-symbolic"));
        }
    }
}

fn horizontal_spacer(width: i32) -> gtk::Box {
    let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    spacer.set_width_request(width);
    spacer
}

fn media_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.set_pixel_size(MEDIA_ICON_SIZE);

    media_icon_button_from_image(&icon, tooltip)
}

fn media_icon_button_from_image(icon: &gtk::Image, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::new();
    button.set_child(Some(icon));
    button.set_tooltip_text(Some(tooltip));
    button.add_css_class("flat");
    button.add_css_class("media-control");
    button.set_focus_on_click(false);
    set_titlebar_control_height(&button);
    button
}

fn volume_icon(icon_name: &str, tooltip: &str) -> gtk::Image {
    let icon = gtk::Image::from_icon_name(icon_name);
    icon.add_css_class("volume-icon");
    icon.set_pixel_size(16);
    icon.set_tooltip_text(Some(tooltip));
    icon
}

fn magnetized_volume_value(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }

    let value = value.clamp(0.0, 1.0);
    if (VOLUME_MAGNET_THRESHOLD..1.0).contains(&value) {
        1.0
    } else {
        value
    }
}

fn set_titlebar_control_height(control: &gtk::Button) {
    control.set_height_request(TITLEBAR_CONTROL_HEIGHT);
}

#[cfg(test)]
mod tests {
    use super::magnetized_volume_value;

    #[test]
    fn volume_values_in_top_guard_range_snap_to_unity() {
        assert_eq!(magnetized_volume_value(0.90), 1.0);
        assert_eq!(magnetized_volume_value(0.95), 1.0);
        assert_eq!(magnetized_volume_value(0.999), 1.0);
    }

    #[test]
    fn volume_values_below_guard_range_are_preserved() {
        assert_eq!(magnetized_volume_value(0.899), 0.899);
    }

    #[test]
    fn volume_values_are_clamped_to_slider_range() {
        assert_eq!(magnetized_volume_value(-1.0), 0.0);
        assert_eq!(magnetized_volume_value(2.0), 1.0);
        assert_eq!(magnetized_volume_value(f64::NAN), 0.0);
    }
}
