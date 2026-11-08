// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Pioneer ANLZ path-hash addressing.
//!
//! A track's analysis files live in a directory derived from the audio
//! file's on-drive path. The algorithm was reverse-engineered from
//! rekordbox's `CreateAnlzFileFolderPath` (documented in the reference
//! project's `PIONEER.md`): the path is hashed as UTF-16 code units with a
//! custom rolling hash, reduced modulo a prime, and a 7-bit "P" value is
//! then extracted from scattered bits of the result.
//!
//! Layout: `/PIONEER/USBANLZ/P{XXX}/{HHHHHHHH}/ANLZ{NNNN}.{DAT,EXT}`
//! where `XXX` is the P value (3 hex digits), `HHHHHHHH` the full hash
//! (8 hex digits), and `NNNN` the file's slot number in that directory.
//!
//! The hash is reduced modulo 200003, so distinct audio paths collide: on
//! a real 11,020-track rekordbox export, 321 tracks share a directory with
//! another track. Rekordbox resolves this by numbering the pair inside the
//! shared directory (`ANLZ0000`, `ANLZ0001`, `ANLZ0002`, …) and recording
//! the numbered `.DAT` path in the PDB's `analyze_path`; [`assign_slots`]
//! reproduces that, so no track ever overwrites another's analysis.

use std::collections::{BTreeSet, HashMap};

/// Compute the Pioneer `(p_value, hash)` pair for an audio file path.
///
/// `audio_path` is the path relative to the drive root, starting with a
/// leading slash (e.g. `/Contents/Artist/Album/01 Title.mp3`). The pair
/// is deterministic: the same path always yields the same directory, so
/// re-syncs address the same analysis folder. The path is hashed exactly as
/// supplied; separators, case, and Unicode are not normalized.
pub fn path_hash(audio_path: &str) -> (u16, u32) {
    let mut hash: u32 = 0;

    for code_unit in audio_path.encode_utf16() {
        let code_unit = u32::from(code_unit);
        let temp = hash.wrapping_mul(0x5BC9).wrapping_add(code_unit);
        hash = temp.wrapping_mul(0x93B5).wrapping_add(code_unit);
    }

    let hash_result = hash % 0x30D43; // modulo 200003 (prime)

    // The P value is assembled from non-contiguous bits of the hash, in
    // the exact bit order the rekordbox disassembly extracts them.
    let mut p_value: u16 = 0;
    p_value |= (hash_result & 1) as u16; // bit 0  -> bit 0
    p_value |= ((hash_result >> 1) & 2) as u16; // bit 2  -> bit 1
    p_value |= ((hash_result >> 4) & 4) as u16; // bit 6  -> bit 2
    p_value |= ((hash_result >> 4) & 8) as u16; // bit 7  -> bit 3
    p_value |= ((hash_result >> 5) & 0x10) as u16; // bit 9  -> bit 4
    p_value |= ((hash_result >> 8) & 0x20) as u16; // bit 13 -> bit 5
    p_value |= ((hash_result >> 10) & 0x40) as u16; // bit 16 -> bit 6

    (p_value, hash_result)
}

/// Directory holding a track's analysis files, relative to the drive
/// root: e.g. `/PIONEER/USBANLZ/P051/0001D603`.
pub fn anlz_dir(audio_path: &str) -> String {
    let (p_value, hash) = path_hash(audio_path);
    format!("/PIONEER/USBANLZ/P{p_value:03X}/{hash:08X}")
}

/// Highest slot number a directory can hold (`ANLZ9999`). Real collision
/// groups hold a handful of tracks; the bound only keeps the four-digit
/// file name well-formed.
const MAX_SLOT_NUMBER: u16 = 9999;

/// One track's analysis pair: its hash directory plus the slot number of
/// the `ANLZ{NNNN}.DAT` / `.EXT` files inside it.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AnlzSlot {
    dir: String,
    number: u16,
}

impl AnlzSlot {
    /// Drive-root path of the `.DAT` file, e.g.
    /// `/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.DAT`. This is also the
    /// value stored in the PDB `analyze_path` string.
    pub fn dat_path(&self) -> String {
        self.file("DAT")
    }

    /// Drive-root path of the `.EXT` file beside the `.DAT`.
    pub fn ext_path(&self) -> String {
        self.file("EXT")
    }

    fn file(&self, extension: &str) -> String {
        format!("{}/ANLZ{:04}.{extension}", self.dir, self.number)
    }

    /// Parse a drive-root `/PIONEER/USBANLZ/P{XXX}/{HHHHHHHH}/ANLZ{NNNN}.DAT`
    /// path back into its slot. Returns `None` for anything that is not
    /// exactly such a path, so a slot parsed from stored state can only
    /// ever name a file inside the analysis tree.
    pub fn from_dat_path(dat_path: &str) -> Option<Self> {
        let (dir, name) = dat_path.rsplit_once('/')?;
        let digits = name.strip_prefix("ANLZ")?.strip_suffix(".DAT")?;
        let (p_dir, hash_dir) = dir.strip_prefix("/PIONEER/USBANLZ/")?.split_once('/')?;
        let p_digits = p_dir.strip_prefix('P')?;
        let is_upper_hex = |text: &str, len: usize| {
            text.len() == len
                && text
                    .bytes()
                    .all(|b| b.is_ascii_digit() || (b'A'..=b'F').contains(&b))
        };
        if !is_upper_hex(p_digits, 3)
            || !is_upper_hex(hash_dir, 8)
            || digits.len() != 4
            || !digits.bytes().all(|b| b.is_ascii_digit())
        {
            return None;
        }
        Some(Self {
            dir: dir.to_owned(),
            number: digits.parse().ok()?,
        })
    }

    /// The slot a previous export recorded for `audio_path`, if
    /// `dat_path` is a well-formed slot inside that audio path's own hash
    /// directory. Anything else is not a slot this track can keep.
    fn recorded(audio_path: &str, dat_path: &str) -> Option<Self> {
        Self::from_dat_path(dat_path).filter(|slot| slot.dir == anlz_dir(audio_path))
    }
}

/// A hash directory had no free slot number left.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SlotsExhausted {
    pub dir: String,
}

impl std::fmt::Display for SlotsExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "no free ANLZ slot left in {}", self.dir)
    }
}

impl std::error::Error for SlotsExhausted {}

/// Assign every track a distinct analysis slot.
///
/// `tracks[i]` is a track's drive-root audio path and the `.DAT` path a
/// previous export recorded for it, if any; audio paths must be unique.
/// `reserved` lists `.DAT` paths still recorded for tracks that are not
/// being exported (their audio may remain on the drive): no track is
/// given one, so a slot only changes hands once its previous owner's
/// record is gone. The result is index-aligned with `tracks`.
///
/// Assignment is sticky: a track keeps its recorded slot whenever that
/// slot is in its own directory and not claimed by an earlier track in
/// the slice, so re-exports do not reshuffle (and needlessly rewrite) a
/// directory's files. Every other track takes the lowest free number in
/// its directory, so a collision-free track gets `ANLZ0000`, as rekordbox
/// does.
pub fn assign_slots(
    tracks: &[(&str, Option<&str>)],
    reserved: &[&str],
) -> Result<Vec<AnlzSlot>, SlotsExhausted> {
    let mut taken: HashMap<String, BTreeSet<u16>> = HashMap::new();
    let mut slots: Vec<Option<AnlzSlot>> = vec![None; tracks.len()];
    for (slot, (audio_path, recorded)) in slots.iter_mut().zip(tracks) {
        let Some(previous) = recorded.and_then(|dat| AnlzSlot::recorded(audio_path, dat)) else {
            continue;
        };
        if taken
            .entry(previous.dir.clone())
            .or_default()
            .insert(previous.number)
        {
            *slot = Some(previous);
        }
    }
    for slot in reserved
        .iter()
        .filter_map(|dat| AnlzSlot::from_dat_path(dat))
    {
        taken.entry(slot.dir).or_default().insert(slot.number);
    }
    slots
        .into_iter()
        .zip(tracks)
        .map(|(slot, (audio_path, _))| match slot {
            Some(slot) => Ok(slot),
            None => {
                let dir = anlz_dir(audio_path);
                let used = taken.entry(dir.clone()).or_default();
                let number = (0..=MAX_SLOT_NUMBER)
                    .find(|number| !used.contains(number))
                    .ok_or_else(|| SlotsExhausted { dir: dir.clone() })?;
                used.insert(number);
                Ok(AnlzSlot { dir, number })
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // Verified test cases from the reverse-engineering effort
    // (reference project PIONEER.md). These pin the algorithm against
    // values captured from real rekordbox exports.
    #[test]
    fn matches_known_vectors() {
        let cases = [
            (
                "/Contents/ARTISTTEST1/ALBUMTEST1/TITLETEST1.mp3",
                0x051,
                0x0001D603,
            ),
            (
                "/Contents/ARTISTTEST2/ALBUMTEST2/TITLETEST2.mp3",
                0x03C,
                0x0000A6CA,
            ),
            (
                "/Contents/ARTISTTEST3/ALBUMTEST3/TITLETEST3.mp3",
                0x045,
                0x0001096B,
            ),
            (
                "/Contents/BROOKLYN BOUNCE/The Theme (Of Progressive Attack)/This Is The Begining.mp3",
                0x04B,
                0x000154A5,
            ),
        ];
        for (path, p, hash) in cases {
            assert_eq!(path_hash(path), (p, hash), "path hash mismatch for {path}");
        }
    }

    #[test]
    fn dir_and_file_formatting() {
        let audio = "/Contents/ARTISTTEST1/ALBUMTEST1/TITLETEST1.mp3";
        assert_eq!(anlz_dir(audio), "/PIONEER/USBANLZ/P051/0001D603");
        let slots = assign_slots(&[(audio, None)], &[]).expect("slots");
        assert_eq!(
            slots[0].dat_path(),
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.DAT"
        );
        assert_eq!(
            slots[0].ext_path(),
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.EXT"
        );
    }

    /// Two distinct paths from a real rekordbox export that rekordbox
    /// itself stored as `ANLZ0000` and `ANLZ0001` of one directory.
    const COLLIDING: [&str; 2] = [
        "/Contents/Rihanna/Rated R/08 Rude Boy.flac",
        "/Contents/Abel Ramos/Unknown Album/Aquarius (Randy Katana Remix).mp3",
    ];

    #[test]
    fn colliding_paths_share_a_directory_but_not_a_slot() {
        assert_eq!(anlz_dir(COLLIDING[0]), anlz_dir(COLLIDING[1]));
        let slots =
            assign_slots(&[(COLLIDING[0], None), (COLLIDING[1], None)], &[]).expect("slots");
        assert!(slots[0].dat_path().ends_with("/ANLZ0000.DAT"));
        assert!(slots[1].dat_path().ends_with("/ANLZ0001.DAT"));
    }

    #[test]
    fn recorded_slots_are_kept_even_when_order_changes() {
        let first =
            assign_slots(&[(COLLIDING[0], None), (COLLIDING[1], None)], &[]).expect("slots");
        let (a, b) = (first[0].dat_path(), first[1].dat_path());
        // The selection order flips; each track keeps its own files.
        let again = assign_slots(&[(COLLIDING[1], Some(&b)), (COLLIDING[0], Some(&a))], &[])
            .expect("slots");
        assert_eq!(again[0].dat_path(), b);
        assert_eq!(again[1].dat_path(), a);
    }

    #[test]
    fn a_new_track_never_takes_a_kept_slot() {
        let first = assign_slots(&[(COLLIDING[1], None)], &[]).expect("slots");
        let kept = first[0].dat_path();
        assert!(kept.ends_with("/ANLZ0000.DAT"));
        // The newcomer is listed first but must not displace the
        // recorded owner of ANLZ0000.
        let again =
            assign_slots(&[(COLLIDING[0], None), (COLLIDING[1], Some(&kept))], &[]).expect("slots");
        assert!(again[0].dat_path().ends_with("/ANLZ0001.DAT"));
        assert_eq!(again[1].dat_path(), kept);
    }

    #[test]
    fn reserved_slots_of_tracks_not_exported_are_never_reused() {
        let first = assign_slots(&[(COLLIDING[0], None)], &[]).expect("slots");
        let reserved = first[0].dat_path();
        // COLLIDING[0] is deselected but its audio (and record) remain:
        // the colliding newcomer must not overwrite its analysis.
        let slots = assign_slots(&[(COLLIDING[1], None)], &[&reserved]).expect("slots");
        assert!(slots[0].dat_path().ends_with("/ANLZ0001.DAT"));
        // A reserved path that a present track itself owns is simply kept.
        let slots = assign_slots(&[(COLLIDING[0], Some(&reserved))], &[&reserved]).expect("slots");
        assert_eq!(slots[0].dat_path(), reserved);
    }

    #[test]
    fn only_analysis_tree_slot_paths_parse() {
        let slot = AnlzSlot::from_dat_path("/PIONEER/USBANLZ/P051/0001D603/ANLZ0001.DAT")
            .expect("well-formed slot");
        assert_eq!(
            slot.ext_path(),
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0001.EXT"
        );
        for path in [
            "/Contents/Artist/ANLZ0000.DAT",
            "/PIONEER/USBANLZ/P051/../ANLZ0000.DAT",
            "/PIONEER/USBANLZ/P051/0001d603/ANLZ0000.DAT",
            "/PIONEER/USBANLZ/P51/0001D603/ANLZ0000.DAT",
            "/PIONEER/USBANLZ/P051/0001D603/x/ANLZ0000.DAT",
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.EXT",
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ000.DAT",
            "PIONEER/USBANLZ/P051/0001D603/ANLZ0000.DAT",
        ] {
            assert_eq!(AnlzSlot::from_dat_path(path), None, "{path}");
        }
    }

    #[test]
    fn a_duplicate_or_foreign_recorded_slot_is_not_kept() {
        let first = assign_slots(&[(COLLIDING[0], None)], &[]).expect("slots");
        let claimed = first[0].dat_path();
        // Both tracks claim the same recorded slot: the first keeps it.
        let slots = assign_slots(
            &[
                (COLLIDING[0], Some(&claimed)),
                (COLLIDING[1], Some(&claimed)),
            ],
            &[],
        )
        .expect("slots");
        assert_eq!(slots[0].dat_path(), claimed);
        assert!(slots[1].dat_path().ends_with("/ANLZ0001.DAT"));

        let elsewhere = "/Contents/ARTISTTEST1/ALBUMTEST1/TITLETEST1.mp3";
        for bogus in [
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0003.DAT", // another dir
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ03.DAT",
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ+003.DAT",
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0003.EXT",
        ] {
            let slot = assign_slots(&[(COLLIDING[0], Some(bogus)), (elsewhere, None)], &[])
                .expect("slots");
            assert!(slot[0].dat_path().ends_with("/ANLZ0000.DAT"), "{bogus}");
        }
    }

    #[test]
    fn supplementary_plane_character_is_hashed_as_a_surrogate_pair() {
        let path = "/Contents/Artist/Album/01 🧪.mp3";
        assert_eq!(path_hash(path), (0x02D, 0x0002_6CD1));
        assert_eq!(anlz_dir(path), "/PIONEER/USBANLZ/P02D/00026CD1");
    }
}
