// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! FAT32/exFAT-safe path component sanitization.
//!
//! The target filesystems for these devices are FAT32/exFAT, which
//! forbid `/ \ : * ? " < > |` and control characters, dislike leading
//! or trailing spaces and dots, and cap component length. Components are
//! also length-limited per the device layout (32 characters for the
//! folder-per-playlist layout's car-stereo target), and every component
//! fits [`COMPONENT_BYTE_LIMIT`] whatever its script.

const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// The longest path component, in UTF-8 bytes, a sync writes. Linux
/// filesystems — ext4, f2fs, and the FUSE layer over Android shared
/// storage — refuse a longer name. FAT32 and exFAT count 255 UTF-16
/// units, which a name of at most this many UTF-8 bytes never exceeds.
const COMPONENT_BYTE_LIMIT: usize = 255;

/// Sanitize a string into a single FAT-safe path component of at most
/// `max_chars` characters. Returns `fallback` if nothing survives.
pub fn component(value: &str, max_chars: usize, fallback: &str) -> String {
    component_with_suffix(value, "", max_chars, fallback)
}

/// Like [`component`], but guarantees `suffix` survives the length cap:
/// its room is reserved first and the sanitized `value` is truncated to
/// fit, so a disambiguator can never be pushed off the end — which would
/// let two names collapse onto one. The suffix is appended verbatim
/// (callers build it from FAT-safe characters, e.g. ` (2)` or `.mp3`), and
/// the caps hold only for a suffix that leaves room for one character.
pub fn component_with_suffix(
    value: &str,
    suffix: &str,
    max_chars: usize,
    fallback: &str,
) -> String {
    let max_chars = max_chars.saturating_sub(suffix.chars().count()).max(1);
    let max_bytes = COMPONENT_BYTE_LIMIT.saturating_sub(suffix.len()).max(1);
    let cleaned: String = value
        .chars()
        .map(|c| {
            if ILLEGAL.contains(&c) || c.is_control() {
                '_'
            } else {
                c
            }
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.').trim();
    let capped = truncate(trimmed, max_chars, max_bytes)
        .trim()
        .trim_matches('.')
        .trim();
    // Fall back when nothing meaningful survives — including the case
    // where every character was an illegal one replaced by '_'.
    let base = if capped.is_empty() || capped.chars().all(|c| c == '_') {
        truncate(fallback, max_chars, max_bytes)
    } else {
        capped
    };
    format!("{base}{suffix}")
}

/// The longest prefix of `value` within both `max_chars` characters and
/// `max_bytes` UTF-8 bytes, ending on a character boundary.
fn truncate(value: &str, max_chars: usize, max_bytes: usize) -> &str {
    let end = value
        .char_indices()
        .take(max_chars)
        .map(|(start, c)| start + c.len_utf8())
        .take_while(|end| *end <= max_bytes)
        .last()
        .unwrap_or(0);
    &value[..end]
}

/// Sanitize a filename while preserving its extension, capping the stem
/// so the whole name stays within `max_chars` characters.
pub fn filename(stem: &str, extension: &str, max_chars: usize) -> String {
    filename_with_suffix(stem, "", extension, max_chars)
}

/// Like [`filename`], but guarantees a disambiguating `suffix`, placed
/// before the extension, survives the length cap (see
/// [`component_with_suffix`]).
pub fn filename_with_suffix(stem: &str, suffix: &str, extension: &str, max_chars: usize) -> String {
    let ext = extension.trim_start_matches('.');
    let tail = if ext.is_empty() {
        suffix.to_owned()
    } else {
        format!("{suffix}.{ext}")
    };
    component_with_suffix(stem, &tail, max_chars, "track")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_illegal_characters() {
        assert_eq!(component("AC/DC: Live?", 64, "x"), "AC_DC_ Live_");
    }

    #[test]
    fn trims_dots_and_spaces() {
        assert_eq!(component("  .hidden.  ", 64, "x"), "hidden");
    }

    #[test]
    fn caps_length() {
        let long = "a".repeat(100);
        assert_eq!(component(&long, 32, "x").chars().count(), 32);
    }

    #[test]
    fn empty_falls_back() {
        assert_eq!(component("///", 64, "Unknown"), "Unknown");
    }

    #[test]
    fn a_long_name_in_a_multibyte_script_fits_the_device_byte_limit() {
        // 100 kana are 300 UTF-8 bytes: within the character cap, past
        // what ext4 or Android storage will create.
        let title = "あ".repeat(100);
        let name = filename_with_suffix(&title, " (7)", "flac", 120);
        assert!(name.len() <= COMPONENT_BYTE_LIMIT, "{} bytes", name.len());
        assert!(name.ends_with("あ (7).flac"), "cut on a character: {name}");
    }

    #[test]
    fn a_suffix_keeps_the_component_within_its_cap() {
        let name = component_with_suffix(&"x".repeat(40), " (2)", 32, "Playlist");
        assert_eq!(name.chars().count(), 32);
        assert!(name.ends_with(" (2)"));
    }

    #[test]
    fn a_fallback_name_keeps_the_byte_limit_too() {
        let extension = "x".repeat(250);
        let name = filename("_", &extension, 255);
        assert!(name.len() <= COMPONENT_BYTE_LIMIT, "{} bytes", name.len());
    }

    #[test]
    fn filename_preserves_extension_within_budget() {
        let name = filename(&"x".repeat(100), "mp3", 32);
        assert!(name.ends_with(".mp3"));
        assert!(name.chars().count() <= 32);
    }
}
