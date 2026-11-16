// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Preview of the duplicate groups the audio verified as one recording
//! (#303): every group merges with the defaults the merge dialog presets,
//! the user can leave any group out, and one confirmation starts the batch.

use std::rc::Rc;

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{ApplicationCommand, Track, TrackId, VerifiedDuplicateMerge};
use sustain_i18n::{gettext, ngettext, plural_count, tr_format};

use crate::{
    command_controller::SharedCommandController,
    duplicate_consolidation::{audio_summary, metadata_field_display, metadata_field_name},
    util::{display_artist, display_title},
};

pub(crate) fn open_verified_duplicates_preview(
    parent: Option<&gtk::Window>,
    command_controller: &SharedCommandController,
    merges: Vec<VerifiedDuplicateMerge>,
) {
    let window = gtk::Window::builder()
        .title(gettext("Merge Verified Duplicates"))
        .modal(true)
        .default_width(820)
        .default_height(640)
        .build();
    window.set_transient_for(parent);

    let content = gtk::Box::new(gtk::Orientation::Vertical, 12);
    content.set_margin_top(18);
    content.set_margin_end(18);
    content.set_margin_bottom(18);
    content.set_margin_start(18);

    let intro = gtk::Label::new(Some(&gettext(
        "Their audio shows each of these groups to be copies of one recording. Each group merges into a single track, keeping the choices shown.",
    )));
    intro.set_xalign(0.0);
    intro.set_wrap(true);
    intro.add_css_class("consolidation-intro");
    content.append(&intro);

    let intro_detail = gtk::Label::new(Some(&gettext(
        "The best audio file and the highest rating are kept, along with the highest-resolution artwork. Play counts and skips are summed, and the oldest date added is kept. Uncheck a group to review it yourself.",
    )));
    intro_detail.set_xalign(0.0);
    intro_detail.set_wrap(true);
    intro_detail.add_css_class("dim-label");
    content.append(&intro_detail);

    let list = gtk::ListBox::new();
    list.set_selection_mode(gtk::SelectionMode::None);
    list.add_css_class("boxed-list");
    let checks = merges
        .iter()
        .map(|merge| {
            let (row, check) = merge_row(merge);
            list.append(&row);
            check
        })
        .collect::<Vec<_>>();
    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&list)
        .build();
    content.append(&scrolled);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label(&gettext("Cancel"));
    let merge_button = gtk::Button::new();
    merge_button.add_css_class("destructive-action");
    buttons.append(&cancel);
    buttons.append(&merge_button);
    content.append(&buttons);

    let checks = Rc::new(checks);
    let merges = Rc::new(merges);
    let update_merge_button = {
        let checks = checks.clone();
        let merge_button = merge_button.clone();
        move || {
            let included = checks.iter().filter(|check| check.is_active()).count();
            merge_button.set_label(&tr_format!(
                ngettext(
                    "Merge {included} Group",
                    "Merge {included} Groups",
                    plural_count(included)
                ),
                included = included
            ));
            merge_button.set_sensitive(included > 0);
        }
    };
    update_merge_button();
    for check in checks.iter() {
        let update_merge_button = update_merge_button.clone();
        check.connect_toggled(move |_| update_merge_button());
    }

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());
    let window_for_merge = window.clone();
    let command_controller = command_controller.clone();
    merge_button.connect_clicked(move |_| {
        let included = merges
            .iter()
            .zip(checks.iter())
            .filter(|(_, check)| check.is_active())
            .map(|(merge, _)| merge.clone())
            .collect::<Vec<_>>();
        if command_controller
            .dispatch_succeeded(ApplicationCommand::ConsolidateVerifiedDuplicates(included))
        {
            window_for_merge.close();
        }
    });

    window.set_child(Some(&content));
    window.set_default_widget(Some(&cancel));
    let key_controller = gtk::EventControllerKey::new();
    let window_for_escape = window.clone();
    key_controller.connect_key_pressed(move |_controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            window_for_escape.close();
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);
    window.present();
}

/// One group: whether it is included, what it is, the file it keeps, and
/// the value it keeps for every field its copies disagree on.
fn merge_row(merge: &VerifiedDuplicateMerge) -> (gtk::ListBoxRow, gtk::CheckButton) {
    let kept_audio = track(merge, merge.choices.audio_track_id);
    let check = gtk::CheckButton::new();
    check.set_active(true);
    check.set_valign(gtk::Align::Start);

    let details = gtk::Box::new(gtk::Orientation::Vertical, 2);
    details.set_hexpand(true);
    details.append(&row_label(
        &format!(
            "{} \u{2014} {}",
            display_artist(&kept_audio.metadata),
            display_title(kept_audio)
        ),
        &["consolidation-field-name"],
    ));
    let removed = merge.tracks.len() - 1;
    details.append(&row_label(
        &tr_format!(
            ngettext(
                "Keeps {audio} \u{00b7} {path} \u{00b7} removes {removed} copy",
                "Keeps {audio} \u{00b7} {path} \u{00b7} removes {removed} copies",
                plural_count(removed)
            ),
            audio = audio_summary(kept_audio),
            path = kept_audio.location.path().display(),
            removed = removed
        ),
        &["dim-label"],
    ));
    let differences = kept_differences(merge);
    if !differences.is_empty() {
        details.append(&row_label(&differences.join(" \u{00b7} "), &["dim-label"]));
    }

    let content = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    content.set_margin_top(6);
    content.set_margin_end(8);
    content.set_margin_bottom(6);
    content.set_margin_start(8);
    content.append(&check);
    content.append(&details);
    let row = gtk::ListBoxRow::new();
    row.set_activatable(false);
    row.set_child(Some(&content));
    (row, check)
}

/// `Field: kept value` for every field the copies disagree on, so the user
/// sees each choice that actually decides something.
fn kept_differences(merge: &VerifiedDuplicateMerge) -> Vec<String> {
    merge
        .choices
        .metadata
        .fields
        .iter()
        .filter_map(|selection| {
            let kept =
                metadata_field_display(&track(merge, selection.track_id).metadata, selection.field);
            let disagree = merge
                .tracks
                .iter()
                .any(|track| metadata_field_display(&track.metadata, selection.field) != kept);
            disagree.then(|| {
                tr_format!(
                    // Translators: a field a merge keeps, as in "Year: 1999".
                    gettext("{field}: {value}"),
                    field = metadata_field_name(selection.field),
                    value = kept.unwrap_or_else(|| gettext("(empty)"))
                )
            })
        })
        .collect()
}

fn track(merge: &VerifiedDuplicateMerge, track_id: TrackId) -> &Track {
    merge
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .expect("a merge's choices name its own tracks")
}

fn row_label(text: &str, classes: &[&str]) -> gtk::Label {
    let label = gtk::Label::new(Some(text));
    label.set_xalign(0.0);
    label.set_wrap(true);
    label.set_wrap_mode(gtk::pango::WrapMode::WordChar);
    for class in classes {
        label.add_css_class(class);
    }
    label
}
