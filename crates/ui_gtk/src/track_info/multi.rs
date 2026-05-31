// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{ApplicationCommand, FieldChange, MetadataChange, TrackId};

use crate::{LibraryChangedHolder, command_controller::SharedCommandController};

use super::{DIALOG_SIDE_MARGIN, DIALOG_WIDTH, NUMBER_ENTRY_WIDTH_CHARS, PAIR_ENTRY_WIDTH_CHARS};

pub(crate) fn open_multi_track_info_dialog(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
    track_ids: Vec<TrackId>,
) {
    if track_ids.len() < 2 {
        return;
    }

    let window = gtk::Window::builder()
        .title(format!("Get Info - {} Tracks", track_ids.len()))
        .transient_for(parent)
        .modal(true)
        .resizable(false)
        .default_width(DIALOG_WIDTH)
        .build();
    window.add_css_class("track-info-window");

    let outer = gtk::Box::new(gtk::Orientation::Vertical, 12);
    outer.set_margin_top(16);
    outer.set_margin_end(DIALOG_SIDE_MARGIN);
    outer.set_margin_bottom(16);
    outer.set_margin_start(DIALOG_SIDE_MARGIN);

    let explanation = gtk::Label::new(Some(
        "Only checked fields are applied to the selected tracks. Unchecked fields keep each track's existing value.",
    ));
    explanation.set_wrap(true);
    explanation.set_xalign(0.0);
    explanation.add_css_class("dim-label");
    outer.append(&explanation);

    let details = BatchDetailsPage::new();
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_propagate_natural_height(true);
    scroll.set_max_content_height(640);
    scroll.set_child(Some(&details.widget));
    outer.append(&scroll);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label("Cancel");
    let ok = gtk::Button::with_label("OK");
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    outer.append(&buttons);
    window.set_child(Some(&outer));
    window.set_default_widget(Some(&ok));

    let window_for_cancel = window.clone();
    cancel.connect_clicked(move |_| window_for_cancel.close());

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

    let window_for_ok = window.clone();
    let command_controller = command_controller.clone();
    let library_changed_holder = library_changed_holder.clone();
    ok.connect_clicked(move |_| {
        let change = match details.metadata_change() {
            Ok(change) => change,
            Err(message) => {
                command_controller.report_command_warning(message);
                return;
            }
        };
        if change == MetadataChange::default() {
            window_for_ok.close();
            return;
        }

        let result = command_controller.dispatch_batch(track_ids.iter().copied().map(|track_id| {
            ApplicationCommand::UpdateMetadata {
                track_id,
                change: Box::new(change.clone()),
            }
        }));
        if result.succeeded > 0
            && let Some(callback) = library_changed_holder.borrow().as_ref()
        {
            callback();
        }
        if result.failed == 0 {
            window_for_ok.close();
        }
    });

    window.present();
}

struct BatchDetailsPage {
    widget: gtk::Grid,
    title: OptionalEntry,
    artist: OptionalEntry,
    album: OptionalEntry,
    album_artist: OptionalEntry,
    composer: OptionalEntry,
    grouping: OptionalEntry,
    genre: OptionalEntry,
    year: OptionalEntry,
    track_number: OptionalEntry,
    track_total: OptionalEntry,
    disc_number: OptionalEntry,
    disc_total: OptionalEntry,
    compilation: OptionalBool,
    bpm: OptionalEntry,
    key: OptionalEntry,
    comments: OptionalTextView,
}

impl BatchDetailsPage {
    fn new() -> Self {
        let widget = gtk::Grid::new();
        widget.set_row_spacing(6);
        widget.set_column_spacing(10);
        widget.set_hexpand(true);

        let mut row = 0;
        let title = OptionalEntry::text(&widget, row, "Title");
        row += 1;
        let artist = OptionalEntry::text(&widget, row, "Artist");
        row += 1;
        let album = OptionalEntry::text(&widget, row, "Album");
        row += 1;
        let album_artist = OptionalEntry::text(&widget, row, "Album artist");
        row += 1;
        let composer = OptionalEntry::text(&widget, row, "Composer");
        row += 1;
        let grouping = OptionalEntry::text(&widget, row, "Grouping");
        row += 1;
        let genre = OptionalEntry::text(&widget, row, "Genre");
        row += 1;
        let year = OptionalEntry::number(&widget, row, "Year", NUMBER_ENTRY_WIDTH_CHARS);
        row += 1;
        let track_number =
            OptionalEntry::number(&widget, row, "Track number", PAIR_ENTRY_WIDTH_CHARS);
        row += 1;
        let track_total =
            OptionalEntry::number(&widget, row, "Track total", PAIR_ENTRY_WIDTH_CHARS);
        row += 1;
        let disc_number =
            OptionalEntry::number(&widget, row, "Disc number", PAIR_ENTRY_WIDTH_CHARS);
        row += 1;
        let disc_total = OptionalEntry::number(&widget, row, "Disc total", PAIR_ENTRY_WIDTH_CHARS);
        row += 1;
        let compilation = OptionalBool::new(
            &widget,
            row,
            "Compilation",
            "Album is a compilation of songs by various artists",
        );
        row += 1;
        let bpm = OptionalEntry::number(&widget, row, "BPM", NUMBER_ENTRY_WIDTH_CHARS);
        row += 1;
        let key = OptionalEntry::text(&widget, row, "Key");
        row += 1;
        key.entry.set_width_chars(8);
        key.entry.set_hexpand(false);
        let comments = OptionalTextView::new(&widget, row, "Comments");

        Self {
            widget,
            title,
            artist,
            album,
            album_artist,
            composer,
            grouping,
            genre,
            year,
            track_number,
            track_total,
            disc_number,
            disc_total,
            compilation,
            bpm,
            key,
            comments,
        }
    }

    fn metadata_change(&self) -> Result<MetadataChange, String> {
        Ok(MetadataChange {
            title: text_change(self.title.selected_text()),
            artist: text_change(self.artist.selected_text()),
            album: text_change(self.album.selected_text()),
            album_artist: text_change(self.album_artist.selected_text()),
            composer: text_change(self.composer.selected_text()),
            grouping: text_change(self.grouping.selected_text()),
            genre: text_change(self.genre.selected_text()),
            year: number_change("Year", self.year.selected_text())?,
            track_number: number_change("Track number", self.track_number.selected_text())?,
            track_total: number_change("Track total", self.track_total.selected_text())?,
            disc_number: number_change("Disc number", self.disc_number.selected_text())?,
            disc_total: number_change("Disc total", self.disc_total.selected_text())?,
            compilation: self.compilation.selected_value(),
            bpm: number_change("BPM", self.bpm.selected_text())?,
            key: text_change(self.key.selected_text()),
            comments: prose_change(self.comments.selected_text()),
            ..MetadataChange::default()
        })
    }
}

struct OptionalEntry {
    enabled: gtk::CheckButton,
    entry: gtk::Entry,
}

impl OptionalEntry {
    fn text(grid: &gtk::Grid, row: i32, label: &str) -> Self {
        Self::new(grid, row, label, gtk::Entry::new())
    }

    fn number(grid: &gtk::Grid, row: i32, label: &str, width_chars: i32) -> Self {
        let entry = gtk::Entry::new();
        entry.set_width_chars(width_chars);
        entry.set_max_width_chars(width_chars);
        entry.set_halign(gtk::Align::Start);
        let field = Self::new(grid, row, label, entry);
        field.entry.set_hexpand(false);
        field
    }

    fn new(grid: &gtk::Grid, row: i32, label: &str, entry: gtk::Entry) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        enabled.set_valign(gtk::Align::Center);
        entry.set_hexpand(true);
        entry.set_sensitive(false);
        entry.set_activates_default(true);
        let entry_for_toggle = entry.clone();
        enabled.connect_toggled(move |button| entry_for_toggle.set_sensitive(button.is_active()));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&entry, 1, row, 1, 1);
        Self { enabled, entry }
    }

    fn selected_text(&self) -> Option<String> {
        self.enabled
            .is_active()
            .then(|| self.entry.text().to_string())
    }
}

struct OptionalBool {
    enabled: gtk::CheckButton,
    value: gtk::CheckButton,
}

impl OptionalBool {
    fn new(grid: &gtk::Grid, row: i32, label: &str, value_label: &str) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        let value = gtk::CheckButton::with_label(value_label);
        value.set_sensitive(false);
        let value_for_toggle = value.clone();
        enabled.connect_toggled(move |button| value_for_toggle.set_sensitive(button.is_active()));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&value, 1, row, 1, 1);
        Self { enabled, value }
    }

    fn selected_value(&self) -> FieldChange<bool> {
        if self.enabled.is_active() {
            FieldChange::Set(self.value.is_active())
        } else {
            FieldChange::Unchanged
        }
    }
}

struct OptionalTextView {
    enabled: gtk::CheckButton,
    view: gtk::TextView,
}

impl OptionalTextView {
    fn new(grid: &gtk::Grid, row: i32, label: &str) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        enabled.set_valign(gtk::Align::Start);
        let view = gtk::TextView::new();
        view.set_sensitive(false);
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_accepts_tab(false);
        let view_for_toggle = view.clone();
        enabled.connect_toggled(move |button| view_for_toggle.set_sensitive(button.is_active()));
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_min_content_height(70);
        scroll.set_hexpand(true);
        scroll.set_child(Some(&view));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&scroll, 1, row, 1, 1);
        Self { enabled, view }
    }

    fn selected_text(&self) -> Option<String> {
        self.enabled.is_active().then(|| {
            let buffer = self.view.buffer();
            buffer
                .text(&buffer.start_iter(), &buffer.end_iter(), true)
                .to_string()
        })
    }
}

fn text_change(value: Option<String>) -> FieldChange<String> {
    match value.map(|value| value.trim().to_owned()) {
        None => FieldChange::Unchanged,
        Some(value) if value.is_empty() => FieldChange::Clear,
        Some(value) => FieldChange::Set(value),
    }
}

fn prose_change(value: Option<String>) -> FieldChange<String> {
    match value {
        None => FieldChange::Unchanged,
        Some(value) if value.trim().is_empty() => FieldChange::Clear,
        Some(value) => FieldChange::Set(value),
    }
}

fn number_change<T>(label: &str, value: Option<String>) -> Result<FieldChange<T>, String>
where
    T: std::str::FromStr,
{
    match value.map(|value| value.trim().to_owned()) {
        None => Ok(FieldChange::Unchanged),
        Some(value) if value.is_empty() => Ok(FieldChange::Clear),
        Some(value) => value
            .parse()
            .map(FieldChange::Set)
            .map_err(|_| format!("{label} must be a whole number.")),
    }
}

#[cfg(test)]
mod tests {
    use sustain_app_runtime::FieldChange;

    use super::{number_change, prose_change, text_change};

    #[test]
    fn unchecked_batch_fields_are_unchanged() {
        assert_eq!(text_change(None), FieldChange::Unchanged);
        assert_eq!(
            number_change::<u32>("BPM", None),
            Ok(FieldChange::Unchanged)
        );
    }

    #[test]
    fn checked_blank_batch_fields_are_cleared() {
        assert_eq!(text_change(Some(" ".to_owned())), FieldChange::Clear);
        assert_eq!(
            number_change::<u32>("BPM", Some(String::new())),
            Ok(FieldChange::Clear)
        );
    }

    #[test]
    fn checked_batch_fields_are_set() {
        assert_eq!(
            text_change(Some("  Album  ".to_owned())),
            FieldChange::Set("Album".to_owned())
        );
        assert_eq!(
            number_change::<u32>("BPM", Some("120".to_owned())),
            Ok(FieldChange::Set(120))
        );
        assert_eq!(
            prose_change(Some("line one\nline two".to_owned())),
            FieldChange::Set("line one\nline two".to_owned())
        );
    }

    #[test]
    fn invalid_batch_numbers_are_rejected() {
        assert_eq!(
            number_change::<u32>("BPM", Some("fast".to_owned())),
            Err("BPM must be a whole number.".to_owned())
        );
    }
}
