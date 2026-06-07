// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The Encoding preferences tab: choose the format an inserted audio CD is
//! ripped into, and how carefully the drive reads it. Both choices are
//! persisted in `UserSettings::encoding`; a running import captured them at
//! preparation, so a change here affects only later imports.
//!
//! Each is a three-stop slider — the same discrete-`gtk::Scale` shape used by
//! the shuffle and analysis tabs. The format slider runs smallest to largest
//! (MP3 256, MP3 320, FLAC); the read-accuracy slider runs fastest to most
//! thorough (Fast, Balanced, Paranoid).

use gtk::prelude::*;

use super::super::{
    ApplicationCommand, CdEncodingProfile, CdReadMode, command_controller::SharedCommandController,
};
use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};

pub(super) fn build(command_controller: SharedCommandController) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.add_css_class("preference-slider-row");
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let encoding = command_controller.runtime().borrow().settings().encoding;

    // --- CD import format ---
    {
        let controller = command_controller.clone();
        append_three_stop_slider(
            &content,
            "CD import format",
            ["MP3, @256", "MP3, @320", "FLAC (Lossless)"],
            "Format used when ripping audio CDs into your library.",
            profile_to_value(encoding.cd_profile),
            0,
            move |value| {
                let profile = value_to_profile(value);
                let mut settings = controller.runtime().borrow().settings().clone();
                if settings.encoding.cd_profile == profile {
                    return;
                }
                settings.encoding.cd_profile = profile;
                // Save failures are reported once by `UiCommandController`.
                let _result = controller.dispatch(ApplicationCommand::UpdateSettings(settings));
            },
        );
    }

    // --- CD read accuracy ---
    {
        let controller = command_controller;
        append_three_stop_slider(
            &content,
            "CD read accuracy",
            ["Fast", "Balanced", "Paranoid"],
            "Error correction used when ripping audio CDs.",
            read_mode_to_value(encoding.cd_read_mode),
            20,
            move |value| {
                let mode = value_to_read_mode(value);
                let mut settings = controller.runtime().borrow().settings().clone();
                if settings.encoding.cd_read_mode == mode {
                    return;
                }
                settings.encoding.cd_read_mode = mode;
                let _result = controller.dispatch(ApplicationCommand::UpdateSettings(settings));
            },
        );
    }

    content.upcast()
}

/// Append a labelled three-stop slider (heading, the discrete scale, its
/// end/centre mark labels, and a helper line) to `content`. `top_margin`
/// spaces a later group from the one above it. The handler is wired after the
/// initial value is set, so seeding the slider never dispatches a write.
fn append_three_stop_slider(
    content: &gtk::Box,
    heading: &str,
    marks: [&str; 3],
    helper_text: &str,
    initial: f64,
    top_margin: i32,
    on_change: impl Fn(f64) + 'static,
) {
    let heading_label = gtk::Label::new(Some(heading));
    heading_label.set_xalign(0.0);
    heading_label.set_margin_top(top_margin);
    content.append(&heading_label);

    // Three discrete stops at 0/1/2, snap-to-tick via `round_digits = 0`,
    // with a separate label row beneath so the end-cap labels can align to
    // the scale's ends without overflowing it.
    let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 2.0, 1.0);
    scale.set_round_digits(0);
    scale.set_draw_value(false);
    scale.set_hexpand(true);
    scale.add_mark(0.0, gtk::PositionType::Bottom, None);
    scale.add_mark(1.0, gtk::PositionType::Bottom, None);
    scale.add_mark(2.0, gtk::PositionType::Bottom, None);
    scale.set_value(initial);
    content.append(&scale);

    let mark_label_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mark_label_row.append(&mark_label(marks[0], gtk::Align::Start));
    mark_label_row.append(&mark_label(marks[1], gtk::Align::Center));
    mark_label_row.append(&mark_label(marks[2], gtk::Align::End));
    content.append(&mark_label_row);

    let helper = gtk::Label::new(Some(helper_text));
    helper.add_css_class("preference-helper");
    helper.set_xalign(0.0);
    helper.set_wrap(true);
    helper.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    helper.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    helper.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    content.append(&helper);

    scale.connect_value_changed(move |scale| on_change(scale.value()));
}

fn mark_label(text: &str, align: gtk::Align) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.add_css_class("preference-helper");
    label.set_halign(align);
    label.set_hexpand(true);
    label
}

/// Slider position for a profile: 0 = MP3 256, 1 = MP3 320, 2 = FLAC.
fn profile_to_value(profile: CdEncodingProfile) -> f64 {
    match profile {
        CdEncodingProfile::Mp3Cbr256 => 0.0,
        CdEncodingProfile::Mp3Cbr320 => 1.0,
        CdEncodingProfile::Flac => 2.0,
    }
}

/// Snap a slider value to the nearest profile stop.
fn value_to_profile(value: f64) -> CdEncodingProfile {
    match value.round() as i32 {
        0 => CdEncodingProfile::Mp3Cbr256,
        1 => CdEncodingProfile::Mp3Cbr320,
        _ => CdEncodingProfile::Flac,
    }
}

/// Slider position for a read mode: 0 = Fast, 1 = Balanced, 2 = Paranoid.
fn read_mode_to_value(mode: CdReadMode) -> f64 {
    match mode {
        CdReadMode::Fast => 0.0,
        CdReadMode::Balanced => 1.0,
        CdReadMode::Paranoid => 2.0,
    }
}

/// Snap a slider value to the nearest read mode.
fn value_to_read_mode(value: f64) -> CdReadMode {
    match value.round() as i32 {
        0 => CdReadMode::Fast,
        1 => CdReadMode::Balanced,
        _ => CdReadMode::Paranoid,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_profile_round_trips_through_its_slider_stop() {
        for profile in CdEncodingProfile::ALL {
            assert_eq!(value_to_profile(profile_to_value(profile)), profile);
        }
    }

    #[test]
    fn slider_stops_run_smallest_to_largest() {
        assert_eq!(value_to_profile(0.0), CdEncodingProfile::Mp3Cbr256);
        assert_eq!(value_to_profile(1.0), CdEncodingProfile::Mp3Cbr320);
        assert_eq!(value_to_profile(2.0), CdEncodingProfile::Flac);
    }

    #[test]
    fn each_read_mode_round_trips_through_its_slider_stop() {
        for mode in CdReadMode::ALL {
            assert_eq!(value_to_read_mode(read_mode_to_value(mode)), mode);
        }
    }

    #[test]
    fn read_mode_stops_run_fastest_to_most_thorough() {
        assert_eq!(value_to_read_mode(0.0), CdReadMode::Fast);
        assert_eq!(value_to_read_mode(1.0), CdReadMode::Balanced);
        assert_eq!(value_to_read_mode(2.0), CdReadMode::Paranoid);
    }
}
