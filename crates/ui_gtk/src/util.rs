// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Small leaf helpers shared across the GTK view models.

use gtk::prelude::*;
use sustain_app_runtime::{Track, TrackMetadata};

/// Unicode glyph for an unfilled rating star.
const EMPTY_STAR: &str = "☆";
/// Unicode glyph for a filled rating star.
const FILLED_STAR: &str = "★";

/// Returns the trimmed text of an optional string field, or `None` when the
/// value is absent or blank once trimmed. Used by the view models to fall back
/// to placeholder text for missing metadata.
pub(crate) fn non_empty_text(value: &Option<String>) -> Option<String> {
    value
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

/// Display title for a track: its tag title, falling back to the file
/// stem when the title tag is blank. Shared by the now-playing tile and
/// the play-queue popover so both render identical titles.
pub(crate) fn display_title(track: &Track) -> String {
    non_empty_text(&track.metadata.title)
        .or_else(|| {
            track
                .location
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::trim)
                .filter(|stem| !stem.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

/// Display artist for a track's metadata, falling back to a neutral
/// placeholder when the artist tag is blank. Used where a row always
/// needs a populated second line (the play-queue popover), unlike the
/// now-playing tile which leaves the line empty.
pub(crate) fn display_artist(metadata: &TrackMetadata) -> String {
    non_empty_text(&metadata.artist).unwrap_or_else(|| "Unknown Artist".to_owned())
}

/// Renders one rating-star button at position `star` against the current
/// `rating`: sets the filled/empty glyph and toggles the `rating-star-filled`
/// / `rating-star-empty` CSS classes. Shared by the track table cells and the
/// track-info detail page so both style identically.
pub(crate) fn sync_rating_button(button: &gtk::Button, star: u8, rating: u8) {
    if star <= rating {
        button.set_label(FILLED_STAR);
        set_exclusive_css_class(button, "rating-star-filled", "rating-star-empty");
    } else {
        button.set_label(EMPTY_STAR);
        set_exclusive_css_class(button, "rating-star-empty", "rating-star-filled");
    }
}

/// Gives `widget` the CSS class `class` and takes away `other`, its
/// mutually exclusive counterpart. Every class change restyles the widget,
/// so a widget that already carries `class` is left untouched: refreshing
/// hundreds of realized cells to the state they already show must not
/// restyle each of them.
pub(crate) fn set_exclusive_css_class(widget: &impl IsA<gtk::Widget>, class: &str, other: &str) {
    if !widget.has_css_class(class) {
        widget.remove_css_class(other);
        widget.add_css_class(class);
    }
}
