// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A track's star rating, drawn and clicked as one widget.
//!
//! The track table shows a rating in every realized row, a couple of
//! hundred at a time, and builds or drops those rows by the hundred on
//! every playlist switch. One widget with one click gesture costs a
//! fraction of five buttons with their labels and controllers.
//!
//! The stars take the widget's CSS `color` (node name `rating-stars`):
//! filled stars in that colour, empty stars dimmed against it, so a
//! selected row's foreground colour carries over to its stars.

use std::cell::Cell;
use std::sync::OnceLock;

use gtk::glib::subclass::Signal;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};
use sustain_app_runtime::Rating;

const FILLED_STAR: &str = "★";
const EMPTY_STAR: &str = "☆";

/// Narrowest slot a star sits in. Every star gets the same slot, so the
/// stars line up from row to row and each is an equally wide click
/// target.
const MIN_STAR_SLOT_WIDTH: i32 = 16;

/// Opacity of an empty star relative to a filled one.
const EMPTY_STAR_OPACITY: f32 = 0.4;

const RATING_CHOSEN_SIGNAL: &str = "rating-chosen";

mod imp {
    use super::*;

    #[derive(Default)]
    pub struct RatingStars {
        pub(super) stars: Cell<u8>,
        /// The star a primary press went down on, until its release.
        pub(super) pressed_star: Cell<Option<u8>>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for RatingStars {
        const NAME: &'static str = "SustainRatingStars";
        type Type = super::RatingStars;
        type ParentType = gtk::Widget;

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("rating-stars");
        }
    }

    impl ObjectImpl for RatingStars {
        fn signals() -> &'static [Signal] {
            static SIGNALS: OnceLock<Vec<Signal>> = OnceLock::new();
            SIGNALS.get_or_init(|| {
                vec![
                    Signal::builder(RATING_CHOSEN_SIGNAL)
                        .param_types([u8::static_type()])
                        .build(),
                ]
            })
        }

        fn constructed(&self) {
            self.parent_constructed();
            self.obj().install_click_gesture();
        }
    }

    impl WidgetImpl for RatingStars {
        fn measure(&self, orientation: gtk::Orientation, _for_size: i32) -> (i32, i32, i32, i32) {
            let size = match orientation {
                gtk::Orientation::Horizontal => {
                    self.obj().star_slot_width() * i32::from(Rating::MAX_STARS)
                }
                _ => self.obj().star_layout(FILLED_STAR).pixel_size().1,
            };
            (size, size, -1, -1)
        }

        fn snapshot(&self, snapshot: &gtk::Snapshot) {
            self.obj().snapshot_stars(snapshot);
        }
    }
}

glib::wrapper! {
    pub struct RatingStars(ObjectSubclass<imp::RatingStars>)
        @extends gtk::Widget,
        @implements gtk::Accessible, gtk::Buildable, gtk::ConstraintTarget;
}

impl RatingStars {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    pub(crate) fn rating(&self) -> Rating {
        Rating::new(self.imp().stars.get()).unwrap_or_else(Rating::unrated)
    }

    pub(crate) fn set_rating(&self, rating: Rating) {
        if self.imp().stars.replace(rating.stars()) != rating.stars() {
            self.queue_draw();
        }
    }

    /// Calls `chosen` with the rating a click on a star asks for: that
    /// star's rating, or no stars when the click lands on the star the
    /// rating already ends at. The widget keeps showing its rating until
    /// its owner sets the new one.
    pub(crate) fn connect_rating_chosen<F: Fn(&Self, Rating) + 'static>(
        &self,
        chosen: F,
    ) -> glib::SignalHandlerId {
        self.connect_closure(
            RATING_CHOSEN_SIGNAL,
            false,
            glib::closure_local!(move |stars: &Self, rating: u8| {
                if let Some(rating) = Rating::new(rating) {
                    chosen(stars, rating);
                }
            }),
        )
    }

    /// A star is clicked when a primary press and its release land on it,
    /// as with a button. The press is claimed, so it neither selects the
    /// row under it nor starts a drag.
    fn install_click_gesture(&self) {
        let gesture = gtk::GestureClick::new();
        gesture.set_button(gdk::BUTTON_PRIMARY);
        gesture.connect_pressed(|gesture, _n_press, x, _y| {
            let Some(stars) = gesture.widget().and_downcast::<Self>() else {
                return;
            };
            gesture.set_state(gtk::EventSequenceState::Claimed);
            stars.imp().pressed_star.set(stars.star_at(x));
        });
        gesture.connect_released(|gesture, _n_press, x, y| {
            let Some(stars) = gesture.widget().and_downcast::<Self>() else {
                return;
            };
            let pressed_star = stars.imp().pressed_star.take();
            let released_inside = y >= 0.0 && y < f64::from(stars.height());
            if let Some(star) = pressed_star
                && released_inside
                && stars.star_at(x) == Some(star)
            {
                let rating = rating_after_click(stars.imp().stars.get(), star);
                stars.emit_by_name::<()>(RATING_CHOSEN_SIGNAL, &[&rating]);
            }
        });
        gesture.connect_cancel(|gesture, _sequence| {
            if let Some(stars) = gesture.widget().and_downcast::<Self>() {
                stars.imp().pressed_star.set(None);
            }
        });
        self.add_controller(gesture);
    }

    /// The star whose slot holds `x`, counting from 1.
    fn star_at(&self, x: f64) -> Option<u8> {
        if x < 0.0 {
            return None;
        }
        let slot = (x / f64::from(self.star_slot_width())).floor();
        (slot < f64::from(Rating::MAX_STARS)).then(|| slot as u8 + 1)
    }

    fn star_slot_width(&self) -> i32 {
        self.star_layout(FILLED_STAR)
            .pixel_size()
            .0
            .max(MIN_STAR_SLOT_WIDTH)
    }

    fn star_layout(&self, glyph: &str) -> gtk::pango::Layout {
        self.create_pango_layout(Some(glyph))
    }

    fn snapshot_stars(&self, snapshot: &gtk::Snapshot) {
        let filled = self.star_layout(FILLED_STAR);
        let empty = self.star_layout(EMPTY_STAR);
        let slot_width = self.star_slot_width();
        let filled_color = self.color();
        let empty_color = filled_color.with_alpha(filled_color.alpha() * EMPTY_STAR_OPACITY);
        let rating = self.imp().stars.get();

        for star in 1..=Rating::MAX_STARS {
            let (layout, color) = if star <= rating {
                (&filled, &filled_color)
            } else {
                (&empty, &empty_color)
            };
            let (glyph_width, glyph_height) = layout.pixel_size();
            let slot_x = slot_width * i32::from(star - 1);
            let x = slot_x + (slot_width - glyph_width) / 2;
            let y = (self.height() - glyph_height) / 2;
            snapshot.save();
            snapshot.translate(&graphene::Point::new(x as f32, y as f32));
            snapshot.append_layout(layout, color);
            snapshot.restore();
        }
    }
}

/// The rating a click on `clicked_star` asks for, given the current
/// rating: clicking the star the rating ends at clears it.
fn rating_after_click(current_stars: u8, clicked_star: u8) -> u8 {
    if current_stars == clicked_star {
        0
    } else {
        clicked_star.min(Rating::MAX_STARS)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use super::*;

    #[test]
    fn clicking_a_star_rates_up_to_it() {
        assert_eq!(rating_after_click(2, 4), 4);
        assert_eq!(rating_after_click(0, 1), 1);
        assert_eq!(rating_after_click(5, 3), 3);
    }

    #[test]
    fn clicking_the_star_the_rating_ends_at_clears_it() {
        assert_eq!(rating_after_click(4, 4), 0);
    }

    #[test]
    fn a_star_past_the_last_rates_the_maximum() {
        assert_eq!(rating_after_click(0, 9), Rating::MAX_STARS);
    }

    #[test]
    fn each_star_owns_an_equal_slot() {
        let ran = crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            let slot = f64::from(stars.star_slot_width());
            assert_eq!(stars.star_at(0.0), Some(1));
            assert_eq!(stars.star_at(slot - 0.5), Some(1));
            assert_eq!(stars.star_at(slot), Some(2));
            assert_eq!(stars.star_at(slot * 4.5), Some(5));
            assert_eq!(stars.star_at(slot * 5.0), None);
            assert_eq!(stars.star_at(-1.0), None);
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }

    #[test]
    fn rating_chosen_reports_the_rating_a_click_asks_for() {
        let ran = crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            stars.set_rating(Rating::new(3).expect("valid rating"));
            let chosen: Rc<RefCell<Vec<u8>>> = Rc::default();
            let chosen_for_signal = chosen.clone();
            stars.connect_rating_chosen(move |_stars, rating| {
                chosen_for_signal.borrow_mut().push(rating.stars());
            });

            for star in [5, 3] {
                let rating = rating_after_click(stars.imp().stars.get(), star);
                stars.emit_by_name::<()>(RATING_CHOSEN_SIGNAL, &[&rating]);
            }

            assert_eq!(*chosen.borrow(), vec![5, 0]);
            assert_eq!(
                stars.imp().stars.get(),
                3,
                "the widget shows its rating until its owner sets the new one"
            );
        });
        if !ran {
            eprintln!("skipped GTK widget test: no display available");
        }
    }
}
