// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Normalize user-facing text for case-insensitive library matching:
/// trim outer whitespace, decompose accents, drop combining marks, and
/// lowercase with Unicode case mapping.
pub fn normalize_search_text(text: &str) -> String {
    let text = text.trim();
    // ASCII is its own decomposition and holds no combining mark, so for
    // it the whole mapping is ASCII lowercasing, done bytewise.
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    fold_search_text(text)
}

fn fold_search_text(text: &str) -> String {
    text.nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect()
}

/// Normalize user-facing text into the collation key library sorting
/// compares by: trim outer whitespace and lowercase per character with
/// Unicode case mapping. Unlike [`normalize_search_text`], accents are
/// kept — sorting distinguishes "déjà" from "deja".
///
/// [`crate::compare_optional_text`] applies exactly this collation
/// lazily, without allocating; code that compares the same strings many
/// times (a whole-table sort) can build these keys once and compare them
/// with `str::cmp`, which orders identically.
pub fn normalize_sort_text(text: &str) -> String {
    let text = text.trim();
    // An ASCII character lowercases to one ASCII character: the same key,
    // built bytewise. Most tags are ASCII, and a table builds three keys
    // per row.
    if text.is_ascii() {
        return text.to_ascii_lowercase();
    }
    fold_sort_text(text)
}

fn fold_sort_text(text: &str) -> String {
    let mut key = String::with_capacity(text.len());
    key.extend(text.chars().flat_map(char::to_lowercase));
    key
}

#[cfg(test)]
mod tests {
    use super::{fold_search_text, fold_sort_text, normalize_search_text, normalize_sort_text};

    #[test]
    fn search_text_normalization_folds_accents_and_case() {
        assert_eq!(normalize_search_text(" Déjà BjÖrk "), "deja bjork");
    }

    #[test]
    fn sort_text_normalization_folds_case_but_keeps_accents() {
        assert_eq!(normalize_sort_text(" Déjà BjÖrk "), "déjà björk");
    }

    #[test]
    fn the_ascii_shortcut_maps_text_as_the_general_folds_do() {
        let ascii: String = (0..=127u8).map(char::from).collect();
        for text in [
            ascii.as_str(),
            "The Beatles",
            "AC/DC - T.N.T. (Live)",
            "  Mixed\tCase  ",
        ] {
            let text = text.trim();
            assert_eq!(
                normalize_search_text(text),
                fold_search_text(text),
                "{text:?}"
            );
            assert_eq!(normalize_sort_text(text), fold_sort_text(text), "{text:?}");
        }
    }
}
