// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

/// The twelve major and twelve minor keys of Western tonality. Used by
/// the analysis crate to report detected key and by the UI to display
/// and sort by it. Lives in the domain layer (not in the analysis
/// crate) so the Track model and view code can carry the value without
/// pulling in symphonia/DSP dependencies.
///
/// Discriminant order is chromatic, majors first (C..B), then minors
/// (Cm..Bm), so a key's pitch class is its index within its mode. Each
/// pitch class has exactly one variant; [`Self::short_code`] spells it
/// the way most music tools do: flats for the black-key majors
/// (Db/Eb/Gb/Ab/Bb), and for the black-key minors C#m and F#m (whose
/// flat spellings Dbm/Gbm are rare) next to Ebm, Abm and Bbm.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum MusicalKey {
    CMajor,
    DbMajor,
    DMajor,
    EbMajor,
    EMajor,
    FMajor,
    GbMajor,
    GMajor,
    AbMajor,
    AMajor,
    BbMajor,
    BMajor,
    CMinor,
    CsMinor,
    DMinor,
    EbMinor,
    EMinor,
    FMinor,
    FsMinor,
    GMinor,
    AbMinor,
    AMinor,
    BbMinor,
    BMinor,
}

impl MusicalKey {
    /// Every variant in declaration (chromatic) order. Useful for UI
    /// dropdowns, round-trip tests, and database enum mappings.
    pub const ALL: [Self; 24] = [
        Self::CMajor,
        Self::DbMajor,
        Self::DMajor,
        Self::EbMajor,
        Self::EMajor,
        Self::FMajor,
        Self::GbMajor,
        Self::GMajor,
        Self::AbMajor,
        Self::AMajor,
        Self::BbMajor,
        Self::BMajor,
        Self::CMinor,
        Self::CsMinor,
        Self::DMinor,
        Self::EbMinor,
        Self::EMinor,
        Self::FMinor,
        Self::FsMinor,
        Self::GMinor,
        Self::AbMinor,
        Self::AMinor,
        Self::BbMinor,
        Self::BMinor,
    ];

    /// Compact display form (e.g. "C", "Ebm", "F#m"). Stable: this is
    /// the canonical value persisted in shared tag frames like ID3v2
    /// `TKEY` so other tools can read it.
    pub const fn short_code(self) -> &'static str {
        match self {
            Self::CMajor => "C",
            Self::DbMajor => "Db",
            Self::DMajor => "D",
            Self::EbMajor => "Eb",
            Self::EMajor => "E",
            Self::FMajor => "F",
            Self::GbMajor => "Gb",
            Self::GMajor => "G",
            Self::AbMajor => "Ab",
            Self::AMajor => "A",
            Self::BbMajor => "Bb",
            Self::BMajor => "B",
            Self::CMinor => "Cm",
            Self::CsMinor => "C#m",
            Self::DMinor => "Dm",
            Self::EbMinor => "Ebm",
            Self::EMinor => "Em",
            Self::FMinor => "Fm",
            Self::FsMinor => "F#m",
            Self::GMinor => "Gm",
            Self::AbMinor => "Abm",
            Self::AMinor => "Am",
            Self::BbMinor => "Bbm",
            Self::BMinor => "Bm",
        }
    }

    /// Human-readable form (e.g. "C major", "F# minor"). For tooltips
    /// and verbose listings; the table view uses `short_code`.
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::CMajor => "C major",
            Self::DbMajor => "Db major",
            Self::DMajor => "D major",
            Self::EbMajor => "Eb major",
            Self::EMajor => "E major",
            Self::FMajor => "F major",
            Self::GbMajor => "Gb major",
            Self::GMajor => "G major",
            Self::AbMajor => "Ab major",
            Self::AMajor => "A major",
            Self::BbMajor => "Bb major",
            Self::BMajor => "B major",
            Self::CMinor => "C minor",
            Self::CsMinor => "C# minor",
            Self::DMinor => "D minor",
            Self::EbMinor => "Eb minor",
            Self::EMinor => "E minor",
            Self::FMinor => "F minor",
            Self::FsMinor => "F# minor",
            Self::GMinor => "G minor",
            Self::AbMinor => "Ab minor",
            Self::AMinor => "A minor",
            Self::BbMinor => "Bb minor",
            Self::BMinor => "B minor",
        }
    }

    /// True for the twelve major keys; false for the twelve minor keys.
    pub const fn is_major(self) -> bool {
        (self as u8) < 12
    }

    /// Parse a `short_code` back into a key. Exact and case-sensitive:
    /// this reads the canonical form Sustain itself stores. Text from
    /// outside (file tags, user input) goes through [`Self::parse`].
    pub fn from_short_code(code: &str) -> Option<Self> {
        Self::ALL
            .iter()
            .find(|key| key.short_code() == code)
            .copied()
    }

    /// Parse a key written in any common notation, or `None` if the text
    /// is not a key. Surrounding whitespace is ignored. Accepted:
    ///
    /// - note names: an upper-case root `A`–`G`, an optional accidental
    ///   (`#`, `♯`, `b`, `♭`), optional whitespace, then a mode — nothing,
    ///   `maj` or `major` for major; `m`, `min` or `minor` for minor (the
    ///   words in any case, the lone `m` lower-case only, since `M` is
    ///   used for major as often as for minor). Enharmonic spellings
    ///   (`G#m`, `A#`, `Cb`, …) resolve to the same key as their usual
    ///   spelling.
    /// - Camelot codes `1A`–`12A` (minor) and `1B`–`12B` (major).
    /// - Open Key codes `1m`–`12m` (minor) and `1d`–`12d` (major).
    ///
    /// Anything else — free text, a URL, a lower-case root (which some
    /// notations use for minor and others do not) — is rejected rather
    /// than guessed at.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        Self::parse_wheel_code(text).or_else(|| Self::parse_note_name(text))
    }

    fn from_pitch_class(pitch_class: u8, minor: bool) -> Self {
        Self::ALL[usize::from(pitch_class % 12) + if minor { 12 } else { 0 }]
    }

    /// Camelot (`8A`) and Open Key (`1m`) codes. Both walk the circle of
    /// fifths, so position `n` is seven semitones above position `n - 1`.
    fn parse_wheel_code(text: &str) -> Option<Self> {
        let split = text.find(|c: char| !c.is_ascii_digit())?;
        let (digits, letter) = text.split_at(split);
        if digits.is_empty() || digits.len() > 2 {
            return None;
        }
        let position: u8 = digits.parse().ok()?;
        if !(1..=12).contains(&position) {
            return None;
        }
        // Pitch class of position 1 and the mode, per notation and letter.
        let (first, minor) = match letter {
            "A" | "a" => (8, true),   // Camelot 1A = Abm
            "B" | "b" => (11, false), // Camelot 1B = B
            "m" | "M" => (9, true),   // Open Key 1m = Am
            "d" | "D" => (0, false),  // Open Key 1d = C
            _ => return None,
        };
        Some(Self::from_pitch_class(first + 7 * (position - 1), minor))
    }

    fn parse_note_name(text: &str) -> Option<Self> {
        let mut chars = text.chars();
        let natural = match chars.next()? {
            'C' => 0,
            'D' => 2,
            'E' => 4,
            'F' => 5,
            'G' => 7,
            'A' => 9,
            'B' => 11,
            _ => return None,
        };
        let rest = chars.as_str();
        let (pitch_class, rest) = if let Some(rest) = rest
            .strip_prefix('#')
            .or_else(|| rest.strip_prefix('\u{266F}'))
        {
            (natural + 1, rest)
        } else if let Some(rest) = rest
            .strip_prefix('b')
            .or_else(|| rest.strip_prefix('\u{266D}'))
        {
            (natural + 11, rest)
        } else {
            (natural, rest)
        };
        let mode = rest.trim_start();
        let minor = if mode.is_empty()
            || mode.eq_ignore_ascii_case("maj")
            || mode.eq_ignore_ascii_case("major")
        {
            false
        } else if mode == "m"
            || mode.eq_ignore_ascii_case("min")
            || mode.eq_ignore_ascii_case("minor")
        {
            true
        } else {
            return None;
        };
        Some(Self::from_pitch_class(pitch_class, minor))
    }
}

/// Writes the [`MusicalKey::short_code`].
impl std::fmt::Display for MusicalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.short_code())
    }
}

#[cfg(test)]
mod tests {
    use super::MusicalKey;

    #[test]
    fn all_keys_round_trip_through_short_code() {
        for key in MusicalKey::ALL {
            assert_eq!(MusicalKey::from_short_code(key.short_code()), Some(key));
        }
    }

    #[test]
    fn first_twelve_keys_are_major() {
        for key in MusicalKey::ALL.iter().take(12) {
            assert!(key.is_major(), "{} should be major", key.display_name());
        }
        for key in MusicalKey::ALL.iter().skip(12) {
            assert!(!key.is_major(), "{} should be minor", key.display_name());
        }
    }

    #[test]
    fn short_codes_distinguish_majors_and_minors() {
        // The minor suffix `m` is what separates "C" from "Cm" at a glance;
        // also covers the sharp/flat black-key spellings.
        assert_eq!(MusicalKey::CMajor.short_code(), "C");
        assert_eq!(MusicalKey::CMinor.short_code(), "Cm");
        assert_eq!(MusicalKey::DbMajor.short_code(), "Db");
        assert_eq!(MusicalKey::CsMinor.short_code(), "C#m");
        assert_eq!(MusicalKey::EbMinor.short_code(), "Ebm");
        assert_eq!(MusicalKey::FsMinor.short_code(), "F#m");
    }

    #[test]
    fn from_short_code_rejects_unknown_input() {
        assert_eq!(MusicalKey::from_short_code(""), None);
        assert_eq!(MusicalKey::from_short_code("H"), None);
        assert_eq!(MusicalKey::from_short_code("c"), None); // case-sensitive
    }

    #[test]
    fn parse_accepts_every_canonical_short_code() {
        for key in MusicalKey::ALL {
            assert_eq!(MusicalKey::parse(key.short_code()), Some(key));
        }
    }

    #[test]
    fn parse_reads_note_names_in_common_spellings() {
        use MusicalKey::*;
        for (text, key) in [
            ("Am", AMinor),
            ("Amin", AMinor),
            ("A min", AMinor),
            ("A minor", AMinor),
            ("A MINOR", AMinor),
            ("A Minor", AMinor),
            ("  Am\t", AMinor),
            ("A", AMajor),
            ("Amaj", AMajor),
            ("A major", AMajor),
            ("A Maj", AMajor),
            ("G#m", AbMinor),
            ("G♯m", AbMinor),
            ("Abm", AbMinor),
            ("A♭m", AbMinor),
            ("A#min", BbMinor),
            ("D#min", EbMinor),
            ("Gbmin", FsMinor),
            ("Dbmin", CsMinor),
            ("C#maj", DbMajor),
            ("D#", EbMajor),
            ("F#", GbMajor),
            ("Cb", BMajor),
            ("B#", CMajor),
            ("E#m", FMinor),
            ("Fb", EMajor),
        ] {
            assert_eq!(MusicalKey::parse(text), Some(key), "{text:?}");
        }
    }

    #[test]
    fn parse_reads_camelot_and_open_key_codes() {
        use MusicalKey::*;
        let camelot_minor = [
            AbMinor, EbMinor, BbMinor, FMinor, CMinor, GMinor, DMinor, AMinor, EMinor, BMinor,
            FsMinor, CsMinor,
        ];
        let camelot_major = [
            BMajor, GbMajor, DbMajor, AbMajor, EbMajor, BbMajor, FMajor, CMajor, GMajor, DMajor,
            AMajor, EMajor,
        ];
        let open_key_minor = [
            AMinor, EMinor, BMinor, FsMinor, CsMinor, AbMinor, EbMinor, BbMinor, FMinor, CMinor,
            GMinor, DMinor,
        ];
        let open_key_major = [
            CMajor, GMajor, DMajor, AMajor, EMajor, BMajor, GbMajor, DbMajor, AbMajor, EbMajor,
            BbMajor, FMajor,
        ];
        for position in 1..=12 {
            let index = position - 1;
            assert_eq!(
                MusicalKey::parse(&format!("{position}A")),
                Some(camelot_minor[index])
            );
            assert_eq!(
                MusicalKey::parse(&format!("{position}B")),
                Some(camelot_major[index])
            );
            assert_eq!(
                MusicalKey::parse(&format!("{position}m")),
                Some(open_key_minor[index])
            );
            assert_eq!(
                MusicalKey::parse(&format!("{position}d")),
                Some(open_key_major[index])
            );
        }
        assert_eq!(MusicalKey::parse("08A"), Some(AMinor));
        assert_eq!(MusicalKey::parse("8a"), Some(AMinor));
        assert_eq!(MusicalKey::parse(" 12B "), Some(EMajor));
    }

    #[test]
    fn parse_rejects_text_that_is_not_a_key() {
        for text in [
            "",
            "   ",
            "www.RnB4U.in",
            "H",
            "am",
            "a",
            "CM",
            "C M",
            "Cmm",
            "C##",
            "Cbb",
            "C#b",
            "C minorish",
            "C-minor",
            "0A",
            "13A",
            "123A",
            "8C",
            "8",
            "A8",
            "8A8",
            "Am/8A",
            "o",
        ] {
            assert_eq!(MusicalKey::parse(text), None, "{text:?}");
        }
    }
}
