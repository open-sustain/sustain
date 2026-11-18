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

use std::str::FromStr;

use sustain_app_runtime::{Bpm, FieldChange, MusicalKey};
use sustain_i18n::{gettext, tr_format};

/// A single-line field's change. Surrounding whitespace is trimmed from a
/// new value, but untouched text is unchanged even when the stored value
/// carries whitespace of its own.
pub(crate) fn text_diff(initial: Option<&str>, current: &str) -> FieldChange<String> {
    let trimmed_current = current.trim();
    match (initial, trimmed_current) {
        (Some(value), _) if value == current => FieldChange::Unchanged,
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

/// The outcome of reading a typed field: its change, or the message that
/// says what the field accepts. Malformed text is never silently dropped.
pub(crate) type ParsedChange<T> = Result<FieldChange<T>, String>;

/// A whole number for the field `label` names, such as "Year".
pub(crate) fn parse_whole_number<T: FromStr>(label: &str, text: &str) -> Result<T, String> {
    text.parse().map_err(|_| {
        // Translators: {label} is a Get Info field, such as "Year".
        tr_format!(gettext("{label} must be a whole number."), label = label)
    })
}

/// A key in any notation [`MusicalKey::parse`] reads (`Am`, `A minor`,
/// `8A`, `1m`).
pub(crate) fn parse_key(text: &str) -> Result<MusicalKey, String> {
    MusicalKey::parse(text)
        .ok_or_else(|| gettext("Key must be a musical key, such as Am, F#, or 8A."))
}

/// A whole (`128`) or decimal (`118.86`) tempo, as [`Bpm`] reads it.
pub(crate) fn parse_bpm(text: &str) -> Result<Bpm, String> {
    text.parse::<Bpm>().map_err(|_| {
        tr_format!(
            // Translators: {max} is the highest tempo Sustain accepts.
            gettext("BPM must be a number above 0 and at most {max}."),
            max = Bpm::MAX_WHOLE
        )
    })
}

pub(crate) fn number_diff(label: &str, initial: Option<u32>, current: &str) -> ParsedChange<u32> {
    parsed_diff(initial, current, |text| parse_whole_number(label, text))
}

pub(crate) fn signed_number_diff(
    label: &str,
    initial: Option<i32>,
    current: &str,
) -> ParsedChange<i32> {
    parsed_diff(initial, current, |text| parse_whole_number(label, text))
}

pub(crate) fn key_diff(initial: Option<MusicalKey>, current: &str) -> ParsedChange<MusicalKey> {
    parsed_diff(initial, current, parse_key)
}

pub(crate) fn bpm_diff(initial: Option<Bpm>, current: &str) -> ParsedChange<Bpm> {
    parsed_diff(initial, current, parse_bpm)
}

/// The change typed text makes to a field `parse` reads: blank text clears
/// a value and leaves an empty field alone; text `parse` accepts sets it,
/// unless it is the current value; text `parse` rejects is an error.
fn parsed_diff<T: Copy + PartialEq>(
    initial: Option<T>,
    current: &str,
    parse: impl FnOnce(&str) -> Result<T, String>,
) -> ParsedChange<T> {
    let trimmed = current.trim();
    if trimmed.is_empty() {
        return Ok(if initial.is_some() {
            FieldChange::Clear
        } else {
            FieldChange::Unchanged
        });
    }
    let value = parse(trimmed)?;
    Ok(if Some(value) == initial {
        FieldChange::Unchanged
    } else {
        FieldChange::Set(value)
    })
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
    fn bpm_diff_reads_whole_and_decimal_tempos_and_rejects_other_text() {
        let tempo = Bpm::from_hundredths(11_886);
        assert_eq!(bpm_diff(None, ""), Ok(FieldChange::Unchanged));
        assert_eq!(bpm_diff(tempo, " "), Ok(FieldChange::Clear));
        assert_eq!(bpm_diff(tempo, "118.86"), Ok(FieldChange::Unchanged));
        assert_eq!(
            bpm_diff(tempo, "128"),
            Ok(FieldChange::Set(Bpm::from_whole(128).expect("tempo")))
        );
        assert_eq!(
            bpm_diff(None, "124.5"),
            Ok(FieldChange::Set(
                Bpm::from_hundredths(12_450).expect("tempo")
            ))
        );
        for text in ["0", "656", "fast", "-120"] {
            assert!(bpm_diff(tempo, text).is_err(), "{text:?}");
        }
    }

    #[test]
    fn key_diff_parses_notations_and_rejects_text_that_is_not_a_key() {
        let am = Some(MusicalKey::AMinor);
        assert_eq!(key_diff(None, ""), Ok(FieldChange::Unchanged));
        assert_eq!(key_diff(am, " "), Ok(FieldChange::Clear));
        assert_eq!(key_diff(am, "Am"), Ok(FieldChange::Unchanged));
        assert_eq!(key_diff(am, "8A"), Ok(FieldChange::Unchanged));
        assert_eq!(key_diff(am, "A minor"), Ok(FieldChange::Unchanged));
        assert_eq!(key_diff(am, "8B"), Ok(FieldChange::Set(MusicalKey::CMajor)));
        assert_eq!(
            key_diff(None, "F#m"),
            Ok(FieldChange::Set(MusicalKey::FsMinor))
        );
        assert!(key_diff(am, "www.example.com").is_err());
    }

    #[test]
    fn text_diff_preserves_unchanged_value() {
        assert_eq!(text_diff(Some("hello"), "hello"), FieldChange::Unchanged);
        assert_eq!(text_diff(None, ""), FieldChange::Unchanged);
    }

    #[test]
    fn untouched_text_is_unchanged_even_with_surrounding_whitespace() {
        assert_eq!(
            text_diff(Some("  verse "), "  verse "),
            FieldChange::Unchanged
        );
        assert_eq!(
            text_diff_preserve_newlines(Some("  verse\n"), "  verse\n"),
            FieldChange::Unchanged
        );
        // An edit still trims the new single-line value.
        assert_eq!(
            text_diff(Some("  verse "), " chorus "),
            FieldChange::Set("chorus".to_owned())
        );
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
    fn number_diff_handles_empty_and_rejects_invalid_inputs() {
        assert_eq!(number_diff("Track", None, ""), Ok(FieldChange::Unchanged));
        assert_eq!(number_diff("Track", Some(3), ""), Ok(FieldChange::Clear));
        assert_eq!(
            number_diff("Track", Some(3), "abc"),
            Err("Track must be a whole number.".to_owned())
        );
        assert_eq!(
            number_diff("Track", Some(3), "3"),
            Ok(FieldChange::Unchanged)
        );
        assert_eq!(number_diff("Track", Some(3), "4"), Ok(FieldChange::Set(4)));
    }

    #[test]
    fn signed_number_diff_handles_negatives_and_rejects_typos() {
        assert_eq!(
            signed_number_diff("Year", Some(2000), "-1"),
            Ok(FieldChange::Set(-1))
        );
        assert_eq!(
            signed_number_diff("Year", None, "1998"),
            Ok(FieldChange::Set(1998))
        );
        assert_eq!(
            signed_number_diff("Year", Some(1998), ""),
            Ok(FieldChange::Clear)
        );
        assert!(signed_number_diff("Year", Some(1998), "20O4").is_err());
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
