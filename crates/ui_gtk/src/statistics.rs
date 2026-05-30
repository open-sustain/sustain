// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The Statistics screen (issue #20): a single scrollable page of
//! whole-library diagnostic charts — genre and bitrate distributions,
//! the most-played and most-liked genres, and release-year / year-added
//! histograms.
//!
//! Every figure comes from the runtime's in-memory track list, which is
//! the authoritative SQLite copy; nothing re-reads file tags at view
//! time. The aggregation itself lives in
//! [`sustain_app_runtime::compute_library_statistics`] so its selection
//! rules are unit-tested without a UI; this module only turns the result
//! into widgets.
//!
//! Every widget speaks one visual language: a labelled horizontal
//! proportion bar (a `GtkDrawingArea` that resolves the live theme accent
//! at draw time, like the device-sync occupation meter), so the page
//! reads consistently and tracks the system accent in both light and
//! dark themes.
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

/// Fixed character width of every chart's left label column, so the bars
/// of all six charts start at the same x and read as one aligned page.
const LABEL_WIDTH_CHARS: i32 = 16;

/// Height of each proportion bar, in pixels.
const BAR_HEIGHT: i32 = 14;

/// Opacity of the accent fill painted over the CSS trough — solid enough
/// to read crisply while letting the trough show at zero width.
const BAR_FILL_ALPHA: f64 = 0.85;

#[derive(Clone)]
pub(crate) struct StatisticsView {
    scroller: gtk::ScrolledWindow,
    /// The vertical stack of chart sections; cleared and rebuilt by
    /// [`Self::refresh`].
    content: gtk::Box,
    runtime: SharedRuntime,
}

impl StatisticsView {
    pub(crate) fn new(runtime: SharedRuntime) -> Self {
        let content = gtk::Box::new(gtk::Orientation::Vertical, 22);
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

        self.content
            .append(&genre_distribution_section(&stats.genre_distribution));
        self.content
            .append(&quality_distribution_section(&stats.quality_distribution));
        self.content
            .append(&most_played_section(&stats.most_played_genres));
        self.content
            .append(&most_liked_section(&stats.most_liked_genres));
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

/// One labelled proportion bar: a left label, a fill whose width is
/// `fraction` of the row, and a right-aligned value readout.
struct BarRow {
    label: String,
    fraction: f64,
    value: String,
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
/// the two distribution charts, whose bars are fractions of the library.
fn share_text(count: usize, total: usize) -> String {
    let percent = if total == 0 {
        0.0
    } else {
        count as f64 / total as f64 * 100.0
    };
    format!("{count} ({percent:.0}%)")
}

/// A genre name for display, mapping the "no genre tag" case to a
/// readable label.
fn genre_label(genre: &Option<String>) -> String {
    genre.clone().unwrap_or_else(|| "Unknown".to_owned())
}

fn genre_distribution_section(distribution: &GenreDistribution) -> gtk::Widget {
    let total = distribution.total_tracks;
    let mut rows: Vec<BarRow> = distribution
        .entries
        .iter()
        .map(|entry| BarRow {
            label: genre_label(&entry.genre),
            fraction: fraction(entry.track_count, total),
            value: share_text(entry.track_count, total),
        })
        .collect();
    if let Some(other) = distribution.other {
        rows.push(BarRow {
            label: format!(
                "Other ({} {})",
                other.genre_count,
                if other.genre_count == 1 {
                    "genre"
                } else {
                    "genres"
                }
            ),
            fraction: fraction(other.track_count, total),
            value: share_text(other.track_count, total),
        });
    }
    section(
        "Genre distribution",
        Some("Share of tracks per genre across the whole library."),
        rows,
        "No genres tagged yet.",
    )
}

fn quality_distribution_section(distribution: &QualityDistribution) -> gtk::Widget {
    let total = distribution.total_with_bitrate;
    let rows: Vec<BarRow> = distribution
        .buckets
        .iter()
        .map(|bucket| BarRow {
            label: quality_label(bucket.range).to_owned(),
            fraction: fraction(bucket.track_count, total),
            value: share_text(bucket.track_count, total),
        })
        .collect();
    let rows = if total == 0 { Vec::new() } else { rows };
    section(
        "Quality distribution",
        Some("Share of tracks per bitrate range."),
        rows,
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
    // Bars are scaled to the busiest genre, so the ranking reads as a
    // relative comparison rather than a share of some total.
    let max = genres
        .iter()
        .map(|genre| genre.total_play_count)
        .max()
        .unwrap_or(0);
    let rows: Vec<BarRow> = genres
        .iter()
        .map(|genre| BarRow {
            label: genre_label(&genre.genre),
            fraction: fraction_u64(genre.total_play_count, max),
            value: genre.total_play_count.to_string(),
        })
        .collect();
    section(
        "Most played genres",
        Some("Top 5 by total play count."),
        rows,
        "No plays recorded yet.",
    )
}

fn most_liked_section(genres: &[GenreRating]) -> gtk::Widget {
    let rows: Vec<BarRow> = genres
        .iter()
        .map(|genre| BarRow {
            label: genre_label(&genre.genre),
            // Absolute 0–5 scale: a full bar is a five-star average.
            fraction: genre.average_stars / f64::from(sustain_app_runtime::Rating::MAX_STARS),
            value: format!("{:.1}★ ({})", genre.average_stars, genre.rated_track_count),
        })
        .collect();
    section(
        "Most liked genres",
        Some("Top 5 by average rating; genres with at least 5 rated tracks."),
        rows,
        "Not enough rated tracks yet.",
    )
}

fn release_years_section(decades: &[DecadeCount]) -> gtk::Widget {
    let max = decades
        .iter()
        .map(|decade| decade.track_count)
        .max()
        .unwrap_or(0);
    let rows: Vec<BarRow> = decades
        .iter()
        .map(|decade| BarRow {
            label: decade
                .decade
                .map(|year| format!("{year}s"))
                .unwrap_or_else(|| "Unknown".to_owned()),
            fraction: fraction(decade.track_count, max),
            value: decade.track_count.to_string(),
        })
        .collect();
    section(
        "Release years",
        Some("Number of tracks per release decade."),
        rows,
        "No release years tagged yet.",
    )
}

fn added_years_section(years: &[YearCount]) -> gtk::Widget {
    let max = years.iter().map(|year| year.track_count).max().unwrap_or(0);
    let rows: Vec<BarRow> = years
        .iter()
        .map(|year| BarRow {
            label: year
                .year
                .map(|year| year.to_string())
                .unwrap_or_else(|| "Unknown".to_owned()),
            fraction: fraction(year.track_count, max),
            value: year.track_count.to_string(),
        })
        .collect();
    section(
        "Year added",
        Some("Number of tracks added to the library each year."),
        rows,
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

fn fraction_u64(numerator: u64, denominator: u64) -> f64 {
    if denominator == 0 {
        0.0
    } else {
        numerator as f64 / denominator as f64
    }
}

/// Assemble one chart: a title, an explanatory caption, and either the
/// aligned grid of proportion bars or — when there is nothing to show — a
/// muted `empty` line in its place.
fn section(title: &str, caption: Option<&str>, rows: Vec<BarRow>, empty: &str) -> gtk::Widget {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 6);
    container.set_hexpand(true);

    let title_label = gtk::Label::new(Some(title));
    title_label.add_css_class("statistics-section-title");
    title_label.set_xalign(0.0);
    container.append(&title_label);

    if let Some(caption) = caption {
        let caption_label = gtk::Label::new(Some(caption));
        caption_label.add_css_class("statistics-section-caption");
        caption_label.set_xalign(0.0);
        caption_label.set_wrap(true);
        container.append(&caption_label);
    }

    if rows.is_empty() {
        let empty_label = gtk::Label::new(Some(empty));
        empty_label.add_css_class("statistics-empty");
        empty_label.set_xalign(0.0);
        container.append(&empty_label);
    } else {
        container.append(&bar_grid(&rows));
    }

    container.upcast()
}

/// The aligned three-column grid of `(label, bar, value)` rows.
fn bar_grid(rows: &[BarRow]) -> gtk::Grid {
    let grid = gtk::Grid::new();
    grid.set_column_spacing(10);
    grid.set_row_spacing(6);
    grid.set_hexpand(true);

    for (index, row) in rows.iter().enumerate() {
        let line = index as i32;

        let label = gtk::Label::new(Some(&row.label));
        label.add_css_class("statistics-row-label");
        label.set_xalign(0.0);
        label.set_halign(gtk::Align::Start);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_width_chars(LABEL_WIDTH_CHARS);
        label.set_max_width_chars(LABEL_WIDTH_CHARS);
        grid.attach(&label, 0, line, 1, 1);

        grid.attach(&proportion_bar(row.fraction), 1, line, 1, 1);

        let value = gtk::Label::new(Some(&row.value));
        value.add_css_class("statistics-row-value");
        value.set_xalign(1.0);
        value.set_halign(gtk::Align::End);
        grid.attach(&value, 2, line, 1, 1);
    }

    grid
}

/// A single proportion bar. The CSS-painted `.statistics-bar` background
/// is the trough; the draw-func fills `fraction` of the width with the
/// live theme accent (resolved from `area.color()`, set by the CSS
/// `color` property), so the meter follows the system accent in both
/// themes without a hard-coded colour.
fn proportion_bar(fraction: f64) -> gtk::DrawingArea {
    let fraction = fraction.clamp(0.0, 1.0);
    let area = gtk::DrawingArea::new();
    area.add_css_class("statistics-bar");
    area.set_hexpand(true);
    area.set_content_height(BAR_HEIGHT);
    area.set_valign(gtk::Align::Center);
    // Clip the square Cairo fill to the trough's CSS border-radius.
    area.set_overflow(gtk::Overflow::Hidden);
    area.set_draw_func(move |area, cr, width, height| {
        let width = width as f64;
        let height = height as f64;
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        let fill_width = fraction * width;
        if fill_width <= 0.0 {
            return;
        }
        let accent = area.color();
        cr.set_source_rgba(
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
            BAR_FILL_ALPHA,
        );
        cr.rectangle(0.0, 0.0, fill_width, height);
        let _ = cr.fill();
    });
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn share_text_pairs_count_with_rounded_percent() {
        assert_eq!(share_text(0, 0), "0 (0%)");
        assert_eq!(share_text(0, 10), "0 (0%)");
        assert_eq!(share_text(1, 4), "1 (25%)");
        assert_eq!(share_text(1, 3), "1 (33%)");
        assert_eq!(share_text(10, 10), "10 (100%)");
    }

    #[test]
    fn fraction_guards_the_empty_denominator() {
        assert_eq!(fraction(3, 0), 0.0);
        assert_eq!(fraction(0, 10), 0.0);
        assert!((fraction(1, 4) - 0.25).abs() < f64::EPSILON);
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
/// and spins the loop so the CSS resolves and the (zero) draw funcs run —
/// proving the widget machinery and CSS classes hold together without a
/// panic.
#[cfg(test)]
mod widget_smoke {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::prelude::*;
    use sustain_app_runtime::ApplicationRuntime;

    use super::StatisticsView;

    #[test]
    fn empty_library_page_builds_and_maps() {
        if gtk::init().is_err() {
            eprintln!("SMOKE: no display, skipping");
            return;
        }
        crate::app_css::install_app_css();

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
    }
}
