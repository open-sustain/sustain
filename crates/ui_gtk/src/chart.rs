// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Native chart widgets, drawn with Cairo on `GtkDrawingArea` so they
//! track the system accent and the light/dark theme without baked-in
//! colours. GTK ships no chart widget; like GNOME's own apps, Sustain
//! draws its own.
//!
//! Two primitives cover the Statistics page:
//! - [`donut`] for part-of-a-whole and comparison breakdowns (genre,
//!   bitrate, most-played, most-liked), with an accent-derived
//!   categorical palette and a legend;
//! - [`vertical_bars`] for histograms over an ordered axis (release
//!   decade, year added).
//!
//! The categorical palette ([`categorical_color`]) is shared with the
//! device-sync occupation meter, so both speak the same accent-hue
//! language.
//!
//! Every canvas reads its accent from `area.color()`, which the CSS
//! `color` property sets to `@theme_selected_bg_color`; nothing here
//! hard-codes a colour.

use std::f64::consts::PI;

use gtk::gdk;
use gtk::prelude::*;

/// CSS class that sources the live system accent for a Cairo canvas: it
/// sets `color: @theme_selected_bg_color`, which the draw funcs read via
/// `area.color()`. Carried by the donut ring and its legend swatches.
const CHART_ACCENT_CLASS: &str = "statistics-chart";

/// Opacity of the single-accent fill on the bar charts — solid enough to
/// read crisply while staying a touch soft against the card behind it.
const BAR_FILL_ALPHA: f64 = 0.85;

/// Corner radius of a histogram bar's fill, in pixels, applied to all
/// four corners so each bar reads as a rounded capsule rather than a
/// flat-edged block. Clamped per-bar to half the bar width and half the
/// fill height, so narrow columns and stub bars round into a stadium
/// shape instead of over-rounding.
const BAR_FILL_RADIUS: f64 = 6.0;

/// Diameter of a donut ring, in pixels.
const DONUT_DIAMETER: i32 = 260;

/// Inner radius as a fraction of the outer radius — the ring thickness.
const DONUT_RING_RATIO: f64 = 0.58;

/// Angular gap between adjacent donut slices, in radians, so the slices
/// read as distinct wedges rather than one continuous band.
const DONUT_SLICE_GAP: f64 = 0.018;

/// Edge of a legend swatch, in pixels.
const SWATCH_SIZE: i32 = 12;

/// Total drawn height of a histogram bar's column (trough), in pixels.
const VBAR_HEIGHT: i32 = 120;

/// A category colour: the live `accent` rotated `hue_offset` degrees
/// around the wheel, with saturation and lightness clamped to a vivid,
/// legible band so every hue stays distinct and never washes out or goes
/// black in either theme. Shared by the donut palette and the device-sync
/// occupation meter.
pub(crate) fn categorical_color(accent: gdk::RGBA, hue_offset: f64) -> (f64, f64, f64) {
    let (h, s, l) = rgb_to_hsl(
        accent.red() as f64,
        accent.green() as f64,
        accent.blue() as f64,
    );
    hsl_to_rgb(
        (h + hue_offset).rem_euclid(360.0),
        (s + 0.08).clamp(0.6, 1.0),
        l.clamp(0.45, 0.62),
    )
}

/// A theme-neutral grey for the aggregated "other" / untagged tail of a
/// categorical palette.
pub(crate) const MUTED_GREY: (f64, f64, f64) = (0.6, 0.6, 0.6);

/// RGB (each `0.0..=1.0`) to HSL with hue in degrees `0.0..360.0`.
pub(crate) fn rgb_to_hsl(r: f64, g: f64, b: f64) -> (f64, f64, f64) {
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    let delta = max - min;
    if delta <= f64::EPSILON {
        return (0.0, 0.0, l);
    }
    let s = delta / (1.0 - (2.0 * l - 1.0).abs());
    let h = if max == r {
        60.0 * (((g - b) / delta).rem_euclid(6.0))
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (h.rem_euclid(360.0), s, l)
}

/// Inverse of [`rgb_to_hsl`]; `h` in degrees, `s`/`l` in `0.0..=1.0`.
pub(crate) fn hsl_to_rgb(h: f64, s: f64, l: f64) -> (f64, f64, f64) {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let h_prime = h / 60.0;
    let x = c * (1.0 - (h_prime.rem_euclid(2.0) - 1.0).abs());
    let (r1, g1, b1) = match h_prime as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    (r1 + m, g1 + m, b1 + m)
}

/// One slice of a [`donut`]: a category label, its fraction of the whole,
/// a value string shown in the legend, and whether it is the muted
/// neutral tail (the folded "other" remainder) rather than a coloured
/// category.
pub(crate) struct DonutSlice {
    pub label: String,
    pub fraction: f64,
    pub value: String,
    pub muted: bool,
}

/// A donut chart: a ring of accent-hued wedges with an optional centred
/// total, paired with a legend listing each slice's swatch, label, and
/// value. The ring and the matching swatch resolve the same colour from
/// the live accent at draw time, so the legend always agrees with the
/// ring across theme and accent changes.
pub(crate) fn donut(slices: Vec<DonutSlice>, center: Option<String>) -> gtk::Widget {
    // Spread the coloured slices evenly around the wheel; the muted tail
    // (if any) takes the neutral grey instead of a hue. Each slice's role
    // is computed once and shared by its wedge and its legend swatch.
    let coloured = slices.iter().filter(|slice| !slice.muted).count().max(1);
    let hue_step = 360.0 / coloured as f64;
    let mut next_hue = 0usize;
    let roles: Vec<Option<f64>> = slices
        .iter()
        .map(|slice| {
            if slice.muted {
                None
            } else {
                let hue = next_hue as f64 * hue_step;
                next_hue += 1;
                Some(hue)
            }
        })
        .collect();

    let row = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    row.set_halign(gtk::Align::Center);
    row.set_valign(gtk::Align::Center);
    row.set_hexpand(true);

    let ring = gtk::DrawingArea::new();
    ring.add_css_class(CHART_ACCENT_CLASS);
    ring.set_content_width(DONUT_DIAMETER);
    ring.set_content_height(DONUT_DIAMETER);
    // Centre the ring against the legend: when the legend is taller than
    // the ring (e.g. the long genre list), Start would pin the ring to the
    // top of the row instead of vertically centring it like the other,
    // shorter-legend donuts.
    ring.set_valign(gtk::Align::Center);
    let wedges: Vec<(f64, Option<f64>)> = slices
        .iter()
        .zip(&roles)
        .map(|(slice, role)| (slice.fraction, *role))
        .collect();
    ring.set_draw_func(move |area, cr, width, height| {
        let w = width as f64;
        let h = height as f64;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let accent = area.color();
        let cx = w / 2.0;
        let cy = h / 2.0;
        let outer = (w.min(h) / 2.0) - 4.0;
        let inner = outer * DONUT_RING_RATIO;
        if outer <= 0.0 {
            return;
        }
        let mut start = -PI / 2.0;
        for (fraction, role) in &wedges {
            let sweep = fraction.clamp(0.0, 1.0) * 2.0 * PI;
            if sweep <= 0.0 {
                continue;
            }
            let end = start + sweep;
            // Inset by a gap only when the wedge is wide enough to keep a
            // visible body afterwards; hair-thin wedges stay full width.
            let (a0, a1) = if sweep > DONUT_SLICE_GAP * 2.0 {
                (start + DONUT_SLICE_GAP / 2.0, end - DONUT_SLICE_GAP / 2.0)
            } else {
                (start, end)
            };
            cr.new_sub_path();
            cr.arc(cx, cy, outer, a0, a1);
            cr.arc_negative(cx, cy, inner, a1, a0);
            cr.close_path();
            let (r, g, b) = match role {
                Some(hue) => categorical_color(accent, *hue),
                None => MUTED_GREY,
            };
            cr.set_source_rgb(r, g, b);
            let _ = cr.fill();
            start = end;
        }
    });

    if let Some(center) = center {
        let overlay = gtk::Overlay::new();
        overlay.set_child(Some(&ring));
        overlay.set_valign(gtk::Align::Center);
        let total = gtk::Label::new(Some(&center));
        total.add_css_class("statistics-donut-total");
        total.set_halign(gtk::Align::Center);
        total.set_valign(gtk::Align::Center);
        total.set_justify(gtk::Justification::Center);
        total.set_can_target(false);
        overlay.add_overlay(&total);
        row.append(&overlay);
    } else {
        row.append(&ring);
    }

    let legend = gtk::Grid::new();
    legend.set_column_spacing(8);
    legend.set_row_spacing(4);
    legend.set_valign(gtk::Align::Center);
    for (index, (slice, role)) in slices.iter().zip(&roles).enumerate() {
        let line = index as i32;

        legend.attach(&swatch(*role), 0, line, 1, 1);

        let label = gtk::Label::new(Some(&slice.label));
        label.add_css_class("statistics-legend-label");
        label.set_xalign(0.0);
        label.set_halign(gtk::Align::Start);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(22);
        legend.attach(&label, 1, line, 1, 1);

        let value = gtk::Label::new(Some(&slice.value));
        value.add_css_class("statistics-legend-value");
        value.set_xalign(1.0);
        value.set_halign(gtk::Align::End);
        legend.attach(&value, 2, line, 1, 1);
    }
    row.append(&legend);

    row.upcast()
}

/// A small legend swatch: a filled disc in the same colour its donut
/// wedge resolves to (a hue, or the muted grey for the tail).
fn swatch(role: Option<f64>) -> gtk::DrawingArea {
    let area = gtk::DrawingArea::new();
    area.add_css_class(CHART_ACCENT_CLASS);
    area.set_content_width(SWATCH_SIZE);
    area.set_content_height(SWATCH_SIZE);
    area.set_valign(gtk::Align::Center);
    area.set_draw_func(move |area, cr, width, height| {
        let w = width as f64;
        let h = height as f64;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let accent = area.color();
        let (r, g, b) = match role {
            Some(hue) => categorical_color(accent, hue),
            None => MUTED_GREY,
        };
        cr.set_source_rgb(r, g, b);
        cr.arc(w / 2.0, h / 2.0, (w.min(h) / 2.0) - 1.0, 0.0, 2.0 * PI);
        let _ = cr.fill();
    });
    area
}

/// One bar of a [`vertical_bars`] histogram: its x-axis label, its height
/// as a fraction of the tallest bar, and the value shown above it.
pub(crate) struct VerticalBar {
    pub label: String,
    pub fraction: f64,
    pub value: String,
}

/// A vertical-bar histogram over an ordered axis. Each bar is one column
/// of a homogeneous grid — value above, bar in the middle, axis label
/// below — so the three line up exactly without any per-pixel alignment.
pub(crate) fn vertical_bars(bars: Vec<VerticalBar>) -> gtk::Widget {
    let grid = gtk::Grid::new();
    grid.set_column_homogeneous(true);
    grid.set_column_spacing(6);
    grid.set_row_spacing(4);
    grid.set_hexpand(true);

    for (index, bar) in bars.iter().enumerate() {
        let col = index as i32;

        let value = gtk::Label::new(Some(&bar.value));
        value.add_css_class("statistics-bar-value");
        value.set_halign(gtk::Align::Center);
        grid.attach(&value, col, 0, 1, 1);

        grid.attach(&vertical_bar(bar.fraction), col, 1, 1, 1);

        let label = gtk::Label::new(Some(&bar.label));
        label.add_css_class("statistics-axis-label");
        label.set_halign(gtk::Align::Center);
        label.set_ellipsize(gtk::pango::EllipsizeMode::End);
        label.set_max_width_chars(8);
        grid.attach(&label, col, 2, 1, 1);
    }

    grid.upcast()
}

/// A single histogram bar: the draw-func paints `fraction` of the height
/// from the bottom up with the live theme accent as a rounded capsule —
/// all four corners swept. The widget has no trough background; the bar
/// floats directly on the card.
fn vertical_bar(fraction: f64) -> gtk::DrawingArea {
    let fraction = fraction.clamp(0.0, 1.0);
    let area = gtk::DrawingArea::new();
    area.add_css_class("statistics-vbar");
    area.set_content_height(VBAR_HEIGHT);
    area.set_hexpand(true);
    area.set_valign(gtk::Align::Fill);
    area.set_overflow(gtk::Overflow::Hidden);
    area.set_draw_func(move |area, cr, width, height| {
        let w = width as f64;
        let h = height as f64;
        if w <= 0.0 || h <= 0.0 {
            return;
        }
        let fill_height = fraction * h;
        if fill_height <= 0.0 {
            return;
        }
        let accent = area.color();
        cr.set_source_rgba(
            accent.red() as f64,
            accent.green() as f64,
            accent.blue() as f64,
            BAR_FILL_ALPHA,
        );
        let top = h - fill_height;
        let radius = BAR_FILL_RADIUS.min(w / 2.0).min(fill_height / 2.0);
        // Trace the fill clockwise as a rounded rectangle, every corner
        // swept by a quarter arc: top-left (PI..1.5PI), top-right
        // (1.5PI..2PI), bottom-right (0..0.5PI), bottom-left (0.5PI..PI).
        // Cairo links consecutive arcs with the straight edges between them.
        cr.new_sub_path();
        cr.arc(radius, top + radius, radius, PI, 1.5 * PI);
        cr.arc(w - radius, top + radius, radius, 1.5 * PI, 2.0 * PI);
        cr.arc(w - radius, h - radius, radius, 0.0, 0.5 * PI);
        cr.arc(radius, h - radius, radius, 0.5 * PI, PI);
        cr.close_path();
        let _ = cr.fill();
    });
    area
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsl_round_trips() {
        for &(r, g, b) in &[
            (0.2, 0.5, 0.8),
            (0.9, 0.1, 0.3),
            (0.5, 0.5, 0.5),
            (0.0, 0.0, 0.0),
            (1.0, 1.0, 1.0),
        ] {
            let (h, s, l) = rgb_to_hsl(r, g, b);
            let (r2, g2, b2) = hsl_to_rgb(h, s, l);
            assert!((r - r2).abs() < 1e-9, "r: {r} != {r2}");
            assert!((g - g2).abs() < 1e-9, "g: {g} != {g2}");
            assert!((b - b2).abs() < 1e-9, "b: {b} != {b2}");
        }
    }

    #[test]
    fn categorical_color_stays_in_gamut() {
        let accent = gdk::RGBA::new(0.2, 0.5, 0.9, 1.0);
        for step in 0..8 {
            let (r, g, b) = categorical_color(accent, step as f64 * 45.0);
            for channel in [r, g, b] {
                assert!(
                    (0.0..=1.0).contains(&channel),
                    "channel out of range: {channel}"
                );
            }
        }
    }
}

/// Headless widget smoke test: builds each primitive with sample data,
/// maps them, and spins the loop, proving the widget machinery and CSS
/// classes hold together without a panic. Skips cleanly with no display.
#[cfg(test)]
mod widget_smoke {
    use super::*;

    #[test]
    fn primitives_build_and_map() {
        let ran = crate::test_support::with_gtk(|| {
            let donut = donut(
                vec![
                    DonutSlice {
                        label: "House".to_owned(),
                        fraction: 0.5,
                        value: "50 (50%)".to_owned(),
                        muted: false,
                    },
                    DonutSlice {
                        label: "Techno".to_owned(),
                        fraction: 0.3,
                        value: "30 (30%)".to_owned(),
                        muted: false,
                    },
                    DonutSlice {
                        label: "Other (3 genres)".to_owned(),
                        fraction: 0.2,
                        value: "20 (20%)".to_owned(),
                        muted: true,
                    },
                ],
                Some("100\ntracks".to_owned()),
            );
            let bars = vertical_bars(vec![
                VerticalBar {
                    label: "1990s".to_owned(),
                    fraction: 1.0,
                    value: "40".to_owned(),
                },
                VerticalBar {
                    label: "2000s".to_owned(),
                    fraction: 0.5,
                    value: "20".to_owned(),
                },
            ]);
            let column = gtk::Box::new(gtk::Orientation::Vertical, 8);
            column.append(&donut);
            column.append(&bars);

            let window = gtk::Window::new();
            window.set_default_size(420, 480);
            window.set_child(Some(&column));
            window.set_visible(true);

            let ctx = gtk::glib::MainContext::default();
            let mut spins = 0;
            while ctx.iteration(false) && spins < 200 {
                spins += 1;
            }

            window.set_child(None::<&gtk::Widget>);
            window.destroy();
        });
        if !ran {
            eprintln!("SMOKE: no display, skipping");
        }
    }
}
