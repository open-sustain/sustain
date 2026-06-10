// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{cell::Cell, rc::Rc};

use gtk::glib;
use gtk::prelude::*;
use sustain_app_runtime::{FieldChange, MetadataChange, PlayStatistics, Rating, TrackMetadata};

use crate::date_format::format_system_time_short;

use crate::metadata_diff::{bool_diff, number_diff, signed_number_diff, text_diff};
use crate::util::sync_rating_button;

use super::{
    NUMBER_ENTRY_WIDTH_CHARS, PAIR_ENTRY_WIDTH_CHARS,
    form::{attach_field, attach_paired_field, number_entry, text_entry},
};

#[derive(Clone)]
pub(super) struct DetailsPage {
    pub(super) widget: gtk::Box,
    title: gtk::Entry,
    artist: gtk::Entry,
    album: gtk::Entry,
    album_artist: gtk::Entry,
    composer: gtk::Entry,
    grouping: gtk::Entry,
    genre: gtk::Entry,
    year: gtk::Entry,
    track_number: gtk::Entry,
    track_total: gtk::Entry,
    disc_number: gtk::Entry,
    disc_total: gtk::Entry,
    compilation: gtk::CheckButton,
    bpm: gtk::Entry,
    key: gtk::Entry,
    comments: gtk::TextView,
    rating_widget: gtk::Box,
    rating: Rc<Cell<u8>>,
    play_count_reset: Rc<Cell<bool>>,
    play_count_label: gtk::Label,
    reset_button: gtk::Button,
    skip_count_label: gtk::Label,
    last_played_label: gtk::Label,
    last_skipped_label: gtk::Label,
}

impl DetailsPage {
    pub(super) fn new(
        initial: &TrackMetadata,
        initial_rating: Rating,
        initial_statistics: &PlayStatistics,
        genre_suggestions: Vec<String>,
    ) -> Self {
        let widget = gtk::Box::new(gtk::Orientation::Vertical, 8);
        widget.add_css_class("track-info-details");
        widget.set_margin_top(10);

        let grid = gtk::Grid::new();
        grid.set_row_spacing(6);
        grid.set_column_spacing(10);
        grid.set_hexpand(true);

        let mut row: i32 = 0;
        let title = text_entry(initial.title.as_deref());
        attach_field(&grid, row, "Title", &title);
        row += 1;
        let artist = text_entry(initial.artist.as_deref());
        attach_field(&grid, row, "Artist", &artist);
        row += 1;
        let album = text_entry(initial.album.as_deref());
        attach_field(&grid, row, "Album", &album);
        row += 1;
        let album_artist = text_entry(initial.album_artist.as_deref());
        attach_field(&grid, row, "Album artist", &album_artist);
        row += 1;
        let composer = text_entry(initial.composer.as_deref());
        attach_field(&grid, row, "Composer", &composer);
        row += 1;
        let grouping = text_entry(initial.grouping.as_deref());
        attach_field(&grid, row, "Grouping", &grouping);
        row += 1;
        let genre = text_entry(initial.genre.as_deref());
        // Offer the library's existing genres as autocomplete so the user
        // reuses a spelling instead of splintering one ("Hip-Hop" vs "Hip
        // Hop") across stats and smart playlists (#160).
        crate::suggestion_entry::attach_suggestions(&genre, genre_suggestions);
        attach_field(&grid, row, "Genre", &genre);
        row += 1;

        let year = number_entry(initial.year, NUMBER_ENTRY_WIDTH_CHARS);
        attach_field(&grid, row, "Year", &year);
        row += 1;

        let track_number = number_entry(initial.track_number, PAIR_ENTRY_WIDTH_CHARS);
        let track_total = number_entry(initial.track_total, PAIR_ENTRY_WIDTH_CHARS);
        attach_paired_field(&grid, row, "Track", &track_number, &track_total);
        row += 1;

        let disc_number = number_entry(initial.disc_number, PAIR_ENTRY_WIDTH_CHARS);
        let disc_total = number_entry(initial.disc_total, PAIR_ENTRY_WIDTH_CHARS);
        attach_paired_field(&grid, row, "Disc", &disc_number, &disc_total);
        row += 1;

        let compilation =
            gtk::CheckButton::with_label("Album is a compilation of songs by various artists");
        compilation.set_active(initial.compilation.unwrap_or(false));
        grid.attach(&compilation, 1, row, 3, 1);
        row += 1;

        let bpm = number_entry(initial.bpm, NUMBER_ENTRY_WIDTH_CHARS);
        attach_field(&grid, row, "BPM", &bpm);
        row += 1;

        let key = text_entry(initial.key.as_deref());
        key.set_width_chars(8);
        key.set_hexpand(false);
        attach_field(&grid, row, "Key", &key);
        row += 1;

        let rating_widget = gtk::Box::new(gtk::Orientation::Horizontal, 2);
        let rating = Rc::new(Cell::new(initial_rating.stars()));
        for star in 1u8..=5 {
            let button = rating_star_button(star);
            sync_rating_button(&button, star, rating.get());
            let rating_state = rating.clone();
            let parent_box = rating_widget.clone();
            button.connect_clicked(move |_| {
                let next = next_rating(rating_state.get(), star);
                rating_state.set(next);
                refresh_rating_buttons(&parent_box, next);
            });
            rating_widget.append(&button);
        }
        attach_field(&grid, row, "Rating", &rating_widget);
        row += 1;

        // Statistics block: counts on the left, dates underneath.
        // The Reset button is shared across play+skip counts because
        // they represent two halves of the same per-track listening
        // history; resetting one half in isolation is rarely what the
        // user wants. The labels snapshot the values at dialog open
        // and stay frozen until the next time the dialog is reopened.
        let stats_reset = Rc::new(Cell::new(false));

        let play_count_row = gtk::Box::new(gtk::Orientation::Horizontal, 8);
        let play_count_label = gtk::Label::new(None);
        play_count_label.set_xalign(0.0);
        play_count_label.set_hexpand(true);
        play_count_label.set_text(&initial_statistics.play_count.to_string());
        let reset_button = gtk::Button::with_label("Reset");
        let reset_state = stats_reset.clone();
        let play_label_for_reset = play_count_label.clone();
        reset_button.connect_clicked(move |button| {
            reset_state.set(true);
            button.set_sensitive(false);
            play_label_for_reset.set_text("0 (will reset on OK)");
        });
        play_count_row.append(&play_count_label);
        play_count_row.append(&reset_button);
        attach_field(&grid, row, "Play count", &play_count_row);
        row += 1;

        let skip_count_label = gtk::Label::new(None);
        skip_count_label.set_xalign(0.0);
        skip_count_label.set_hexpand(true);
        skip_count_label.set_text(&initial_statistics.skip_count.to_string());
        attach_field(&grid, row, "Skip count", &skip_count_label);
        row += 1;

        let last_played_label = gtk::Label::new(None);
        last_played_label.set_xalign(0.0);
        last_played_label.set_hexpand(true);
        last_played_label.set_text(&format_stat_date(initial_statistics.last_played_at));
        attach_field(&grid, row, "Last played", &last_played_label);
        row += 1;

        let last_skipped_label = gtk::Label::new(None);
        last_skipped_label.set_xalign(0.0);
        last_skipped_label.set_hexpand(true);
        last_skipped_label.set_text(&format_stat_date(initial_statistics.last_skipped_at));
        attach_field(&grid, row, "Last skipped", &last_skipped_label);
        row += 1;

        let play_count_reset = stats_reset;

        let comments = gtk::TextView::new();
        comments.set_wrap_mode(gtk::WrapMode::WordChar);
        comments.set_accepts_tab(false);
        comments.set_top_margin(16);
        comments.set_bottom_margin(16);
        comments.set_left_margin(16);
        comments.set_right_margin(16);
        if let Some(text) = initial.comments.as_deref() {
            comments.buffer().set_text(text);
        }
        let comments_scroll = gtk::ScrolledWindow::new();
        comments_scroll.set_min_content_height(70);
        comments_scroll.set_hexpand(true);
        comments_scroll.set_child(Some(&comments));
        let comments_label = gtk::Label::new(Some("Comments"));
        comments_label.set_xalign(0.0);
        comments_label.set_valign(gtk::Align::Start);
        comments_label.add_css_class("track-info-field-label");
        grid.attach(&comments_label, 0, row, 1, 1);
        grid.attach(&comments_scroll, 1, row, 3, 1);

        widget.append(&grid);

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
            rating_widget,
            rating,
            play_count_reset,
            play_count_label,
            reset_button,
            skip_count_label,
            last_played_label,
            last_skipped_label,
        }
    }

    pub(super) fn reload(
        &self,
        metadata: &TrackMetadata,
        rating: Rating,
        statistics: &PlayStatistics,
    ) {
        set_text(&self.title, metadata.title.as_deref());
        set_text(&self.artist, metadata.artist.as_deref());
        set_text(&self.album, metadata.album.as_deref());
        set_text(&self.album_artist, metadata.album_artist.as_deref());
        set_text(&self.composer, metadata.composer.as_deref());
        set_text(&self.grouping, metadata.grouping.as_deref());
        set_text(&self.genre, metadata.genre.as_deref());
        set_optional_number(&self.year, metadata.year);
        set_optional_number(&self.track_number, metadata.track_number);
        set_optional_number(&self.track_total, metadata.track_total);
        set_optional_number(&self.disc_number, metadata.disc_number);
        set_optional_number(&self.disc_total, metadata.disc_total);
        self.compilation
            .set_active(metadata.compilation.unwrap_or(false));
        set_optional_number(&self.bpm, metadata.bpm);
        set_text(&self.key, metadata.key.as_deref());
        self.comments
            .buffer()
            .set_text(metadata.comments.as_deref().unwrap_or_default());

        self.rating.set(rating.stars());
        refresh_rating_buttons(&self.rating_widget, rating.stars());
        self.play_count_reset.set(false);
        self.reset_button.set_sensitive(true);
        self.play_count_label
            .set_text(&statistics.play_count.to_string());
        self.skip_count_label
            .set_text(&statistics.skip_count.to_string());
        self.last_played_label
            .set_text(&format_stat_date(statistics.last_played_at));
        self.last_skipped_label
            .set_text(&format_stat_date(statistics.last_skipped_at));
    }

    pub(super) fn metadata_diff(&self, initial: &TrackMetadata) -> MetadataChange {
        MetadataChange {
            title: text_diff(initial.title.as_deref(), &self.title.text()),
            artist: text_diff(initial.artist.as_deref(), &self.artist.text()),
            album: text_diff(initial.album.as_deref(), &self.album.text()),
            album_artist: text_diff(initial.album_artist.as_deref(), &self.album_artist.text()),
            composer: text_diff(initial.composer.as_deref(), &self.composer.text()),
            grouping: text_diff(initial.grouping.as_deref(), &self.grouping.text()),
            genre: text_diff(initial.genre.as_deref(), &self.genre.text()),
            track_number: number_diff(initial.track_number, &self.track_number.text()),
            track_total: number_diff(initial.track_total, &self.track_total.text()),
            disc_number: number_diff(initial.disc_number, &self.disc_number.text()),
            disc_total: number_diff(initial.disc_total, &self.disc_total.text()),
            year: signed_number_diff(initial.year, &self.year.text()),
            compilation: bool_diff(initial.compilation, self.compilation.is_active()),
            bpm: number_diff(initial.bpm, &self.bpm.text()),
            key: text_diff(initial.key.as_deref(), &self.key.text()),
            comments: text_diff(
                initial.comments.as_deref(),
                &self.text_view_text(&self.comments),
            ),
            lyrics: FieldChange::Unchanged,
            ..MetadataChange::default()
        }
    }

    pub(super) fn current_rating(&self) -> Rating {
        Rating::new(self.rating.get()).unwrap_or_else(Rating::unrated)
    }

    pub(super) fn play_count_reset_requested(&self) -> bool {
        self.play_count_reset.get()
    }

    fn text_view_text(&self, view: &gtk::TextView) -> glib::GString {
        let buffer = view.buffer();
        buffer.text(&buffer.start_iter(), &buffer.end_iter(), false)
    }
}

fn set_text(entry: &gtk::Entry, value: Option<&str>) {
    entry.set_text(value.unwrap_or_default());
}

fn set_optional_number<T: ToString>(entry: &gtk::Entry, value: Option<T>) {
    entry.set_text(&value.map(|value| value.to_string()).unwrap_or_default());
}

fn rating_star_button(star: u8) -> gtk::Button {
    let button = gtk::Button::new();
    button.add_css_class("flat");
    button.add_css_class("track-info-rating-star");
    button.set_focusable(false);
    button.set_tooltip_text(Some(&format!(
        "{star} star{}",
        if star == 1 { "" } else { "s" }
    )));
    button
}

fn refresh_rating_buttons(parent: &gtk::Box, rating: u8) {
    let mut child = parent.first_child();
    let mut star: u8 = 1;
    while let Some(widget) = child {
        if let Some(button) = widget.downcast_ref::<gtk::Button>() {
            sync_rating_button(button, star, rating);
        }
        child = widget.next_sibling();
        star += 1;
    }
}

fn next_rating(current: u8, clicked: u8) -> u8 {
    if current == clicked { 0 } else { clicked }
}

fn format_stat_date(value: Option<std::time::SystemTime>) -> String {
    value
        .and_then(format_system_time_short)
        .unwrap_or_else(|| "Never".to_owned())
}

#[cfg(test)]
mod tests {
    use super::next_rating;

    #[test]
    fn next_rating_toggles_off_when_clicking_current_star() {
        assert_eq!(next_rating(3, 3), 0);
        assert_eq!(next_rating(3, 4), 4);
        assert_eq!(next_rating(0, 2), 2);
    }
}
