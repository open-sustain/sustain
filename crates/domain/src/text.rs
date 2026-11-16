// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cmp::Ordering;
use std::collections::HashMap;

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
    base_letters(text).collect()
}

fn base_letters(text: &str) -> impl Iterator<Item = char> + '_ {
    text.nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .flat_map(undecomposed_base_letters)
}

/// The base letters of a lowercase Latin letter that Unicode gives no
/// decomposition: a stroke or a ligature is part of the letter itself, not
/// a combining mark. They fold the way the Unicode collation algorithm
/// files them, so "royksopp" finds "Røyksopp" and "coeur" finds "Cœur".
fn undecomposed_base_letters(letter: char) -> impl Iterator<Item = char> {
    let (first, second) = match letter {
        'ø' => ('o', None),
        'œ' => ('o', Some('e')),
        'æ' => ('a', Some('e')),
        'ß' => ('s', Some('s')),
        'ł' => ('l', None),
        'đ' | 'ð' => ('d', None),
        'ħ' => ('h', None),
        'ŧ' => ('t', None),
        'ı' => ('i', None),
        other => (other, None),
    };
    std::iter::once(first).chain(second)
}

/// Where a library text value files in sort order.
///
/// Text sorts by its base letters first: trimmed, accents dropped and case
/// folded, the fold [`normalize_search_text`] applies. "Édith" files under
/// E, next to "Edith", rather than after every unaccented name. Two values
/// with the same base letters then order by their case-folded text with the
/// accents kept, so "deja" and "déjà" stay distinct and in a fixed order.
/// Values that differ only in case, in outer whitespace, or in how Unicode
/// spells an accented letter (composed or decomposed) are equal.
///
/// [`compare_optional_text`] applies exactly this order lazily, without
/// allocating; code that compares the same strings many times (a
/// whole-table sort) builds these keys once and compares them instead.
#[derive(Clone, Debug, Default, Eq, PartialEq, Hash)]
pub struct CollationKey {
    base: String,
    /// The case-folded text with its accents kept, when that differs from
    /// `base`. Empty otherwise, so an unaccented value holds one string.
    accented: String,
}

impl CollationKey {
    pub fn new(text: &str) -> Self {
        let text = text.trim();
        // ASCII holds no accent and lowercases bytewise. Most tags are
        // ASCII, and a table builds four keys per row.
        if text.is_ascii() {
            return Self {
                base: text.to_ascii_lowercase(),
                accented: String::new(),
            };
        }
        let base = fold_search_text(text);
        let accented: String = case_folded(text).collect();
        Self {
            accented: if accented == base {
                String::new()
            } else {
                accented
            },
            base,
        }
    }

    fn accented(&self) -> &str {
        if self.accented.is_empty() {
            &self.base
        } else {
            &self.accented
        }
    }
}

impl Ord for CollationKey {
    fn cmp(&self, other: &Self) -> Ordering {
        self.base
            .cmp(&other.base)
            .then_with(|| self.accented().cmp(other.accented()))
    }
}

impl PartialOrd for CollationKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// The text lowercased with its accents kept, in one canonical form: a
/// tag writes "é" either as one character or as "e" and a combining
/// accent, and both spellings are the same text.
fn case_folded(text: &str) -> impl Iterator<Item = char> + '_ {
    text.nfd().flat_map(char::to_lowercase)
}

/// The one spelling each value of a library text field goes by.
///
/// Tags written by different tools spell one artist "Daft Punk" here and
/// "DAFT PUNK" there. Search, sorting and smart-playlist rules already
/// treat those as one value; a list that names each value once does too.
/// Spellings that are equal as [`CollationKey`]s share the spelling most
/// occurrences use, and the first in byte order when several are used
/// equally.
pub struct Spellings<'a> {
    shown: HashMap<&'a str, &'a str>,
}

impl<'a> Spellings<'a> {
    /// `values` holds one item per occurrence: a value tagged on forty
    /// tracks comes forty times.
    pub fn of(values: impl IntoIterator<Item = &'a str>) -> Self {
        let mut uses: HashMap<&str, usize> = HashMap::new();
        for value in values {
            *uses.entry(value).or_insert(0) += 1;
        }

        let keyed: Vec<(CollationKey, &str, usize)> = uses
            .into_iter()
            .map(|(spelling, count)| (CollationKey::new(spelling), spelling, count))
            .collect();
        let mut preferred: HashMap<&CollationKey, (&str, usize)> = HashMap::new();
        for (key, spelling, count) in &keyed {
            let candidate = (*spelling, *count);
            preferred
                .entry(key)
                .and_modify(|best| {
                    let more_used = candidate.1 > best.1;
                    let as_used_and_first = candidate.1 == best.1 && candidate.0 < best.0;
                    if more_used || as_used_and_first {
                        *best = candidate;
                    }
                })
                .or_insert(candidate);
        }

        Self {
            shown: keyed
                .iter()
                .map(|(key, spelling, _)| (*spelling, preferred[key].0))
                .collect(),
        }
    }

    /// The spelling `value` goes by, or `None` for a value that was not
    /// among the occurrences.
    pub fn shown(&self, value: &str) -> Option<&'a str> {
        self.shown.get(value).copied()
    }

    /// Every value once, under the spelling it goes by, in sort order.
    pub fn distinct(&self) -> Vec<&'a str> {
        let mut distinct: Vec<&str> = self.shown.values().copied().collect();
        distinct.sort_unstable();
        distinct.dedup();
        distinct.sort_by_cached_key(|value| CollationKey::new(value));
        distinct
    }
}

/// Compares two optional text fields for library sort order.
///
/// The order is the one [`CollationKey`] defines, applied lazily here so a
/// comparison allocates nothing and stops at the first differing
/// character. A missing value (`None`) sorts as the empty string.
pub fn compare_optional_text(left: Option<&str>, right: Option<&str>) -> Ordering {
    let left = left.unwrap_or_default().trim();
    let right = right.unwrap_or_default().trim();
    base_letters(left)
        .cmp(base_letters(right))
        .then_with(|| case_folded(left).cmp(case_folded(right)))
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
        CollationKey, Spellings, compare_optional_text, effective_sort_key, fold_search_text,
        normalize_search_text,
    };

    #[test]
    fn search_text_normalization_folds_accents_and_case() {
        assert_eq!(normalize_search_text(" Déjà BjÖrk "), "deja bjork");
    }

    #[test]
    fn search_text_normalization_folds_strokes_and_ligatures() {
        for (text, folded) in [
            ("Røyksopp", "royksopp"),
            ("Cœur de pirate", "coeur de pirate"),
            ("ÆON", "aeon"),
            ("Straße", "strasse"),
            ("STRAẞE", "strasse"),
            ("Łódź", "lodz"),
            ("Đorđe", "dorde"),
            ("Sigur Rós – Með", "sigur ros – med"),
            ("Kadıköy", "kadikoy"),
        ] {
            assert_eq!(normalize_search_text(text), folded, "{text:?}");
        }
    }

    #[test]
    fn an_accented_letter_sorts_with_its_base_letter() {
        let mut names = [
            "Zoé",
            "Rush",
            "Røyksopp",
            "Étienne",
            "Eric",
            "edith",
            "Édith",
            "Björk",
            "Bjork",
        ];
        names.sort_by_key(|name| CollationKey::new(name));
        assert_eq!(
            names,
            [
                "Bjork",
                "Björk",
                "edith",
                "Édith",
                "Eric",
                "Étienne",
                "Røyksopp",
                "Rush",
                "Zoé"
            ]
        );
    }

    #[test]
    fn sort_keys_ignore_case_and_outer_whitespace_but_not_accents() {
        assert_eq!(
            CollationKey::new(" Déjà BjÖrk "),
            CollationKey::new("déjà björk")
        );
        assert_ne!(CollationKey::new("déjà"), CollationKey::new("deja"));
        // The Kelvin sign lowercases to an ASCII "k".
        assert_eq!(CollationKey::new("\u{212A}"), CollationKey::new("k"));
    }

    #[test]
    fn a_composed_and_a_decomposed_accent_are_the_same_text() {
        let composed = "Bj\u{f6}rk \u{2013} D\u{c9}J\u{c0}";
        let decomposed = "Bjo\u{308}rk \u{2013} DE\u{301}JA\u{300}";
        assert_eq!(CollationKey::new(composed), CollationKey::new(decomposed));
        assert_eq!(
            compare_optional_text(Some(composed), Some(decomposed)),
            Ordering::Equal
        );
        assert_ne!(
            CollationKey::new(decomposed),
            CollationKey::new("bjork – deja")
        );
    }

    #[test]
    fn a_value_goes_by_its_most_used_spelling() {
        let spellings = Spellings::of([
            "daft punk",
            "Daft Punk",
            "Daft Punk",
            "DAFT PUNK",
            // Equally used: the first in byte order.
            "air",
            "Air",
            // An accent makes another value, not another spelling.
            "A\u{ef}r",
            "Zero 7",
        ]);

        assert_eq!(spellings.shown("daft punk"), Some("Daft Punk"));
        assert_eq!(spellings.shown("DAFT PUNK"), Some("Daft Punk"));
        assert_eq!(spellings.shown("air"), Some("Air"));
        assert_eq!(spellings.shown("A\u{ef}r"), Some("A\u{ef}r"));
        assert_eq!(spellings.shown("Moby"), None);
        assert_eq!(
            spellings.distinct(),
            ["Air", "A\u{ef}r", "Daft Punk", "Zero 7"]
        );
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
            assert_eq!(
                CollationKey::new(text),
                CollationKey {
                    base: fold_search_text(text),
                    accented: String::new(),
                },
                "{text:?}"
            );
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
        // The lazy comparison and a comparison of pre-built keys must stay
        // interchangeable: the track table sorts most text columns by
        // stored keys and compares the others directly, and the two orders
        // may never diverge.
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
            Some("µ"),
            Some("μ"),
            Some("\u{212A}"),
            Some("k"),
            Some("ka"),
            Some("é"),
            Some("ez"),
        ];
        for left in samples {
            for right in samples {
                assert_eq!(
                    compare_optional_text(left, right),
                    CollationKey::new(left.unwrap_or_default())
                        .cmp(&CollationKey::new(right.unwrap_or_default())),
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
