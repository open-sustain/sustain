// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Field-level diff helpers shared by every metadata-editing surface.
//!
//! Both the File Info dialog ([`crate::track_info`]) and the inline cell
//! editor ([`crate::track_table`]) turn a freshly-typed string into a
//! [`FieldChange`] by comparing it against the track's current value, so
//! the two surfaces agree exactly on what counts as "unchanged", what
//! clears a tag, and what sets a new value. Keeping the rules in one
//! place is what guarantees an inline edit writes the same thing the
//! dialog would for the same input.

use sustain_app_runtime::{Bpm, FieldChange, MusicalKey};

pub(crate) fn text_diff(initial: Option<&str>, current: &str) -> FieldChange<String> {
    let trimmed_current = current.trim();
    match (initial, trimmed_current) {
        (Some(value), candidate) if value == candidate => FieldChange::Unchanged,
        (None, "") => FieldChange::Unchanged,
        (_, "") => FieldChange::Clear,
        (_, candidate) => FieldChange::Set(candidate.to_owned()),
    }
}

/// Like `text_diff` but preserves internal whitespace (newlines, indentation).
/// Used for free-form prose fields like lyrics where formatting matters.
/// Empty/whitespace-only buffers still clear the tag.
pub(crate) fn text_diff_preserve_newlines(
    initial: Option<&str>,
    current: &str,
) -> FieldChange<String> {
    let trimmed = current.trim();
    match (initial, trimmed) {
        (None, "") => FieldChange::Unchanged,
        (_, "") => FieldChange::Clear,
        (Some(value), _) if value == current => FieldChange::Unchanged,
        _ => FieldChange::Set(current.to_owned()),
    }
}

pub(crate) fn number_diff(initial: Option<u32>, current: &str) -> FieldChange<u32> {
    parsed_diff(initial, current, |text| text.parse().ok())
}

/// Typed text is read with [`MusicalKey::parse`], so any common notation
/// (`Am`, `A minor`, `8A`, `1m`) sets the key. Text that is not a key is
/// left unchanged, like an unparsable number.
pub(crate) fn key_diff(initial: Option<MusicalKey>, current: &str) -> FieldChange<MusicalKey> {
    parsed_diff(initial, current, MusicalKey::parse)
}

/// Typed text is read with [`Bpm::parse`], so a whole (`128`) or decimal
/// (`118.86`) tempo sets the BPM. Text that is not a tempo is left
/// unchanged, like an unparsable number.
pub(crate) fn bpm_diff(initial: Option<Bpm>, current: &str) -> FieldChange<Bpm> {
    parsed_diff(initial, current, Bpm::parse)
}

pub(crate) fn signed_number_diff(initial: Option<i32>, current: &str) -> FieldChange<i32> {
    parsed_diff(initial, current, |text| text.parse().ok())
}

/// The change typed text makes to a field `parse` reads: blank text clears
/// a value and leaves an empty field alone; text `parse` accepts sets it,
/// unless it is the current value; text `parse` rejects changes nothing.
fn parsed_diff<T: Copy + PartialEq>(
    initial: Option<T>,
    current: &str,
    parse: impl FnOnce(&str) -> Option<T>,
) -> FieldChange<T> {
    let trimmed = current.trim();
    if trimmed.is_empty() {
        return if initial.is_some() {
            FieldChange::Clear
        } else {
            FieldChange::Unchanged
        };
    }
    match parse(trimmed) {
        Some(value) if Some(value) != initial => FieldChange::Set(value),
        _ => FieldChange::Unchanged,
    }
}

pub(crate) fn bool_diff(initial: Option<bool>, current: bool) -> FieldChange<bool> {
    if initial.unwrap_or(false) == current {
        FieldChange::Unchanged
    } else {
        FieldChange::Set(current)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        bool_diff, bpm_diff, key_diff, number_diff, signed_number_diff, text_diff,
        text_diff_preserve_newlines,
    };
    use sustain_app_runtime::{Bpm, FieldChange, MusicalKey};

    #[test]
    fn bpm_diff_reads_whole_and_decimal_tempos_and_ignores_other_text() {
        let tempo = Bpm::from_hundredths(11_886);
        assert_eq!(bpm_diff(None, ""), FieldChange::Unchanged);
        assert_eq!(bpm_diff(tempo, " "), FieldChange::Clear);
        assert_eq!(bpm_diff(tempo, "118.86"), FieldChange::Unchanged);
        assert_eq!(
            bpm_diff(tempo, "128"),
            FieldChange::Set(Bpm::from_whole(128).expect("tempo"))
        );
        assert_eq!(
            bpm_diff(None, "124.5"),
            FieldChange::Set(Bpm::from_hundredths(12_450).expect("tempo"))
        );
        for text in ["0", "656", "fast", "-120"] {
            assert_eq!(bpm_diff(tempo, text), FieldChange::Unchanged, "{text:?}");
        }
    }

    #[test]
    fn key_diff_parses_notations_and_ignores_text_that_is_not_a_key() {
        let am = Some(MusicalKey::AMinor);
        assert_eq!(key_diff(None, ""), FieldChange::Unchanged);
        assert_eq!(key_diff(am, " "), FieldChange::Clear);
        assert_eq!(key_diff(am, "Am"), FieldChange::Unchanged);
        assert_eq!(key_diff(am, "8A"), FieldChange::Unchanged);
        assert_eq!(key_diff(am, "A minor"), FieldChange::Unchanged);
        assert_eq!(key_diff(am, "8B"), FieldChange::Set(MusicalKey::CMajor));
        assert_eq!(key_diff(None, "F#m"), FieldChange::Set(MusicalKey::FsMinor));
        assert_eq!(key_diff(am, "www.example.com"), FieldChange::Unchanged);
    }

    #[test]
    fn text_diff_preserves_unchanged_value() {
        assert_eq!(text_diff(Some("hello"), "hello"), FieldChange::Unchanged);
        assert_eq!(text_diff(None, ""), FieldChange::Unchanged);
    }

    #[test]
    fn text_diff_clears_when_field_emptied() {
        assert_eq!(text_diff(Some("hello"), ""), FieldChange::Clear);
        assert_eq!(text_diff(Some("hello"), "   "), FieldChange::Clear);
    }

    #[test]
    fn text_diff_sets_when_value_changes() {
        assert_eq!(text_diff(Some("a"), "b"), FieldChange::Set("b".to_owned()));
        assert_eq!(text_diff(None, "b"), FieldChange::Set("b".to_owned()));
    }

    #[test]
    fn number_diff_handles_empty_and_invalid_inputs() {
        assert_eq!(number_diff(None, ""), FieldChange::Unchanged);
        assert_eq!(number_diff(Some(3), ""), FieldChange::Clear);
        assert_eq!(number_diff(Some(3), "abc"), FieldChange::Unchanged);
        assert_eq!(number_diff(Some(3), "3"), FieldChange::Unchanged);
        assert_eq!(number_diff(Some(3), "4"), FieldChange::Set(4));
    }

    #[test]
    fn signed_number_diff_handles_negatives() {
        assert_eq!(signed_number_diff(Some(2000), "-1"), FieldChange::Set(-1));
        assert_eq!(signed_number_diff(None, "1998"), FieldChange::Set(1998));
        assert_eq!(signed_number_diff(Some(1998), ""), FieldChange::Clear);
    }

    #[test]
    fn bool_diff_treats_none_as_false_baseline() {
        assert_eq!(bool_diff(None, false), FieldChange::Unchanged);
        assert_eq!(bool_diff(None, true), FieldChange::Set(true));
        assert_eq!(bool_diff(Some(true), false), FieldChange::Set(false));
        assert_eq!(bool_diff(Some(true), true), FieldChange::Unchanged);
    }

    #[test]
    fn text_diff_preserve_newlines_keeps_internal_whitespace() {
        assert_eq!(
            text_diff_preserve_newlines(None, "line one\n\nline two"),
            FieldChange::Set("line one\n\nline two".to_owned())
        );
        assert_eq!(
            text_diff_preserve_newlines(Some("a\nb"), "a\nb"),
            FieldChange::Unchanged
        );
        assert_eq!(
            text_diff_preserve_newlines(Some("a\nb"), "  \n  \n"),
            FieldChange::Clear
        );
        assert_eq!(
            text_diff_preserve_newlines(None, ""),
            FieldChange::Unchanged
        );
    }
}
