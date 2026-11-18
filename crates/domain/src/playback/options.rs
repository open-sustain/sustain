// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

/// Which shuffle algorithm — if any — the playback queue is using. The
/// transport's shuffle button cycles `Off → Pure → Smart → Off`; the
/// chosen variant determines both how the queue lays out its next-track
/// sequence and how the now-playing icon paints.
///
/// `Pure` is the Fisher-Yates random walk over the ordered list.
/// `Smart` defers next-track selection to the Smart Shuffle picker,
/// which scores each candidate as a *continuation* of the currently
/// playing track using a fixed, interpretable perceptual transition
/// metric (no learned model).
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ShuffleMode {
    #[default]
    Off,
    Pure,
    Smart,
}

impl ShuffleMode {
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    pub const fn is_smart(self) -> bool {
        matches!(self, Self::Smart)
    }

    /// Next mode in the user-facing tri-state cycle. The transport
    /// button advances through this on every click; the cycle exists
    /// here so the queue and UI agree on the order without hard-coding
    /// it in two places.
    pub const fn next_in_cycle(self) -> Self {
        match self {
            Self::Off => Self::Pure,
            Self::Pure => Self::Smart,
            Self::Smart => Self::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum RepeatMode {
    #[default]
    Off,
    One,
    All,
}

impl RepeatMode {
    pub const fn is_enabled(self) -> bool {
        !matches!(self, Self::Off)
    }

    /// Next mode in the transport button's cycle, in iTunes's order
    /// (`Off → All → One → Off`).
    pub const fn next_in_cycle(self) -> Self {
        match self {
            Self::Off => Self::All,
            Self::All => Self::One,
            Self::One => Self::Off,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct PlaybackOptions {
    pub shuffle_mode: ShuffleMode,
    pub repeat_mode: RepeatMode,
}

impl PlaybackOptions {
    pub const fn with_shuffle_cycled(self) -> Self {
        Self {
            shuffle_mode: self.shuffle_mode.next_in_cycle(),
            repeat_mode: self.repeat_mode,
        }
    }

    pub const fn with_shuffle_mode(self, shuffle_mode: ShuffleMode) -> Self {
        Self {
            shuffle_mode,
            repeat_mode: self.repeat_mode,
        }
    }

    pub const fn with_repeat_cycled(self) -> Self {
        Self {
            shuffle_mode: self.shuffle_mode,
            repeat_mode: self.repeat_mode.next_in_cycle(),
        }
    }

    pub const fn repeat_enabled(self) -> bool {
        self.repeat_mode.is_enabled()
    }

    pub const fn shuffle_enabled(self) -> bool {
        self.shuffle_mode.is_enabled()
    }
}

#[cfg(test)]
mod tests {
    use super::{PlaybackOptions, RepeatMode, ShuffleMode};

    #[test]
    fn shuffle_mode_cycles_off_pure_smart_off() {
        assert_eq!(ShuffleMode::Off.next_in_cycle(), ShuffleMode::Pure);
        assert_eq!(ShuffleMode::Pure.next_in_cycle(), ShuffleMode::Smart);
        assert_eq!(ShuffleMode::Smart.next_in_cycle(), ShuffleMode::Off);
    }

    #[test]
    fn playback_options_cycle_shuffle_without_affecting_repeat() {
        let options = PlaybackOptions {
            shuffle_mode: ShuffleMode::Off,
            repeat_mode: RepeatMode::All,
        };

        assert_eq!(
            options.with_shuffle_cycled(),
            PlaybackOptions {
                shuffle_mode: ShuffleMode::Pure,
                repeat_mode: RepeatMode::All,
            }
        );
    }

    #[test]
    fn playback_options_cycle_repeat_without_affecting_shuffle() {
        let options = PlaybackOptions {
            shuffle_mode: ShuffleMode::Smart,
            repeat_mode: RepeatMode::Off,
        };

        assert_eq!(
            options.with_repeat_cycled(),
            PlaybackOptions {
                shuffle_mode: ShuffleMode::Smart,
                repeat_mode: RepeatMode::All,
            }
        );
    }

    #[test]
    fn playback_options_sets_shuffle_mode_without_affecting_repeat() {
        let options = PlaybackOptions {
            shuffle_mode: ShuffleMode::Pure,
            repeat_mode: RepeatMode::All,
        };

        assert_eq!(
            options.with_shuffle_mode(ShuffleMode::Off),
            PlaybackOptions {
                shuffle_mode: ShuffleMode::Off,
                repeat_mode: RepeatMode::All,
            }
        );
    }

    #[test]
    fn repeat_cycles_through_every_mode_in_itunes_order() {
        assert_eq!(RepeatMode::Off.next_in_cycle(), RepeatMode::All);
        assert_eq!(RepeatMode::All.next_in_cycle(), RepeatMode::One);
        assert_eq!(RepeatMode::One.next_in_cycle(), RepeatMode::Off);
    }

    #[test]
    fn shuffle_enabled_reports_off_state_correctly() {
        assert!(!ShuffleMode::Off.is_enabled());
        assert!(ShuffleMode::Pure.is_enabled());
        assert!(ShuffleMode::Smart.is_enabled());
    }
}
