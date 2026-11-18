// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use crate::CalendarDate;

/// The current time, and the calendar the user reads it in.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;

    /// The user's calendar as it stands now. Read it once to read many
    /// instants: it stays as it was, even if the user's time zone
    /// changes.
    fn calendar(&self) -> Arc<dyn Calendar>;
}

/// A calendar instants are read in: the user's time zone, with its
/// history of offset changes.
pub trait Calendar: Send + Sync {
    /// The day `instant` falls on.
    fn date_of(&self, instant: SystemTime) -> CalendarDate;
}

/// A calendar a fixed offset from UTC, which never changes it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FixedOffsetCalendar {
    utc_offset_seconds: i32,
}

impl FixedOffsetCalendar {
    pub const UTC: Self = Self::new(0);

    /// The calendar `utc_offset_seconds` ahead of UTC.
    pub const fn new(utc_offset_seconds: i32) -> Self {
        Self { utc_offset_seconds }
    }
}

impl Calendar for FixedOffsetCalendar {
    fn date_of(&self, instant: SystemTime) -> CalendarDate {
        CalendarDate::of_instant(instant, self.utc_offset_seconds)
    }
}

/// Another clock stopped at one reading: every [`Clock::now`] answers
/// that reading, in the calendar the other clock had then. Lets several
/// evaluations agree on one present.
#[derive(Clone)]
pub struct FrozenClock {
    now: SystemTime,
    calendar: Arc<dyn Calendar>,
}

impl FrozenClock {
    /// `clock`, stopped at the time it reads now.
    pub fn new(clock: &dyn Clock) -> Self {
        Self {
            now: clock.now(),
            calendar: clock.calendar(),
        }
    }
}

impl Clock for FrozenClock {
    fn now(&self) -> SystemTime {
        self.now
    }

    fn calendar(&self) -> Arc<dyn Calendar> {
        self.calendar.clone()
    }
}

pub trait MonotonicClock: Send + Sync {
    fn now(&self) -> Duration;
}

#[derive(Clone, Copy, Debug)]
pub struct SystemMonotonicClock {
    origin: Instant,
}

impl SystemMonotonicClock {
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
        }
    }
}

impl Default for SystemMonotonicClock {
    fn default() -> Self {
        Self::new()
    }
}

impl MonotonicClock for SystemMonotonicClock {
    fn now(&self) -> Duration {
        self.origin.elapsed()
    }
}
