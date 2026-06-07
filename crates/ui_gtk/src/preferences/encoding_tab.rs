// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The Encoding preferences tab: choose the format an inserted audio CD is
//! ripped into. The choice is persisted in `UserSettings::encoding`; a
//! running import captured its profile at preparation, so a change here
//! affects only later imports.
//!
//! Presented as a three-stop slider — the same discrete-`gtk::Scale` shape
//! used by the shuffle and analysis tabs — ordered smallest to largest:
//! MP3 256, MP3 320, then FLAC (lossless).

use gtk::prelude::*;

use super::super::{
    ApplicationCommand, CdEncodingProfile, command_controller::SharedCommandController,
};
use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};

pub(super) fn build(command_controller: SharedCommandController) -> gtk::Widget {
    let content = gtk::Box::new(gtk::Orientation::Vertical, 4);
    content.add_css_class("preference-slider-row");
    content.set_margin_top(24);
    content.set_margin_end(24);
    content.set_margin_bottom(24);
    content.set_margin_start(24);

    let heading = gtk::Label::new(Some("CD import format"));
    heading.set_xalign(0.0);
    content.append(&heading);

    let initial = command_controller
        .runtime()
        .borrow()
        .settings()
        .encoding
        .cd_profile;

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
    scale.set_value(profile_to_value(initial));
    content.append(&scale);

    let mark_label_row = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    mark_label_row.append(&mark_label("MP3 256", gtk::Align::Start));
    mark_label_row.append(&mark_label("MP3 320", gtk::Align::Center));
    mark_label_row.append(&mark_label("FLAC", gtk::Align::End));
    content.append(&mark_label_row);

    let helper = gtk::Label::new(Some(
        "Format used when ripping audio CDs into your library.",
    ));
    helper.add_css_class("preference-helper");
    helper.set_xalign(0.0);
    helper.set_wrap(true);
    helper.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    helper.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    helper.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);
    content.append(&helper);

    let controller = command_controller;
    scale.connect_value_changed(move |scale| {
        let profile = value_to_profile(scale.value());
        let mut settings = controller.runtime().borrow().settings().clone();
        if settings.encoding.cd_profile == profile {
            return;
        }
        settings.encoding.cd_profile = profile;
        // Save failures are reported once by `UiCommandController`.
        let _result = controller.dispatch(ApplicationCommand::UpdateSettings(settings));
    });

    content.upcast()
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
}
