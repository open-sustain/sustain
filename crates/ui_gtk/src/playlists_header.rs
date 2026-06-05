// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Header strip drawn above the Playlists view's track table. Mirrors the
//! album-detail "title + play/shuffle + summary" layout but for the
//! sidebar's currently selected entry (regular playlist, smart playlist,
//! or the Library pseudo-row). Hidden when the selection has no tracks of
//! its own to play (folders, no selection).

use std::rc::Rc;

use gtk::prelude::*;
use sustain_i18n::{gettext, ngettext, tr_format};

use crate::TITLEBAR_HEIGHT;
use crate::status_bar::duration_text;

/// Snapshot of what the header should display. Computed once per refresh
/// alongside the table rows; `None` means "no playable selection — hide
/// the header entirely".
pub(crate) struct PlaylistsHeaderState {
    pub(crate) title: String,
    pub(crate) track_count: usize,
    pub(crate) duration_seconds: u64,
}

#[derive(Clone)]
pub(crate) struct PlaylistsHeader {
    root: gtk::Box,
    title: gtk::Label,
    subtitle: gtk::Label,
    play_button: gtk::Button,
    shuffle_button: gtk::Button,
}

impl PlaylistsHeader {
    pub(crate) fn new() -> Self {
        let root = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        root.add_css_class("playlists-header");
        // The user-facing requirement is "same height as the title bar";
        // TITLEBAR_HEIGHT is the single source of truth for that value.
        root.set_height_request(TITLEBAR_HEIGHT);
        root.set_hexpand(true);
        root.set_visible(false);

        let title_block = gtk::Box::new(gtk::Orientation::Vertical, 2);
        title_block.set_valign(gtk::Align::Center);
        title_block.set_hexpand(false);

        let title_row = gtk::Box::new(gtk::Orientation::Horizontal, 6);

        let title = gtk::Label::new(None);
        title.add_css_class("playlists-header-title");
        title.set_xalign(0.0);
        title.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_row.append(&title);

        let play_button = header_icon_button("media-playback-start-symbolic", &gettext("Play"));
        title_row.append(&play_button);

        let shuffle_button =
            header_icon_button("media-playlist-shuffle-symbolic", &gettext("Shuffle"));
        title_row.append(&shuffle_button);

        title_block.append(&title_row);

        let subtitle = gtk::Label::new(None);
        subtitle.add_css_class("playlists-header-subtitle");
        subtitle.set_xalign(0.0);
        subtitle.set_ellipsize(gtk::pango::EllipsizeMode::End);
        title_block.append(&subtitle);

        root.append(&title_block);

        // Trailing spacer absorbs the rest of the row so the title block
        // stays anchored to the left rather than being pushed around by
        // the title's natural width.
        let spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
        spacer.set_hexpand(true);
        root.append(&spacer);

        Self {
            root,
            title,
            subtitle,
            play_button,
            shuffle_button,
        }
    }

    pub(crate) fn widget(&self) -> &gtk::Box {
        &self.root
    }

    pub(crate) fn set_state(&self, state: Option<PlaylistsHeaderState>) {
        match state {
            Some(state) => {
                self.title.set_text(&state.title);
                self.subtitle
                    .set_text(&summary_text(state.track_count, state.duration_seconds));
                self.root.set_visible(true);
            }
            None => {
                self.root.set_visible(false);
            }
        }
    }

    pub(crate) fn connect_play(&self, callback: Rc<dyn Fn()>) {
        self.play_button
            .connect_clicked(move |_| callback.as_ref()());
    }

    pub(crate) fn connect_shuffle(&self, callback: Rc<dyn Fn()>) {
        self.shuffle_button
            .connect_clicked(move |_| callback.as_ref()());
    }
}

fn summary_text(track_count: usize, duration_seconds: u64) -> String {
    let songs = tr_format!(
        // Translators: a track count, e.g. "12 songs".
        ngettext("{count} song", "{count} songs", track_count as u32),
        count = track_count,
    );
    // Translators: {songs} and {duration} are already-localized phrases such as
    // "12 songs" / "2 hours"; keep the placeholders verbatim.
    tr_format!(
        gettext("{songs}, {duration}"),
        songs = songs,
        duration = duration_text(duration_seconds),
    )
}

fn header_icon_button(icon_name: &str, tooltip: &str) -> gtk::Button {
    let image = gtk::Image::from_icon_name(icon_name);
    image.set_pixel_size(18);

    let button = gtk::Button::new();
    button.add_css_class("playlists-header-icon-button");
    button.set_child(Some(&image));
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

#[cfg(test)]
mod tests {
    use super::summary_text;

    #[test]
    fn summary_uses_plural_and_hours() {
        assert_eq!(summary_text(12, 7_200), "12 songs, 2 hours");
    }

    #[test]
    fn summary_uses_singular_song_and_minute() {
        assert_eq!(summary_text(1, 60), "1 song, 1 minute");
    }

    #[test]
    fn summary_uses_days_for_very_long_lists() {
        assert_eq!(summary_text(500, 172_800), "500 songs, 2 days");
    }
}
