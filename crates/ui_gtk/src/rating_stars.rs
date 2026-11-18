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
//!
//! The widget is a slider to assistive technology, valued from 0 (unrated)
//! to five stars, which assistive technology can also set. An owner that
//! edits one rating, such as Get Info, builds it
//! [keyboard operable](RatingStars::keyboard_operable); the track table's
//! hundreds of ratings carry no key controller.

use std::cell::Cell;
use std::sync::OnceLock;

use gtk::glib::subclass::Signal;
use gtk::prelude::*;
use gtk::subclass::prelude::*;
use gtk::{gdk, glib, graphene};
use sustain_app_runtime::Rating;
use sustain_i18n::{gettext, ngettext, plural_count, tr_format};

const FILLED_STAR: &str = "★";
const EMPTY_STAR: &str = "☆";

/// Narrowest slot a star sits in. Every star gets the same slot, so the
/// stars line up from row to row and each is an equally wide click
/// target.
const MIN_STAR_SLOT_WIDTH: i32 = 16;

/// Opacity of an empty star relative to a filled one.
const EMPTY_STAR_OPACITY: f32 = 0.4;

const RATING_CHOSEN_SIGNAL: &str = "rating-chosen";

/// Ask for the rating `step` stars away from the current one (an `i32`).
const STEP_ACTION: &str = "rating.step";
/// Ask for a rating of the given number of stars (a `u8`).
const SET_ACTION: &str = "rating.set";

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
        type Interfaces = (gtk::AccessibleRange,);

        fn class_init(klass: &mut Self::Class) {
            klass.set_css_name("rating-stars");
            klass.set_accessible_role(gtk::AccessibleRole::Slider);

            klass.install_action(
                STEP_ACTION,
                Some(glib::VariantTy::INT32),
                |stars, _, step| {
                    if let Some(step) = step.and_then(i32::from_variant) {
                        stars.choose(rating_after_step(stars.imp().stars.get(), step));
                    }
                },
            );
            klass.install_action(
                SET_ACTION,
                Some(glib::VariantTy::BYTE),
                |stars, _, rating| {
                    if let Some(rating) = rating.and_then(u8::from_variant) {
                        stars.choose(rating.min(Rating::MAX_STARS));
                    }
                },
            );
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
            let obj = self.obj();
            obj.update_property(&[
                gtk::accessible::Property::ValueMin(0.0),
                gtk::accessible::Property::ValueMax(f64::from(Rating::MAX_STARS)),
            ]);
            obj.announce_rating();
            obj.install_click_gesture();
        }
    }

    impl AccessibleImpl for RatingStars {}

    impl AccessibleRangeImpl for RatingStars {
        /// Assistive technology sets the slider as a click would: by
        /// asking the owner for the rating. It is set when the owner
        /// shows the rating asked for.
        fn set_current_value(&self, value: f64) -> bool {
            let stars = stars_for_accessible_value(value);
            self.obj().choose(stars);
            self.stars.get() == stars
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
        @implements gtk::Accessible, gtk::AccessibleRange, gtk::Buildable, gtk::ConstraintTarget;
}

impl RatingStars {
    pub(crate) fn new() -> Self {
        glib::Object::new()
    }

    /// A rating that takes the keyboard, for an owner that edits one
    /// rating: it is focusable, the arrow keys and `+`/`-` step it, the
    /// digit keys set it, End sets the maximum, and Home, Backspace or
    /// Delete clear it.
    pub(crate) fn keyboard_operable() -> Self {
        let stars = Self::new();
        stars.set_focusable(true);
        stars.add_controller(keyboard_shortcuts());
        stars
    }

    pub(crate) fn rating(&self) -> Rating {
        Rating::new(self.imp().stars.get()).unwrap_or_else(Rating::unrated)
    }

    pub(crate) fn set_rating(&self, rating: Rating) {
        if self.imp().stars.replace(rating.stars()) != rating.stars() {
            self.announce_rating();
            self.queue_draw();
        }
    }

    /// Publish the shown rating as the slider's accessible value.
    fn announce_rating(&self) {
        let stars = self.imp().stars.get();
        let text = if stars == 0 {
            gettext("Unrated")
        } else {
            tr_format!(
                ngettext(
                    "{count} star",
                    "{count} stars",
                    plural_count(usize::from(stars))
                ),
                count = stars
            )
        };
        self.update_property(&[
            gtk::accessible::Property::ValueNow(f64::from(stars)),
            gtk::accessible::Property::ValueText(&text),
        ]);
    }

    /// Ask the owner for a rating of `stars`, as a click would.
    fn choose(&self, stars: u8) {
        self.emit_by_name::<()>(RATING_CHOSEN_SIGNAL, &[&stars]);
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
            if stars.is_focusable() {
                // A click leaves the keyboard where the pointer was.
                stars.grab_focus();
            }
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
                stars.choose(rating_after_click(stars.imp().stars.get(), star));
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

/// The key bindings of [`RatingStars::keyboard_operable`]. They live on
/// that instance rather than the class: GTK gives every instance of a
/// class with shortcuts its own shortcut controller, and the track table
/// realizes hundreds of ratings that never take the keyboard.
fn keyboard_shortcuts() -> gtk::ShortcutController {
    let controller = gtk::ShortcutController::new();
    let bind = |key: gdk::Key, action: &str, argument: glib::Variant| {
        controller.add_shortcut(gtk::Shortcut::with_arguments(
            Some(gtk::KeyvalTrigger::new(key, gdk::ModifierType::empty())),
            Some(gtk::NamedAction::new(action)),
            &argument,
        ));
    };
    for key in [
        gdk::Key::Right,
        gdk::Key::Up,
        gdk::Key::KP_Right,
        gdk::Key::KP_Up,
        gdk::Key::plus,
        gdk::Key::KP_Add,
    ] {
        bind(key, STEP_ACTION, 1_i32.to_variant());
    }
    for key in [
        gdk::Key::Left,
        gdk::Key::Down,
        gdk::Key::KP_Left,
        gdk::Key::KP_Down,
        gdk::Key::minus,
        gdk::Key::KP_Subtract,
    ] {
        bind(key, STEP_ACTION, (-1_i32).to_variant());
    }
    for key in [gdk::Key::Home, gdk::Key::BackSpace, gdk::Key::Delete] {
        bind(key, SET_ACTION, 0_u8.to_variant());
    }
    bind(gdk::Key::End, SET_ACTION, Rating::MAX_STARS.to_variant());
    let digits = [
        (gdk::Key::_0, gdk::Key::KP_0),
        (gdk::Key::_1, gdk::Key::KP_1),
        (gdk::Key::_2, gdk::Key::KP_2),
        (gdk::Key::_3, gdk::Key::KP_3),
        (gdk::Key::_4, gdk::Key::KP_4),
        (gdk::Key::_5, gdk::Key::KP_5),
    ];
    for (stars, (key, keypad_key)) in (0_u8..).zip(digits) {
        bind(key, SET_ACTION, stars.to_variant());
        bind(keypad_key, SET_ACTION, stars.to_variant());
    }
    controller
}

/// The rating an accessible slider value asks for: the nearest whole
/// number of stars, within zero to the maximum.
fn stars_for_accessible_value(value: f64) -> u8 {
    if value.is_nan() {
        return 0;
    }
    // Clamped to 0..=MAX_STARS first, so the cast is exact.
    value.round().clamp(0.0, f64::from(Rating::MAX_STARS)) as u8
}

/// The rating `step` stars away from `current_stars`, within zero to the
/// maximum.
fn rating_after_step(current_stars: u8, step: i32) -> u8 {
    let stepped = i32::from(current_stars).saturating_add(step);
    u8::try_from(stepped.clamp(0, i32::from(Rating::MAX_STARS))).unwrap_or(0)
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
    fn stepping_stays_between_unrated_and_the_maximum() {
        assert_eq!(rating_after_step(3, 1), 4);
        assert_eq!(rating_after_step(3, -1), 2);
        assert_eq!(rating_after_step(Rating::MAX_STARS, 1), Rating::MAX_STARS);
        assert_eq!(rating_after_step(0, -1), 0);
    }

    /// Presses `key`, with no modifier held, on `stars` as its shortcut
    /// controller would: runs the action bound to that key, with its
    /// arguments.
    fn press(stars: &RatingStars, key: gdk::Key) {
        let controllers = stars.observe_controllers();
        let shortcut = (0..controllers.n_items())
            .filter_map(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::ShortcutController>()
            })
            .flat_map(|controller| {
                (0..controller.n_items())
                    .filter_map(|index| controller.item(index).and_downcast::<gtk::Shortcut>())
                    .collect::<Vec<_>>()
            })
            .find(|shortcut| {
                shortcut
                    .trigger()
                    .and_downcast::<gtk::KeyvalTrigger>()
                    .is_some_and(|trigger| {
                        trigger.keyval() == key && trigger.modifiers().is_empty()
                    })
            })
            .expect("a shortcut is bound to the key");
        let action = shortcut
            .action()
            .and_downcast::<gtk::NamedAction>()
            .expect("a rating key names a rating action");
        stars
            .activate_action(&action.action_name(), shortcut.arguments().as_ref())
            .expect("the rating action exists");
    }

    #[test]
    fn the_keys_ask_for_the_stepped_or_typed_rating() {
        crate::test_support::with_gtk(|| {
            let stars = RatingStars::keyboard_operable();
            stars.set_rating(Rating::new(3).expect("valid rating"));
            stars.connect_rating_chosen(RatingStars::set_rating);
            let shown_after = |key: gdk::Key| {
                press(&stars, key);
                stars.rating().stars()
            };

            assert!(stars.is_focusable());
            assert_eq!(shown_after(gdk::Key::Right), 4);
            assert_eq!(shown_after(gdk::Key::KP_Add), 5);
            assert_eq!(shown_after(gdk::Key::Up), 5, "the maximum holds");
            assert_eq!(shown_after(gdk::Key::Left), 4);
            assert_eq!(shown_after(gdk::Key::minus), 3);
            assert_eq!(shown_after(gdk::Key::_2), 2);
            assert_eq!(shown_after(gdk::Key::End), 5);
            assert_eq!(shown_after(gdk::Key::KP_4), 4);
            assert_eq!(shown_after(gdk::Key::BackSpace), 0);
            assert_eq!(shown_after(gdk::Key::Down), 0, "unrated holds");
        });
    }

    #[test]
    fn a_table_rating_carries_no_key_controller() {
        crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            let controllers = stars.observe_controllers();
            let has_shortcuts = (0..controllers.n_items()).any(|index| {
                controllers
                    .item(index)
                    .and_downcast::<gtk::ShortcutController>()
                    .is_some()
            });
            assert!(!has_shortcuts);
            assert!(!stars.is_focusable());
        });
    }

    #[test]
    fn assistive_technology_sets_the_rating_through_the_slider() {
        crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            stars.connect_rating_chosen(RatingStars::set_rating);
            assert!(stars.is::<gtk::AccessibleRange>());

            assert!(stars.imp().set_current_value(3.6));
            assert_eq!(stars.rating().stars(), 4);
            assert!(stars.imp().set_current_value(9.0));
            assert_eq!(stars.rating().stars(), Rating::MAX_STARS);
            assert!(stars.imp().set_current_value(-2.0));
            assert_eq!(stars.rating().stars(), 0);
        });
    }

    #[test]
    fn a_rating_its_owner_refuses_is_not_reported_as_set() {
        crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            stars.set_rating(Rating::new(2).expect("valid rating"));
            stars.connect_rating_chosen(|_stars, _refused| {});

            assert!(!stars.imp().set_current_value(4.0));
            assert_eq!(stars.rating().stars(), 2);
        });
    }

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
        crate::test_support::with_gtk(|| {
            let stars = RatingStars::new();
            let slot = f64::from(stars.star_slot_width());
            assert_eq!(stars.star_at(0.0), Some(1));
            assert_eq!(stars.star_at(slot - 0.5), Some(1));
            assert_eq!(stars.star_at(slot), Some(2));
            assert_eq!(stars.star_at(slot * 4.5), Some(5));
            assert_eq!(stars.star_at(slot * 5.0), None);
            assert_eq!(stars.star_at(-1.0), None);
        });
    }

    #[test]
    fn rating_chosen_reports_the_rating_a_click_asks_for() {
        crate::test_support::with_gtk(|| {
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
    }
}
