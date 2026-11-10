// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cmp::Ordering;

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
/// [`compare_optional_text`] applies exactly this collation
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

/// Compares two optional text fields for library sort order.
///
/// Each side collates as its trimmed, Unicode-lowercased form — exactly
/// the key [`normalize_sort_text`] builds, applied lazily here so
/// a comparison allocates nothing and stops at the first differing
/// character. A missing value (`None`) collates as the empty string.
/// Code that compares the same strings many times (a whole-table sort)
/// can instead build [`normalize_sort_text`] keys once and compare them
/// with `str::cmp`, which orders identically.
pub fn compare_optional_text(left: Option<&str>, right: Option<&str>) -> Ordering {
    fn collation_chars(value: Option<&str>) -> impl Iterator<Item = char> + '_ {
        value
            .unwrap_or_default()
            .trim()
            .chars()
            .flat_map(char::to_lowercase)
    }

    collation_chars(left).cmp(collation_chars(right))
}

/// The text a column actually sorts by, honoring the "sort as" tag
/// fields (issue #13). When `honor_sort_tags` is on and `sort_field`
/// carries a non-blank value it wins; otherwise we fall back to the
/// display field. With the preference off the display field always
/// wins, giving strict alphabetic-as-shown ordering for libraries that
/// tag sort fields inconsistently.
pub fn effective_sort_key<'a>(
    sort_field: Option<&'a str>,
    display_field: Option<&'a str>,
    honor_sort_tags: bool,
) -> Option<&'a str> {
    if honor_sort_tags
        && let Some(value) = sort_field
        && !value.trim().is_empty()
    {
        return Some(value);
    }
    display_field
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{
        compare_optional_text, effective_sort_key, fold_search_text, fold_sort_text,
        normalize_search_text, normalize_sort_text,
    };

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

    #[test]
    fn optional_text_collation_trims_and_folds_unicode_case() {
        assert_eq!(
            compare_optional_text(Some("  Édith  "), Some("édith")),
            Ordering::Equal
        );
        assert_eq!(
            compare_optional_text(Some("ABBA"), Some("abba")),
            Ordering::Equal
        );
    }

    #[test]
    fn optional_text_collation_treats_none_as_empty() {
        assert_eq!(compare_optional_text(None, Some("")), Ordering::Equal);
        assert_eq!(compare_optional_text(None, Some("a")), Ordering::Less);
        assert_eq!(compare_optional_text(Some("a"), None), Ordering::Greater);
    }

    #[test]
    fn optional_text_collation_orders_exactly_like_sort_key_comparison() {
        // The lazy comparison and a `str::cmp` over pre-built
        // `normalize_sort_text` keys must stay interchangeable: the track
        // table sorts most text columns by stored keys and compares the
        // others directly, and the two orders may never diverge.
        let samples = [
            None,
            Some(""),
            Some("   "),
            Some("  Édith  "),
            Some("édith"),
            Some("ABBA"),
            Some("abba"),
            Some("déjà"),
            Some("deja"),
            Some("weiß"),
            Some("İstanbul"),
            Some("ΟΔΥΣΣΕΥΣ"),
        ];
        for left in samples {
            for right in samples {
                assert_eq!(
                    compare_optional_text(left, right),
                    normalize_sort_text(left.unwrap_or_default())
                        .cmp(&normalize_sort_text(right.unwrap_or_default())),
                    "collation diverged for {left:?} vs {right:?}"
                );
            }
        }
    }

    #[test]
    fn effective_sort_key_honors_sort_fields_with_display_fallback() {
        // Sort field present and honored -> it wins ("The Beatles" files under B).
        assert_eq!(
            effective_sort_key(Some("Beatles, The"), Some("The Beatles"), true),
            Some("Beatles, The")
        );
        // A blank sort field is treated as absent and falls back to display.
        assert_eq!(
            effective_sort_key(Some("   "), Some("The Beatles"), true),
            Some("The Beatles")
        );
        // No sort field -> display field.
        assert_eq!(
            effective_sort_key(None, Some("The Beatles"), true),
            Some("The Beatles")
        );
        // Preference off -> display field always, even when a sort field exists.
        assert_eq!(
            effective_sort_key(Some("Beatles, The"), Some("The Beatles"), false),
            Some("The Beatles")
        );
    }
}
