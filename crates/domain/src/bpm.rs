// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{fmt, str::FromStr};

/// A tempo in beats per minute, held to the hundredth of a beat — the
/// precision Pioneer players and rekordbox carry (`tempo × 100`). A tempo
/// is always positive: "no tempo" is the absence of a `Bpm`, never zero.
///
/// The hundredth matters for beat-matched playback: a track at 118.86 BPM
/// rounded to 119 drifts by more than half a beat over five minutes.
/// File formats are less precise: ID3 `TBPM` and MP4 `tmpo` hold whole
/// beats, so tags carry [`Self::whole`]; only Vorbis comments keep the
/// fraction.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct Bpm {
    hundredths: u32,
}

impl Bpm {
    /// Fastest tempo a track can carry: 655 BPM, the most a Pioneer
    /// `tempo × 100` field (16 bits) holds.
    pub const MAX_WHOLE: u32 = 655;
    const MAX_HUNDREDTHS: u32 = Self::MAX_WHOLE * 100;

    /// A tempo from hundredths of a beat per minute (`11886` = 118.86).
    pub const fn from_hundredths(hundredths: u32) -> Option<Self> {
        if hundredths == 0 || hundredths > Self::MAX_HUNDREDTHS {
            None
        } else {
            Some(Self { hundredths })
        }
    }

    /// A tempo of exactly `whole` beats per minute.
    pub const fn from_whole(whole: u32) -> Option<Self> {
        if whole == 0 || whole > Self::MAX_WHOLE {
            None
        } else {
            Some(Self {
                hundredths: whole * 100,
            })
        }
    }

    /// A measured tempo, rounded to the hundredth. `None` for values that
    /// are not a tempo: non-finite, or outside (0, [`Self::MAX_WHOLE`]]
    /// once rounded.
    pub fn from_f64(value: f64) -> Option<Self> {
        if !value.is_finite() || value <= 0.0 {
            return None;
        }
        let hundredths = (value * 100.0).round();
        if hundredths > f64::from(Self::MAX_HUNDREDTHS) {
            return None;
        }
        Self::from_hundredths(hundredths as u32)
    }

    /// Parse a written tempo: a whole number (`128`) or a decimal with a
    /// `.` separator (`118.86`, `128.00`), surrounding whitespace ignored.
    /// Digits past the hundredth are rounded. Anything else — signs,
    /// exponents, other separators, text — is not a tempo.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
        let all_digits = |part: &str| part.bytes().all(|byte| byte.is_ascii_digit());
        if whole.is_empty() || !all_digits(whole) || !all_digits(fraction) {
            return None;
        }
        if text.ends_with('.') {
            return None;
        }
        Self::from_f64(text.parse().ok()?)
    }

    pub const fn hundredths(self) -> u32 {
        self.hundredths
    }

    pub fn as_f64(self) -> f64 {
        f64::from(self.hundredths) / 100.0
    }

    /// The tempo to the nearest whole beat (halves round up), as file tags
    /// that hold whole beats store it. At least 1: a tempo never rounds
    /// to "no tempo".
    pub const fn whole(self) -> u32 {
        let rounded = (self.hundredths + 50) / 100;
        if rounded == 0 { 1 } else { rounded }
    }
}

/// Whole tempos print without a fraction (`128`); others print to the
/// hundredth without trailing zeros (`118.86`, `124.5`).
impl fmt::Display for Bpm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let whole = self.hundredths / 100;
        let fraction = self.hundredths % 100;
        if fraction == 0 {
            write!(f, "{whole}")
        } else if fraction.is_multiple_of(10) {
            write!(f, "{whole}.{}", fraction / 10)
        } else {
            write!(f, "{whole}.{fraction:02}")
        }
    }
}

/// [`Bpm::parse`] as a `FromStr`, for generic numeric entry fields.
impl FromStr for Bpm {
    type Err = InvalidBpm;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text).ok_or(InvalidBpm)
    }
}

/// Text that is not a tempo Sustain can carry.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidBpm;

impl fmt::Display for InvalidBpm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "BPM must be a number above 0 and at most {}",
            Bpm::MAX_WHOLE
        )
    }
}

impl std::error::Error for InvalidBpm {}

#[cfg(test)]
mod tests {
    use super::Bpm;

    fn bpm(hundredths: u32) -> Bpm {
        Bpm::from_hundredths(hundredths).expect("valid tempo")
    }

    #[test]
    fn a_tempo_is_positive_and_bounded() {
        assert_eq!(Bpm::from_hundredths(0), None);
        assert_eq!(Bpm::from_whole(0), None);
        assert_eq!(Bpm::from_hundredths(65_500).map(Bpm::whole), Some(655));
        assert_eq!(Bpm::from_hundredths(65_501), None);
        assert_eq!(Bpm::from_whole(656), None);
        assert_eq!(Bpm::from_whole(128), Some(bpm(12_800)));
    }

    #[test]
    fn measured_tempos_round_to_the_hundredth() {
        assert_eq!(Bpm::from_f64(118.86), Some(bpm(11_886)));
        assert_eq!(Bpm::from_f64(127.996), Some(bpm(12_800)));
        assert_eq!(Bpm::from_f64(0.004), None);
        assert_eq!(Bpm::from_f64(-120.0), None);
        assert_eq!(Bpm::from_f64(f64::NAN), None);
        assert_eq!(Bpm::from_f64(f64::INFINITY), None);
        assert_eq!(Bpm::from_f64(655.004), Some(bpm(65_500)));
        assert_eq!(Bpm::from_f64(655.006), None);
    }

    #[test]
    fn parse_reads_whole_and_decimal_tempos_only() {
        assert_eq!(Bpm::parse("128"), Some(bpm(12_800)));
        assert_eq!(Bpm::parse(" 118.86 "), Some(bpm(11_886)));
        assert_eq!(Bpm::parse("128.00"), Some(bpm(12_800)));
        assert_eq!(Bpm::parse("124.5"), Some(bpm(12_450)));
        assert_eq!(Bpm::parse("99.999"), Some(bpm(10_000)));
        for text in [
            "", " ", "0", "0.00", "656", "-120", "+120", "1e2", "120,5", ".5", "120.", "12 8",
            "abc", "120bpm",
        ] {
            assert_eq!(Bpm::parse(text), None, "{text:?}");
        }
    }

    #[test]
    fn display_omits_needless_fraction_digits() {
        assert_eq!(bpm(12_800).to_string(), "128");
        assert_eq!(bpm(12_450).to_string(), "124.5");
        assert_eq!(bpm(11_886).to_string(), "118.86");
        assert_eq!(bpm(12_805).to_string(), "128.05");
        for hundredths in [1, 99, 12_800, 11_886, 12_450, 65_500] {
            let tempo = bpm(hundredths);
            assert_eq!(Bpm::parse(&tempo.to_string()), Some(tempo));
        }
    }

    #[test]
    fn whole_rounds_halves_up_and_never_to_zero() {
        assert_eq!(bpm(11_886).whole(), 119);
        assert_eq!(bpm(12_449).whole(), 124);
        assert_eq!(bpm(12_450).whole(), 125);
        assert_eq!(bpm(49).whole(), 1);
    }
}
