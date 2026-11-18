// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! A day on the calendar, with no time of day and no time zone.

use std::fmt;
use std::time::SystemTime;

const SECONDS_PER_DAY: i64 = 86_400;

/// A day of the proleptic Gregorian calendar, from 0001-01-01 to
/// 9999-12-31: what a person means by "January 1, 2024". It names no
/// instant: which day an instant falls on depends on where it is read —
/// see [`crate::Calendar::date_of`].
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct CalendarDate {
    year: u16,
    month: u8,
    day: u8,
}

impl CalendarDate {
    /// The date `year`-`month`-`day`, if it exists in the supported range.
    pub fn new(year: u16, month: u8, day: u8) -> Option<Self> {
        ((1..=9999).contains(&year)
            && (1..=12).contains(&month)
            && (1..=days_in_month(year, month)).contains(&day))
        .then_some(Self { year, month, day })
    }

    /// Parse the ISO 8601 calendar form `YYYY-MM-DD`.
    pub fn parse_iso(text: &str) -> Option<Self> {
        let mut parts = text.split('-');
        let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some()
            || year.len() != 4
            || month.len() != 2
            || day.len() != 2
            || ![year, month, day]
                .iter()
                .all(|part| part.bytes().all(|byte| byte.is_ascii_digit()))
        {
            return None;
        }
        Self::new(year.parse().ok()?, month.parse().ok()?, day.parse().ok()?)
    }

    pub fn year(self) -> u16 {
        self.year
    }

    pub fn month(self) -> u8 {
        self.month
    }

    pub fn day(self) -> u8 {
        self.day
    }

    /// The day `instant` falls on, read on a clock `utc_offset_seconds`
    /// ahead of UTC. An instant before the first supported day reads as
    /// that day, one after the last as the last.
    pub fn of_instant(instant: SystemTime, utc_offset_seconds: i32) -> Self {
        let days = unix_seconds_floor(instant)
            .saturating_add(i64::from(utc_offset_seconds))
            .div_euclid(SECONDS_PER_DAY);
        Self::from_days_since_unix_epoch(days.clamp(
            Self::FIRST.days_since_unix_epoch(),
            Self::LAST.days_since_unix_epoch(),
        ))
    }

    const FIRST: Self = Self {
        year: 1,
        month: 1,
        day: 1,
    };

    const LAST: Self = Self {
        year: 9999,
        month: 12,
        day: 31,
    };

    /// Days from 1970-01-01 to this date, negative before it (Howard
    /// Hinnant's `days_from_civil`).
    fn days_since_unix_epoch(self) -> i64 {
        let month = i64::from(self.month);
        let year = i64::from(self.year) - i64::from(month <= 2);
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let day_of_year =
            (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }

    /// The date `days` after 1970-01-01, for `days` within the supported
    /// range (Howard Hinnant's `civil_from_days`).
    fn from_days_since_unix_epoch(days: i64) -> Self {
        let shifted = days + 719_468;
        let era = shifted.div_euclid(146_097);
        let day_of_era = shifted - era * 146_097;
        let year_of_era =
            (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
        let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
        let shifted_month = (5 * day_of_year + 2) / 153;
        let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
        let month = if shifted_month < 10 {
            shifted_month + 3
        } else {
            shifted_month - 9
        };
        let year = year_of_era + era * 400 + i64::from(month <= 2);
        Self {
            year: u16::try_from(year).expect("a supported day's year fits in u16"),
            month: u8::try_from(month).expect("a month fits in u8"),
            day: u8::try_from(day).expect("a day of the month fits in u8"),
        }
    }
}

/// The whole seconds from the Unix epoch to `instant`, rounded down —
/// toward the past, on either side of the epoch — and saturated at the
/// ends of `i64`. The second an instant falls within, so a calendar reads
/// it in the offset of that second.
pub fn unix_seconds_floor(instant: SystemTime) -> i64 {
    match instant.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_secs()).unwrap_or(i64::MAX),
        Err(before) => {
            let before = before.duration();
            let whole = before.as_secs() + u64::from(before.subsec_nanos() > 0);
            i64::try_from(whole).map_or(i64::MIN, |seconds| -seconds)
        }
    }
}

impl fmt::Display for CalendarDate {
    /// The ISO 8601 calendar form `YYYY-MM-DD`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{:04}-{:02}-{:02}",
            self.year, self.month, self.day
        )
    }
}

fn days_in_month(year: u16, month: u8) -> u8 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if (year.is_multiple_of(4) && !year.is_multiple_of(100)) || year.is_multiple_of(400) => {
            29
        }
        2 => 28,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::CalendarDate;
    use std::time::{Duration, SystemTime};

    fn date(year: u16, month: u8, day: u8) -> CalendarDate {
        CalendarDate::new(year, month, day).expect("valid date")
    }

    #[test]
    fn only_days_that_exist_are_dates() {
        assert!(CalendarDate::new(2024, 2, 29).is_some());
        assert!(CalendarDate::new(2023, 2, 29).is_none());
        assert!(CalendarDate::new(1900, 2, 29).is_none());
        assert!(CalendarDate::new(2000, 2, 29).is_some());
        assert!(CalendarDate::new(2024, 4, 31).is_none());
        assert!(CalendarDate::new(2024, 13, 1).is_none());
        assert!(CalendarDate::new(0, 1, 1).is_none());
        assert!(CalendarDate::new(10_000, 1, 1).is_none());
    }

    #[test]
    fn the_iso_form_round_trips_and_nothing_else_parses() {
        let parsed = CalendarDate::parse_iso("2024-01-05").expect("parses");
        assert_eq!(parsed, date(2024, 1, 5));
        assert_eq!(parsed.to_string(), "2024-01-05");
        assert_eq!(date(7, 3, 9).to_string(), "0007-03-09");
        for text in [
            "2024-1-05",
            "24-01-05",
            "2024-01-05-01",
            "2024/01/05",
            "+024-01-05",
            "2024-02-30",
            "",
        ] {
            assert_eq!(CalendarDate::parse_iso(text), None, "{text}");
        }
    }

    fn unix(seconds: i64) -> SystemTime {
        if seconds >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs())
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
        }
    }

    #[test]
    fn an_instant_falls_on_its_day_in_the_offset_it_is_read_in() {
        assert_eq!(CalendarDate::of_instant(unix(0), 0), date(1970, 1, 1));
        assert_eq!(CalendarDate::of_instant(unix(-1), 0), date(1969, 12, 31));
        assert_eq!(
            CalendarDate::of_instant(unix(0), -3_600),
            date(1969, 12, 31)
        );
        // 2024-02-28T23:30Z, an hour later in Paris.
        assert_eq!(
            CalendarDate::of_instant(unix(1_709_163_000), 3_600),
            date(2024, 2, 29)
        );
        assert_eq!(
            CalendarDate::of_instant(unix(1_709_163_000 + 86_400), 3_600),
            date(2024, 3, 1)
        );
        assert_eq!(
            CalendarDate::of_instant(unix(-62_135_596_800), 0),
            date(1, 1, 1)
        );
        assert_eq!(
            CalendarDate::of_instant(unix(253_402_300_799), 0),
            date(9999, 12, 31)
        );
    }

    #[test]
    fn every_supported_day_is_the_day_of_its_utc_midnight() {
        let mut days = 0;
        for year in [
            1, 4, 100, 400, 1600, 1900, 1969, 1970, 2000, 2024, 2100, 9999,
        ] {
            for month in 1..=12 {
                for day in [1, 28, 29, 30, 31] {
                    if let Some(calendar_date) = CalendarDate::new(year, month, day) {
                        let midnight = calendar_date.days_since_unix_epoch() * 86_400;
                        assert_eq!(CalendarDate::of_instant(unix(midnight), 0), calendar_date);
                        if calendar_date != CalendarDate::FIRST {
                            assert_eq!(
                                CalendarDate::of_instant(unix(midnight - 1), 0)
                                    .days_since_unix_epoch(),
                                calendar_date.days_since_unix_epoch() - 1
                            );
                        }
                        days += 1;
                    }
                }
            }
        }
        assert!(days > 500);
    }

    #[test]
    fn an_instant_a_fraction_of_a_second_before_a_day_is_on_the_day_before() {
        let just_before_the_epoch = SystemTime::UNIX_EPOCH - Duration::from_nanos(1);
        assert_eq!(super::unix_seconds_floor(just_before_the_epoch), -1);
        assert_eq!(
            CalendarDate::of_instant(just_before_the_epoch, 0),
            date(1969, 12, 31)
        );
        let just_after_the_epoch = SystemTime::UNIX_EPOCH + Duration::from_nanos(1);
        assert_eq!(super::unix_seconds_floor(just_after_the_epoch), 0);
    }

    #[test]
    fn instants_beyond_the_calendar_read_as_its_nearest_end() {
        assert_eq!(
            CalendarDate::of_instant(unix(-62_135_596_801), 0),
            date(1, 1, 1)
        );
        assert_eq!(
            CalendarDate::of_instant(unix(253_402_300_800), 0),
            date(9999, 12, 31)
        );
        let far_future = SystemTime::UNIX_EPOCH + Duration::from_secs(u64::MAX / 2);
        assert_eq!(CalendarDate::of_instant(far_future, 0), date(9999, 12, 31));
    }
}
