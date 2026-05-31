// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! FAT32/exFAT-safe path component sanitization.
//!
//! The target filesystems for these devices are FAT32/exFAT, which
//! forbid `/ \ : * ? " < > |` and control characters, dislike leading
//! or trailing spaces and dots, and cap component length. Components are
//! also length-limited per the device layout (32 characters for the
//! folder-per-playlist layout's car-stereo target).

const ILLEGAL: &[char] = &['/', '\\', ':', '*', '?', '"', '<', '>', '|'];

/// Sanitize a string into a single FAT-safe path component, capped to
/// `max_chars` characters. Returns `fallback` if nothing survives.
pub fn component(value: &str, max_chars: usize, fallback: &str) -> String {
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
    let capped: String = trimmed.chars().take(max_chars).collect();
    let capped = capped.trim().trim_matches('.').trim();
    // Fall back when nothing meaningful survives — including the case
    // where every character was an illegal one replaced by '_'.
    if capped.is_empty() || capped.chars().all(|c| c == '_') {
        fallback.to_owned()
    } else {
        capped.to_owned()
    }
}

/// Sanitize a filename while preserving its extension, capping the stem
/// so the whole name stays within `max_chars` characters.
pub fn filename(stem: &str, extension: &str, max_chars: usize) -> String {
    let ext = extension.trim_start_matches('.');
    let ext_len = if ext.is_empty() { 0 } else { ext.len() + 1 };
    let stem_budget = max_chars.saturating_sub(ext_len).max(1);
    let safe_stem = component(stem, stem_budget, "track");
    if ext.is_empty() {
        safe_stem
    } else {
        format!("{safe_stem}.{ext}")
    }
}

/// Like [`filename`], but guarantees a disambiguating `suffix` survives the
/// length cap. Room for the suffix (and extension) is reserved first and
/// the base stem is truncated to fit, so a long title can never push the
/// suffix off the end — which would let two distinct tracks collapse onto
/// the same on-device name. The suffix is appended verbatim (callers build
/// it from FAT-safe characters, e.g. a parenthesised numeric track id).
pub fn filename_with_suffix(stem: &str, suffix: &str, extension: &str, max_chars: usize) -> String {
    let ext = extension.trim_start_matches('.');
    let ext_len = if ext.is_empty() { 0 } else { ext.len() + 1 };
    let suffix_len = suffix.chars().count();
    let stem_budget = max_chars
        .saturating_sub(ext_len)
        .saturating_sub(suffix_len)
        .max(1);
    let safe_stem = component(stem, stem_budget, "track");
    let name = format!("{safe_stem}{suffix}");
    if ext.is_empty() {
        name
    } else {
        format!("{name}.{ext}")
    }
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
    fn filename_preserves_extension_within_budget() {
        let name = filename(&"x".repeat(100), "mp3", 32);
        assert!(name.ends_with(".mp3"));
        assert!(name.chars().count() <= 32);
    }
}
