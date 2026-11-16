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
//! Layout: a row of headline figures (tracks, listening time, size,
//! plays) opens the page. Each chart then sits in its own card. The two
//! part-of-a-whole charts (genre, bitrate) are donuts drawn by
//! [`crate::chart`] with a hue per slice and a legend; the two top-5
//! genre rankings (most played, most liked) are ranked horizontal bars,
//! a hue per genre.
//! These four cards flow into a responsive two-up grid that reflows to
//! one column when the window is narrow; the two distributions over time
//! (release decade, year added) run full width below as vertical-bar
//! histograms. Every chart is drawn with
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
use sustain_i18n::{gettext, ngettext, pgettext, plural_count, tr_format};

use crate::SharedRuntime;
use crate::chart::{self, ChartBar, ChartTone, DonutSlice};
use crate::status_bar::{duration_text, file_size_text, grouped};
use crate::track_summary::TrackSummary;

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

        let (stats, summary) = {
            let runtime = self.runtime.borrow();
            let tracks = runtime.library_tracks();
            (
                compute_library_statistics(tracks, local_year),
                TrackSummary::of_tracks(tracks),
            )
        };

        if stats.total_tracks == 0 {
            let empty = gtk::Label::new(Some(&gettext(
                "No statistics yet — add music to your library to see its breakdown here.",
            )));
            empty.add_css_class("statistics-empty-page");
            empty.set_xalign(0.0);
            empty.set_wrap(true);
            self.content.append(&empty);
            return;
        }

        self.content
            .append(&headline_figures(&summary, stats.total_plays));

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
///
/// It runs once per library track, and GLib's local-time conversion looks
/// the time zone up on every call, so the year comes from UTC arithmetic
/// wherever the two cannot differ. A UTC offset stays within a day, so only
/// an instant on a UTC 31 December or 1 January can fall in another local
/// year; only those take the local-time conversion.
fn local_year(time: SystemTime) -> Option<i32> {
    let seconds = time.duration_since(UNIX_EPOCH).ok()?;
    let seconds = i64::try_from(seconds.as_secs()).ok()?;
    let (year, month, day) = utc_date(seconds.div_euclid(SECONDS_PER_DAY))?;
    if matches!((month, day), (12, 31) | (1, 1)) {
        return Some(glib::DateTime::from_unix_local(seconds).ok()?.year());
    }
    Some(year)
}

const SECONDS_PER_DAY: i64 = 86_400;

/// The proleptic Gregorian `(year, month, day)` of the day `days` days
/// after 1970-01-01, or `None` past the range of an `i32` year (Howard
/// Hinnant's `civil_from_days`).
fn utc_date(days: i64) -> Option<(i32, u32, u32)> {
    let shifted = days.checked_add(719_468)?;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some((
        i32::try_from(year).ok()?,
        u32::try_from(month).ok()?,
        u32::try_from(day).ok()?,
    ))
}

/// `"<count> (<pct>%)"` — a count alongside its share of `total`. Used by
/// integer-count distribution donut legends.
fn count_share_text(count: usize, total: usize) -> String {
    quantity_share_text(count as f64, total as f64)
}

/// `"<quantity> (<pct>%)"` for weighted quantities, the quantity rounded.
fn quantity_share_text(quantity: f64, total: f64) -> String {
    let percent = if total == 0.0 {
        0.0
    } else {
        quantity / total * 100.0
    };
    format!("{} ({percent:.0}%)", quantity_text(quantity))
}

/// A weighted quantity for display. A track tagged with several genres
/// counts a share toward each, so the sums are fractional. Large sums
/// show whole with their thousands grouped (`2274.25` → `"2,274"`), where
/// the fraction is noise; small ones keep a tenth (`0.33` → `"0.3"`), so
/// a real contribution never reads as zero.
fn quantity_text(quantity: f64) -> String {
    let quantity = quantity.max(0.0);
    let tenths = (quantity * 10.0).round() as u64;
    if tenths == 0 && quantity > 0.0 {
        "< 0.1".to_owned()
    } else if quantity >= 10.0 || tenths.is_multiple_of(10) {
        grouped(quantity.round() as u64)
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

/// A genre name for display, mapping the "no genre tag" case to a
/// readable label.
fn genre_label(genre: &Option<String>) -> String {
    genre.clone().unwrap_or_else(|| gettext("Unknown"))
}

/// The centred readout shown inside a donut's hole: the denominator the
/// slices are shares of, over a unit word.
fn donut_center(total: &str, unit: &str) -> String {
    format!("{total}\n{unit}")
}

fn genre_distribution_section(distribution: &GenreDistribution) -> gtk::Widget {
    let total = distribution.total_tracks;
    let ranked = distribution.entries.len();
    let mut slices: Vec<DonutSlice> = distribution
        .entries
        .iter()
        .enumerate()
        .map(|(rank, entry)| DonutSlice {
            label: genre_label(&entry.genre),
            fraction: fraction_f64(entry.track_weight, total as f64),
            value: quantity_share_text(entry.track_weight, total as f64),
            tone: ChartTone::category(rank, ranked),
        })
        .collect();
    if let Some(other) = distribution.other {
        slices.push(DonutSlice {
            label: tr_format!(
                ngettext(
                    "Other ({count} genre)",
                    "Other ({count} genres)",
                    plural_count(other.genre_count)
                ),
                count = other.genre_count
            ),
            fraction: fraction_f64(other.track_weight, total as f64),
            value: quantity_share_text(other.track_weight, total as f64),
            tone: ChartTone::Muted,
        });
    }
    let content = (!slices.is_empty()).then(|| {
        chart::donut(
            slices,
            Some(donut_center(
                &grouped(total as u64),
                &ngettext("track", "tracks", plural_count(total)),
            )),
        )
    });
    section(
        &pgettext("statistics section", "Genre"),
        Some(&gettext(
            "Share of tracks per genre across the whole library.",
        )),
        content,
        &gettext("No genres tagged yet."),
    )
}

fn quality_distribution_section(distribution: &QualityDistribution) -> gtk::Widget {
    let total = distribution.total_with_bitrate;
    let slices: Vec<DonutSlice> = if total == 0 {
        Vec::new()
    } else {
        // The best range takes the accent's own hue.
        let buckets = distribution.buckets.len();
        distribution
            .buckets
            .iter()
            .enumerate()
            .map(|(index, bucket)| DonutSlice {
                label: quality_label(bucket.range).to_owned(),
                fraction: fraction(bucket.track_count, total),
                value: count_share_text(bucket.track_count, total),
                tone: ChartTone::category(buckets - 1 - index, buckets),
            })
            .collect()
    };
    let content = (!slices.is_empty()).then(|| {
        chart::donut(
            slices,
            Some(donut_center(
                &grouped(total as u64),
                &ngettext("track", "tracks", plural_count(total)),
            )),
        )
    });
    section(
        &gettext("Quality"),
        Some(&gettext("Share of tracks per bitrate range.")),
        content,
        &gettext("No tracks carry a bitrate yet."),
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
    let most = genres
        .iter()
        .map(|genre| genre.total_play_count)
        .fold(0.0, f64::max);
    let bars: Vec<ChartBar> = genres
        .iter()
        .map(|genre| ChartBar {
            label: genre_label(&genre.genre),
            fraction: fraction_f64(genre.total_play_count, most),
            value: quantity_text(genre.total_play_count),
        })
        .collect();
    let content = (most > 0.0).then(|| chart::ranked_bars(bars));
    section(
        &gettext("Most played genres"),
        Some(&gettext("Top 5 by total play count.")),
        content,
        &gettext("No plays recorded yet."),
    )
}

fn most_liked_section(genres: &[GenreRating]) -> gtk::Widget {
    let most = genres
        .iter()
        .map(|genre| genre.total_stars)
        .fold(0.0, f64::max);
    let bars: Vec<ChartBar> = genres
        .iter()
        .map(|genre| ChartBar {
            label: genre_label(&genre.genre),
            fraction: fraction_f64(genre.total_stars, most),
            value: format!("{}★", quantity_text(genre.total_stars)),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::ranked_bars(bars));
    section(
        &gettext("Most liked genres"),
        Some(&gettext(
            "Top 5 by total rating points across rated tracks.",
        )),
        content,
        &gettext("No ratings recorded yet."),
    )
}

fn release_years_section(decades: &[DecadeCount]) -> gtk::Widget {
    let max = decades
        .iter()
        .map(|decade| decade.track_count)
        .max()
        .unwrap_or(0);
    let bars: Vec<ChartBar> = decades
        .iter()
        .map(|decade| ChartBar {
            label: decade
                .decade
                .map(|year| {
                    // Translators: a decade, as in "1990s".
                    tr_format!(gettext("{year}s"), year = year)
                })
                .unwrap_or_else(|| gettext("Unknown")),
            fraction: fraction(decade.track_count, max),
            value: grouped(decade.track_count as u64),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::vertical_bars(bars));
    section(
        &gettext("Release years"),
        Some(&gettext("Number of tracks per release decade.")),
        content,
        &gettext("No release years tagged yet."),
    )
}

fn added_years_section(years: &[YearCount]) -> gtk::Widget {
    let max = years.iter().map(|year| year.track_count).max().unwrap_or(0);
    let bars: Vec<ChartBar> = years
        .iter()
        .map(|year| ChartBar {
            label: year
                .year
                .map(|year| year.to_string())
                .unwrap_or_else(|| gettext("Unknown")),
            fraction: fraction(year.track_count, max),
            value: grouped(year.track_count as u64),
        })
        .collect();
    let content = (!bars.is_empty()).then(|| chart::vertical_bars(bars));
    section(
        &gettext("Year added"),
        Some(&gettext("Number of tracks added to the library each year.")),
        content,
        &gettext("Nothing added yet."),
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

/// The row of big figures opening the page: how many tracks, how long
/// they play, how much room they take, how often they were played. The
/// tiles flow onto fewer per line as the window narrows.
fn headline_figures(summary: &TrackSummary, total_plays: u64) -> gtk::Widget {
    const FIGURES: u32 = 4;
    let row = gtk::FlowBox::new();
    row.set_homogeneous(true);
    row.set_hexpand(true);
    row.set_max_children_per_line(FIGURES);
    row.set_column_spacing(CARD_SPACING as u32);
    row.set_row_spacing(CARD_SPACING as u32);
    row.set_selection_mode(gtk::SelectionMode::None);
    // Display only: no tile takes focus or reacts to clicks.
    row.set_can_focus(false);
    let tracks = summary.track_count;
    let plays = usize::try_from(total_plays).unwrap_or(usize::MAX);
    for (figure, caption) in [
        (
            grouped(tracks as u64),
            ngettext("track", "tracks", plural_count(tracks)),
        ),
        (duration_text(summary.duration_seconds), gettext("of music")),
        (file_size_text(summary.size_bytes), gettext("on disk")),
        (
            grouped(total_plays),
            ngettext("play", "plays", plural_count(plays)),
        ),
    ] {
        let tile = gtk::Box::new(gtk::Orientation::Vertical, 2);
        tile.add_css_class("statistics-figure");
        let figure_label = gtk::Label::new(Some(&figure));
        figure_label.add_css_class("statistics-figure-value");
        figure_label.set_xalign(0.0);
        let caption_label = gtk::Label::new(Some(&caption));
        caption_label.add_css_class("statistics-section-caption");
        caption_label.set_xalign(0.0);
        tile.append(&figure_label);
        tile.append(&caption_label);
        row.append(&tile);
    }
    row.upcast()
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
    use std::time::Duration;

    use super::*;

    #[test]
    fn utc_date_matches_glib_across_the_calendar() {
        // Every day from the epoch to 2200, through leap and century years.
        for days in 0..84_000 {
            let utc = glib::DateTime::from_unix_utc(days * SECONDS_PER_DAY).expect("valid instant");
            assert_eq!(
                utc_date(days),
                Some((
                    utc.year(),
                    u32::try_from(utc.month()).expect("month"),
                    u32::try_from(utc.day_of_month()).expect("day"),
                )),
                "day {days}"
            );
        }
    }

    #[test]
    fn local_year_matches_the_local_calendar_around_new_year() {
        // Every hour of the two days either side of each UTC new year,
        // where the local year can differ, and a few instants in between.
        for year in [1999, 2000, 2016, 2024, 2025, 2026] {
            let new_year = glib::DateTime::from_utc(year, 1, 1, 0, 0, 0.0)
                .expect("valid date")
                .to_unix();
            for hour in -48..=48 {
                let seconds = new_year + hour * 3_600;
                let time = UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs());
                let local = glib::DateTime::from_unix_local(seconds).expect("valid instant");
                assert_eq!(local_year(time), Some(local.year()), "{seconds}");
            }
            let mid_year =
                UNIX_EPOCH + Duration::from_secs((new_year + 180 * 86_400).unsigned_abs());
            assert_eq!(local_year(mid_year), Some(year));
        }
    }

    #[test]
    fn count_share_text_pairs_count_with_rounded_percent() {
        assert_eq!(count_share_text(0, 0), "0 (0%)");
        assert_eq!(count_share_text(0, 10), "0 (0%)");
        assert_eq!(count_share_text(1, 4), "1 (25%)");
        assert_eq!(count_share_text(1, 3), "1 (33%)");
        assert_eq!(count_share_text(10, 10), "10 (100%)");
    }

    #[test]
    fn quantity_share_text_keeps_small_weights_visible() {
        assert_eq!(quantity_share_text(0.4, 10.0), "0.4 (4%)");
        assert_eq!(quantity_share_text(2274.25, 10265.0), "2,274 (22%)");
        assert_eq!(quantity_share_text(10.0, 10.0), "10 (100%)");
    }

    #[test]
    fn quantities_keep_a_tenth_only_while_small() {
        assert_eq!(quantity_text(7296.08), "7,296");
        assert_eq!(quantity_text(12.4), "12");
        assert_eq!(quantity_text(9.96), "10");
        assert_eq!(quantity_text(3.0), "3");
        assert_eq!(quantity_text(1.0 / 3.0), "0.3");
        assert_eq!(quantity_text(0.02), "< 0.1");
        assert_eq!(quantity_text(0.0), "0");
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
