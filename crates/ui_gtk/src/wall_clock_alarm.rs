// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A one-shot alarm on the wall clock, rung from the GLib main loop.
//!
//! GLib's timeouts count monotonic time, which stops while the system is
//! suspended: a timeout set for a day from now rings a day of *uptime*
//! later, however long the laptop slept in between. Something that must
//! happen when the wall clock reaches an instant — a track leaving a
//! "played in the last 7 days" window — needs a timer on the wall clock
//! itself. This is a Linux `timerfd` on `CLOCK_REALTIME`, armed at an
//! absolute instant and watched by the main loop.
//!
//! The alarm also rings whenever the wall clock is set (by the user, or
//! NTP stepping it), set to an instant or not: whatever depends on the
//! time has to be looked at again against the new one. An instant it was
//! set to may have passed, or a date left behind may be back in reach.

use std::{
    cell::{Cell, RefCell},
    io,
    os::fd::{AsRawFd, OwnedFd},
    rc::Rc,
    time::{SystemTime, UNIX_EPOCH},
};

use gtk::glib;
use rustix::{
    io::Errno,
    time::{
        Itimerspec, TimerfdClockId, TimerfdFlags, TimerfdTimerFlags, Timespec, timerfd_create,
        timerfd_settime,
    },
};

/// A wall-clock alarm. Clones share it; dropping the last stops it.
#[derive(Clone)]
pub(crate) struct WallClockAlarm {
    state: Rc<AlarmState>,
}

struct AlarmState {
    timer: OwnedFd,
    /// The instant the alarm is set to ring at, if it is set.
    rings_at: Cell<Option<SystemTime>>,
    /// The main-loop watch on the timer, once something listens.
    watch: RefCell<Option<glib::SourceId>>,
}

impl WallClockAlarm {
    /// An alarm set to nothing, already watching for the wall clock being
    /// set. It rings no one until [`connect_ring`](Self::connect_ring) is
    /// called.
    pub(crate) fn new() -> io::Result<Self> {
        let timer = timerfd_create(
            TimerfdClockId::Realtime,
            TimerfdFlags::CLOEXEC | TimerfdFlags::NONBLOCK,
        )?;
        let alarm = Self {
            state: Rc::new(AlarmState {
                timer,
                rings_at: Cell::new(None),
                watch: RefCell::new(None),
            }),
        };
        alarm.set(None);
        Ok(alarm)
    }

    /// Calls `on_ring` from the main loop each time the alarm rings,
    /// replacing whatever it called before. A ringing alarm is no longer
    /// set.
    pub(crate) fn connect_ring(&self, on_ring: impl Fn() + 'static) {
        let state = Rc::downgrade(&self.state);
        let watch = glib::unix_fd_add_local(
            self.state.timer.as_raw_fd(),
            glib::IOCondition::IN,
            move |_, _| {
                let Some(state) = state.upgrade() else {
                    return glib::ControlFlow::Break;
                };
                let mut expirations = [0_u8; 8];
                match rustix::io::read(&state.timer, &mut expirations) {
                    // The instant came, or the wall clock was set.
                    Ok(_) | Err(Errno::CANCELED) => {
                        state.rings_at.set(None);
                        on_ring();
                    }
                    // Woken for an expiry a later setting replaced.
                    Err(_) => {}
                }
                glib::ControlFlow::Continue
            },
        );
        if let Some(previous) = self.state.watch.replace(Some(watch)) {
            previous.remove();
        }
    }

    /// Sets the alarm to ring at `at`, or unsets it for `None`. An
    /// instant already past rings at once. Either way, it rings when the
    /// wall clock is set.
    pub(crate) fn set(&self, at: Option<SystemTime>) {
        // Re-arming forgets a ring the watch has not delivered yet, so
        // one owed still rings: the timer reported its instant or the
        // wall clock being set, or the instant it replaced had come by
        // the time it was replaced — which the kernel reports a moment
        // later rather than as it comes.
        let replaced = self.state.rings_at.get();
        let reported = self.take_unread_report();
        self.arm(at);
        if reported || replaced.is_some_and(|instant| instant <= SystemTime::now()) {
            self.arm(Some(UNIX_EPOCH));
        }
    }

    /// Rings the alarm from the main loop as soon as it runs, for a
    /// change to the time that the wall clock does not show: the local
    /// time zone.
    pub(crate) fn ring_now(&self) {
        self.set(Some(UNIX_EPOCH));
    }

    /// Sets the alarm to ring at `at` unless it is already set to ring
    /// sooner.
    pub(crate) fn ring_no_later_than(&self, at: SystemTime) {
        if self.state.rings_at.get().is_none_or(|current| at < current) {
            self.set(Some(at));
        }
    }

    /// The instant the alarm is set to ring at.
    #[cfg(test)]
    pub(crate) fn rings_at(&self) -> Option<SystemTime> {
        self.state.rings_at.get()
    }

    /// Reads what the timer has to report, if anything: whether its
    /// instant came, or the wall clock was set, since the watch last read
    /// it.
    fn take_unread_report(&self) -> bool {
        let mut expirations = [0_u8; 8];
        match rustix::io::read(&self.state.timer, &mut expirations) {
            Ok(_) | Err(Errno::CANCELED) => true,
            // Nothing to report.
            Err(_) => false,
        }
    }

    fn arm(&self, at: Option<SystemTime>) {
        let setting = Itimerspec {
            it_interval: DISARMED,
            it_value: at.map_or(DISARMED, absolute_instant),
        };
        self.state.rings_at.set(at);
        // Set or not, the timer watches for the wall clock being set.
        match timerfd_settime(
            &self.state.timer,
            TimerfdTimerFlags::ABSTIME | TimerfdTimerFlags::CANCEL_ON_SET,
            &setting,
        ) {
            Ok(_) => {}
            // The wall clock was set since the timer was last read, which
            // it reports here instead of to the watch; the new setting
            // took regardless. That is a ring: an instant long past rings
            // at once.
            Err(Errno::CANCELED) => self.arm(Some(UNIX_EPOCH)),
            Err(error) => {
                unreachable!("a realtime timerfd rejected a valid setting: {error}")
            }
        }
    }
}

impl Drop for AlarmState {
    fn drop(&mut self) {
        if let Some(watch) = self.watch.take() {
            watch.remove();
        }
    }
}

/// The timer setting that unsets it.
const DISARMED: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 0,
};

/// The earliest instant a timer can be set to: zero would unset it.
const EARLIEST_INSTANT: Timespec = Timespec {
    tv_sec: 0,
    tv_nsec: 1,
};

/// `at` as an absolute `CLOCK_REALTIME` setting. Before the epoch is long
/// past, so the earliest settable instant stands for it; an instant
/// beyond what the clock can count never comes.
fn absolute_instant(at: SystemTime) -> Timespec {
    match at.duration_since(UNIX_EPOCH).map(Timespec::try_from) {
        Ok(Ok(setting)) if setting != DISARMED => setting,
        // At the epoch, or before it.
        Ok(Ok(_)) | Err(_) => EARLIEST_INSTANT,
        // Past the last second the clock can count, which it never
        // reaches.
        Ok(Err(_)) => DISARMED,
    }
}

#[cfg(test)]
#[path = "wall_clock_alarm_tests.rs"]
mod tests;
