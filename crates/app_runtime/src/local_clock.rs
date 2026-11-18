// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The system clock, read in the user's local time zone.

use std::sync::Arc;
use std::time::SystemTime;

use sustain_domain::{Calendar, CalendarDate, Clock, unix_seconds_floor};

/// The system clock, its days read in the local time zone — the same
/// zone the library displays its dates in.
#[derive(Clone, Copy, Debug, Default)]
pub struct LocalClock;

impl Clock for LocalClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }

    fn calendar(&self) -> Arc<dyn Calendar> {
        Arc::new(ZoneCalendar(glib::TimeZone::local()))
    }
}

/// A time zone's calendar, with every offset change in its history.
struct ZoneCalendar(glib::TimeZone);

impl Calendar for ZoneCalendar {
    fn date_of(&self, instant: SystemTime) -> CalendarDate {
        let interval = self
            .0
            .find_interval(glib::TimeType::Universal, unix_seconds_floor(instant));
        CalendarDate::of_instant(instant, self.0.offset(interval))
    }
}

#[cfg(test)]
mod tests {
    use super::ZoneCalendar;
    use std::time::{Duration, SystemTime};
    use sustain_domain::{Calendar, CalendarDate};

    fn unix(seconds: i64) -> SystemTime {
        if seconds >= 0 {
            SystemTime::UNIX_EPOCH + Duration::from_secs(seconds.unsigned_abs())
        } else {
            SystemTime::UNIX_EPOCH - Duration::from_secs(seconds.unsigned_abs())
        }
    }

    fn date(text: &str) -> CalendarDate {
        CalendarDate::parse_iso(text).expect("a valid date")
    }

    fn zone(identifier: &str) -> ZoneCalendar {
        ZoneCalendar(glib::TimeZone::from_identifier(Some(identifier)).expect(identifier))
    }

    #[test]
    fn an_instant_falls_on_its_local_day() {
        let paris = zone("Europe/Paris");
        // 23:30 UTC on 2023-12-31 is 00:30 in Paris, winter time.
        assert_eq!(
            paris.date_of(unix(1_704_067_200 - 1_800)),
            date("2024-01-01")
        );
        assert_eq!(
            paris.date_of(unix(1_704_067_200 - 3_601)),
            date("2023-12-31")
        );
        // In summer, Paris is two hours ahead.
        assert_eq!(
            paris.date_of(unix(1_719_792_000 - 7_200)),
            date("2024-07-01")
        );
        assert_eq!(
            paris.date_of(unix(1_719_792_000 - 7_201)),
            date("2024-06-30")
        );
    }

    #[test]
    fn a_clock_stepped_back_across_midnight_returns_to_the_previous_day() {
        // At 02:31 UTC on 2009-11-01, St. John's stepped from 00:01 back
        // to 23:01 on October 31.
        let st_johns = zone("America/St_Johns");
        assert_eq!(st_johns.date_of(unix(1_257_042_600)), date("2009-11-01"));
        assert_eq!(st_johns.date_of(unix(1_257_042_659)), date("2009-11-01"));
        assert_eq!(st_johns.date_of(unix(1_257_042_660)), date("2009-10-31"));
    }

    #[test]
    fn a_skipped_day_holds_no_instant() {
        // Manila went from 1844-12-30 straight to 1845-01-01.
        let manila = zone("Asia/Manila");
        assert_eq!(manila.date_of(unix(-3_944_621_033)), date("1844-12-30"));
        assert_eq!(manila.date_of(unix(-3_944_621_032)), date("1845-01-01"));
        // Within the last second before the jump, still the day before.
        assert_eq!(
            manila.date_of(unix(-3_944_621_032) - Duration::from_nanos(1)),
            date("1844-12-30")
        );
    }

    #[test]
    fn a_repeated_midnight_stays_on_its_day() {
        // Havana stepped from 01:00 back to 00:00 on 2024-11-03.
        let havana = zone("America/Havana");
        assert_eq!(havana.date_of(unix(1_730_606_399)), date("2024-11-02"));
        assert_eq!(havana.date_of(unix(1_730_606_400)), date("2024-11-03"));
        assert_eq!(havana.date_of(unix(1_730_610_000)), date("2024-11-03"));
    }
}
