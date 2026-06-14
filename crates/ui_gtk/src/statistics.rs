// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The Statistics screen (issue #20): a single scrollable page of
//! whole-library diagnostic charts — genre and bitrate distributions,
//! the most-played and most-liked genres, and release-decade / year-added
//! histograms.
//!
//! Every figure comes from the runtime's in-memory track list, which is
//! the authoritative SQLite copy; nothing re-reads file tags at view
//! time. The aggregation itself lives in
//! [`sustain_app_runtime::compute_library_statistics`] so its selection
//! rules are unit-tested without a UI; this module only turns the result
//! into widgets.
//!
//! Layout: each chart sits in its own card (a contrasting surface). The
//! the three part-of-a-whole charts use donuts drawn by [`crate::chart`] with
//! an accent-derived palette and a legend; the weighted most-liked ranking is
//! a bar chart. These four cards flow into a responsive
//! two-up grid that reflows to one column when the window is narrow; the
//! two distributions over time (release decade, year added) run full
//! width below as vertical-bar histograms. Every chart is drawn with
//! Cairo against the live theme accent, so the page tracks the system
//! accent in both light and dark themes.
//!
//! The page is rebuilt lazily — on the first switch to it and on a
//! library change while it is on screen — never during cold start, so
//! the startup budget covers only the cheap default Music page.

use std::time::{SystemTime, UNIX_EPOCH};

use gtk::glib;
use gtk::prelude::*;

use sustain_app_runtime::{
    DecadeCount, GenreDistribution, GenrePlayCount, GenreRating, QualityDistribution, QualityRange,
    YearCount, compute_library_statistics,
};

use crate::SharedRuntime;
use crate::chart::{self, DonutSlice, VerticalBar};

/// Gap between cards, in pixels, both within the donut grid and above the
/// full-width histograms.
const CARD_SPACING: i32 = 14;

#[derive(Clone)]
pub(crate) struct StatisticsView {
    scroller: gtk::ScrolledWindow,
    /// The vertical stack of cards; cleared and rebuilt by
    /// [`Self::refresh`].
    content: gtk::Box,
    runtime: SharedRuntime,
}

impl StatisticsView {
    pub(crate) fn new(runtime: SharedRuntime) -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, CARD_SPACING);
        content.add_css_class("statistics-page");
        content.set_hexpand(true);

        let scroller = gtk::ScrolledWindow::new();
        scroller.set_policy(gtk::PolicyType::Never, gtk::PolicyType::Automatic);
        scroller.set_hexpand(true);
        scroller.set_vexpand(true);
        scroller.set_child(Some(&content));

        Self {
            scroller,
            content,
            runtime,
        }
    }

    pub(crate) fn widget(&self) -> gtk::ScrolledWindow {
        self.scroller.clone()
    }

    /// Rebuild the page from the current library. Cheap (one O(n) pass
    /// plus a few dozen small widgets), so it runs on every switch to the
    /// view rather than carrying dirty-flag bookkeeping — the figures are
    /// always current.
    pub(crate) fn refresh(&self) {
        while let Some(child) = self.content.first_child() {
            self.content.remove(&child);
        }

        let stats = compute_library_statistics(self.runtime.borrow().library_tracks(), local_year);

        if stats.total_tracks == 0 {
            let empty = gtk::Label::new(Some(
                "No statistics yet — add music to your library to see its breakdown here.",
            ));
            empty.add_css_class("statistics-empty-page");
            empty.set_xalign(0.0);
            empty.set_wrap(true);
            self.content.append(&empty);
            return;
        }

        // The four compact cards reflow into a two-up grid (one column when
        // narrow); the homogeneous sizing keeps the tiles aligned.
        let grid = gtk::FlowBox::new();
        grid.set_orientation(gtk::Orientation::Horizontal);
        grid.set_min_children_per_line(1);
        grid.set_max_children_per_line(2);
        grid.set_column_spacing(CARD_SPACING.unsigned_abs());
        grid.set_row_spacing(CARD_SPACING.unsigned_abs());
        grid.set_homogeneous(true);
        grid.set_selection_mode(gtk::SelectionMode::None);
        grid.set_hexpand(true);
        grid.insert(&genre_distribution_section(&stats.genre_distribution), -1);
        grid.insert(
            &quality_distribution_section(&stats.quality_distribution),
            -1,
        );
        grid.insert(&most_played_section(&stats.most_played_genres), -1);
        grid.insert(&most_liked_section(&stats.most_liked_genres), -1);
        self.content.append(&grid);

        self.content
            .append(&release_years_section(&stats.release_decades));
        self.content
            .append(&added_years_section(&stats.added_years));
    }

    /// Rebuild only when the page is currently on screen. Used by the
    /// library-changed path: an off-screen page is refreshed by its
    /// activator the next time it is shown, so there is no point
    /// rebuilding it now.
    pub(crate) fn refresh_if_visible(&self) {
        if self.scroller.is_mapped() {
            self.refresh();
        }
    }
}

/// The local calendar year of a `SystemTime`, or `None` for instants the
/// calendar cannot place (before the Unix epoch). Backs the year-added
/// bucketing; the release-year axis needs no calendar (it is already a
/// plain year).
fn local_year(time: SystemTime) -> Option<i32> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?;
    let seconds = i64::try_from(seconds.as_secs()).ok()?;
    Some(glib::DateTime::from_unix_local(seconds).ok()?.year())
}

/// `"<count> (<pct>%)"` — a count alongside its share of `total`. Used by
/// integer-count distribution donut legends.
fn count_share_text(count: usize, total: usize) -> String {
    quantity_share_text(count as f64, total as f64)
}

/// `"<quantity> (<pct>%)"` for fractional weighted quantities.
fn quantity_share_text(quantity: f64, total: f64) -> String {
    let percent = if total == 0.0 {
        0.0
    } else {
        quantity / total * 100.0
    };
    format!("{} ({percent:.0}%)", quantity_text(quantity))
}

fn quantity_text(quantity: f64) -> String {
    let rounded = quantity.round();
    if (quantity - rounded).abs() < 0.005 {
        return format!("{rounded:.0}");
    }

    let mut text = format!("{quantity:.2}");
    while text.ends_with('0') {
        text.pop();
    }
    if text.ends_with('.') {
        text.pop();
    }
    text
}

/// A genre name for display, mapping the "no genre tag" case to a
/// readable label.
fn genre_label(genre: &Option<String>) -> String {
    genre.clone().unwrap_or_else(|| "Unknown".to_owned())
}

/// The centred readout shown inside a donut's hole: the denominator the
/// slices are shares of, over a unit word.
fn donut_center(total: impl std::fmt::Display, unit: &str) -> String {
    format!("{total}\n{unit}")
}

fn genre_distribution_section(distribution: &GenreDistribution) -> gtk::Widget {
    let total = distribution.total_tracks;
    let mut slices: Vec<DonutSlice> = distribution
        .entries
        .iter()
        .map(|entry| DonutSlice {
            label: genre_label(&entry.genre),
            fraction: fraction_f64(entry.track_weight, total as f64),
            value: quantity_share_text(entry.track_weight, total as f64),
            muted: false,
        })
        .collect();
    if let Some(other) = distribution.other {
        slices.push(DonutSlice {
            label: format!(
                "Other ({} {})",
                other.genre_count,
                if other.genre_count == 1 {
                    "genre"
                } else {
                    "genres"
                }
            ),
            fraction: fraction_f64(other.track_weight, total as f64),
            value: quantity_share_text(other.track_weight, total as f64),
            muted: true,
        });
    }
    let content =
        (!slices.is_empty()).then(|| chart::donut(slices, Some(donut_center(total, "tracks"))));
    section(
        "Genre",
        Some("Share of tracks per genre across the whole library."),
        content,
        "No genres tagged yet.",
    )
}

fn quality_distribution_section(distribution: &QualityDistribution) -> gtk::Widget {
    let total = distribution.total_with_bitrate;
    let slices: Vec<DonutSlice> = if total == 0 {
        Vec::new()
    } else {
        distribution
            .buckets
            .iter()
            .map(|bucket| DonutSlice {
                label: quality_label(bucket.range).to_owned(),
                fraction: fraction(bucket.track_count, total),
                value: count_share_text(bucket.track_count, total),
                muted: false,
            })
            .collect()
    };
    let content =
        (!slices.is_empty()).then(|| chart::donut(slices, Some(donut_center(total, "tracks"))));
    section(
        "Quality",
        Some("Share of tracks per bitrate range."),
        content,
        "No tracks carry a bitrate yet.",
    )
}

fn quality_label(range: QualityRange) -> &'static str {
    match range {
        QualityRange::UpTo128 => "≤ 128 kbps",
        QualityRange::Between129And255 => "129–255 kbps",
        QualityRange::Between256And320 => "256–320 kbps",
        QualityRange::Above320 => "> 320 kbps",
    }
}

fn most_played_section(genres: &[GenrePlayCount]) -> gtk::Widget {
    // A play count is a real quantity, so the donut shows each genre's
    // share of the plays among the most-played five.
    let total: f64 = genres.iter().map(|genre| genre.total_play_count).sum();
    let slices: Vec<DonutSlice> = genres
        .iter()
        .map(|genre| DonutSlice {
            label: genre_label(&genre.genre),
            fraction: fraction_f64(genre.total_play_count, total),
            value: quantity_share_text(genre.total_play_count, total),
            muted: false,
        })
        .collect();
    let content = (total > 0.0)
        .then(|| chart::donut(slices, Some(donut_center(quantity_text(total), "plays"))));
    section(
        "Most played genres",
        Some("Top 5 by total play count."),
        content,
        "No plays recorded yet.",
    )
}

fn most_liked_section(genres: &[GenreRating]) -> gtk::Widget {
    let max = genres
        .iter()
        .map(|genre| genre.total_stars)
        .max_by(|left, right| left.partial_cmp(right).unwrap_or(std::cmp::Ordering::Equal))
        .unwrap_or(0.0);
    let bars: Vec<VerticalBar> = genres
        .iter()
        .map(|genre| VerticalBar {
            label: genre_label(&genre.genre),
            fraction: fraction_f64(genre.total_stars, max),
            value: format!("{}★", quantity_text(genre.total_stars)),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::vertical_bars(bars));
    section(
        "Most liked genres",
        Some("Top 5 by total rating points across rated tracks."),
        content,
        "No ratings recorded yet.",
    )
}

fn release_years_section(decades: &[DecadeCount]) -> gtk::Widget {
    let max = decades
        .iter()
        .map(|decade| decade.track_count)
        .max()
        .unwrap_or(0);
    let bars: Vec<VerticalBar> = decades
        .iter()
        .map(|decade| VerticalBar {
            label: decade
                .decade
                .map(|year| format!("{year}s"))
                .unwrap_or_else(|| "Unknown".to_owned()),
            fraction: fraction(decade.track_count, max),
            value: decade.track_count.to_string(),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::vertical_bars(bars));
    section(
        "Release years",
        Some("Number of tracks per release decade."),
        content,
        "No release years tagged yet.",
    )
}

fn added_years_section(years: &[YearCount]) -> gtk::Widget {
    let max = years.iter().map(|year| year.track_count).max().unwrap_or(0);
    let bars: Vec<VerticalBar> = years
        .iter()
        .map(|year| VerticalBar {
            label: year
                .year
                .map(|year| year.to_string())
                .unwrap_or_else(|| "Unknown".to_owned()),
            fraction: fraction(year.track_count, max),
            value: year.track_count.to_string(),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::vertical_bars(bars));
    section(
        "Year added",
        Some("Number of tracks added to the library each year."),
        content,
        "Nothing added yet.",
    )
}

/// `numerator / denominator` as a `0.0..=1.0` bar fraction, guarding the
/// empty-denominator case.
fn fraction(numerator: usize, denominator: usize) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

fn fraction_f64(numerator: f64, denominator: f64) -> f64 {
    if denominator == 0.0 {
        0.0
    } else {
        numerator / denominator
    }
}

/// Assemble one chart card: a contrasting surface holding a title, an
/// explanatory caption, and either the supplied chart widget or — when
/// there is nothing to show — a muted `empty` line in its place.
fn section(
    title: &str,
    caption: Option<&str>,
    content: Option<gtk::Widget>,
    empty: &str,
) -> gtk::Widget {
    let card = gtk::Box::new(gtk::Orientation::Vertical, 12);
    card.add_css_class("statistics-card");
    card.set_hexpand(true);
    card.set_halign(gtk::Align::Fill);
    card.set_valign(gtk::Align::Fill);

    // Title and caption ride in a tight header block so the description
    // sits right under the title; the card spacing then separates that
    // header from the chart below.
    let header = gtk::Box::new(gtk::Orientation::Vertical, 1);

    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("statistics-section-title");
    title_label.set_xalign(0.0);
    header.append(&title_label);

    if let Some(caption) = caption {
        let caption_label = gtk::Label::new(Some(caption));
        caption_label.add_css_class("statistics-section-caption");
        caption_label.set_xalign(0.0);
        caption_label.set_wrap(true);
        header.append(&caption_label);
    }
    card.append(&header);

    match content {
        Some(widget) => {
            widget.set_vexpand(true);
            card.append(&widget);
        }
        None => {
            let empty_label = gtk::Label::new(Some(empty));
            empty_label.add_css_class("statistics-empty");
            empty_label.set_xalign(0.0);
            empty_label.set_vexpand(true);
            empty_label.set_valign(gtk::Align::Center);
            card.append(&empty_label);
        }
    }

    card.upcast()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn count_share_text_pairs_count_with_rounded_percent() {
        assert_eq!(count_share_text(0, 0), "0 (0%)");
        assert_eq!(count_share_text(0, 10), "0 (0%)");
        assert_eq!(count_share_text(1, 4), "1 (25%)");
        assert_eq!(count_share_text(1, 3), "1 (33%)");
        assert_eq!(count_share_text(10, 10), "10 (100%)");
    }

    #[test]
    fn quantity_share_text_trims_fractional_weights() {
        assert_eq!(quantity_share_text(0.5, 10.0), "0.5 (5%)");
        assert_eq!(quantity_share_text(1.0 / 3.0, 1.0), "0.33 (33%)");
        assert_eq!(quantity_share_text(10.0, 10.0), "10 (100%)");
    }

    #[test]
    fn fraction_guards_the_empty_denominator() {
        assert_eq!(fraction(3, 0), 0.0);
        assert_eq!(fraction(0, 10), 0.0);
        assert!((fraction(1, 4) - 0.25).abs() < f64::EPSILON);
        assert_eq!(fraction_f64(3.0, 0.0), 0.0);
        assert!((fraction_f64(0.5, 2.0) - 0.25).abs() < f64::EPSILON);
    }

    #[test]
    fn genre_label_maps_the_untagged_case() {
        assert_eq!(genre_label(&Some("House".to_owned())), "House");
        assert_eq!(genre_label(&None), "Unknown");
    }

    #[test]
    fn quality_labels_cover_every_range() {
        for range in QualityRange::ALL {
            assert!(!quality_label(range).is_empty());
        }
    }
}

/// Headless widget smoke test. Mirrors `shuffle_tab`'s `size_probe`: it
/// needs a display, so it skips cleanly under headless CI and runs on the
/// maintainer's machine. Builds the page over an empty library, maps it,
/// and spins the loop so the CSS resolves — proving the widget machinery
/// and CSS classes hold together without a panic.
#[cfg(test)]
mod widget_smoke {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::prelude::*;
    use sustain_app_runtime::ApplicationRuntime;

    use super::StatisticsView;

    #[test]
    fn empty_library_page_builds_and_maps() {
        let ran = crate::test_support::with_gtk(|| {
            let runtime = Rc::new(RefCell::new(ApplicationRuntime::new()));
            let view = StatisticsView::new(runtime);
            view.refresh();

            let window = gtk::Window::new();
            window.set_child(Some(&view.widget()));
            window.set_visible(true);
            let ctx = gtk::glib::MainContext::default();
            let mut spins = 0;
            while ctx.iteration(false) && spins < 200 {
                spins += 1;
            }

            // Mapping resolved without a panic; the scroller has realised a
            // child (a viewport wrapping the content box).
            assert!(view.widget().child().is_some());

            window.set_child(None::<&gtk::Widget>);
            window.destroy();
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
