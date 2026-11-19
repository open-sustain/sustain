// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::Cell,
    rc::Rc,
    time::{Duration, Instant, SystemTime},
};

use gtk::glib;

use super::WallClockAlarm;
use crate::test_support::with_gtk;

/// An alarm whose rings are counted.
fn counted_alarm() -> (WallClockAlarm, Rc<Cell<u32>>) {
    let alarm = WallClockAlarm::new().expect("create a wall-clock timer");
    let rings = Rc::new(Cell::new(0));
    let counter = rings.clone();
    alarm.connect_ring(move || counter.set(counter.get() + 1));
    (alarm, rings)
}

/// Runs the main loop for `duration`, or until `done`.
fn run_main_loop(duration: Duration, done: impl Fn() -> bool) {
    let context = glib::MainContext::default();
    let deadline = Instant::now() + duration;
    while !done() && Instant::now() < deadline {
        context.iteration(false);
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn an_alarm_rings_once_the_wall_clock_reaches_its_instant() {
    with_gtk(|| {
        let alarm = WallClockAlarm::new().expect("create a wall-clock timer");
        let rung_at = Rc::new(Cell::new(None));
        let rung_at_for_ring = rung_at.clone();
        alarm.connect_ring(move || rung_at_for_ring.set(Some(SystemTime::now())));
        let rings_at = SystemTime::now() + Duration::from_millis(50);
        alarm.set(Some(rings_at));

        run_main_loop(Duration::from_secs(5), || rung_at.get().is_some());

        let rung_at = rung_at.get().expect("the alarm rang");
        assert!(rung_at >= rings_at, "rang before its instant");
    });
}

#[test]
fn an_alarm_set_in_the_past_rings_at_once_and_only_once() {
    with_gtk(|| {
        let (alarm, rings) = counted_alarm();
        alarm.set(Some(SystemTime::UNIX_EPOCH));

        run_main_loop(Duration::from_millis(200), || false);

        assert_eq!(rings.get(), 1);
    });
}

#[test]
fn an_unset_alarm_does_not_ring() {
    with_gtk(|| {
        let (alarm, rings) = counted_alarm();
        alarm.set(Some(SystemTime::now() + Duration::from_millis(20)));
        alarm.set(None);

        run_main_loop(Duration::from_millis(200), || false);

        assert_eq!(rings.get(), 0);
    });
}

#[test]
fn an_alarm_rings_at_the_sooner_of_its_instants() {
    with_gtk(|| {
        let (alarm, rings) = counted_alarm();
        let soon = SystemTime::now() + Duration::from_millis(20);
        let later = SystemTime::now() + Duration::from_secs(60);

        // A later instant leaves the sooner one set.
        alarm.set(Some(soon));
        alarm.ring_no_later_than(later);
        run_main_loop(Duration::from_secs(5), || rings.get() > 0);
        assert_eq!(rings.get(), 1);

        // A sooner instant replaces the later one.
        alarm.set(Some(later));
        alarm.ring_no_later_than(SystemTime::now() + Duration::from_millis(20));
        run_main_loop(Duration::from_secs(5), || rings.get() > 1);
        assert_eq!(rings.get(), 2);
    });
}

#[test]
fn a_rung_alarm_is_no_longer_set() {
    with_gtk(|| {
        let (alarm, rings) = counted_alarm();
        alarm.set(Some(SystemTime::UNIX_EPOCH));
        run_main_loop(Duration::from_secs(5), || rings.get() > 0);

        // Not set any more, so any instant is sooner.
        alarm.ring_no_later_than(SystemTime::now() + Duration::from_millis(20));
        run_main_loop(Duration::from_secs(5), || rings.get() > 1);

        assert_eq!(rings.get(), 2);
    });
}

#[test]
fn ringing_now_rings_from_the_main_loop_once() {
    with_gtk(|| {
        let (alarm, rings) = counted_alarm();
        alarm.set(Some(SystemTime::now() + Duration::from_secs(60)));

        alarm.ring_now();
        assert_eq!(rings.get(), 0, "rings from the main loop, not at once");
        run_main_loop(Duration::from_millis(200), || false);

        assert_eq!(rings.get(), 1);
        assert_eq!(alarm.rings_at(), None);
    });
}

#[test]
fn what_the_timer_reported_still_rings_after_re_arming() {
    with_gtk(|| {
        for later in [None, Some(SystemTime::now() + Duration::from_secs(60))] {
            let (alarm, rings) = counted_alarm();
            // The instant has come before the main loop runs to read it.
            alarm.set(Some(SystemTime::UNIX_EPOCH));
            alarm.set(later);

            run_main_loop(Duration::from_millis(200), || false);

            assert_eq!(rings.get(), 1, "re-armed to {later:?}");
        }
    });
}

#[test]
fn an_instant_that_has_come_rings_after_re_arming_before_the_timer_reports_it() {
    with_gtk(|| {
        for later in [None, Some(SystemTime::now() + Duration::from_secs(60))] {
            let (alarm, rings) = counted_alarm();
            alarm.set(Some(SystemTime::now() - Duration::from_secs(1)));
            // The kernel reports an instant already past a moment after it
            // is set, not as it is set: until then the timer has nothing
            // to read. Taking the report here leaves it in that state.
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut taken = alarm.take_unread_report();
            while !taken && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(1));
                taken = alarm.take_unread_report();
            }
            assert!(taken, "the timer never reported its instant");
            alarm.set(later);

            run_main_loop(Duration::from_millis(200), || false);

            assert_eq!(rings.get(), 1, "re-armed to {later:?}");
        }
    });
}
