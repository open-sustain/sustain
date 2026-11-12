// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A horizontal bar of three parts: one at each edge and one centered
//! between them, none ever laid over another.
//!
//! `GtkCenterBox` sizes each edge child against the other's *minimum*
//! width, so in a window too narrow for all three natural widths both
//! edges can take their natural width at once and the end child is laid
//! over the center one. This bar hands the width out once:
//!
//! - the center part keeps its natural width until both edges are at
//!   their minimum, and only then shrinks;
//! - what is left goes to the edges, each up to half of it first, so the
//!   center part stays centered for as long as the edges allow;
//! - an edge takes more than its half only from what the other cannot
//!   use, which pushes the center part aside without covering it.

use std::cell::OnceCell;

use gtk::glib;
use gtk::prelude::*;
use gtk::subclass::prelude::*;

/// The narrowest a part may get and the width it takes when there is room.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct WidthRange {
    minimum: i32,
    natural: i32,
}

/// Where each part of the bar goes, measured from the bar's start edge.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Placement {
    start_width: i32,
    center_offset: i32,
    center_width: i32,
    end_width: i32,
}

/// Share `width` between the three parts.
///
/// For any `width` of at least the three minimums together, every part
/// gets a width within its range and the parts do not overlap.
fn place(width: i32, start: WidthRange, center: WidthRange, end: WidthRange) -> Placement {
    let center_width = (width - start.minimum - end.minimum)
        .min(center.natural)
        .max(center.minimum);
    let edges = width - center_width;

    let start_width = (edges / 2)
        .min(start.natural)
        .min(edges - end.minimum)
        .max(start.minimum);
    let end_width = (edges - start_width).min(end.natural).max(end.minimum);
    // Whatever the end part left unused.
    let start_width = (edges - end_width).min(start.natural).max(start_width);

    let center_offset = ((width - center_width) / 2)
        .min(width - end_width - center_width)
        .max(start_width);

    Placement {
        start_width,
        center_offset,
        center_width,
        end_width,
    }
}

mod imp {
    use super::*;

    pub(super) struct Parts {
        pub(super) start: gtk::Widget,
        pub(super) center: gtk::Widget,
        pub(super) end: gtk::Widget,
    }

    #[derive(Default)]
    pub struct CenteredBar {
        pub(super) parts: OnceCell<Parts>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for CenteredBar {
        const NAME: &'static str = "SustainCenteredBar";
        type Type = super::CenteredBar;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("centered-bar");
        }
    }

    impl ObjectImpl for CenteredBar {
        fn dispose(&self) {
            while let Some(child) = self.obj().first_child() {
                child.unparent();
            }
        }
    }

    impl WidgetImpl for CenteredBar {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let Some(parts) = self.parts.get() else {
                return (0, 0, -1, -1);
            };
            if orientation == gtk::Orientation::Vertical {
                let (minimum, natural) = [&parts.start, &parts.center, &parts.end]
                    .into_iter()
                    .map(|part| part.measure(orientation, -1))
                    .fold((0, 0), |(minimum, natural), (part_min, part_nat, _, _)| {
                        (minimum.max(part_min), natural.max(part_nat))
                    });
                return (minimum, natural, -1, -1);
            }
            let start = width_range(&parts.start);
            let center = width_range(&parts.center);
            let end = width_range(&parts.end);
            (
                start.minimum + center.minimum + end.minimum,
                // Wide enough to center the center part between two edges
                // at their natural widths.
                center.natural + 2 * start.natural.max(end.natural),
                -1,
                -1,
            )
        }

        fn size_allocate(&self, width: i32, height: i32, _baseline: i32) {
            let Some(parts) = self.parts.get() else {
                return;
            };
            let placement = place(
                width,
                width_range(&parts.start),
                width_range(&parts.center),
                width_range(&parts.end),
            );
            let end_offset = width - placement.end_width;
            let spans = [
                (&parts.start, 0, placement.start_width),
                (
                    &parts.center,
                    placement.center_offset,
                    placement.center_width,
                ),
                (&parts.end, end_offset, placement.end_width),
            ];
            let right_to_left = self.obj().direction() == gtk::TextDirection::Rtl;
            for (part, offset, part_width) in spans {
                let x = if right_to_left {
                    width - offset - part_width
                } else {
                    offset
                };
                part.size_allocate(&gtk::Allocation::new(x, 0, part_width, height), -1);
            }
        }
    }

    fn width_range(part: &gtk::Widget) -> WidthRange {
        let (minimum, natural, _, _) = part.measure(gtk::Orientation::Horizontal, -1);
        WidthRange { minimum, natural }
    }
}

glib::wrapper! {
    pub struct CenteredBar(ObjectSubclass<imp::CenteredBar>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl CenteredBar {
    /// A bar with `start` at its start edge, `end` at its end edge and
    /// `center` centered between them.
    pub(crate) fn new(
        start: &impl IsA<gtk::Widget>,
        center: &impl IsA<gtk::Widget>,
        end: &impl IsA<gtk::Widget>,
    ) -> Self {
        let bar: Self = glib::Object::new();
        let parts = imp::Parts {
            start: start.clone().upcast(),
            center: center.clone().upcast(),
            end: end.clone().upcast(),
        };
        // Parented in reading order, which is also the focus order.
        for part in [&parts.start, &parts.center, &parts.end] {
            part.set_parent(&bar);
        }
        assert!(
            bar.imp().parts.set(parts).is_ok(),
            "a new bar has no parts yet"
        );
        bar
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const START: WidthRange = WidthRange {
        minimum: 352,
        natural: 532,
    };
    const CENTER: WidthRange = WidthRange {
        minimum: 300,
        natural: 600,
    };
    const END: WidthRange = WidthRange {
        minimum: 176,
        natural: 336,
    };

    fn assert_fits(width: i32, start: WidthRange, center: WidthRange, end: WidthRange) {
        let placement = place(width, start, center, end);
        let context = format!("{width}: {placement:?}");
        for (part_width, range) in [
            (placement.start_width, start),
            (placement.center_width, center),
            (placement.end_width, end),
        ] {
            assert!(
                (range.minimum..=range.natural).contains(&part_width),
                "{context}"
            );
        }
        assert!(
            placement.center_offset >= placement.start_width,
            "{context}"
        );
        assert!(
            placement.center_offset + placement.center_width <= width - placement.end_width,
            "{context}"
        );
    }

    #[test]
    fn no_part_overlaps_another_at_any_width() {
        let minimum = START.minimum + CENTER.minimum + END.minimum;
        for width in minimum..=2_000 {
            assert_fits(width, START, CENTER, END);
            // The wider edge on the other side.
            assert_fits(width, END, CENTER, START);
        }
    }

    #[test]
    fn the_center_part_shrinks_only_once_both_edges_are_at_their_minimum() {
        let edges_minimum = START.minimum + END.minimum;

        let placement = place(edges_minimum + CENTER.natural, START, CENTER, END);
        assert_eq!(placement.center_width, CENTER.natural);
        assert_eq!(placement.start_width, START.minimum);
        assert_eq!(placement.end_width, END.minimum);

        let placement = place(edges_minimum + CENTER.natural - 100, START, CENTER, END);
        assert_eq!(placement.center_width, CENTER.natural - 100);
    }

    #[test]
    fn the_center_part_is_centered_once_both_edges_fit_beside_it() {
        let width = CENTER.natural + 2 * START.natural;
        let placement = place(width, START, CENTER, END);
        assert_eq!(placement.center_offset, (width - CENTER.natural) / 2);
        assert_eq!(placement.start_width, START.natural);
        assert_eq!(placement.end_width, END.natural);
    }

    #[test]
    fn an_edge_takes_what_the_other_cannot_use() {
        // Room for every natural width, though not for centering: each
        // edge's half of the room is less than the start part's natural
        // width and more than the end part's.
        let width = START.natural + CENTER.natural + END.natural;
        let placement = place(width, START, CENTER, END);
        assert_eq!(placement.start_width, START.natural);
        assert_eq!(placement.end_width, END.natural);
        assert_eq!(placement.center_offset, START.natural);
    }

    #[test]
    fn the_narrower_edge_grows_first_to_keep_the_center_part_near_the_middle() {
        // 100 px to share between the edges: the start part is already
        // past its half, so all of it goes to the end part.
        let width = START.minimum + CENTER.natural + END.minimum + 100;
        let placement = place(width, START, CENTER, END);
        assert_eq!(placement.start_width, START.minimum);
        assert_eq!(placement.end_width, END.minimum + 100);
    }
}
