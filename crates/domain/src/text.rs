// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

/// Normalize user-facing text for case-insensitive library matching:
/// trim outer whitespace, decompose accents, drop combining marks, and
/// lowercase with Unicode case mapping.
pub fn normalize_search_text(text: &str) -> String {
    text.trim()
        .nfkd()
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
    text.trim().chars().flat_map(char::to_lowercase).collect()
}

#[cfg(test)]
mod tests {
    use super::{normalize_search_text, normalize_sort_text};

    #[test]
    fn search_text_normalization_folds_accents_and_case() {
        assert_eq!(normalize_search_text(" Déjà BjÖrk "), "deja bjork");
    }

    #[test]
    fn sort_text_normalization_folds_case_but_keeps_accents() {
        assert_eq!(normalize_sort_text(" Déjà BjÖrk "), "déjà björk");
    }
}
