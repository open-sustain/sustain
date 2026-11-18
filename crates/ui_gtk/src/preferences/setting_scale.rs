// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use gtk::prelude::*;

/// Connect a preference slider to the setting it controls. Connect it
/// after the slider shows the current setting. `apply` receives each new
/// position, hands it to the application, and returns the position of the
/// setting the application holds afterwards. A position that maps to the
/// current setting must dispatch nothing.
///
/// When the held position differs from the requested one, the slider
/// moves there, so it never shows a setting the application did not take.
/// The move emits `value-changed` again. That position maps to the held
/// setting, so `apply` returns it unchanged without dispatching anything.
pub(super) fn connect_setting_scale(scale: &gtk::Scale, apply: impl Fn(f64) -> f64 + 'static) {
    scale.connect_value_changed(move |scale| {
        let held = apply(scale.value());
        if held != scale.value() {
            scale.set_value(held);
        }
    });
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gtk::prelude::*;

    use super::connect_setting_scale;

    fn three_stop_scale() -> gtk::Scale {
        let scale = gtk::Scale::with_range(gtk::Orientation::Horizontal, 0.0, 2.0, 1.0);
        scale.set_value(1.0);
        scale
    }

    #[test]
    fn a_refused_position_puts_the_slider_back() {
        crate::test_support::with_gtk(|| {
            let scale = three_stop_scale();
            let requests = Rc::new(RefCell::new(Vec::new()));
            let requests_for_apply = requests.clone();
            connect_setting_scale(&scale, move |requested| {
                requests_for_apply.borrow_mut().push(requested);
                // The application keeps its current setting.
                1.0
            });

            scale.set_value(2.0);

            assert_eq!(scale.value(), 1.0);
            assert_eq!(*requests.borrow(), [2.0, 1.0]);
        });
    }

    #[test]
    fn the_slider_shows_the_setting_the_application_holds() {
        crate::test_support::with_gtk(|| {
            let scale = three_stop_scale();
            // A change the application only half takes lands elsewhere.
            connect_setting_scale(
                &scale,
                |requested| if requested == 2.0 { 0.0 } else { requested },
            );

            scale.set_value(2.0);

            assert_eq!(scale.value(), 0.0);
        });
    }
}
