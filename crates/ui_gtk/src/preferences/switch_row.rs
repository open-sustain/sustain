// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cell::Cell;
use std::rc::Rc;

use gtk::glib::{self, Propagation};
use gtk::prelude::*;

use super::{HELPER_MAX_WIDTH_CHARS, HELPER_MIN_WIDTH_CHARS};

/// A label-on-the-left, switch-on-the-right row with a muted helper line
/// underneath. Shared layout for every capability toggle in the Preferences
/// window — keeping it in one place means the Library, Analysis, and Online
/// tabs cannot drift visually.
pub(super) struct SwitchRow {
    pub(super) container: gtk::Box,
    pub(super) switch: gtk::Switch,
}

pub(super) fn build_switch_row(label_text: &str, helper_text: &str, initial: bool) -> SwitchRow {
    let container = gtk::Box::new(gtk::Orientation::Vertical, 4);
    container.add_css_class("preference-switch-row");

    let header = gtk::Box::new(gtk::Orientation::Horizontal, 8);

    let label = gtk::Label::new(Some(label_text));
    label.set_xalign(0.0);
    label.set_hexpand(true);

    let switch = gtk::Switch::new();
    switch.set_valign(gtk::Align::Center);
    switch.set_active(initial);

    header.append(&label);
    header.append(&switch);

    let helper = gtk::Label::new(Some(helper_text));
    helper.add_css_class("preference-helper");
    helper.set_xalign(0.0);
    helper.set_wrap(true);
    helper.set_natural_wrap_mode(gtk::NaturalWrapMode::Word);
    // `width_chars` + `max_width_chars` together bound the natural-width
    // request. Without `width_chars`, `max_width_chars` is treated as an
    // ellipsize ceiling only and the label requests its full unwrapped
    // single-line width — which would propagate up through the Stack
    // (hhomogeneous = true) and widen the whole window.
    helper.set_width_chars(HELPER_MIN_WIDTH_CHARS);
    helper.set_max_width_chars(HELPER_MAX_WIDTH_CHARS);

    container.append(&header);
    container.append(&helper);

    SwitchRow { container, switch }
}

/// Connect a preference switch to the setting it controls. `apply`
/// hands a requested state to the application; `held` reads back the
/// state the setting holds. The switch shows the held state, even when a
/// refused or half-applied change left the setting somewhere other than
/// where it was requested.
///
/// Stopping `state-set` alone is not enough to refuse a change: GTK keeps
/// the knob at the requested position. It also publishes the requested
/// accessible `checked` state after the handler returns, so the switch
/// moves to the held state from the main loop, once that emission is over.
/// The held state is read when the move runs, not when it was queued: by
/// then a newer request, or code mirroring one setting onto another, may
/// have moved the setting again. The move sets both the knob and the
/// state, since either may already sit where the setting is. It emits
/// `state-set` again; the `reverting` guard lets that through without
/// calling `apply` a second time.
pub(super) fn connect_setting_switch(
    switch: &gtk::Switch,
    apply: impl Fn(bool) + 'static,
    held: impl Fn() -> bool + 'static,
) {
    let held = Rc::new(held);
    let reverting = Rc::new(Cell::new(false));
    switch.connect_state_set(move |switch, requested_state| {
        if reverting.get() {
            return Propagation::Proceed;
        }
        apply(requested_state);
        if held() == requested_state {
            return Propagation::Proceed;
        }
        let switch = switch.downgrade();
        let held = held.clone();
        let reverting = reverting.clone();
        glib::idle_add_local_once(move || {
            if let Some(switch) = switch.upgrade() {
                let held_state = held();
                reverting.set(true);
                switch.set_active(held_state);
                switch.set_state(held_state);
                reverting.set(false);
            }
        });
        Propagation::Stop
    });
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::rc::Rc;

    use gtk::glib;

    use super::connect_setting_switch;

    fn run_pending_main_loop_work() {
        let context = glib::MainContext::default();
        while context.iteration(false) {}
    }

    /// A setting stored in a cell, whose `apply` adopts a request only
    /// when `adopts` says so.
    fn connect_cell_setting(
        switch: &gtk::Switch,
        adopts: impl Fn(usize, bool) -> bool + 'static,
    ) -> (Rc<Cell<bool>>, Rc<Cell<usize>>) {
        let setting = Rc::new(Cell::new(switch.is_active()));
        let calls = Rc::new(Cell::new(0));
        let (setting_for_apply, calls_for_apply) = (setting.clone(), calls.clone());
        let setting_for_held = setting.clone();
        connect_setting_switch(
            switch,
            move |requested| {
                calls_for_apply.set(calls_for_apply.get() + 1);
                if adopts(calls_for_apply.get(), requested) {
                    setting_for_apply.set(requested);
                }
            },
            move || setting_for_held.get(),
        );
        (setting, calls)
    }

    #[test]
    fn a_refused_setting_puts_the_switch_back() {
        crate::test_support::with_gtk(|| {
            let switch = gtk::Switch::new();
            let (_setting, calls) = connect_cell_setting(&switch, |_call, _requested| false);

            switch.set_active(true);
            run_pending_main_loop_work();

            assert!(!switch.is_active());
            assert!(!switch.state());
            assert_eq!(calls.get(), 1);
        });
    }

    #[test]
    fn a_newer_request_supersedes_a_pending_refusal() {
        crate::test_support::with_gtk(|| {
            let switch = gtk::Switch::new();
            // The first request is refused; the setting adopts the rest.
            let (_setting, calls) = connect_cell_setting(&switch, |call, _requested| call > 1);

            switch.set_active(true);
            switch.set_active(false);
            switch.set_active(true);
            run_pending_main_loop_work();

            assert!(switch.is_active());
            assert!(switch.state());
            assert_eq!(calls.get(), 3);
        });
    }

    #[test]
    fn a_setting_moved_elsewhere_before_the_switch_goes_back_is_shown() {
        crate::test_support::with_gtk(|| {
            let switch = gtk::Switch::new();
            let (setting, _calls) = connect_cell_setting(&switch, |_call, _requested| false);

            // Refused, then turned on by another setting (Audio analysis
            // forcing BPM on) while the knob already sits on.
            switch.set_active(true);
            setting.set(true);
            switch.set_active(true);
            run_pending_main_loop_work();

            assert!(switch.is_active());
            assert!(switch.state());
        });
    }

    #[test]
    fn an_adopted_setting_keeps_the_switch_where_the_user_put_it() {
        crate::test_support::with_gtk(|| {
            let switch = gtk::Switch::new();
            let (setting, _calls) = connect_cell_setting(&switch, |_call, _requested| true);

            switch.set_active(true);
            run_pending_main_loop_work();

            assert!(switch.is_active());
            assert!(switch.state());
            assert!(setting.get());
        });
    }
}
