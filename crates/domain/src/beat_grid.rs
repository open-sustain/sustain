// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{fmt, time::Duration};

use crate::Bpm;

/// Beats in one bar. Sustain grids are in common time, as the DJ players
/// that consume them assume.
pub const BEATS_PER_BAR: u8 = 4;

/// A constant-tempo beat grid: beats fall every [`Self::beat_interval`]
/// from [`Self::first_beat`], numbered 1–[`BEATS_PER_BAR`] within their bar.
///
/// Times are on the track's decoded timeline — the audio as a gapless
/// decoder renders it, encoder delay and padding removed — so a grid
/// stays meaningful whatever the file format. Consumers whose decoder
/// keeps the delay shift the grid by it ([`Self::delayed`]).
///
/// The tempo is held to the hundredth of a beat, like [`Bpm`]: the beats
/// are laid out against exactly that tempo, so a player that reads the
/// tempo back reproduces the same beats.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct BeatGrid {
    tempo: Bpm,
    first_beat: Duration,
    first_beat_number: u8,
}

/// One beat of a [`BeatGrid`].
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Beat {
    /// When the beat falls, from the start of the timeline.
    pub time: Duration,
    /// Position in its bar, 1 (the downbeat) to [`BEATS_PER_BAR`].
    pub number: u8,
}

impl BeatGrid {
    /// A grid of `tempo` whose first beat falls at `first_beat` with bar
    /// position `first_beat_number`. The first beat must be the first of
    /// the timeline — earlier than one beat interval — and the number in
    /// 1..=[`BEATS_PER_BAR`]; anything else describes no grid.
    pub fn new(
        tempo: Bpm,
        first_beat: Duration,
        first_beat_number: u8,
    ) -> Result<Self, InvalidBeatGrid> {
        let interval = beat_interval(tempo);
        if first_beat >= interval || !(1..=BEATS_PER_BAR).contains(&first_beat_number) {
            return Err(InvalidBeatGrid);
        }
        Ok(Self {
            tempo,
            first_beat,
            first_beat_number,
        })
    }

    pub const fn tempo(&self) -> Bpm {
        self.tempo
    }

    pub const fn first_beat(&self) -> Duration {
        self.first_beat
    }

    pub const fn first_beat_number(&self) -> u8 {
        self.first_beat_number
    }

    /// Whether this grid's tempo is `bpm`, as far as a tempo stored in
    /// whole beats can tell: within half a beat per minute. A library
    /// tempo read from a tag that holds whole beats agrees with the grid
    /// it was rounded from; a tempo the user changed — to another
    /// metrical level, or to correct a detection — does not, and the grid
    /// no longer describes the tempo the library states.
    pub fn agrees_with(&self, bpm: Bpm) -> bool {
        self.tempo.hundredths().abs_diff(bpm.hundredths()) <= 50
    }

    /// This grid, if it describes a track whose library tempo is `bpm`.
    /// A grid at a tempo the library does not state (the user changed
    /// the BPM, or the track has none) would contradict the track's row;
    /// a player is better off showing no grid.
    pub fn for_library_tempo(self, bpm: Option<Bpm>) -> Option<Self> {
        bpm.is_some_and(|bpm| self.agrees_with(bpm)).then_some(self)
    }

    /// Time between consecutive beats.
    pub fn beat_interval(&self) -> Duration {
        beat_interval(self.tempo)
    }

    /// The same beats on a timeline that runs `delay` behind this one —
    /// a decoder that renders `delay` of extra audio before the track's
    /// first sample. Beats that the delay pushes past one interval leave
    /// room for earlier beats, which the delayed grid starts from.
    pub fn delayed(&self, delay: Duration) -> Self {
        let interval = self.beat_interval().as_secs_f64();
        let shifted = (self.first_beat + delay).as_secs_f64();
        let mut beats_before = (shifted / interval).floor();
        let mut first_beat = Duration::from_secs_f64(shifted - beats_before * interval);
        // Rounding can land the remainder on the interval itself.
        if first_beat >= self.beat_interval() {
            beats_before += 1.0;
            first_beat = Duration::ZERO;
        }
        let bar = i64::from(BEATS_PER_BAR);
        let index = (i64::from(self.first_beat_number) - 1 - beats_before as i64).rem_euclid(bar);
        Self {
            tempo: self.tempo,
            first_beat,
            first_beat_number: index as u8 + 1,
        }
    }

    /// Every beat before `end`, in order.
    pub fn beats_before(&self, end: Duration) -> impl Iterator<Item = Beat> + '_ {
        let interval = self.beat_interval().as_secs_f64();
        let first = self.first_beat.as_secs_f64();
        let end = end.as_secs_f64();
        let count = if end > first {
            ((end - first) / interval).ceil() as u64
        } else {
            0
        };
        (0..count)
            .map(move |index| Beat {
                time: Duration::from_secs_f64(first + index as f64 * interval),
                number: ((u64::from(self.first_beat_number) - 1 + index) % u64::from(BEATS_PER_BAR))
                    as u8
                    + 1,
            })
            .take_while(move |beat| beat.time.as_secs_f64() < end)
    }
}

fn beat_interval(tempo: Bpm) -> Duration {
    Duration::from_secs_f64(60.0 / tempo.as_f64())
}

/// A first beat that is not the first of its timeline, or a bar position
/// outside the bar.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBeatGrid;

impl fmt::Display for InvalidBeatGrid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "a beat grid starts within one beat of the timeline, at bar position 1–{BEATS_PER_BAR}"
        )
    }
}

impl std::error::Error for InvalidBeatGrid {}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Beat, BeatGrid, InvalidBeatGrid};
    use crate::Bpm;

    fn tempo(hundredths: u32) -> Bpm {
        Bpm::from_hundredths(hundredths).expect("valid tempo")
    }

    fn millis(ms: u64) -> Duration {
        Duration::from_millis(ms)
    }

    #[test]
    fn a_grid_starts_within_its_first_beat_on_a_bar_position() {
        // 120 BPM: one beat every 500 ms.
        assert!(BeatGrid::new(tempo(12_000), millis(499), 1).is_ok());
        assert_eq!(
            BeatGrid::new(tempo(12_000), millis(500), 1),
            Err(InvalidBeatGrid)
        );
        assert_eq!(
            BeatGrid::new(tempo(12_000), millis(0), 0),
            Err(InvalidBeatGrid)
        );
        assert_eq!(
            BeatGrid::new(tempo(12_000), millis(0), 5),
            Err(InvalidBeatGrid)
        );
    }

    #[test]
    fn beats_count_through_the_bar_until_the_end() {
        let grid = BeatGrid::new(tempo(12_000), millis(100), 3).expect("grid");
        let beats: Vec<Beat> = grid.beats_before(millis(2_100)).collect();
        let expected: Vec<(u64, u8)> = vec![(100, 3), (600, 4), (1_100, 1), (1_600, 2)];
        assert_eq!(
            beats
                .iter()
                .map(|beat| (beat.time.as_millis() as u64, beat.number))
                .collect::<Vec<_>>(),
            expected
        );
        assert_eq!(grid.beats_before(millis(100)).count(), 0);
    }

    #[test]
    fn fractional_tempos_do_not_drift() {
        // 118.86 BPM: beat 1000 lands 1000 × 60/118.86 s after the first.
        let grid = BeatGrid::new(tempo(11_886), Duration::ZERO, 1).expect("grid");
        let beat = grid
            .beats_before(Duration::from_secs(600))
            .nth(1_000)
            .expect("beat 1000");
        let expected = 1_000.0 * 60.0 / 118.86;
        assert!((beat.time.as_secs_f64() - expected).abs() < 1e-6);
        assert_eq!(beat.number, 1);
    }

    #[test]
    fn a_grid_agrees_with_tempos_within_half_a_beat() {
        let grid = BeatGrid::new(tempo(12_798), Duration::ZERO, 1).expect("grid");
        assert!(grid.agrees_with(tempo(12_800)));
        assert!(grid.agrees_with(tempo(12_750)));
        assert!(grid.agrees_with(tempo(12_848)));
        assert!(!grid.agrees_with(tempo(12_849)));
        assert!(!grid.agrees_with(tempo(6_400)));
    }

    #[test]
    fn a_grid_describes_only_a_library_tempo_it_agrees_with() {
        let grid = BeatGrid::new(tempo(12_798), Duration::ZERO, 1).expect("grid");
        assert_eq!(grid.for_library_tempo(Some(tempo(12_800))), Some(grid));
        assert_eq!(grid.for_library_tempo(Some(tempo(6_400))), None);
        assert_eq!(grid.for_library_tempo(None), None);
    }

    #[test]
    fn delaying_a_grid_makes_room_for_earlier_beats() {
        let grid = BeatGrid::new(tempo(12_000), millis(400), 1).expect("grid");
        // A short delay only moves the first beat.
        let short = grid.delayed(millis(50));
        assert_eq!(
            (short.first_beat(), short.first_beat_number()),
            (millis(450), 1)
        );
        // A delay past the interval leaves room for the bar's last beat.
        let long = grid.delayed(millis(150));
        assert_eq!(
            (long.first_beat(), long.first_beat_number()),
            (millis(50), 4)
        );
        let beats: Vec<(u64, u8)> = long
            .beats_before(millis(1_100))
            .map(|beat| (beat.time.as_millis() as u64, beat.number))
            .collect();
        assert_eq!(beats, vec![(50, 4), (550, 1), (1_050, 2)]);
        // Several bars of delay wrap the bar position all the way round.
        let bars = grid.delayed(millis(4_000));
        assert_eq!(
            (bars.first_beat(), bars.first_beat_number()),
            (millis(400), 1)
        );
    }
}
