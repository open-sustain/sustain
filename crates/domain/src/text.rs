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

#[cfg(test)]
mod tests {
    use super::normalize_search_text;

    #[test]
    fn search_text_normalization_folds_accents_and_case() {
        assert_eq!(normalize_search_text(" Déjà BjÖrk "), "deja bjork");
    }
}
