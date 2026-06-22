// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{ApplicationCommand, FieldChange, MetadataChange, Track, TrackId};

use crate::{
    LibraryChangedHolder, TrackRowChangedKind, TrackRowsChangedHolder,
    command_controller::SharedCommandController,
};

use super::{DIALOG_SIDE_MARGIN, DIALOG_WIDTH, NUMBER_ENTRY_WIDTH_CHARS, PAIR_ENTRY_WIDTH_CHARS};

pub(crate) fn open_multi_track_info_dialog(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
    library_changed_holder: &LibraryChangedHolder,
    track_rows_changed_holder: &TrackRowsChangedHolder,
    track_ids: Vec<TrackId>,
) {
    if track_ids.len() < 2 {
        return;
    }
    let Some(tracks) = selected_tracks(command_controller, &track_ids) else {
        return;
    };
    if tracks.len() < 2 {
        return;
    }
    let initial_values = BatchInitialValues::from_tracks(&tracks);

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

    let details = BatchDetailsPage::new(&initial_values);
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
    let track_rows_changed_holder = track_rows_changed_holder.clone();
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

        let change_kind = TrackRowChangedKind::for_metadata_change(&change);
        let result = dispatch_batch_metadata_change(&command_controller, &track_ids, &change);
        if !result.succeeded_track_ids.is_empty() {
            if let Some(callback) = track_rows_changed_holder.borrow().as_ref() {
                callback(&result.succeeded_track_ids, change_kind);
            } else if let Some(callback) = library_changed_holder.borrow().as_ref() {
                callback();
            }
        }
        if result.failed == 0 {
            window_for_ok.close();
        }
    });

    window.present();
}

fn selected_tracks(
    command_controller: &SharedCommandController,
    track_ids: &[TrackId],
) -> Option<Vec<Track>> {
    let runtime = command_controller.runtime();
    let runtime = runtime.borrow();
    track_ids
        .iter()
        .map(|track_id| runtime.library_track(*track_id).cloned())
        .collect()
}

struct BatchDispatchResult {
    succeeded_track_ids: Vec<TrackId>,
    failed: usize,
}

fn dispatch_batch_metadata_change(
    command_controller: &SharedCommandController,
    track_ids: &[TrackId],
    change: &MetadataChange,
) -> BatchDispatchResult {
    let mut result = BatchDispatchResult {
        succeeded_track_ids: Vec::new(),
        failed: 0,
    };
    let mut first_error = None;

    for track_id in track_ids {
        match command_controller.dispatch_unreported(ApplicationCommand::UpdateMetadata {
            track_id: *track_id,
            change: Box::new(change.clone()),
        }) {
            Ok(()) => result.succeeded_track_ids.push(*track_id),
            Err(error) => {
                result.failed += 1;
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    match (
        result.succeeded_track_ids.is_empty(),
        result.failed,
        first_error.as_ref(),
    ) {
        (_, 0, _) => {}
        (true, _, Some(error)) => command_controller.report_command_error(error),
        (false, _, Some(_error)) => {
            command_controller.report_command_warning("Some selected tracks could not be updated.");
        }
        (_, _, None) => {}
    }

    result
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum CommonValue<T> {
    Mixed,
    Shared(Option<T>),
}

impl<T: Clone + Eq> CommonValue<T> {
    fn from_values<'a>(mut values: impl Iterator<Item = &'a Option<T>>) -> Self
    where
        T: 'a,
    {
        let Some(first) = values.next() else {
            return Self::Mixed;
        };
        if values.all(|value| value == first) {
            Self::Shared(first.clone())
        } else {
            Self::Mixed
        }
    }

    fn shared_value(&self) -> Option<&T> {
        match self {
            Self::Shared(Some(value)) => Some(value),
            Self::Shared(None) | Self::Mixed => None,
        }
    }

    fn matches(&self, value: &Option<T>) -> bool {
        matches!(self, Self::Shared(initial) if initial == value)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BatchInitialValues {
    title: CommonValue<String>,
    artist: CommonValue<String>,
    album: CommonValue<String>,
    album_artist: CommonValue<String>,
    composer: CommonValue<String>,
    grouping: CommonValue<String>,
    genre: CommonValue<String>,
    year: CommonValue<i32>,
    track_number: CommonValue<u32>,
    track_total: CommonValue<u32>,
    disc_number: CommonValue<u32>,
    disc_total: CommonValue<u32>,
    compilation: CommonValue<bool>,
    bpm: CommonValue<u32>,
    key: CommonValue<String>,
    comments: CommonValue<String>,
}

impl BatchInitialValues {
    fn from_tracks(tracks: &[Track]) -> Self {
        Self {
            title: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.title)),
            artist: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.artist)),
            album: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.album)),
            album_artist: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.album_artist),
            ),
            composer: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.composer)),
            grouping: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.grouping)),
            genre: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.genre)),
            year: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.year)),
            track_number: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.track_number),
            ),
            track_total: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.track_total),
            ),
            disc_number: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.disc_number),
            ),
            disc_total: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.disc_total),
            ),
            compilation: CommonValue::from_values(
                tracks.iter().map(|track| &track.metadata.compilation),
            ),
            bpm: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.bpm)),
            key: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.key)),
            comments: CommonValue::from_values(tracks.iter().map(|track| &track.metadata.comments)),
        }
    }
}

struct BatchDetailsPage {
    widget: gtk::Grid,
    title: OptionalEntry<String>,
    artist: OptionalEntry<String>,
    album: OptionalEntry<String>,
    album_artist: OptionalEntry<String>,
    composer: OptionalEntry<String>,
    grouping: OptionalEntry<String>,
    genre: OptionalEntry<String>,
    year: OptionalEntry<i32>,
    track_number: OptionalEntry<u32>,
    track_total: OptionalEntry<u32>,
    disc_number: OptionalEntry<u32>,
    disc_total: OptionalEntry<u32>,
    compilation: OptionalBool,
    bpm: OptionalEntry<u32>,
    key: OptionalEntry<String>,
    comments: OptionalTextView,
}

impl BatchDetailsPage {
    fn new(initial: &BatchInitialValues) -> Self {
        let widget = gtk::Grid::new();
        widget.set_row_spacing(6);
        widget.set_column_spacing(10);
        widget.set_hexpand(true);

        let mut row = 0;
        let title = OptionalEntry::text(&widget, row, "Title", initial.title.clone());
        row += 1;
        let artist = OptionalEntry::text(&widget, row, "Artist", initial.artist.clone());
        row += 1;
        let album = OptionalEntry::text(&widget, row, "Album", initial.album.clone());
        row += 1;
        let album_artist =
            OptionalEntry::text(&widget, row, "Album artist", initial.album_artist.clone());
        row += 1;
        let composer = OptionalEntry::text(&widget, row, "Composer", initial.composer.clone());
        row += 1;
        let grouping = OptionalEntry::text(&widget, row, "Grouping", initial.grouping.clone());
        row += 1;
        let genre = OptionalEntry::text(&widget, row, "Genre", initial.genre.clone());
        row += 1;
        let year = OptionalEntry::number(
            &widget,
            row,
            "Year",
            NUMBER_ENTRY_WIDTH_CHARS,
            initial.year.clone(),
        );
        row += 1;
        let track_number = OptionalEntry::number(
            &widget,
            row,
            "Track number",
            PAIR_ENTRY_WIDTH_CHARS,
            initial.track_number.clone(),
        );
        row += 1;
        let track_total = OptionalEntry::number(
            &widget,
            row,
            "Track total",
            PAIR_ENTRY_WIDTH_CHARS,
            initial.track_total.clone(),
        );
        row += 1;
        let disc_number = OptionalEntry::number(
            &widget,
            row,
            "Disc number",
            PAIR_ENTRY_WIDTH_CHARS,
            initial.disc_number.clone(),
        );
        row += 1;
        let disc_total = OptionalEntry::number(
            &widget,
            row,
            "Disc total",
            PAIR_ENTRY_WIDTH_CHARS,
            initial.disc_total.clone(),
        );
        row += 1;
        let compilation = OptionalBool::new(
            &widget,
            row,
            "Compilation",
            "Album is a compilation of songs by various artists",
            initial.compilation.clone(),
        );
        row += 1;
        let bpm = OptionalEntry::number(
            &widget,
            row,
            "BPM",
            NUMBER_ENTRY_WIDTH_CHARS,
            initial.bpm.clone(),
        );
        row += 1;
        let key = OptionalEntry::text(&widget, row, "Key", initial.key.clone());
        row += 1;
        key.entry.set_width_chars(8);
        key.entry.set_hexpand(false);
        let comments = OptionalTextView::new(&widget, row, "Comments", initial.comments.clone());

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
            title: text_change(&self.title.initial, self.title.selected_text()),
            artist: text_change(&self.artist.initial, self.artist.selected_text()),
            album: text_change(&self.album.initial, self.album.selected_text()),
            album_artist: text_change(
                &self.album_artist.initial,
                self.album_artist.selected_text(),
            ),
            composer: text_change(&self.composer.initial, self.composer.selected_text()),
            grouping: text_change(&self.grouping.initial, self.grouping.selected_text()),
            genre: text_change(&self.genre.initial, self.genre.selected_text()),
            year: number_change("Year", &self.year.initial, self.year.selected_text())?,
            track_number: number_change(
                "Track number",
                &self.track_number.initial,
                self.track_number.selected_text(),
            )?,
            track_total: number_change(
                "Track total",
                &self.track_total.initial,
                self.track_total.selected_text(),
            )?,
            disc_number: number_change(
                "Disc number",
                &self.disc_number.initial,
                self.disc_number.selected_text(),
            )?,
            disc_total: number_change(
                "Disc total",
                &self.disc_total.initial,
                self.disc_total.selected_text(),
            )?,
            compilation: self.compilation.selected_value(),
            bpm: number_change("BPM", &self.bpm.initial, self.bpm.selected_text())?,
            key: text_change(&self.key.initial, self.key.selected_text()),
            comments: prose_change(&self.comments.initial, self.comments.selected_text()),
            ..MetadataChange::default()
        })
    }
}

struct OptionalEntry<T> {
    enabled: gtk::CheckButton,
    entry: gtk::Entry,
    initial: CommonValue<T>,
}

impl OptionalEntry<String> {
    fn text(grid: &gtk::Grid, row: i32, label: &str, initial: CommonValue<String>) -> Self {
        Self::new(grid, row, label, gtk::Entry::new(), initial)
    }
}

impl<T> OptionalEntry<T>
where
    T: Clone + Eq + ToString + 'static,
{
    fn number(
        grid: &gtk::Grid,
        row: i32,
        label: &str,
        width_chars: i32,
        initial: CommonValue<T>,
    ) -> Self {
        let entry = gtk::Entry::new();
        entry.set_width_chars(width_chars);
        entry.set_max_width_chars(width_chars);
        entry.set_halign(gtk::Align::Start);
        let field = Self::new(grid, row, label, entry, initial);
        field.entry.set_hexpand(false);
        field
    }

    fn new(
        grid: &gtk::Grid,
        row: i32,
        label: &str,
        entry: gtk::Entry,
        initial: CommonValue<T>,
    ) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        enabled.set_valign(gtk::Align::Center);
        if let Some(value) = initial.shared_value() {
            entry.set_text(&value.to_string());
        }
        entry.set_hexpand(true);
        entry.set_activates_default(true);
        let enabled_for_change = enabled.clone();
        entry.connect_changed(move |_| enabled_for_change.set_active(true));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&entry, 1, row, 1, 1);
        Self {
            enabled,
            entry,
            initial,
        }
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
    initial: CommonValue<bool>,
}

impl OptionalBool {
    fn new(
        grid: &gtk::Grid,
        row: i32,
        label: &str,
        value_label: &str,
        initial: CommonValue<bool>,
    ) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        let value = gtk::CheckButton::with_label(value_label);
        value.set_active(initial.shared_value().copied().unwrap_or(false));
        let enabled_for_value = enabled.clone();
        value.connect_toggled(move |_| enabled_for_value.set_active(true));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&value, 1, row, 1, 1);
        Self {
            enabled,
            value,
            initial,
        }
    }

    fn selected_value(&self) -> FieldChange<bool> {
        if !self.enabled.is_active() {
            return FieldChange::Unchanged;
        }
        let current = Some(self.value.is_active());
        if self.initial.matches(&current) {
            FieldChange::Unchanged
        } else {
            FieldChange::Set(self.value.is_active())
        }
    }
}

struct OptionalTextView {
    enabled: gtk::CheckButton,
    view: gtk::TextView,
    initial: CommonValue<String>,
}

impl OptionalTextView {
    fn new(grid: &gtk::Grid, row: i32, label: &str, initial: CommonValue<String>) -> Self {
        let enabled = gtk::CheckButton::with_label(label);
        enabled.set_valign(gtk::Align::Start);
        let view = gtk::TextView::new();
        view.set_wrap_mode(gtk::WrapMode::WordChar);
        view.set_accepts_tab(false);
        if let Some(value) = initial.shared_value() {
            view.buffer().set_text(value);
        }
        let enabled_for_buffer = enabled.clone();
        view.buffer()
            .connect_changed(move |_| enabled_for_buffer.set_active(true));
        let scroll = gtk::ScrolledWindow::new();
        scroll.set_min_content_height(70);
        scroll.set_hexpand(true);
        scroll.set_child(Some(&view));
        grid.attach(&enabled, 0, row, 1, 1);
        grid.attach(&scroll, 1, row, 1, 1);
        Self {
            enabled,
            view,
            initial,
        }
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

fn text_change(initial: &CommonValue<String>, value: Option<String>) -> FieldChange<String> {
    let Some(value) = value else {
        return FieldChange::Unchanged;
    };
    let trimmed = value.trim().to_owned();
    let current = (!trimmed.is_empty()).then_some(trimmed);
    if initial.matches(&current) {
        FieldChange::Unchanged
    } else {
        match current {
            None => FieldChange::Clear,
            Some(value) => FieldChange::Set(value),
        }
    }
}

fn prose_change(initial: &CommonValue<String>, value: Option<String>) -> FieldChange<String> {
    let Some(value) = value else {
        return FieldChange::Unchanged;
    };
    let current = (!value.trim().is_empty()).then_some(value);
    if initial.matches(&current) {
        FieldChange::Unchanged
    } else {
        match current {
            None => FieldChange::Clear,
            Some(value) => FieldChange::Set(value),
        }
    }
}

fn number_change<T>(
    label: &str,
    initial: &CommonValue<T>,
    value: Option<String>,
) -> Result<FieldChange<T>, String>
where
    T: Clone + Eq + std::str::FromStr,
{
    let Some(value) = value else {
        return Ok(FieldChange::Unchanged);
    };
    let value = value.trim().to_owned();
    let current = if value.is_empty() {
        None
    } else {
        Some(
            value
                .parse()
                .map_err(|_| format!("{label} must be a whole number."))?,
        )
    };
    if initial.matches(&current) {
        Ok(FieldChange::Unchanged)
    } else {
        match current {
            None => Ok(FieldChange::Clear),
            Some(value) => Ok(FieldChange::Set(value)),
        }
    }
}

#[cfg(test)]
mod tests {
    use gtk::prelude::*;
    use sustain_app_runtime::FieldChange;

    use super::{
        CommonValue, OptionalBool, OptionalEntry, OptionalTextView, number_change, prose_change,
        text_change,
    };

    #[test]
    fn unchecked_batch_fields_are_unchanged() {
        assert_eq!(
            text_change(&CommonValue::Mixed, None),
            FieldChange::Unchanged
        );
        assert_eq!(
            number_change::<u32>("BPM", &CommonValue::Mixed, None),
            Ok(FieldChange::Unchanged)
        );
    }

    #[test]
    fn checked_blank_mixed_batch_fields_are_cleared() {
        assert_eq!(
            text_change(&CommonValue::Mixed, Some(" ".to_owned())),
            FieldChange::Clear
        );
        assert_eq!(
            number_change::<u32>("BPM", &CommonValue::Mixed, Some(String::new())),
            Ok(FieldChange::Clear)
        );
    }

    #[test]
    fn checked_batch_fields_are_set() {
        assert_eq!(
            text_change(&CommonValue::Mixed, Some("  Album  ".to_owned())),
            FieldChange::Set("Album".to_owned())
        );
        assert_eq!(
            number_change::<u32>("BPM", &CommonValue::Mixed, Some("120".to_owned())),
            Ok(FieldChange::Set(120))
        );
        assert_eq!(
            prose_change(&CommonValue::Mixed, Some("line one\nline two".to_owned())),
            FieldChange::Set("line one\nline two".to_owned())
        );
    }

    #[test]
    fn checked_common_values_that_stay_same_are_unchanged() {
        assert_eq!(
            text_change(
                &CommonValue::Shared(Some("Album".to_owned())),
                Some("  Album  ".to_owned())
            ),
            FieldChange::Unchanged
        );
        assert_eq!(
            number_change::<u32>(
                "BPM",
                &CommonValue::Shared(Some(120)),
                Some("120".to_owned())
            ),
            Ok(FieldChange::Unchanged)
        );
        assert_eq!(
            prose_change(
                &CommonValue::Shared(Some("line one\nline two".to_owned())),
                Some("line one\nline two".to_owned())
            ),
            FieldChange::Unchanged
        );
    }

    #[test]
    fn checked_blank_common_empty_values_are_unchanged() {
        assert_eq!(
            text_change(&CommonValue::Shared(None), Some(" ".to_owned())),
            FieldChange::Unchanged
        );
        assert_eq!(
            number_change::<u32>("BPM", &CommonValue::Shared(None), Some(String::new())),
            Ok(FieldChange::Unchanged)
        );
    }

    #[test]
    fn invalid_batch_numbers_are_rejected() {
        assert_eq!(
            number_change::<u32>("BPM", &CommonValue::Mixed, Some("fast".to_owned())),
            Err("BPM must be a whole number.".to_owned())
        );
    }

    #[test]
    fn common_value_detects_identical_optional_values() {
        let first = Some("Album".to_owned());
        let second = Some("Album".to_owned());
        assert_eq!(
            CommonValue::from_values([&first, &second].into_iter()),
            CommonValue::Shared(Some("Album".to_owned()))
        );

        let third = Some("Other".to_owned());
        assert_eq!(
            CommonValue::from_values([&first, &third].into_iter()),
            CommonValue::Mixed
        );
    }

    #[test]
    fn editing_entry_enables_batch_field_without_initial_prefill_doing_so() {
        crate::test_support::with_gtk(|| {
            let grid = gtk::Grid::new();
            let field = OptionalEntry::text(
                &grid,
                0,
                "Title",
                CommonValue::Shared(Some("Original".to_owned())),
            );

            assert_eq!(field.entry.text().as_str(), "Original");
            assert!(!field.enabled.is_active());

            field.entry.set_text("Changed");

            assert!(field.enabled.is_active());
        });
    }

    #[test]
    fn editing_comments_enables_batch_field_without_initial_prefill_doing_so() {
        crate::test_support::with_gtk(|| {
            let grid = gtk::Grid::new();
            let field = OptionalTextView::new(
                &grid,
                0,
                "Comments",
                CommonValue::Shared(Some("Original".to_owned())),
            );

            assert!(!field.enabled.is_active());

            field.view.buffer().set_text("Changed");

            assert!(field.enabled.is_active());
        });
    }

    #[test]
    fn toggling_boolean_value_enables_batch_field() {
        crate::test_support::with_gtk(|| {
            let grid = gtk::Grid::new();
            let field = OptionalBool::new(
                &grid,
                0,
                "Compilation",
                "Album is a compilation of songs by various artists",
                CommonValue::Shared(Some(false)),
            );

            assert!(!field.enabled.is_active());

            field.value.set_active(true);

            assert!(field.enabled.is_active());
        });
    }
}
