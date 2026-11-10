// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A track's listening statistics.
///
/// The library keeps their times to the whole second, which is what its
/// database stores; [`Self::added_at`], [`Self::record_play`] and
/// [`Self::record_skip`] record a time that way, so the statistics the
/// application holds are the ones the database gives back.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PlayStatistics {
    pub play_count: u64,
    pub skip_count: u64,
    pub last_played_at: Option<SystemTime>,
    pub last_skipped_at: Option<SystemTime>,
    pub date_added_at: Option<SystemTime>,
}

impl PlayStatistics {
    /// The statistics of a track added to the library at `at`.
    pub fn added_at(at: SystemTime) -> Self {
        Self {
            date_added_at: Some(whole_second(at)),
            ..Self::default()
        }
    }

    /// Count a play that reached the play threshold at `at`.
    pub fn record_play(&mut self, at: SystemTime) {
        self.play_count = self.play_count.saturating_add(1);
        self.last_played_at = Some(whole_second(at));
    }

    /// Count a skip at `at`.
    pub fn record_skip(&mut self, at: SystemTime) {
        self.skip_count = self.skip_count.saturating_add(1);
        self.last_skipped_at = Some(whole_second(at));
    }
}

/// `at` without its fraction of a second.
fn whole_second(at: SystemTime) -> SystemTime {
    at.duration_since(UNIX_EPOCH).map_or(at, |since| {
        UNIX_EPOCH + Duration::from_secs(since.as_secs())
    })
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use super::PlayStatistics;

    #[test]
    fn recorded_times_drop_their_fraction_of_a_second() {
        let at = UNIX_EPOCH + Duration::new(1_700_000_000, 987_654_321);
        let second = UNIX_EPOCH + Duration::from_secs(1_700_000_000);

        let mut statistics = PlayStatistics::added_at(at);
        statistics.record_play(at);
        statistics.record_skip(at);

        assert_eq!(
            statistics,
            PlayStatistics {
                play_count: 1,
                skip_count: 1,
                last_played_at: Some(second),
                last_skipped_at: Some(second),
                date_added_at: Some(second),
            }
        );
    }
}
