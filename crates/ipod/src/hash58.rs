// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! hash58: the signature the iPod Classic and the nano 3G/4G require on
//! their `iTunesDB`. A database whose signature does not match is shown as
//! an empty library.
//!
//! The signature is an HMAC-SHA1 over the whole file, keyed by the
//! device's FireWire GUID:
//!
//! 1. The GUID's eight bytes are taken in four adjacent pairs. Each pair
//!    gives the least common multiple of its two bytes — 1 when either is
//!    zero — split into a high and a low byte.
//! 2. Each pair contributes four bytes: the AES S-box of the high byte, the
//!    inverse S-box of the high byte, then the same two of the low byte.
//! 3. The HMAC key is the SHA-1 of an 18-byte seed followed by those
//!    sixteen bytes.
//! 4. The database's hashing scheme (`mhbd+0x30`) is set to 1; its database
//!    id (`+0x18..0x20`), twenty header bytes (`+0x32..0x46`) and the
//!    signature slot (`+0x58..0x6c`) are zeroed for the computation; the
//!    first two are restored and the digest stored in the third.
//!
//! The S-boxes are generated from their definition in FIPS 197 rather
//! than transcribed; the seed is the one protocol-specific constant. See
//! `PROVENANCE.md` for where the algorithm is documented.

use hmac::{Hmac, KeyInit, Mac};
use sha1::{Digest, Sha1};

/// The 18-byte seed of the key derivation.
const SEED: [u8; 18] = [
    0x67, 0x23, 0xfe, 0x30, 0x45, 0x33, 0xf8, 0x90, 0x99, 0x21, 0x07, 0xc1, 0xd0, 0x12, 0xb2, 0xa1,
    0x07, 0x81,
];

const HASHING_SCHEME: std::ops::Range<usize> = 0x30..0x32;
const DATABASE_ID: std::ops::Range<usize> = 0x18..0x20;
const HEADER_BYTES: std::ops::Range<usize> = 0x32..0x46;
const SIGNATURE: std::ops::Range<usize> = 0x58..0x6c;
/// The scheme value hash58 records in the database header.
const SCHEME_HASH58: u16 = 1;

/// The AES forward S-box (FIPS 197, §5.1.1), generated: each entry is the
/// multiplicative inverse in GF(2⁸) of its index, put through the affine
/// transformation. The loop walks the field's multiplicative group by
/// powers of the generator 3, keeping its inverse alongside.
const FORWARD_SBOX: [u8; 256] = forward_sbox();
const INVERSE_SBOX: [u8; 256] = inverse_sbox(&FORWARD_SBOX);

const fn forward_sbox() -> [u8; 256] {
    let mut sbox = [0u8; 256];
    let mut power: u8 = 1;
    let mut inverse: u8 = 1;
    loop {
        // power ← power × 3
        power ^= (power << 1) ^ if power & 0x80 != 0 { 0x1b } else { 0 };
        // inverse ← inverse ÷ 3 (multiplying by 0xf6, the inverse of 3)
        inverse ^= inverse << 1;
        inverse ^= inverse << 2;
        inverse ^= inverse << 4;
        if inverse & 0x80 != 0 {
            inverse ^= 0x09;
        }
        let affine = inverse
            ^ inverse.rotate_left(1)
            ^ inverse.rotate_left(2)
            ^ inverse.rotate_left(3)
            ^ inverse.rotate_left(4);
        sbox[power as usize] = affine ^ 0x63;
        if power == 1 {
            break;
        }
    }
    // Zero has no inverse; the affine transformation maps it to 0x63.
    sbox[0] = 0x63;
    sbox
}

const fn inverse_sbox(forward: &[u8; 256]) -> [u8; 256] {
    let mut inverse = [0u8; 256];
    let mut index = 0;
    while index < 256 {
        inverse[forward[index] as usize] = index as u8;
        index += 1;
    }
    inverse
}

/// A device's FireWire GUID: eight bytes, printed as sixteen hexadecimal
/// digits. USB iPods still carry it under that name; it is also the USB
/// serial number of the Classic and the nano 3G/4G.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FirewireGuid([u8; 8]);

impl FirewireGuid {
    /// Parse sixteen hexadecimal digits, with an optional `0x` prefix and
    /// surrounding whitespace, in printed order.
    pub fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let digits = text
            .strip_prefix("0x")
            .or_else(|| text.strip_prefix("0X"))
            .unwrap_or(text);
        if digits.len() != 16 || !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        let mut bytes = [0u8; 8];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&digits[index * 2..index * 2 + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// The HMAC key this GUID derives.
    fn key(self) -> [u8; 20] {
        let mut mixed = [0u8; 16];
        for (pair, out) in self.0.chunks_exact(2).zip(mixed.chunks_exact_mut(4)) {
            let [high, low] = least_common_multiple(pair[0], pair[1]).to_be_bytes();
            out.copy_from_slice(&[
                FORWARD_SBOX[usize::from(high)],
                INVERSE_SBOX[usize::from(high)],
                FORWARD_SBOX[usize::from(low)],
                INVERSE_SBOX[usize::from(low)],
            ]);
        }
        let mut hasher = Sha1::new();
        hasher.update(SEED);
        hasher.update(mixed);
        hasher.finalize().into()
    }
}

/// The least common multiple of two bytes, with the protocol's convention
/// that a zero byte makes it 1. Two bytes never exceed `255 × 255`.
fn least_common_multiple(a: u8, b: u8) -> u16 {
    if a == 0 || b == 0 {
        return 1;
    }
    let (a, b) = (u16::from(a), u16::from(b));
    let (mut x, mut y) = (a, b);
    while y != 0 {
        (x, y) = (y, x % y);
    }
    a / x * b
}

/// Why a database could not be signed.
#[derive(Debug, Eq, PartialEq)]
pub enum SignError {
    /// The bytes are not an `iTunesDB`: too short for its header, or not
    /// starting with `mhbd`.
    NotADatabase,
}

impl std::fmt::Display for SignError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotADatabase => write!(f, "not an iTunesDB"),
        }
    }
}

impl std::error::Error for SignError {}

/// Sign `database` in place for the device with `guid`.
pub fn sign(database: &mut [u8], guid: FirewireGuid) -> Result<(), SignError> {
    if database.len() < SIGNATURE.end || !database.starts_with(b"mhbd") {
        return Err(SignError::NotADatabase);
    }
    sign_written(database, guid);
    Ok(())
}

/// Sign a database whose `mhbd` header is known to be complete — one the
/// writer just produced.
pub(crate) fn sign_written(database: &mut [u8], guid: FirewireGuid) {
    database[HASHING_SCHEME].copy_from_slice(&SCHEME_HASH58.to_le_bytes());
    let database_id: [u8; 8] = database[DATABASE_ID]
        .try_into()
        .expect("the range is eight bytes");
    let header_bytes: [u8; 20] = database[HEADER_BYTES]
        .try_into()
        .expect("the range is twenty bytes");
    database[DATABASE_ID].fill(0);
    database[HEADER_BYTES].fill(0);
    database[SIGNATURE].fill(0);
    let mut mac =
        Hmac::<Sha1>::new_from_slice(&guid.key()).expect("HMAC accepts a key of any length");
    mac.update(database);
    let digest = mac.finalize().into_bytes();
    database[DATABASE_ID].copy_from_slice(&database_id);
    database[HEADER_BYTES].copy_from_slice(&header_bytes);
    database[SIGNATURE].copy_from_slice(&digest);
}

#[cfg(test)]
mod tests {
    use super::{FORWARD_SBOX, FirewireGuid, INVERSE_SBOX, SIGNATURE, SignError, sign};

    /// An empty catalogue libgpod 0.8.3 (Debian `libgpod4t64` 0.8.3-24)
    /// wrote and signed for a synthetic Classic 120 GB. See `PROVENANCE.md`.
    const LIBGPOD_EMPTY_CLASSIC: &[u8] =
        include_bytes!("../tests/fixtures/libgpod-empty-classic.itunesdb");
    /// The GUID that fixture was signed for. Its zero bytes exercise the
    /// least-common-multiple convention.
    const LIBGPOD_GUID: &str = "0x000A27001C0FFEE5";

    #[test]
    fn the_generated_sboxes_are_the_aes_tables() {
        // FIPS 197, figures 7 and 14.
        assert_eq!(
            &FORWARD_SBOX[..8],
            &[0x63, 0x7c, 0x77, 0x7b, 0xf2, 0x6b, 0x6f, 0xc5]
        );
        assert_eq!(FORWARD_SBOX[0x53], 0xed);
        assert_eq!(FORWARD_SBOX[0xff], 0x16);
        assert_eq!(
            &INVERSE_SBOX[..8],
            &[0x52, 0x09, 0x6a, 0xd5, 0x30, 0x36, 0xa5, 0x38]
        );
        assert_eq!(INVERSE_SBOX[0xff], 0x7d);
        for index in 0..=255u8 {
            assert_eq!(
                INVERSE_SBOX[usize::from(FORWARD_SBOX[usize::from(index)])],
                index
            );
        }
    }

    #[test]
    fn signing_reproduces_libgpods_signature() {
        let guid = FirewireGuid::parse(LIBGPOD_GUID).expect("valid GUID");
        let mut database = LIBGPOD_EMPTY_CLASSIC.to_vec();
        database[SIGNATURE].fill(0xaa);
        sign(&mut database, guid).expect("signs");
        assert_eq!(database, LIBGPOD_EMPTY_CLASSIC);
    }

    #[test]
    fn a_different_device_gives_a_different_signature() {
        let guid = FirewireGuid::parse("000A27001C0FFEE6").expect("valid GUID");
        let mut database = LIBGPOD_EMPTY_CLASSIC.to_vec();
        sign(&mut database, guid).expect("signs");
        assert_ne!(database[SIGNATURE], LIBGPOD_EMPTY_CLASSIC[SIGNATURE]);
        assert_eq!(
            database[..SIGNATURE.start],
            LIBGPOD_EMPTY_CLASSIC[..SIGNATURE.start]
        );
    }

    #[test]
    fn guids_parse_in_printed_order() {
        assert_eq!(
            FirewireGuid::parse(" 0x000a27001C0FFEE5\n"),
            Some(FirewireGuid([
                0x00, 0x0a, 0x27, 0x00, 0x1c, 0x0f, 0xfe, 0xe5
            ]))
        );
        assert_eq!(FirewireGuid::parse("000A27001C0FFEE"), None);
        assert_eq!(FirewireGuid::parse("000A27001C0FFEE5A"), None);
        assert_eq!(FirewireGuid::parse("000A27001C0FFEG5"), None);
        assert_eq!(FirewireGuid::parse("+0A27001C0FFEE5"), None);
    }

    #[test]
    fn only_a_database_is_signed() {
        let guid = FirewireGuid::parse(LIBGPOD_GUID).expect("valid GUID");
        assert_eq!(sign(&mut [0u8; 0x6b], guid), Err(SignError::NotADatabase));
        let mut not_mhbd = LIBGPOD_EMPTY_CLASSIC.to_vec();
        not_mhbd[0] = b'x';
        assert_eq!(sign(&mut not_mhbd, guid), Err(SignError::NotADatabase));
    }
}
