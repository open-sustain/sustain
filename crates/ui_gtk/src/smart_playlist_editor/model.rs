// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

use sustain_app_runtime::{
    Rating, SmartPlaylistBoolField, SmartPlaylistBoolRule, SmartPlaylistDateField,
    SmartPlaylistLimitSelection, SmartPlaylistMatchKind, SmartPlaylistNumberField,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField,
    SmartPlaylistTextOperator,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorField {
    Text(SmartPlaylistTextField),
    Number(SmartPlaylistNumberField),
    Rating,
    Date(SmartPlaylistDateField),
    Bool(SmartPlaylistBoolField),
    /// The track's file availability (present / missing). Carries no
    /// sub-field — it is a whole-track attribute (#79).
    FileStatus,
}

pub(super) const EDITOR_FIELDS: &[(EditorField, &str)] = &[
    (EditorField::Text(SmartPlaylistTextField::Artist), "Artist"),
    (
        EditorField::Text(SmartPlaylistTextField::AlbumArtist),
        "Album Artist",
    ),
    (EditorField::Text(SmartPlaylistTextField::Album), "Album"),
    (EditorField::Text(SmartPlaylistTextField::Title), "Title"),
    (
        EditorField::Text(SmartPlaylistTextField::Composer),
        "Composer",
    ),
    (EditorField::Text(SmartPlaylistTextField::Genre), "Genre"),
    (
        EditorField::Text(SmartPlaylistTextField::FileName),
        "File Name",
    ),
    (
        EditorField::Text(SmartPlaylistTextField::MusicalKey),
        "Music Key",
    ),
    (EditorField::Rating, "Rating"),
    (EditorField::Number(SmartPlaylistNumberField::Year), "Year"),
    (
        EditorField::Number(SmartPlaylistNumberField::PlayCount),
        "Play Count",
    ),
    (
        EditorField::Number(SmartPlaylistNumberField::SkipCount),
        "Skip Count",
    ),
    (
        EditorField::Number(SmartPlaylistNumberField::TrackNumber),
        "Track Number",
    ),
    (
        EditorField::Number(SmartPlaylistNumberField::DiscNumber),
        "Disc Number",
    ),
    (
        EditorField::Number(SmartPlaylistNumberField::DurationSeconds),
        "Duration (seconds)",
    ),
    (
        EditorField::Number(SmartPlaylistNumberField::BitrateKbps),
        "Bitrate (kbps)",
    ),
    (EditorField::Number(SmartPlaylistNumberField::Bpm), "BPM"),
    (
        EditorField::Date(SmartPlaylistDateField::DateAdded),
        "Date Added",
    ),
    (
        EditorField::Date(SmartPlaylistDateField::LastPlayed),
        "Last Played",
    ),
    (
        EditorField::Date(SmartPlaylistDateField::LastSkipped),
        "Last Skipped",
    ),
    (
        EditorField::Bool(SmartPlaylistBoolField::HasLyrics),
        "Lyrics",
    ),
    (EditorField::FileStatus, "File"),
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum EditorOperator {
    TextContains,
    TextDoesNotContain,
    TextIs,
    TextIsNot,
    TextStartsWith,
    TextEndsWith,
    TextIsEmpty,
    TextIsPresent,
    NumberEqual,
    NumberNotEqual,
    NumberGreaterThan,
    NumberGreaterThanOrEqual,
    NumberLessThan,
    NumberLessThanOrEqual,
    NumberIsEmpty,
    NumberIsPresent,
    DateBefore,
    DateAfter,
    DateInLast,
    DateNotInLast,
    DateIsEmpty,
    DateIsPresent,
    BoolEqual,
    FileMissing,
    FilePresent,
}

impl EditorOperator {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::TextContains => "contains",
            Self::TextDoesNotContain => "does not contain",
            Self::TextIs => "is",
            Self::TextIsNot => "is not",
            Self::TextStartsWith => "starts with",
            Self::TextEndsWith => "ends with",
            Self::TextIsEmpty => "is empty",
            Self::TextIsPresent => "is present",
            Self::NumberEqual => "is",
            Self::NumberNotEqual => "is not",
            Self::NumberGreaterThan => "is greater than",
            Self::NumberGreaterThanOrEqual => "is greater than or equal to",
            Self::NumberLessThan => "is less than",
            Self::NumberLessThanOrEqual => "is less than or equal to",
            Self::NumberIsEmpty => "is empty",
            Self::NumberIsPresent => "is present",
            Self::DateBefore => "is before",
            Self::DateAfter => "is after",
            Self::DateInLast => "is in the last",
            Self::DateNotInLast => "is not in the last",
            Self::DateIsEmpty => "is empty",
            Self::DateIsPresent => "is present",
            Self::BoolEqual => "is",
            Self::FileMissing => "is missing",
            Self::FilePresent => "is present",
        }
    }

    fn value_kind(self) -> ValueKind {
        match self {
            Self::TextContains
            | Self::TextDoesNotContain
            | Self::TextIs
            | Self::TextIsNot
            | Self::TextStartsWith
            | Self::TextEndsWith => ValueKind::Text,
            Self::TextIsEmpty | Self::TextIsPresent => ValueKind::None,
            Self::NumberEqual
            | Self::NumberNotEqual
            | Self::NumberGreaterThan
            | Self::NumberGreaterThanOrEqual
            | Self::NumberLessThan
            | Self::NumberLessThanOrEqual => ValueKind::Number,
            Self::NumberIsEmpty | Self::NumberIsPresent => ValueKind::None,
            Self::DateBefore | Self::DateAfter => ValueKind::Date,
            Self::DateInLast | Self::DateNotInLast => ValueKind::Days,
            Self::DateIsEmpty | Self::DateIsPresent => ValueKind::None,
            Self::BoolEqual => ValueKind::Bool,
            Self::FileMissing | Self::FilePresent => ValueKind::None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ValueKind {
    Text,
    Number,
    Rating,
    Date,
    Days,
    Bool,
    None,
}

const TEXT_OPERATORS: &[EditorOperator] = &[
    EditorOperator::TextContains,
    EditorOperator::TextDoesNotContain,
    EditorOperator::TextIs,
    EditorOperator::TextIsNot,
    EditorOperator::TextStartsWith,
    EditorOperator::TextEndsWith,
    EditorOperator::TextIsEmpty,
    EditorOperator::TextIsPresent,
];

const NUMBER_OPERATORS: &[EditorOperator] = &[
    EditorOperator::NumberEqual,
    EditorOperator::NumberNotEqual,
    EditorOperator::NumberGreaterThan,
    EditorOperator::NumberGreaterThanOrEqual,
    EditorOperator::NumberLessThan,
    EditorOperator::NumberLessThanOrEqual,
    EditorOperator::NumberIsEmpty,
    EditorOperator::NumberIsPresent,
];

// Rating is never absent — an unrated track is zero stars, not a missing
// value — so the rating field offers only the comparison operators, not
// the empty/present pair that numeric tag fields carry.
const RATING_OPERATORS: &[EditorOperator] = &[
    EditorOperator::NumberEqual,
    EditorOperator::NumberNotEqual,
    EditorOperator::NumberGreaterThan,
    EditorOperator::NumberGreaterThanOrEqual,
    EditorOperator::NumberLessThan,
    EditorOperator::NumberLessThanOrEqual,
];

const DATE_OPERATORS: &[EditorOperator] = &[
    EditorOperator::DateBefore,
    EditorOperator::DateAfter,
    EditorOperator::DateInLast,
    EditorOperator::DateNotInLast,
    EditorOperator::DateIsEmpty,
    EditorOperator::DateIsPresent,
];

// File availability is a present/missing toggle: the two operators are the
// only meaningful choices and neither takes a value.
const FILE_OPERATORS: &[EditorOperator] =
    &[EditorOperator::FileMissing, EditorOperator::FilePresent];
const BOOL_OPERATORS: &[EditorOperator] = &[EditorOperator::BoolEqual];

pub(super) fn operators_for_field(field: EditorField) -> &'static [EditorOperator] {
    match field {
        EditorField::Text(_) => TEXT_OPERATORS,
        EditorField::Number(_) => NUMBER_OPERATORS,
        EditorField::Rating => RATING_OPERATORS,
        EditorField::Date(_) => DATE_OPERATORS,
        EditorField::Bool(_) => BOOL_OPERATORS,
        EditorField::FileStatus => FILE_OPERATORS,
    }
}

pub(super) const MATCH_KINDS: &[(SmartPlaylistMatchKind, &str)] = &[
    (SmartPlaylistMatchKind::All, "all"),
    (SmartPlaylistMatchKind::Any, "any"),
];

pub(super) const LIMIT_SELECTIONS: &[(SmartPlaylistLimitSelection, &str)] = &[
    (SmartPlaylistLimitSelection::Random, "random"),
    (
        SmartPlaylistLimitSelection::MostRecentlyAdded,
        "most recently added",
    ),
    (
        SmartPlaylistLimitSelection::LeastRecentlyAdded,
        "least recently added",
    ),
    (
        SmartPlaylistLimitSelection::MostRecentlyPlayed,
        "most recently played",
    ),
    (
        SmartPlaylistLimitSelection::LeastRecentlyPlayed,
        "least recently played",
    ),
    (
        SmartPlaylistLimitSelection::MostOftenPlayed,
        "most often played",
    ),
    (
        SmartPlaylistLimitSelection::LeastOftenPlayed,
        "least often played",
    ),
    (SmartPlaylistLimitSelection::HighestRating, "highest rating"),
    (SmartPlaylistLimitSelection::LowestRating, "lowest rating"),
    (SmartPlaylistLimitSelection::TitleAscending, "title"),
    (SmartPlaylistLimitSelection::ArtistAscending, "artist"),
    (SmartPlaylistLimitSelection::AlbumAscending, "album"),
    (SmartPlaylistLimitSelection::GenreAscending, "genre"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum ValueInput {
    Text(String),
    Number(i64),
    Rating(u32),
    Date(String),
    Days(u32),
    Bool(bool),
    None,
}

pub(super) fn effective_value_kind(field: EditorField, operator: EditorOperator) -> ValueKind {
    match (field, operator.value_kind()) {
        (EditorField::Rating, ValueKind::Number) => ValueKind::Rating,
        (_, kind) => kind,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) enum RuleError {
    FieldOperatorMismatch,
    EmptyTextValue,
    OutOfRangeRating { offending: String },
    InvalidDate { offending: String },
    InvalidDays,
}

impl RuleError {
    pub(super) fn message(&self) -> String {
        match self {
            Self::FieldOperatorMismatch => {
                "This combination of field and operator is not supported.".to_owned()
            }
            Self::EmptyTextValue => "A text rule needs a value.".to_owned(),
            Self::OutOfRangeRating { offending } => {
                format!("\"{offending}\" is not a valid rating (0 to 5).")
            }
            Self::InvalidDate { offending } => {
                format!("\"{offending}\" is not a valid date (use YYYY-MM-DD).")
            }
            Self::InvalidDays => "The number of days must be at least 1.".to_owned(),
        }
    }
}

pub(super) fn extract_rule(
    field: EditorField,
    operator: EditorOperator,
    value: &ValueInput,
) -> Result<SmartPlaylistRule, RuleError> {
    match (field, operator) {
        (EditorField::Text(text_field), op) if matches!(op.value_kind(), ValueKind::Text) => {
            let raw = read_text(value)?;
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return Err(RuleError::EmptyTextValue);
            }
            Ok(SmartPlaylistRule::Text {
                field: text_field,
                operator: text_operator(op).ok_or(RuleError::FieldOperatorMismatch)?,
                value: trimmed.to_owned(),
            })
        }
        (EditorField::Text(text_field), EditorOperator::TextIsEmpty) => {
            Ok(SmartPlaylistRule::TextIsEmpty { field: text_field })
        }
        (EditorField::Text(text_field), EditorOperator::TextIsPresent) => {
            Ok(SmartPlaylistRule::TextIsPresent { field: text_field })
        }
        (EditorField::Number(number_field), op) if matches!(op.value_kind(), ValueKind::Number) => {
            let parsed = read_number(value)?;
            Ok(SmartPlaylistRule::Number {
                field: number_field,
                operator: number_operator(op).ok_or(RuleError::FieldOperatorMismatch)?,
                value: parsed,
            })
        }
        (EditorField::Number(number_field), EditorOperator::NumberIsEmpty) => {
            Ok(SmartPlaylistRule::NumberIsEmpty {
                field: number_field,
            })
        }
        (EditorField::Number(number_field), EditorOperator::NumberIsPresent) => {
            Ok(SmartPlaylistRule::NumberIsPresent {
                field: number_field,
            })
        }
        (EditorField::Rating, op) if matches!(op.value_kind(), ValueKind::Number) => {
            let parsed = read_rating(value)?;
            Ok(SmartPlaylistRule::Rating {
                operator: number_operator(op).ok_or(RuleError::FieldOperatorMismatch)?,
                value: parsed,
            })
        }
        (EditorField::Date(date_field), EditorOperator::DateBefore) => {
            Ok(SmartPlaylistRule::DateBefore {
                field: date_field,
                date: read_date(value)?,
            })
        }
        (EditorField::Date(date_field), EditorOperator::DateAfter) => {
            Ok(SmartPlaylistRule::DateAfter {
                field: date_field,
                date: read_date(value)?,
            })
        }
        (EditorField::Date(date_field), EditorOperator::DateInLast) => {
            Ok(SmartPlaylistRule::DateInLast {
                field: date_field,
                days: read_days(value)?,
            })
        }
        (EditorField::Date(date_field), EditorOperator::DateNotInLast) => {
            Ok(SmartPlaylistRule::DateNotInLast {
                field: date_field,
                days: read_days(value)?,
            })
        }
        (EditorField::Date(date_field), EditorOperator::DateIsEmpty) => {
            Ok(SmartPlaylistRule::DateIsEmpty { field: date_field })
        }
        (EditorField::Date(date_field), EditorOperator::DateIsPresent) => {
            Ok(SmartPlaylistRule::DateIsPresent { field: date_field })
        }
        (EditorField::Bool(bool_field), EditorOperator::BoolEqual) => {
            Ok(SmartPlaylistRule::Bool(SmartPlaylistBoolRule {
                field: bool_field,
                equals: read_bool(value)?,
            }))
        }
        (EditorField::FileStatus, EditorOperator::FileMissing) => {
            Ok(SmartPlaylistRule::FileIsMissing)
        }
        (EditorField::FileStatus, EditorOperator::FilePresent) => {
            Ok(SmartPlaylistRule::FileIsPresent)
        }
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_text(value: &ValueInput) -> Result<String, RuleError> {
    match value {
        ValueInput::Text(text) => Ok(text.clone()),
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_number(value: &ValueInput) -> Result<i64, RuleError> {
    match value {
        ValueInput::Number(number) => Ok(*number),
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_rating(value: &ValueInput) -> Result<Rating, RuleError> {
    match value {
        ValueInput::Rating(raw) => {
            let stars = u8::try_from(*raw).map_err(|_| RuleError::OutOfRangeRating {
                offending: raw.to_string(),
            })?;
            Rating::new(stars).ok_or(RuleError::OutOfRangeRating {
                offending: stars.to_string(),
            })
        }
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_date(value: &ValueInput) -> Result<SystemTime, RuleError> {
    match value {
        ValueInput::Date(text) => parse_iso_date(text.trim()).ok_or(RuleError::InvalidDate {
            offending: text.clone(),
        }),
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_days(value: &ValueInput) -> Result<NonZeroU32, RuleError> {
    match value {
        ValueInput::Days(raw) => NonZeroU32::new(*raw).ok_or(RuleError::InvalidDays),
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn read_bool(value: &ValueInput) -> Result<bool, RuleError> {
    match value {
        ValueInput::Bool(value) => Ok(*value),
        _ => Err(RuleError::FieldOperatorMismatch),
    }
}

fn text_operator(operator: EditorOperator) -> Option<SmartPlaylistTextOperator> {
    Some(match operator {
        EditorOperator::TextContains => SmartPlaylistTextOperator::Contains,
        EditorOperator::TextDoesNotContain => SmartPlaylistTextOperator::DoesNotContain,
        EditorOperator::TextIs => SmartPlaylistTextOperator::Is,
        EditorOperator::TextIsNot => SmartPlaylistTextOperator::IsNot,
        EditorOperator::TextStartsWith => SmartPlaylistTextOperator::StartsWith,
        EditorOperator::TextEndsWith => SmartPlaylistTextOperator::EndsWith,
        _ => return None,
    })
}

fn editor_operator_from_text(operator: SmartPlaylistTextOperator) -> EditorOperator {
    match operator {
        SmartPlaylistTextOperator::Contains => EditorOperator::TextContains,
        SmartPlaylistTextOperator::DoesNotContain => EditorOperator::TextDoesNotContain,
        SmartPlaylistTextOperator::Is => EditorOperator::TextIs,
        SmartPlaylistTextOperator::IsNot => EditorOperator::TextIsNot,
        SmartPlaylistTextOperator::StartsWith => EditorOperator::TextStartsWith,
        SmartPlaylistTextOperator::EndsWith => EditorOperator::TextEndsWith,
    }
}

fn number_operator(operator: EditorOperator) -> Option<SmartPlaylistNumberOperator> {
    Some(match operator {
        EditorOperator::NumberEqual => SmartPlaylistNumberOperator::Equal,
        EditorOperator::NumberNotEqual => SmartPlaylistNumberOperator::NotEqual,
        EditorOperator::NumberGreaterThan => SmartPlaylistNumberOperator::GreaterThan,
        EditorOperator::NumberGreaterThanOrEqual => SmartPlaylistNumberOperator::GreaterThanOrEqual,
        EditorOperator::NumberLessThan => SmartPlaylistNumberOperator::LessThan,
        EditorOperator::NumberLessThanOrEqual => SmartPlaylistNumberOperator::LessThanOrEqual,
        _ => return None,
    })
}

fn editor_operator_from_number(operator: SmartPlaylistNumberOperator) -> EditorOperator {
    match operator {
        SmartPlaylistNumberOperator::Equal => EditorOperator::NumberEqual,
        SmartPlaylistNumberOperator::NotEqual => EditorOperator::NumberNotEqual,
        SmartPlaylistNumberOperator::GreaterThan => EditorOperator::NumberGreaterThan,
        SmartPlaylistNumberOperator::GreaterThanOrEqual => EditorOperator::NumberGreaterThanOrEqual,
        SmartPlaylistNumberOperator::LessThan => EditorOperator::NumberLessThan,
        SmartPlaylistNumberOperator::LessThanOrEqual => EditorOperator::NumberLessThanOrEqual,
    }
}

pub(super) fn decompose_rule(
    rule: &SmartPlaylistRule,
) -> (EditorField, EditorOperator, ValueInput) {
    match rule {
        SmartPlaylistRule::Text {
            field,
            operator,
            value,
        } => (
            EditorField::Text(*field),
            editor_operator_from_text(*operator),
            ValueInput::Text(value.clone()),
        ),
        SmartPlaylistRule::TextIsEmpty { field } => (
            EditorField::Text(*field),
            EditorOperator::TextIsEmpty,
            ValueInput::None,
        ),
        SmartPlaylistRule::TextIsPresent { field } => (
            EditorField::Text(*field),
            EditorOperator::TextIsPresent,
            ValueInput::None,
        ),
        SmartPlaylistRule::Number {
            field,
            operator,
            value,
        } => (
            EditorField::Number(*field),
            editor_operator_from_number(*operator),
            ValueInput::Number(*value),
        ),
        SmartPlaylistRule::NumberIsEmpty { field } => (
            EditorField::Number(*field),
            EditorOperator::NumberIsEmpty,
            ValueInput::None,
        ),
        SmartPlaylistRule::NumberIsPresent { field } => (
            EditorField::Number(*field),
            EditorOperator::NumberIsPresent,
            ValueInput::None,
        ),
        SmartPlaylistRule::Rating { operator, value } => (
            EditorField::Rating,
            editor_operator_from_number(*operator),
            ValueInput::Rating(u32::from(value.stars())),
        ),
        SmartPlaylistRule::DateBefore { field, date } => (
            EditorField::Date(*field),
            EditorOperator::DateBefore,
            ValueInput::Date(format_iso_date(*date)),
        ),
        SmartPlaylistRule::DateAfter { field, date } => (
            EditorField::Date(*field),
            EditorOperator::DateAfter,
            ValueInput::Date(format_iso_date(*date)),
        ),
        SmartPlaylistRule::DateInLast { field, days } => (
            EditorField::Date(*field),
            EditorOperator::DateInLast,
            ValueInput::Days(days.get()),
        ),
        SmartPlaylistRule::DateNotInLast { field, days } => (
            EditorField::Date(*field),
            EditorOperator::DateNotInLast,
            ValueInput::Days(days.get()),
        ),
        SmartPlaylistRule::DateIsEmpty { field } => (
            EditorField::Date(*field),
            EditorOperator::DateIsEmpty,
            ValueInput::None,
        ),
        SmartPlaylistRule::DateIsPresent { field } => (
            EditorField::Date(*field),
            EditorOperator::DateIsPresent,
            ValueInput::None,
        ),
        SmartPlaylistRule::Bool(rule) => (
            EditorField::Bool(rule.field),
            EditorOperator::BoolEqual,
            ValueInput::Bool(rule.equals),
        ),
        SmartPlaylistRule::FileIsMissing => (
            EditorField::FileStatus,
            EditorOperator::FileMissing,
            ValueInput::None,
        ),
        SmartPlaylistRule::FileIsPresent => (
            EditorField::FileStatus,
            EditorOperator::FilePresent,
            ValueInput::None,
        ),
    }
}

pub(super) fn index_of_field(field: EditorField) -> u32 {
    EDITOR_FIELDS
        .iter()
        .position(|(f, _)| *f == field)
        .map(|i| i as u32)
        .unwrap_or(0)
}

pub(super) fn index_of_operator(field: EditorField, op: EditorOperator) -> u32 {
    operators_for_field(field)
        .iter()
        .position(|o| *o == op)
        .map(|i| i as u32)
        .unwrap_or(0)
}

pub(super) fn index_of_match_kind(kind: SmartPlaylistMatchKind) -> u32 {
    MATCH_KINDS
        .iter()
        .position(|(k, _)| *k == kind)
        .map(|i| i as u32)
        .unwrap_or(0)
}

pub(super) fn index_of_limit_selection(selection: SmartPlaylistLimitSelection) -> u32 {
    LIMIT_SELECTIONS
        .iter()
        .position(|(s, _)| *s == selection)
        .map(|i| i as u32)
        .unwrap_or(0)
}

pub(super) fn automatic_name_for_single_text_rule(
    rule_set: &SmartPlaylistRuleSet,
) -> Option<String> {
    let [SmartPlaylistRule::Text { value, .. }] = rule_set.rules.as_slice() else {
        return None;
    };
    let trimmed = value.trim();
    if is_short_single_line_text(trimmed) {
        Some(trimmed.to_owned())
    } else {
        None
    }
}

fn is_short_single_line_text(value: &str) -> bool {
    !value.is_empty() && value.chars().count() <= 64 && !value.chars().any(char::is_control)
}

fn format_iso_date(date: SystemTime) -> String {
    let seconds = date
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    let mut remaining_days = seconds / 86_400;
    let mut year: i32 = 1970;
    loop {
        let year_days = if is_leap_year(year) { 366 } else { 365 } as u64;
        if remaining_days < year_days {
            break;
        }
        remaining_days -= year_days;
        year += 1;
    }
    let mut month: u32 = 1;
    while month <= 12 {
        let month_days = u64::from(days_in_month(year, month));
        if remaining_days < month_days {
            break;
        }
        remaining_days -= month_days;
        month += 1;
    }
    let day = (remaining_days + 1) as u32;
    format!("{year:04}-{month:02}-{day:02}")
}

fn parse_iso_date(text: &str) -> Option<SystemTime> {
    let mut parts = text.split('-');
    let year: i32 = parts.next()?.parse().ok()?;
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    if !(1970..=9999).contains(&year) || !(1..=12).contains(&month) {
        return None;
    }
    let max_day = days_in_month(year, month);
    if !(1..=max_day).contains(&day) {
        return None;
    }
    let days = days_since_unix_epoch(year, month, day)?;
    Some(SystemTime::UNIX_EPOCH + Duration::from_secs(days * 86_400))
}

fn days_in_month(year: i32, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(year) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

fn is_leap_year(year: i32) -> bool {
    (year % 4 == 0 && year % 100 != 0) || year % 400 == 0
}

fn days_since_unix_epoch(year: i32, month: u32, day: u32) -> Option<u64> {
    if year < 1970 {
        return None;
    }
    let mut days: u64 = 0;
    for y in 1970..year {
        days += if is_leap_year(y) { 366 } else { 365 };
    }
    for m in 1..month {
        days += u64::from(days_in_month(year, m));
    }
    days += u64::from(day - 1);
    Some(days)
}

#[cfg(test)]
#[path = "model_tests.rs"]
mod tests;
