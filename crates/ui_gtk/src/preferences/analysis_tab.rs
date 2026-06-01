// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib::Propagation;
use gtk::prelude::*;
use sustain_app_runtime::AnalysisSettings;
use sustain_app_runtime::priority::resolve_worker_count;

use super::super::{
    ApplicationCommand, BackgroundResourceUsage, command_controller::SharedCommandController,
};
use super::switch_row::build_switch_row;
use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};

pub(super) fn build(command_controller: SharedCommandController) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 18);
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let initial = command_controller.runtime().borrow().settings().analysis;

    let bpm_row = build_switch_row(
        "BPM detection",
        "Runs in the background on tracks missing a BPM value. \
         Never modifies tracks that already have one.",
        initial.bpm,
    );
    let key_row = build_switch_row(
        "Key detection",
        "Runs in the background on tracks missing a musical key. \
         Never modifies tracks that already have one.",
        initial.key,
    );
    let audio_row = build_switch_row(
        "Audio analysis",
        "Decodes each track once to produce the color waveforms and the perceptual \
         features Smart Shuffle uses to match transitions, and reads BPM and key off \
         the same decode — so turning this on enables and locks both. The heaviest \
         pass; runs in the background on tracks that are missing it.",
        initial.audio,
    );

    // Audio analysis produces BPM, key, waveforms, and the perceptual
    // features from one decode, so while it is on the BPM and Key
    // switches are forced on and locked: the user must not be able to
    // switch off a value the audio pass is already computing. A shared
    // `syncing` flag lets the audio handler drive the BPM/Key switches
    // programmatically without re-entering their own change handlers.
    let syncing = Rc::new(Cell::new(false));
    wire_capability_switch(
        &bpm_row.switch,
        command_controller.clone(),
        CapabilityFlag::Bpm,
        syncing.clone(),
    );
    wire_capability_switch(
        &key_row.switch,
        command_controller.clone(),
        CapabilityFlag::Key,
        syncing.clone(),
    );
    wire_audio_switch(
        &audio_row.switch,
        &bpm_row.switch,
        &key_row.switch,
        command_controller.clone(),
        syncing,
    );

    // Reflect the locked state for the initial settings (already
    // normalized on load, so BPM/Key read as on whenever audio is).
    bpm_row
        .switch
        .set_sensitive(!bpm_switch_state(initial).locked);
    key_row
        .switch
        .set_sensitive(!key_switch_state(initial).locked);

    content.append(&bpm_row.container);
    content.append(&key_row.container);
    content.append(&audio_row.container);

    // Separator + resource-usage slider. The separator keeps the
    // "what to compute" toggles visually distinct from the "how
    // aggressively to compute" knob beneath them.
    let separator = gtk::Separator::new(gtk::Orientation::Horizontal);
    separator.add_css_class("preference-section-separator");
    content.append(&separator);

    content.append(&build_resource_usage_slider(command_controller));

    content.upcast()
}

#[derive(Clone, Copy)]
enum CapabilityFlag {
    Bpm,
    Key,
}

/// Visual state of a BPM/Key switch derived from the analysis settings:
/// shown `active` per the (already-normalized) flag, and `locked`
/// (insensitive) whenever audio analysis is on — because the audio pass
/// yields BPM and key too, so the user must not be able to switch them
/// off underneath it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct SwitchState {
    active: bool,
    locked: bool,
}

fn bpm_switch_state(analysis: AnalysisSettings) -> SwitchState {
    SwitchState {
        active: analysis.bpm,
        locked: analysis.audio,
    }
}

fn key_switch_state(analysis: AnalysisSettings) -> SwitchState {
    SwitchState {
        active: analysis.key,
        locked: analysis.audio,
    }
}

/// Wire a BPM or Key switch. User toggles dispatch the matching flag;
/// programmatic changes made by the audio handler (while `syncing` is
/// set) are ignored, since they are not user gestures.
fn wire_capability_switch(
    switch: &gtk::Switch,
    command_controller: SharedCommandController,
    flag: CapabilityFlag,
    syncing: Rc<Cell<bool>>,
) {
    switch.connect_state_set(move |_switch, requested_state| {
        if syncing.get() {
            return Propagation::Proceed;
        }
        let mut settings = command_controller.runtime().borrow().settings().clone();
        match flag {
            CapabilityFlag::Bpm => settings.analysis.bpm = requested_state,
            CapabilityFlag::Key => settings.analysis.key = requested_state,
        }
        if command_controller
            .dispatch(ApplicationCommand::UpdateSettings(settings))
            .is_ok()
        {
            Propagation::Proceed
        } else {
            Propagation::Stop
        }
    });
}

/// Wire the Audio analysis switch. Toggling it dispatches the new
/// `audio` flag, then drives the BPM/Key switches to match and locks them
/// while audio is on. Audio is the umbrella background job: enabling it
/// forces BPM + key on, and disabling it disables all three capabilities
/// so stopping the expensive pass cannot accidentally start two cheaper
/// queues. The `syncing` guard keeps the programmatic `set_active` calls
/// from re-entering the BPM/Key handlers.
fn wire_audio_switch(
    audio_switch: &gtk::Switch,
    bpm_switch: &gtk::Switch,
    key_switch: &gtk::Switch,
    command_controller: SharedCommandController,
    syncing: Rc<Cell<bool>>,
) {
    let bpm_switch = bpm_switch.clone();
    let key_switch = key_switch.clone();
    audio_switch.connect_state_set(move |_switch, audio_on| {
        let mut settings = command_controller.runtime().borrow().settings().clone();
        settings.analysis = audio_switch_settings(settings.analysis, audio_on);
        let bpm_state = bpm_switch_state(settings.analysis);
        let key_state = key_switch_state(settings.analysis);
        let dispatched = command_controller
            .dispatch(ApplicationCommand::UpdateSettings(settings))
            .is_ok();
        // Mirror the (possibly forced) BPM/Key state onto their switches
        // without re-entering their handlers, then lock/unlock them.
        syncing.set(true);
        bpm_switch.set_active(bpm_state.active);
        key_switch.set_active(key_state.active);
        syncing.set(false);
        bpm_switch.set_sensitive(!bpm_state.locked);
        key_switch.set_sensitive(!key_state.locked);
        if dispatched {
            Propagation::Proceed
        } else {
            Propagation::Stop
        }
    });
}

fn audio_switch_settings(current: AnalysisSettings, audio_on: bool) -> AnalysisSettings {
    if audio_on {
        AnalysisSettings {
            audio: true,
            ..current
        }
        .normalized()
    } else {
        AnalysisSettings::default()
    }
}

/// Slider with three discrete stops (Innocuous / Balanced /
/// Aggressive) controlling how many worker threads the background
/// analysis scheduler spawns and at what nice + ionice priority.
/// `snap_to_ticks` is enabled so the user cannot stop the slider
/// between the labelled positions.
fn build_resource_usage_slider(command_controller: SharedCommandController) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    container.add_css_class("preference-slider-row");

    let header = gtk::Label::new(Some("Background resource usage"));
    header.set_xalign(0.0);
    container.append(&header);

    // GtkScale with three discrete tick stops at 0/1/2. The numeric
    // values are an internal mapping to BackgroundResourceUsage —
    // the user only ever sees the three text labels printed below
    // (in `mark_label_row`, not as GtkScale marks: GTK centres each
    // mark's text on its tick, which makes the end-cap labels
    // overflow the scale at positions 0 and 2).
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 2.0, 1.0);
    scale.set_round_digits(0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.add_mark(0.0, gtk::PositionType::Bottom, None);
    scale.add_mark(1.0, gtk::PositionType::Bottom, None);
    scale.add_mark(2.0, gtk::PositionType::Bottom, None);
    // Snap-to-tick: `round_digits = 0` combined with the integer
    // tick step means GTK rounds the value as the handle moves, so
    // the user cannot land between marks.

    let initial = command_controller
        .runtime()
        .borrow()
        .settings()
        .background_jobs
        .resource_usage;
    scale.set_value(usage_to_value(initial));

    container.append(&scale);

    // Mark labels in their own row. The three children all carry
    // `hexpand = true` so they share the width equally, and their
    // individual `halign` settings push each label toward the slider
    // tick it belongs to (Start/Center/End ⇒ tick positions 0/1/2).
    // Using GtkScale's built-in mark labels would centre each label
    // on its tick, and the labels at the 0 and 2 endpoints would
    // overflow the scale's bounds.
    let mark_label_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    let innocuous_label = mark_label("Innocuous", gtk::Align::Start);
    let balanced_label = mark_label("Balanced", gtk::Align::Center);
    let aggressive_label = mark_label("Aggressive", gtk::Align::End);
    mark_label_row.append(&innocuous_label);
    mark_label_row.append(&balanced_label);
    mark_label_row.append(&aggressive_label);
    container.append(&mark_label_row);

    // Caption: "uses N of M cores on this machine". Reflects the
    // currently-selected preset; updates on every slider move so the
    // user can see the worker count change as they drag.
    let caption = gtk::Label::new(None);
    caption.add_css_class("preference-helper");
    caption.set_xalign(0.0);
    caption.set_wrap(true);
    caption.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    caption.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    caption.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    caption.set_text(&caption_text(initial));
    container.append(&caption);

    let scale_controller = command_controller.clone();
    let caption_for_callback = caption.clone();
    scale.connect_value_changed(move |s| {
        let usage = value_to_usage(s.value());
        // Update the caption immediately even before the runtime
        // accepts the command — the slider's purpose is to give the
        // user a preview of "what does this preset mean on my
        // machine".
        caption_for_callback.set_text(&caption_text(usage));
        let mut settings = scale_controller.runtime().borrow().settings().clone();
        if settings.background_jobs.resource_usage == usage {
            return;
        }
        settings.background_jobs.resource_usage = usage;
        let _ = scale_controller.dispatch(ApplicationCommand::UpdateSettings(settings));
    });

    container.upcast()
}

/// One of the three tick labels under the slider. `align` controls
/// which edge of the label's allocated cell the text sits on; combined
/// with `hexpand = true` on every label, the row splits the width
/// equally and the text ends up under its slider tick (Start under
/// position 0, Center under 1, End under 2).
fn mark_label(text: &str, align: gtk::Align) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_halign(align);
    label.set_hexpand(true);
    label
}

fn usage_to_value(usage: BackgroundResourceUsage) -> f64 {
    match usage {
        BackgroundResourceUsage::Innocuous => 0.0,
        BackgroundResourceUsage::Balanced => 1.0,
        BackgroundResourceUsage::Aggressive => 2.0,
    }
}

/// Round the slider's f64 value to the nearest tick and map it to a
/// `BackgroundResourceUsage`. Values outside the 0..=2 range collapse
/// to the nearest endpoint — this is defensive: GtkScale's `set_range`
/// bounds the value, but a future style tweak (different range) must
/// not silently fall off the enum.
fn value_to_usage(value: f64) -> BackgroundResourceUsage {
    let snapped = value.round() as i32;
    match snapped {
        n if n <= 0 => BackgroundResourceUsage::Innocuous,
        1 => BackgroundResourceUsage::Balanced,
        _ => BackgroundResourceUsage::Aggressive,
    }
}

fn caption_text(usage: BackgroundResourceUsage) -> String {
    let workers = resolve_worker_count(usage);
    let total = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let plural = if workers == 1 { "core" } else { "cores" };
    format!("Uses {workers} of {total} {plural} on this machine.")
}

#[cfg(test)]
mod tests {
    use super::{
        AnalysisSettings, BackgroundResourceUsage, SwitchState, audio_switch_settings,
        bpm_switch_state, key_switch_state, usage_to_value, value_to_usage,
    };

    #[test]
    fn audio_locks_the_bpm_and_key_switches() {
        // Audio off: each switch reflects its own flag and stays editable.
        let off = AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
        };
        assert_eq!(
            bpm_switch_state(off),
            SwitchState {
                active: true,
                locked: false
            }
        );
        assert_eq!(
            key_switch_state(off),
            SwitchState {
                active: false,
                locked: false
            }
        );

        // Audio on (settings are normalized to all-true): both switches
        // are forced active and locked — the user cannot opt out of a
        // value the audio pass computes.
        let on = AnalysisSettings {
            bpm: true,
            key: true,
            audio: true,
        };
        assert_eq!(
            bpm_switch_state(on),
            SwitchState {
                active: true,
                locked: true
            }
        );
        assert_eq!(
            key_switch_state(on),
            SwitchState {
                active: true,
                locked: true
            }
        );
    }

    #[test]
    fn disabling_audio_disables_all_background_analysis_capabilities() {
        assert_eq!(
            audio_switch_settings(
                AnalysisSettings {
                    bpm: true,
                    key: true,
                    audio: true,
                },
                false,
            ),
            AnalysisSettings::default()
        );
        assert_eq!(
            audio_switch_settings(AnalysisSettings::default(), true),
            AnalysisSettings {
                bpm: true,
                key: true,
                audio: true,
            }
        );
    }

    #[test]
    fn slider_values_round_trip_through_usage() {
        for usage in [
            BackgroundResourceUsage::Innocuous,
            BackgroundResourceUsage::Balanced,
            BackgroundResourceUsage::Aggressive,
        ] {
            assert_eq!(value_to_usage(usage_to_value(usage)), usage);
        }
    }

    #[test]
    fn slider_snaps_between_ticks() {
        // Values between the discrete stops snap to the nearest tick
        // (GtkScale draws the handle on a tick, but the underlying
        // value is still an f64 from the model). Defensive in case
        // someone replaces the scale with one that does not round.
        assert_eq!(
            value_to_usage(0.4),
            BackgroundResourceUsage::Innocuous,
            "below 0.5 should round down to Innocuous"
        );
        assert_eq!(value_to_usage(0.6), BackgroundResourceUsage::Balanced);
        assert_eq!(value_to_usage(1.4), BackgroundResourceUsage::Balanced);
        assert_eq!(value_to_usage(1.6), BackgroundResourceUsage::Aggressive);
    }

    #[test]
    fn slider_clamps_out_of_range_values() {
        assert_eq!(value_to_usage(-10.0), BackgroundResourceUsage::Innocuous);
        assert_eq!(value_to_usage(99.0), BackgroundResourceUsage::Aggressive);
    }
}
