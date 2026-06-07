// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Custom widget that wraps the track-table `ScrolledWindow` and continues
//! the zebra-stripe pattern into the empty region below the last data row.
//!
//! When the library or playlist holds fewer rows than the viewport can fit,
//! the area beneath the last row would otherwise be an empty void. iTunes
//! 11 fills that region with alternating empty rows so the list visually
//! extends to the bottom of the viewport; Sustain mirrors that behaviour
//! here without touching the underlying `gio::ListModel`.
//!
//! The painter takes the `ScrolledWindow` as its only child (via
//! `gtk::Widget::set_parent`). It forwards measure/allocate to the child
//! unchanged and, in `snapshot()`, paints alpha bands matching
//! `.track-table-row-even` from the bottom of the last real row down to
//! the viewport bottom. Geometry is driven by two inputs: the y where data
//! rows begin and a single authoritative pitch constant, [`ROW_HEIGHT_PX`],
//! pinned by CSS.
//!
//! The painter ([`EmptyRowPainter::new`]) wraps a `ColumnView` whose first
//! child is a header row; it resolves the rows' top from that header's
//! bottom edge. Every table that uses it — the library and playlist tables
//! and the CD-import table — shares the same row pitch and the same
//! theme-derived band colour, so they all stripe identically.

use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib};
use std::cell::Cell;

/// Authoritative row pitch for the track table, in pixels.
///
/// `.track-table-cell` in `app.css` is pinned to `min-height: 28px` plus
/// `border-bottom: 1px`. GTK4 CSS applies `min-height` to the content box
/// and adds the border outside, so the rendered row pitch is `28 + 1 = 29`.
/// The painter tiles bands at this pitch; any drift produces a visible
/// seam between real and filler rows. If you change the cell padding, the
/// content `min-height`, or the border width, update this constant in the
/// same commit.
pub(crate) const ROW_HEIGHT_PX: i32 = 29;

pub(crate) mod imp {
    use super::*;
    use std::cell::OnceCell;

    #[derive(Default)]
    pub struct EmptyRowPainter {
        pub(super) scroller: OnceCell<gtk::ScrolledWindow>,
        /// Present for the column-view track table (rows begin below the
        /// header); absent for a headerless list (rows begin at the top).
        pub(super) column_view: OnceCell<gtk::ColumnView>,
        pub(super) row_count: Cell<u32>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for EmptyRowPainter {
        const NAME: &'static str = "SustainEmptyRowPainter";
        type Type = super::EmptyRowPainter;
        type ParentType = gtk::Widget;
    }

    impl ObjectImpl for EmptyRowPainter {
        fn dispose(&self) {
            if let Some(scroller) = self.scroller.get() {
                scroller.unparent();
            }
        }
    }

    impl WidgetImpl for EmptyRowPainter {
        fn measure(&self, orientation: gtk::Orientation, for_size: i32) -> (i32, i32, i32, i32) {
            self.scroller
                .get()
                .map(|scroller| scroller.measure(orientation, for_size))
                .unwrap_or((0, 0, -1, -1))
        }

        fn size_allocate(&self, width: i32, height: i32, baseline: i32) {
            if let Some(scroller) = self.scroller.get() {
                scroller.allocate(width, height, baseline, None);
            }
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.parent_snapshot(snapshot);

            let widget = self.obj();
            let widget_width = widget.width();
            let widget_height = widget.height();
            if widget_width <= 0 || widget_height <= 0 {
                return;
            }

            let Some(content_top) = self.measure_content_top() else {
                // Layout has not settled yet (the column-view header has no
                // height). Emit nothing; GTK re-snapshots once it has.
                return;
            };
            let row_count = i32::try_from(self.row_count.get()).unwrap_or(i32::MAX);
            let bands = compute_filler_bands(
                widget_width,
                widget_height,
                content_top,
                row_count,
                ROW_HEIGHT_PX,
            );

            if bands.is_empty() {
                return;
            }

            let color = even_band_color(widget.upcast_ref::<gtk::Widget>());
            for band in bands {
                let rect = gtk::graphene::Rect::new(
                    band.x as f32,
                    band.y as f32,
                    band.width as f32,
                    band.height as f32,
                );
                snapshot.append_color(&color, &rect);
            }
        }
    }

    impl EmptyRowPainter {
        /// Resolve the y-coordinate where data rows begin, in painter-local
        /// pixels, or `None` when layout has not settled enough to know.
        ///
        /// The `ColumnView`'s first child is the header row widget;
        /// translating its bottom edge into our coordinate space accounts for
        /// any chrome the `ScrolledWindow` may insert above the viewport
        /// without hard-coding numbers the theme could shift. Before the first
        /// layout pass the header height is `0`; that returns `None` so the
        /// caller paints nothing until GTK re-snapshots post-allocation. The
        /// `None`-column-view fallback to `0` is a defensive default that the
        /// single [`EmptyRowPainter::new`] constructor never actually hits.
        fn measure_content_top(&self) -> Option<i32> {
            let Some(column_view) = self.column_view.get() else {
                return Some(0);
            };
            let header = column_view.first_child()?;
            let header_height = header.height();
            if header_height <= 0 {
                return None;
            }
            let widget = self.obj();
            let bottom_in_header = gtk::graphene::Point::new(0.0, header_height as f32);
            Some(
                header
                    .compute_point(widget.upcast_ref::<gtk::Widget>(), &bottom_in_header)
                    .map(|point| point.y() as i32)
                    .unwrap_or(0)
                    .max(0),
            )
        }
    }
}

glib::wrapper! {
    pub struct EmptyRowPainter(ObjectSubclass<imp::EmptyRowPainter>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl EmptyRowPainter {
    pub(crate) fn new(scroller: &gtk::ScrolledWindow, column_view: &gtk::ColumnView) -> Self {
        let painter: Self = glib::Object::new();
        // Custom `gtk::Widget` subclasses default to non-expanding. The
        // painter exists purely to wrap the scroller, so it must claim the
        // same space the bare scroller previously claimed — otherwise the
        // surrounding `Box`/`Overlay` allocates the painter its natural
        // (tiny) size and the table collapses to its own internal scroll
        // viewport. Mirror the scroller's expansion exactly so swapping
        // the painter in is invisible to the rest of the layout.
        painter.set_hexpand(scroller.hexpands());
        painter.set_vexpand(scroller.vexpands());
        scroller.set_parent(&painter);
        // OnceCell::set returns Err if already populated; that cannot happen
        // because `new` is the only path that populates these cells and it
        // populates each exactly once.
        let _ = painter.imp().scroller.set(scroller.clone());
        let _ = painter.imp().column_view.set(column_view.clone());
        painter
    }

    /// Update the painter's view of how many real rows the table is showing.
    /// No-op when the value is unchanged so we don't queue redundant draws
    /// during bulk updates that hit the same final count.
    pub(crate) fn set_row_count(&self, row_count: u32) {
        let imp = self.imp();
        if imp.row_count.get() == row_count {
            return;
        }
        imp.row_count.set(row_count);
        self.queue_draw();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FillerBand {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Pure geometry helper: returns the rectangles that should be painted with
/// the "even row" tint so the alternating zebra continues past
/// `content_top + row_count * row_pitch` down to the viewport bottom.
///
/// `content_top` is the y where data rows begin (the column-view header's
/// bottom edge, or `0` for a headerless list). Readiness — "has layout
/// settled enough to know where rows begin?" — is the caller's
/// responsibility; this function trusts the `content_top` it is given.
///
/// Returns an empty vector when:
/// - the widget has no positive area, or
/// - `content_top` is negative (a degenerate input), or
/// - the real rows already fill or overflow the viewport.
///
/// The "even" parity is computed from the row index that the first filler
/// band would occupy if filler rows were real — matching the
/// `apply_row_tint` rule in `cells.rs` (`row_position % 2 == 0` → tinted).
pub(crate) fn compute_filler_bands(
    widget_width: i32,
    widget_height: i32,
    content_top: i32,
    row_count: i32,
    row_pitch: i32,
) -> Vec<FillerBand> {
    if widget_width <= 0 || widget_height <= 0 || row_pitch <= 0 || content_top < 0 {
        return Vec::new();
    }
    let row_count = row_count.max(0);
    let data_end_y = content_top.saturating_add(row_count.saturating_mul(row_pitch));
    if data_end_y >= widget_height {
        return Vec::new();
    }

    let mut bands = Vec::new();
    let mut y = data_end_y;
    let mut index = row_count;
    while y < widget_height {
        let band_height = (widget_height - y).min(row_pitch);
        if index % 2 == 0 {
            bands.push(FillerBand {
                x: 0,
                y,
                width: widget_width,
                height: band_height,
            });
        }
        y += row_pitch;
        index += 1;
    }
    bands
}

fn even_band_color(widget: &gtk::Widget) -> gdk::RGBA {
    // Mirrors `.track-table-row-even { background-color: alpha(@theme_fg_color, 0.025); }`.
    // `Widget::color()` resolves `@theme_fg_color` against the live style
    // context, so light/dark mode and the system accent flow through to
    // the filler bands without any extra wiring — the only three
    // user-controlled visual inputs Sustain respects.
    let fg = widget.color();
    gdk::RGBA::new(fg.red(), fg.green(), fg.blue(), fg.alpha() * 0.025)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_bands_when_data_overflows_viewport() {
        // 50 rows of 28px is 1400px of content. A 400px viewport cannot show
        // it; the painter must emit nothing so it stays inert in the common
        // "large library" case.
        let bands = compute_filler_bands(800, 400, 28, 50, 28);
        assert!(bands.is_empty());
    }

    #[test]
    fn no_bands_when_data_exactly_fills_viewport() {
        // header (28) + 13 rows × 28 = 392 == widget_height.
        let bands = compute_filler_bands(800, 392, 28, 13, 28);
        assert!(bands.is_empty());
    }

    #[test]
    fn paints_even_index_bands_after_partial_fill() {
        // 4 real rows occupy y=28..140. Filler indices start at 4 (even),
        // so the painted bands fall at indices 4, 6, 8, 10, 12.
        let bands = compute_filler_bands(800, 400, 28, 4, 28);
        let painted_y: Vec<i32> = bands.iter().map(|band| band.y).collect();
        assert_eq!(painted_y, vec![140, 196, 252, 308, 364]);
    }

    #[test]
    fn parity_flips_when_row_count_is_odd() {
        // 5 real rows → first filler index is 5 (odd, NOT painted), so the
        // first emitted band is index 6 at y=196.
        let bands = compute_filler_bands(800, 400, 28, 5, 28);
        let painted_y: Vec<i32> = bands.iter().map(|band| band.y).collect();
        assert_eq!(painted_y, vec![196, 252, 308, 364]);
    }

    #[test]
    fn empty_table_paints_full_viewport_below_header() {
        let bands = compute_filler_bands(800, 200, 28, 0, 28);
        let painted_y: Vec<i32> = bands.iter().map(|band| band.y).collect();
        assert_eq!(painted_y, vec![28, 84, 140, 196]);
        // The trailing band gets clipped to the viewport edge.
        assert_eq!(bands.last().map(|band| band.height), Some(4));
    }

    #[test]
    fn zero_content_top_paints_from_the_top() {
        // With `content_top = 0` an empty list tiles the whole viewport from
        // the very top. (The "header not measured yet" case is gated in the
        // painter via `measure_content_top`, not here.)
        let bands = compute_filler_bands(800, 200, 0, 0, 28);
        let painted_y: Vec<i32> = bands.iter().map(|band| band.y).collect();
        assert_eq!(painted_y, vec![0, 56, 112, 168]);
    }

    #[test]
    fn no_bands_for_negative_content_top() {
        // Defensive: a negative top is a degenerate input and paints nothing.
        assert!(compute_filler_bands(800, 400, -1, 0, 28).is_empty());
    }

    #[test]
    fn no_bands_for_degenerate_widget_size() {
        assert!(compute_filler_bands(0, 400, 28, 0, 28).is_empty());
        assert!(compute_filler_bands(800, 0, 28, 0, 28).is_empty());
    }

    #[test]
    fn bands_span_full_widget_width() {
        let bands = compute_filler_bands(800, 400, 28, 4, 28);
        for band in &bands {
            assert_eq!(band.x, 0);
            assert_eq!(band.width, 800);
        }
    }

    #[test]
    fn negative_row_count_is_clamped_to_zero() {
        // Defensive — the painter clamps the u32 row count into i32 and we
        // never see negatives in practice, but the geometry function takes
        // a signed value, so cover the invariant.
        let bands_zero = compute_filler_bands(800, 200, 28, 0, 28);
        let bands_negative = compute_filler_bands(800, 200, 28, -3, 28);
        assert_eq!(bands_zero, bands_negative);
    }
}
