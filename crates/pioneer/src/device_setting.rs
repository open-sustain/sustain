// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `PIONEER/DEVSETTING.DAT`: the player display settings a drive carries.
//!
//! Pioneer players apply the display settings a drive holds when it is
//! loaded — notably the waveform colour, which defaults to BLUE, a
//! monochrome rendering, when the drive holds none. Rekordbox writes the
//! file on every export; a player rewrites it when the settings are
//! changed on the player itself.
//!
//! Layout (little-endian), as reverse-engineered by rekordcrate and
//! checked against files written by rekordbox and by an XDJ-XZ:
//!
//! - `u32` length of the string block, `0x60`;
//! - three NUL-padded 32-byte strings: brand, software, version;
//! - `u32` length of the data block, `0x20`;
//! - the data block: a 9-byte constant prefix, then one byte each for the
//!   overview waveform type, waveform colour, a constant `0x01`, key
//!   display format and waveform current position, then 18 zero bytes;
//! - `u16` CRC-16/XMODEM of the data block, then a zero `u16`.

const STRING_FIELD_LEN: usize = 0x20;
const BRAND: &str = "PIONEER DJ";
const SOFTWARE: &str = "Sustain";
const DATA_PREFIX: [u8; 9] = [0x78, 0x56, 0x34, 0x12, 0x01, 0x00, 0x00, 0x00, 0x01];
const DATA_LEN: usize = 0x20;

/// Drive-relative path of the settings file.
pub const DEVSETTING_RELATIVE_PATH: &str = "PIONEER/DEVSETTING.DAT";

const HALF_OVERVIEW_WAVEFORM: u8 = 0x01;
const RGB_WAVEFORM_COLOUR: u8 = 0x03;
const CLASSIC_KEY_DISPLAY: u8 = 0x01;
const CENTRED_CURRENT_POSITION: u8 = 0x01;

/// The settings file Sustain seeds a drive with: rekordbox's defaults,
/// except that the waveforms are shown in RGB colour, so players render
/// the colour waveforms the export carries.
pub fn colour_waveform_bytes() -> Vec<u8> {
    let mut data = Vec::with_capacity(DATA_LEN);
    data.extend_from_slice(&DATA_PREFIX);
    data.extend_from_slice(&[
        HALF_OVERVIEW_WAVEFORM,
        RGB_WAVEFORM_COLOUR,
        0x01,
        CLASSIC_KEY_DISPLAY,
        CENTRED_CURRENT_POSITION,
    ]);
    data.resize(DATA_LEN, 0);

    let mut out = Vec::with_capacity(4 + 3 * STRING_FIELD_LEN + 4 + DATA_LEN + 4);
    out.extend_from_slice(&((3 * STRING_FIELD_LEN) as u32).to_le_bytes());
    for field in [BRAND, SOFTWARE, env!("CARGO_PKG_VERSION")] {
        push_string_field(&mut out, field);
    }
    out.extend_from_slice(&(DATA_LEN as u32).to_le_bytes());
    out.extend_from_slice(&data);
    out.extend_from_slice(&crc16_xmodem(&data).to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes());
    out
}

/// A NUL-padded string field; the value always leaves room for a NUL.
fn push_string_field(out: &mut Vec<u8>, value: &str) {
    let bytes = &value.as_bytes()[..value.len().min(STRING_FIELD_LEN - 1)];
    out.extend_from_slice(bytes);
    out.resize(out.len() + STRING_FIELD_LEN - bytes.len(), 0);
}

/// CRC-16/XMODEM: polynomial `0x1021`, initial value 0, no reflection.
fn crc16_xmodem(bytes: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in bytes {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc_matches_the_xmodem_check_value() {
        assert_eq!(crc16_xmodem(b"123456789"), 0x31C3);
    }

    #[test]
    fn layout_and_checksum_match_a_rekordbox_rgb_file() {
        let bytes = colour_waveform_bytes();
        assert_eq!(bytes.len(), 140);
        assert_eq!(&bytes[..4], &0x60u32.to_le_bytes());
        assert_eq!(&bytes[4..14], b"PIONEER DJ");
        assert_eq!(&bytes[0x64..0x68], &0x20u32.to_le_bytes());
        // The data block and checksum of a DEVSETTING.DAT rekordbox 6.6.1
        // writes with the waveform colour set to RGB.
        let mut rekordbox_data = [0u8; DATA_LEN];
        rekordbox_data[..14].copy_from_slice(&[
            0x78, 0x56, 0x34, 0x12, 0x01, 0x00, 0x00, 0x00, 0x01, 0x01, 0x03, 0x01, 0x01, 0x01,
        ]);
        assert_eq!(&bytes[0x68..0x88], &rekordbox_data);
        assert_eq!(&bytes[0x88..], &[0xA6, 0x0B, 0x00, 0x00]);
    }

    #[test]
    fn string_fields_are_nul_padded() {
        let bytes = colour_waveform_bytes();
        let software = &bytes[0x24..0x44];
        assert!(software.starts_with(b"Sustain\0"));
        assert!(software[SOFTWARE.len()..].iter().all(|&b| b == 0));
        assert_eq!(bytes[0x44 + STRING_FIELD_LEN - 1], 0);
    }
}
