// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Pioneer ANLZ path-hash addressing.
//!
//! Pioneer CDJ/XDJ hardware ignores the `analyze_path` recorded in the
//! PDB and instead recomputes the analysis directory from the audio
//! file's on-drive path. If the `.DAT`/`.EXT` files are not at the
//! exact path the hardware derives, waveforms never display. The
//! algorithm was reverse-engineered from rekordbox's
//! `CreateAnlzFileFolderPath` (documented in the reference project's
//! `PIONEER.md`): the path is hashed as UTF-16 code units with a custom
//! rolling hash, reduced modulo a prime, and a 7-bit "P" value is then
//! extracted from scattered bits of the result.
//!
//! Layout: `/PIONEER/USBANLZ/P{XXX}/{HHHHHHHH}/ANLZ0000.{DAT,EXT}`
//! where `XXX` is the P value (3 hex digits) and `HHHHHHHH` is the full
//! hash (8 hex digits).

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
/// root: e.g. `/PIONEER/USBANLZ/P051/0001D603`. The `.DAT`/`.EXT`
/// files live directly inside as `ANLZ0000.DAT` / `ANLZ0000.EXT`.
pub fn anlz_dir(audio_path: &str) -> String {
    let (p_value, hash) = path_hash(audio_path);
    format!("/PIONEER/USBANLZ/P{p_value:03X}/{hash:08X}")
}

/// Relative path of a track's analysis file with the given uppercase
/// extension (`DAT` or `EXT`), e.g.
/// `/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.EXT`. This is also the
/// value stored in the PDB `analyze_path` string.
pub fn anlz_file(audio_path: &str, extension: &str) -> String {
    format!("{}/ANLZ0000.{extension}", anlz_dir(audio_path))
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
        assert_eq!(
            anlz_dir("/Contents/ARTISTTEST1/ALBUMTEST1/TITLETEST1.mp3"),
            "/PIONEER/USBANLZ/P051/0001D603"
        );
        assert_eq!(
            anlz_file("/Contents/ARTISTTEST1/ALBUMTEST1/TITLETEST1.mp3", "EXT"),
            "/PIONEER/USBANLZ/P051/0001D603/ANLZ0000.EXT"
        );
    }

    #[test]
    fn supplementary_plane_character_is_hashed_as_a_surrogate_pair() {
        let path = "/Contents/Artist/Album/01 🧪.mp3";
        assert_eq!(path_hash(path), (0x02D, 0x0002_6CD1));
        assert_eq!(anlz_dir(path), "/PIONEER/USBANLZ/P02D/00026CD1");
    }
}
