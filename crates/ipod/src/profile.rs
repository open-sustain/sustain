// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Which iPod a device is, and what its firmware needs.
//!
//! An iPod names its model in `iPod_Control/Device/SysInfo`
//! (`ModelNumStr: xB562`). The model number gives the family, and the
//! family gives what the catalogue must carry. Only the families Sustain
//! supports are in the table: an iPod shuffle, a nano 5G or later, or a
//! touch has no entry and is reported as unsupported.
//!
//! The model numbers and families are libgpod's (`itdb_device.c`); see
//! `PROVENANCE.md`.

use crate::hash58::FirewireGuid;

/// A family of iPods that share one firmware lineage.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Family {
    /// iPod (scroll wheel), 2001.
    First,
    /// iPod (touch wheel), 2002.
    Second,
    /// iPod (dock connector), 2003.
    Third,
    /// iPod (click wheel), 2004.
    Fourth,
    /// iPod photo / iPod with color display.
    Photo,
    Mini1,
    Mini2,
    Nano1,
    Nano2,
    /// iPod with video (5th generation).
    Video1,
    /// iPod with video, late 2006 ("5.5th generation").
    Video2,
    /// iPod classic 80 and 160 GB, 2007.
    Classic1,
    /// iPod classic 120 GB, 2008.
    Classic2,
    /// iPod classic 160 GB, late 2009.
    Classic3,
    /// iPod nano with video.
    Nano3,
    Nano4,
}

impl Family {
    /// The family of the model number a `SysInfo` reports, if Sustain
    /// supports it. The firmware prefixes the number with a region letter
    /// (`x`, `M`, `P`, …) that libgpod's table leaves out, so a leading
    /// letter is skipped, as libgpod skips it.
    pub fn from_model_number(model_number: &str) -> Option<Self> {
        let model_number = model_number.trim();
        let number = match model_number.as_bytes().first() {
            Some(first) if first.is_ascii_alphabetic() => &model_number[1..],
            _ => model_number,
        };
        MODEL_NUMBERS
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(number))
            .map(|&(_, family)| family)
    }

    /// What the firmware of this family needs.
    pub const fn profile(self) -> Profile {
        let checksum = match self {
            Self::Classic1 | Self::Classic2 | Self::Classic3 | Self::Nano3 | Self::Nano4 => {
                Checksum::Hash58
            }
            Self::First
            | Self::Second
            | Self::Third
            | Self::Fourth
            | Self::Photo
            | Self::Mini1
            | Self::Mini2
            | Self::Nano1
            | Self::Nano2
            | Self::Video1
            | Self::Video2 => Checksum::None,
        };
        let plays_video = matches!(
            self,
            Self::Video1
                | Self::Video2
                | Self::Classic1
                | Self::Classic2
                | Self::Classic3
                | Self::Nano3
                | Self::Nano4
        );
        // Families from the nano 3G on let tracks share one artwork
        // image; older firmwares look a cover up by the track that owns it.
        let sparse_artwork = matches!(
            self,
            Self::Classic1 | Self::Classic2 | Self::Classic3 | Self::Nano3 | Self::Nano4
        );
        let cover_formats: &'static [CoverFormat] = match self {
            Self::Photo => &PHOTO_COVERS,
            Self::Video1 | Self::Video2 => &VIDEO_COVERS,
            Self::Nano1 | Self::Nano2 => &NANO_COVERS,
            Self::Classic1 | Self::Classic2 | Self::Classic3 | Self::Nano3 => &CLASSIC_COVERS,
            Self::Nano4 => &NANO4_COVERS,
            Self::First | Self::Second | Self::Third | Self::Fourth | Self::Mini1 | Self::Mini2 => {
                &[]
            }
        };
        Profile {
            family: self,
            checksum,
            plays_video,
            cover_formats,
            sparse_artwork,
            validated: false,
        }
    }
}

/// One size in which the firmware shows album covers. Each cover is stored
/// once per format, as little-endian RGB565 rows of `row_bytes`, in the
/// `.ithmb` files named after the format's id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CoverFormat {
    pub id: u32,
    pub width: u16,
    pub height: u16,
    /// At least `width × 2`; a row is padded to it.
    pub row_bytes: u16,
}

impl CoverFormat {
    const fn tight(id: u32, edge: u16) -> Self {
        Self {
            id,
            width: edge,
            height: edge,
            row_bytes: edge * 2,
        }
    }

    /// The bytes one cover takes in this format.
    pub const fn image_bytes(self) -> u32 {
        self.row_bytes as u32 * self.height as u32
    }
}

/// iPod photo: libgpod's table, matched by the device's own capability list.
const PHOTO_COVERS: [CoverFormat; 2] =
    [CoverFormat::tight(1017, 56), CoverFormat::tight(1016, 140)];
/// iPod with video, confirmed in its firmware.
const VIDEO_COVERS: [CoverFormat; 2] =
    [CoverFormat::tight(1028, 100), CoverFormat::tight(1029, 200)];
const NANO_COVERS: [CoverFormat; 2] = [CoverFormat::tight(1031, 42), CoverFormat::tight(1027, 100)];
/// iPod classic and nano 3G: the three formats an iTunes-written classic
/// catalogue uses. The firmware's F1061 is 55 pixels wide in 112-byte rows
/// (libgpod's 56 × 56 is a guess its own comment flags); F1068, which
/// libgpod also writes, belongs to another media association.
const CLASSIC_COVERS: [CoverFormat; 3] = [
    CoverFormat {
        id: 1061,
        width: 55,
        height: 55,
        row_bytes: 112,
    },
    CoverFormat::tight(1055, 128),
    CoverFormat::tight(1060, 320),
];
/// iPod nano 4G: libgpod's table, which matches the device's own list.
const NANO4_COVERS: [CoverFormat; 6] = [
    CoverFormat::tight(1055, 128),
    CoverFormat::tight(1068, 128),
    CoverFormat::tight(1071, 240),
    CoverFormat::tight(1074, 50),
    CoverFormat::tight(1078, 80),
    CoverFormat::tight(1084, 240),
];

/// How a family's catalogue must be signed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Checksum {
    /// Unsigned.
    None,
    /// [`crate::hash58`], keyed by the device's FireWire GUID.
    Hash58,
}

/// What the catalogue must carry for one family.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Profile {
    pub family: Family,
    pub checksum: Checksum,
    /// Whether the firmware has video menus. Such a firmware files a track
    /// as music only when its media type says so; older firmwares predate
    /// the field and are given zero, as libgpod does.
    pub plays_video: bool,
    /// The sizes the firmware shows covers in; empty when it shows none.
    pub cover_formats: &'static [CoverFormat],
    /// Whether tracks may share one artwork image (`mhii`), linked from
    /// the track record; otherwise each track with a cover has its own.
    pub sparse_artwork: bool,
    /// Whether a library Sustain wrote has been checked on a real device of
    /// this family (`docs/ipod-sync-design.md`, §10).
    pub validated: bool,
}

impl Profile {
    /// The edge, in pixels, a cover must have for every format to be
    /// rendered from it without enlarging it; `None` when the firmware shows
    /// no covers.
    pub fn cover_edge(self) -> Option<u32> {
        self.cover_formats
            .iter()
            .map(|format| u32::from(format.width.max(format.height)))
            .max()
    }

    /// The bytes one distinct cover takes on the device, all formats
    /// together.
    pub fn bytes_per_cover(self) -> u64 {
        self.cover_formats
            .iter()
            .map(|format| u64::from(format.image_bytes()))
            .sum()
    }
}

/// What an iPod's `iPod_Control/Device/SysInfo` says about it. The file is
/// lines of `Key: value`; only the two keys Sustain needs are kept.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct SysInfo {
    pub model_number: Option<String>,
    /// Present only when some tool wrote it there: iTunes does not, and a
    /// stock iPod classic's `SysInfo` usually lacks it.
    pub firewire_guid: Option<FirewireGuid>,
}

impl SysInfo {
    pub fn parse(text: &str) -> Self {
        let mut info = Self::default();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "ModelNumStr" if !value.is_empty() => {
                    info.model_number = Some(value.to_owned());
                }
                "FirewireGuid" => info.firewire_guid = FirewireGuid::parse(value),
                _ => {}
            }
        }
        info
    }
}

/// Model numbers of the supported iPods, from libgpod's table.
const MODEL_NUMBERS: &[(&str, Family)] = &[
    ("8513", Family::First),
    ("8541", Family::First),
    ("8697", Family::First),
    ("8709", Family::First),
    ("8737", Family::Second),
    ("8740", Family::Second),
    ("8738", Family::Second),
    ("8741", Family::Second),
    ("8976", Family::Third),
    ("8946", Family::Third),
    ("9460", Family::Third),
    ("9244", Family::Third),
    ("8948", Family::Third),
    ("9245", Family::Third),
    ("9282", Family::Fourth),
    ("9787", Family::Fourth),
    ("9268", Family::Fourth),
    ("E436", Family::Fourth),
    ("9160", Family::Mini1),
    ("9436", Family::Mini1),
    ("9435", Family::Mini1),
    ("9434", Family::Mini1),
    ("9437", Family::Mini1),
    ("9800", Family::Mini2),
    ("9802", Family::Mini2),
    ("9804", Family::Mini2),
    ("9806", Family::Mini2),
    ("9801", Family::Mini2),
    ("9803", Family::Mini2),
    ("9805", Family::Mini2),
    ("9807", Family::Mini2),
    ("A079", Family::Photo),
    ("A127", Family::Photo),
    ("9829", Family::Photo),
    ("9585", Family::Photo),
    ("9830", Family::Photo),
    ("9586", Family::Photo),
    ("S492", Family::Photo),
    ("A350", Family::Nano1),
    ("A352", Family::Nano1),
    ("A004", Family::Nano1),
    ("A099", Family::Nano1),
    ("A005", Family::Nano1),
    ("A107", Family::Nano1),
    ("A002", Family::Video1),
    ("A146", Family::Video1),
    ("A003", Family::Video1),
    ("A147", Family::Video1),
    ("A452", Family::Video1),
    ("A444", Family::Video2),
    ("A446", Family::Video2),
    ("A664", Family::Video2),
    ("A448", Family::Video2),
    ("A450", Family::Video2),
    ("A477", Family::Nano2),
    ("A426", Family::Nano2),
    ("A428", Family::Nano2),
    ("A487", Family::Nano2),
    ("A489", Family::Nano2),
    ("A725", Family::Nano2),
    ("A726", Family::Nano2),
    ("A497", Family::Nano2),
    ("B029", Family::Classic1),
    ("B147", Family::Classic1),
    ("B145", Family::Classic1),
    ("B150", Family::Classic1),
    ("B562", Family::Classic2),
    ("B565", Family::Classic2),
    ("C293", Family::Classic3),
    ("C297", Family::Classic3),
    ("A978", Family::Nano3),
    ("A980", Family::Nano3),
    ("B261", Family::Nano3),
    ("B249", Family::Nano3),
    ("B253", Family::Nano3),
    ("B257", Family::Nano3),
    ("B480", Family::Nano4),
    ("B651", Family::Nano4),
    ("B654", Family::Nano4),
    ("B657", Family::Nano4),
    ("B660", Family::Nano4),
    ("B663", Family::Nano4),
    ("B666", Family::Nano4),
    ("B598", Family::Nano4),
    ("B732", Family::Nano4),
    ("B735", Family::Nano4),
    ("B739", Family::Nano4),
    ("B742", Family::Nano4),
    ("B745", Family::Nano4),
    ("B748", Family::Nano4),
    ("B751", Family::Nano4),
    ("B754", Family::Nano4),
    ("B903", Family::Nano4),
    ("B905", Family::Nano4),
    ("B907", Family::Nano4),
    ("B909", Family::Nano4),
    ("B911", Family::Nano4),
    ("B913", Family::Nano4),
    ("B915", Family::Nano4),
    ("B917", Family::Nano4),
    ("B918", Family::Nano4),
];

#[cfg(test)]
mod tests {
    use super::{Checksum, Family, FirewireGuid, SysInfo};

    #[test]
    fn the_testers_classic_is_a_signed_video_capable_family() {
        let family = Family::from_model_number("xB562").expect("supported");
        assert_eq!(family, Family::Classic2);
        let profile = family.profile();
        assert_eq!(profile.checksum, Checksum::Hash58);
        assert!(profile.plays_video);
    }

    #[test]
    fn the_region_letter_is_skipped_and_case_is_ignored() {
        assert_eq!(Family::from_model_number("Mb562 "), Some(Family::Classic2));
        assert_eq!(Family::from_model_number("PA350"), Some(Family::Nano1));
    }

    #[test]
    fn unsupported_and_unknown_models_have_no_family() {
        // A shuffle, a nano 5G, a touch-era number, nothing at all.
        for number in ["xA546", "xC027", "xZZZZ", "", "x"] {
            assert_eq!(Family::from_model_number(number), None, "{number}");
        }
    }

    #[test]
    fn the_classic_shows_covers_in_three_sizes_with_a_padded_smallest() {
        let profile = Family::Classic2.profile();
        let ids: Vec<u32> = profile
            .cover_formats
            .iter()
            .map(|format| format.id)
            .collect();
        assert_eq!(ids, [1061, 1055, 1060]);
        assert_eq!(profile.cover_formats[0].image_bytes(), 6_160);
        assert_eq!(profile.bytes_per_cover(), 6_160 + 32_768 + 204_800);
        assert_eq!(profile.cover_edge(), Some(320));
        assert!(profile.sparse_artwork);
    }

    #[test]
    fn monochrome_families_show_no_covers() {
        for family in [Family::Fourth, Family::Mini2] {
            let profile = family.profile();
            assert!(profile.cover_formats.is_empty());
            assert_eq!(profile.cover_edge(), None);
            assert_eq!(profile.bytes_per_cover(), 0);
        }
        assert!(!Family::Video2.profile().sparse_artwork);
    }

    #[test]
    fn every_cover_format_holds_its_rows() {
        for family in [
            Family::Photo,
            Family::Video1,
            Family::Nano1,
            Family::Classic1,
            Family::Nano4,
        ] {
            for format in family.profile().cover_formats {
                assert!(format.row_bytes >= format.width * 2, "{format:?}");
                assert_eq!(format.row_bytes % 4, 0, "{format:?}");
            }
        }
    }

    #[test]
    fn older_families_are_unsigned_and_have_no_media_type() {
        let profile = Family::Mini2.profile();
        assert_eq!(profile.checksum, Checksum::None);
        assert!(!profile.plays_video);
    }

    #[test]
    fn sysinfo_keeps_the_model_and_guid() {
        let info = SysInfo::parse(
            "BoardHwName: iPod Q98\r\nModelNumStr: xB562\r\nFirewireGuid: 0x000A27001C0FFEE5\r\n",
        );
        assert_eq!(info.model_number.as_deref(), Some("xB562"));
        assert_eq!(info.firewire_guid, FirewireGuid::parse("000A27001C0FFEE5"));
    }

    #[test]
    fn sysinfo_without_a_guid_or_with_a_malformed_one_has_none() {
        assert_eq!(SysInfo::parse("ModelNumStr: xB562\n").firewire_guid, None);
        assert_eq!(SysInfo::parse("FirewireGuid: 0x12\n").firewire_guid, None);
        assert_eq!(SysInfo::parse("garbage\n\n:\n"), SysInfo::default());
    }
}
