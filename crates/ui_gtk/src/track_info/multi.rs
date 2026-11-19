// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;
use gtk::{gdk, glib};
use sustain_app_runtime::{
    ApplicationCommand, Bpm, FieldChange, MetadataChange, MusicalKey, Track, TrackId,
};
use sustain_i18n::{gettext, ngettext, pgettext, plural_count, tr_format};

use crate::command_controller::SharedCommandController;
use crate::metadata_diff::{parse_bpm, parse_key, parse_whole_number};

use super::{DIALOG_SIDE_MARGIN, DIALOG_WIDTH, NUMBER_ENTRY_WIDTH_CHARS, PAIR_ENTRY_WIDTH_CHARS};

pub(crate) fn open_multi_track_info_dialog(
    parent: &gtk::Window,
    command_controller: &SharedCommandController,
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
        .title(tr_format!(
            ngettext(
                "Get Info - {count} Track",
                "Get Info - {count} Tracks",
                plural_count(track_ids.len())
            ),
            count = track_ids.len()
        ))
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

    let explanation = gtk::Label::new(Some(&gettext(
        "Only checked fields are applied to the selected tracks. Unchecked fields keep each track's existing value.",
    )));
    explanation.set_wrap(true);
    explanation.set_xalign(0.0);
    explanation.add_css_class("dim-label");
    outer.append(&explanation);

    let genre_suggestions = command_controller.runtime().borrow().distinct_genres();
    let details = BatchDetailsPage::new(&initial_values, genre_suggestions);
    let scroll = gtk::ScrolledWindow::new();
    scroll.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
    scroll.set_propagate_natural_height(true);
    scroll.set_max_content_height(640);
    scroll.set_child(Some(&details.widget));
    outer.append(&scroll);

    let buttons = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    buttons.set_halign(gtk::Align::End);
    let cancel = gtk::Button::with_label(&gettext("Cancel"));
    let ok = gtk::Button::with_label(&gettext("OK"));
    ok.add_css_class("suggested-action");
    buttons.append(&cancel);
    buttons.append(&ok);
    outer.append(&buttons);
    window.set_child(Some(&outer));
    window.set_default_widget(Some(&ok));

    let window_for_cancel = window.downgrade();
    cancel.connect_clicked(move |_| {
        if let Some(window) = window_for_cancel.upgrade() {
            window.close();
        }
    });

    let key_controller = gtk::EventControllerKey::new();
    key_controller.connect_key_pressed(|controller, key, _keycode, _state| {
        if key == gdk::Key::Escape {
            if let Some(window) = controller.widget().and_downcast::<gtk::Window>() {
                window.close();
            }
            glib::Propagation::Stop
        } else {
            glib::Propagation::Proceed
        }
    });
    window.add_controller(key_controller);

    let window_for_ok = window.downgrade();
    let command_controller = command_controller.clone();
    ok.connect_clicked(move |_| {
        let close = || {
            if let Some(window) = window_for_ok.upgrade() {
                window.close();
            }
        };
        let change = match details.metadata_change() {
            Ok(change) => change,
            Err(message) => {
                command_controller.report_command_warning(message);
                return;
            }
        };
        if change == MetadataChange::default() {
            close();
            return;
        }

        if dispatch_batch_metadata_change(&command_controller, &track_ids, &change) == 0 {
            close();
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

/// Dispatch `change` to every track, reporting failures once for the
/// batch. Returns how many tracks could not be updated.
fn dispatch_batch_metadata_change(
    command_controller: &SharedCommandController,
    track_ids: &[TrackId],
    change: &MetadataChange,
) -> usize {
    let mut succeeded = 0usize;
    let mut failed = 0usize;
    let mut first_error = None;

    for track_id in track_ids {
        match command_controller.dispatch_unreported(ApplicationCommand::UpdateMetadata {
            track_id: *track_id,
            change: Box::new(change.clone()),
        }) {
            Ok(()) => succeeded += 1,
            Err(error) => {
                failed += 1;
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
    }

    match (succeeded, first_error.as_ref()) {
        (0, Some(error)) => command_controller.report_command_error(error),
        (_, Some(_error)) => {
            command_controller
                .report_command_warning(gettext("Some selected tracks could not be updated."));
        }
        (_, None) => {}
    }

    failed
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
    bpm: CommonValue<Bpm>,
    key: CommonValue<MusicalKey>,
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
    bpm: OptionalEntry<Bpm>,
    key: OptionalEntry<MusicalKey>,
    comments: OptionalTextView,
}

impl BatchDetailsPage {
    fn new(initial: &BatchInitialValues, genre_suggestions: Vec<String>) -> Self {
        let widget = gtk::Grid::new();
        widget.set_row_spacing(6);
        widget.set_column_spacing(10);
        widget.set_hexpand(true);

        let mut row = 0;
        let title = OptionalEntry::text(&widget, row, &gettext("Title"), initial.title.clone());
        row += 1;
        let artist = OptionalEntry::text(&widget, row, &gettext("Artist"), initial.artist.clone());
        row += 1;
        let album = OptionalEntry::text(&widget, row, &gettext("Album"), initial.album.clone());
        row += 1;
        let album_artist = OptionalEntry::text(
            &widget,
            row,
            &gettext("Album artist"),
            initial.album_artist.clone(),
        );
        row += 1;
        let composer =
            OptionalEntry::text(&widget, row, &gettext("Composer"), initial.composer.clone());
        row += 1;
        let grouping =
            OptionalEntry::text(&widget, row, &gettext("Grouping"), initial.grouping.clone());
        row += 1;
        let genre = OptionalEntry::text(&widget, row, &gettext("Genre"), initial.genre.clone());
        // The library's genres, as on the single-track page (#301).
        crate::suggestion_entry::attach_suggestions(&genre.entry, genre_suggestions);
        row += 1;
        let year = OptionalEntry::number(
            &widget,
            row,
            &gettext("Year"),
            NUMBER_ENTRY_WIDTH_CHARS,
            initial.year.clone(),
        );
        row += 1;
        let track_number = OptionalEntry::number(
            &widget,
            row,
            &gettext("Track number"),
            PAIR_ENTRY_WIDTH_CHARS,
            initial.track_number.clone(),
        );
        row += 1;
        let track_total = OptionalEntry::number(
            &widget,
            row,
            &gettext("Track total"),
            PAIR_ENTRY_WIDTH_CHARS,
            initial.track_total.clone(),
        );
        row += 1;
        let disc_number = OptionalEntry::number(
            &widget,
            row,
            &gettext("Disc number"),
            PAIR_ENTRY_WIDTH_CHARS,
            initial.disc_number.clone(),
        );
        row += 1;
        let disc_total = OptionalEntry::number(
            &widget,
            row,
            &gettext("Disc total"),
            PAIR_ENTRY_WIDTH_CHARS,
            initial.disc_total.clone(),
        );
        row += 1;
        let compilation = OptionalBool::new(
            &widget,
            row,
            &gettext("Compilation"),
            &gettext("Album is a compilation of songs by various artists"),
            initial.compilation.clone(),
        );
        row += 1;
        let bpm = OptionalEntry::number(
            &widget,
            row,
            &gettext("BPM"),
            NUMBER_ENTRY_WIDTH_CHARS,
            initial.bpm.clone(),
        );
        row += 1;
        let key = OptionalEntry::new(
            &widget,
            row,
            &pgettext("musical key", "Key"),
            gtk::Entry::new(),
            initial.key.clone(),
        );
        row += 1;
        key.entry.set_width_chars(8);
        key.entry.set_hexpand(false);
        let comments =
            OptionalTextView::new(&widget, row, &gettext("Comments"), initial.comments.clone());

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
            year: number_change(
                &gettext("Year"),
                &self.year.initial,
                self.year.selected_text(),
            )?,
            track_number: number_change(
                &gettext("Track number"),
                &self.track_number.initial,
                self.track_number.selected_text(),
            )?,
            track_total: number_change(
                &gettext("Track total"),
                &self.track_total.initial,
                self.track_total.selected_text(),
            )?,
            disc_number: number_change(
                &gettext("Disc number"),
                &self.disc_number.initial,
                self.disc_number.selected_text(),
            )?,
            disc_total: number_change(
                &gettext("Disc total"),
                &self.disc_total.initial,
                self.disc_total.selected_text(),
            )?,
            compilation: self.compilation.selected_value(),
            bpm: bpm_change(&self.bpm.initial, self.bpm.selected_text())?,
            key: key_change(&self.key.initial, self.key.selected_text())?,
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

/// A key in any notation [`MusicalKey::parse`] reads.
fn key_change(
    initial: &CommonValue<MusicalKey>,
    value: Option<String>,
) -> Result<FieldChange<MusicalKey>, String> {
    parsed_change(initial, value, parse_key)
}

/// A whole or decimal tempo, as [`Bpm::parse`] reads it.
fn bpm_change(
    initial: &CommonValue<Bpm>,
    value: Option<String>,
) -> Result<FieldChange<Bpm>, String> {
    parsed_change(initial, value, parse_bpm)
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
    parsed_change(initial, value, |text| parse_whole_number(label, text))
}

/// The change an enabled batch entry makes: blank text clears the field,
/// text `parse` reads sets it unless every track already holds that
/// value, and text `parse` rejects is an error stating what the field
/// accepts. A disabled entry (`value` is `None`) changes nothing.
fn parsed_change<T: Clone + Eq>(
    initial: &CommonValue<T>,
    value: Option<String>,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> Result<FieldChange<T>, String> {
    let Some(value) = value else {
        return Ok(FieldChange::Unchanged);
    };
    let value = value.trim();
    let current = if value.is_empty() {
        None
    } else {
        Some(parse(value)?)
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
    fn the_closed_dialog_is_released() {
        crate::test_support::with_gtk(|| {
            // A genre gives the Genre field its suggestion list.
            let mut first = crate::test_support::track(1, "One");
            first.metadata.genre = Some("Rock".to_owned());
            let runtime = crate::test_support::runtime_with_tracks(vec![
                first,
                crate::test_support::track(2, "Two"),
            ]);
            let command_controller: crate::command_controller::SharedCommandController =
                std::rc::Rc::new(crate::command_controller::UiCommandController::new(runtime));
            let parent = gtk::Window::new();
            super::open_multi_track_info_dialog(
                &parent,
                &command_controller,
                vec![
                    sustain_app_runtime::TrackId::new(1).expect("track id"),
                    sustain_app_runtime::TrackId::new(2).expect("track id"),
                ],
            );
            crate::test_support::assert_closes_and_disposes("Get Info - 2 Tracks");
            parent.destroy();
        });
    }

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
