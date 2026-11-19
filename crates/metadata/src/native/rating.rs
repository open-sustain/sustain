// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! How a rating item's stored value maps to stars.
//!
//! No tag format standardises a star scale, so each application picked
//! its own. Sustain reads and writes a rating item in the scale its value
//! is already in, so updating another application's item keeps it
//! readable by that application:
//!
//! - an ID3v2 `POPM` frame holds a byte. Picard's frames
//!   (`users@musicbrainz.org`) use 51 per star; every other owner the
//!   Windows Media Player steps MusicBee also writes (1, 64, 128, 196,
//!   255). A byte of 0 is the specification's "unknown": no rating.
//! - a Vorbis `RATING` comment or MP4 `rate` atom holding a whole number
//!   is a percentage, 20 per star (MusicBee, foobar2000); one holding a
//!   decimal is a fraction of five stars (Quod Libet, Picard). Zero is no
//!   rating.
//!
//! Reading maps a value between two steps up to the next star, the way
//! lofty and the applications above read them.

use sustain_domain::Rating;

/// The owner of the `POPM` frames Picard writes.
const PICARD_OWNER: &str = "users@musicbrainz.org";

/// The owner Sustain writes a new `POPM` frame under: MusicBee's, whose
/// scale most applications read.
pub(super) const DEFAULT_POPM_OWNER: &str = "MusicBee";

/// The rating a `POPM` frame of `owner` holding `byte` stands for.
pub(super) fn popm_rating(owner: &str, byte: u8) -> Rating {
    let stars = match (owner == PICARD_OWNER, byte) {
        (_, 0) => 0,
        (true, ..=51) => 1,
        (true, ..=102) => 2,
        (true, ..=153) => 3,
        (true, ..=204) => 4,
        (false, ..=1) => 1,
        (false, ..=64) => 2,
        (false, ..=128) => 3,
        (false, ..=196) => 4,
        _ => 5,
    };
    stars_rating(stars)
}

/// The byte a `POPM` frame of `owner` holds for `rating`.
pub(super) fn popm_byte(owner: &str, rating: Rating) -> u8 {
    let stars = rating.stars();
    if owner == PICARD_OWNER {
        return stars * 51;
    }
    match stars {
        0 => 0,
        1 => 1,
        2 => 64,
        3 => 128,
        4 => 196,
        _ => 255,
    }
}

/// The form a text rating value is written in.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TextScale {
    /// A whole percentage, 20 per star.
    Percent,
    /// A fraction of five stars.
    Fraction,
}

impl TextScale {
    /// The scale a newly written text rating uses.
    pub(super) const DEFAULT: Self = Self::Percent;

    /// The scale of `text` and the rating it stands for; `None` when it
    /// is no rating Sustain can read.
    pub(super) fn parse(text: &str) -> Option<(Self, Rating)> {
        let text = text.trim();
        if text.contains('.') {
            let fraction = text.parse::<f64>().ok()?;
            if !(0.0..=1.0).contains(&fraction) {
                return None;
            }
            // Fifths of a star past a step round up to it, as integer
            // scales do; the epsilon keeps 0.6 from reading as 3.0000001.
            let stars = (fraction * 5.0 - 1e-9).ceil().max(0.0);
            return Some((Self::Fraction, stars_rating(stars as u8)));
        }
        let percent = text.parse::<u8>().ok().filter(|percent| *percent <= 100)?;
        Some((Self::Percent, stars_rating(percent.div_ceil(20))))
    }

    /// `rating` written in this scale.
    pub(super) fn text(self, rating: Rating) -> String {
        let stars = rating.stars();
        match self {
            Self::Percent => (u32::from(stars) * 20).to_string(),
            Self::Fraction => format!("{:.1}", f64::from(stars) / 5.0),
        }
    }
}

fn stars_rating(stars: u8) -> Rating {
    Rating::new(stars.min(5)).unwrap_or_else(Rating::unrated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stars(rating: Rating) -> u8 {
        rating.stars()
    }

    #[test]
    fn a_popm_byte_reads_in_its_owners_scale_and_zero_is_unrated() {
        assert_eq!(stars(popm_rating("MusicBee", 0)), 0);
        assert_eq!(stars(popm_rating("MusicBee", 1)), 1);
        assert_eq!(stars(popm_rating("MusicBee", 196)), 4);
        assert_eq!(stars(popm_rating("MusicBee", 197)), 5);
        assert_eq!(stars(popm_rating("anyone@example.org", 128)), 3);
        assert_eq!(stars(popm_rating(PICARD_OWNER, 0)), 0);
        assert_eq!(stars(popm_rating(PICARD_OWNER, 102)), 2);
        assert_eq!(stars(popm_rating(PICARD_OWNER, 204)), 4);
        assert_eq!(stars(popm_rating(PICARD_OWNER, 205)), 5);
    }

    #[test]
    fn every_rating_survives_its_owners_popm_scale() {
        for owner in ["MusicBee", "Windows Media Player 9 Series", PICARD_OWNER] {
            for value in 0..=5 {
                let rating = Rating::new(value).expect("rating");
                assert_eq!(
                    popm_rating(owner, popm_byte(owner, rating)),
                    rating,
                    "{owner}"
                );
            }
        }
    }

    #[test]
    fn a_text_rating_reads_as_a_percentage_or_a_fraction() {
        let read = |text| TextScale::parse(text).map(|(scale, rating)| (scale, stars(rating)));
        assert_eq!(read("80"), Some((TextScale::Percent, 4)));
        assert_eq!(read(" 81 "), Some((TextScale::Percent, 5)));
        assert_eq!(read("0"), Some((TextScale::Percent, 0)));
        assert_eq!(read("0.6"), Some((TextScale::Fraction, 3)));
        assert_eq!(read("0.0"), Some((TextScale::Fraction, 0)));
        assert_eq!(read("1.0"), Some((TextScale::Fraction, 5)));
        assert_eq!(read("101"), None);
        assert_eq!(read("1.5"), None);
        assert_eq!(read("MusicBee|4|0"), None);
    }

    #[test]
    fn every_rating_survives_each_text_scale() {
        for scale in [TextScale::Percent, TextScale::Fraction] {
            for value in 0..=5 {
                let rating = Rating::new(value).expect("rating");
                assert_eq!(
                    TextScale::parse(&scale.text(rating)),
                    Some((scale, rating)),
                    "{scale:?}"
                );
            }
        }
    }
}
