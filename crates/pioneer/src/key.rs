// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Mapping from Sustain's [`MusicalKey`] to Pioneer's key-table IDs.
//!
//! Rekordbox's `keys` table numbers the 24 keys chromatically from A:
//! minor keys take IDs 1–12 (Am, Bbm, Bm, …), major keys 13–24 (A, Bb,
//! B, …). The PDB writes a fixed `keys` table with exactly these
//! rows, and each track row references its key by this ID.

use sustain_domain::MusicalKey;

/// Rekordbox key-table ID (1–24) for a Sustain key.
pub fn rekordbox_id(key: MusicalKey) -> u32 {
    match key {
        // Minor keys, chromatic from A (IDs 1–12).
        MusicalKey::AMinor => 1,
        MusicalKey::BbMinor => 2,
        MusicalKey::BMinor => 3,
        MusicalKey::CMinor => 4,
        MusicalKey::CsMinor => 5,
        MusicalKey::DMinor => 6,
        MusicalKey::EbMinor => 7,
        MusicalKey::EMinor => 8,
        MusicalKey::FMinor => 9,
        MusicalKey::FsMinor => 10,
        MusicalKey::GMinor => 11,
        MusicalKey::AbMinor => 12,
        // Major keys, chromatic from A (IDs 13–24).
        MusicalKey::AMajor => 13,
        MusicalKey::BbMajor => 14,
        MusicalKey::BMajor => 15,
        MusicalKey::CMajor => 16,
        MusicalKey::DbMajor => 17,
        MusicalKey::DMajor => 18,
        MusicalKey::EbMajor => 19,
        MusicalKey::EMajor => 20,
        MusicalKey::FMajor => 21,
        MusicalKey::GbMajor => 22,
        MusicalKey::GMajor => 23,
        MusicalKey::AbMajor => 24,
    }
}

/// The 24 `(id, name)` rows of the Pioneer `keys` table, in ID order.
/// Names are spelled exactly as rekordbox writes them — flats for every
/// black key except F#/F#m (`Dbm`, `Ebm`, `F#m`, `Abm`, `Bbm`; `Db`, `Eb`,
/// `F#`, `Ab`, `Bb`), as read from a rekordbox 6 export. rekordbox numbers
/// its rows in first-use order, so players identify a key by this name,
/// not by its ID.
pub const KEY_TABLE: [(u32, &str); 24] = [
    (1, "Am"),
    (2, "Bbm"),
    (3, "Bm"),
    (4, "Cm"),
    (5, "Dbm"),
    (6, "Dm"),
    (7, "Ebm"),
    (8, "Em"),
    (9, "Fm"),
    (10, "F#m"),
    (11, "Gm"),
    (12, "Abm"),
    (13, "A"),
    (14, "Bb"),
    (15, "B"),
    (16, "C"),
    (17, "Db"),
    (18, "D"),
    (19, "Eb"),
    (20, "E"),
    (21, "F"),
    (22, "F#"),
    (23, "G"),
    (24, "Ab"),
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_unique_and_in_range() {
        let mut seen = [false; 25];
        for key in MusicalKey::ALL {
            let id = rekordbox_id(key);
            assert!((1..=24).contains(&id));
            assert!(!seen[id as usize], "duplicate id {id}");
            seen[id as usize] = true;
        }
    }

    #[test]
    fn key_names_are_rekordbox_spellings() {
        let names: Vec<&str> = KEY_TABLE.iter().map(|(_, name)| *name).collect();
        assert_eq!(
            names,
            [
                "Am", "Bbm", "Bm", "Cm", "Dbm", "Dm", "Ebm", "Em", "Fm", "F#m", "Gm", "Abm", "A",
                "Bb", "B", "C", "Db", "D", "Eb", "E", "F", "F#", "G", "Ab",
            ]
        );
    }

    #[test]
    fn key_table_covers_every_id() {
        for (index, (id, _)) in KEY_TABLE.iter().enumerate() {
            assert_eq!(*id as usize, index + 1);
        }
    }
}
