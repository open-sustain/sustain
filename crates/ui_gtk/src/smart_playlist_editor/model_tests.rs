// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use super::*;

#[test]
fn parse_iso_date_accepts_padded_dates() {
    let date = parse_iso_date("2024-05-23").expect("valid date");
    let elapsed = date
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("after epoch");
    let expected_days: u64 = (1970..2024)
        .map(|y| if is_leap_year(y) { 366 } else { 365 })
        .sum::<u64>()
        + 31
        + 29
        + 31
        + 30
        + 22;
    assert_eq!(elapsed.as_secs(), expected_days * 86_400);
}

#[test]
fn parse_iso_date_rejects_invalid_dates() {
    assert!(parse_iso_date("2024-02-30").is_none());
    assert!(parse_iso_date("2024-13-01").is_none());
    assert!(parse_iso_date("not-a-date").is_none());
    assert!(parse_iso_date("2024-05").is_none());
    assert!(parse_iso_date("2024-05-23-extra").is_none());
    assert!(parse_iso_date("1969-12-31").is_none());
}

#[test]
fn leap_year_rules_match_gregorian_calendar() {
    assert!(is_leap_year(2000));
    assert!(!is_leap_year(1900));
    assert!(is_leap_year(2024));
    assert!(!is_leap_year(2023));
}

#[test]
fn extract_rule_text_is_empty_creates_text_is_empty_variant() {
    let rule = extract_rule(
        EditorField::Text(SmartPlaylistTextField::Genre),
        EditorOperator::TextIsEmpty,
        &ValueInput::None,
    )
    .expect("extracts");
    assert_eq!(
        rule,
        SmartPlaylistRule::TextIsEmpty {
            field: SmartPlaylistTextField::Genre,
        }
    );
}

#[test]
fn extract_rule_number_is_empty_creates_number_is_empty_variant() {
    let rule = extract_rule(
        EditorField::Number(SmartPlaylistNumberField::Year),
        EditorOperator::NumberIsEmpty,
        &ValueInput::None,
    )
    .expect("extracts");
    assert_eq!(
        rule,
        SmartPlaylistRule::NumberIsEmpty {
            field: SmartPlaylistNumberField::Year,
        }
    );
}

#[test]
fn file_status_rules_round_trip_through_the_editor_model() {
    // The File field offers exactly the two availability operators, each
    // valueless, and round-trips to and from the domain rule (#79).
    assert_eq!(
        operators_for_field(EditorField::FileStatus),
        &[EditorOperator::FileMissing, EditorOperator::FilePresent]
    );

    for (operator, rule) in [
        (
            EditorOperator::FileMissing,
            SmartPlaylistRule::FileIsMissing,
        ),
        (
            EditorOperator::FilePresent,
            SmartPlaylistRule::FileIsPresent,
        ),
    ] {
        let extracted = extract_rule(EditorField::FileStatus, operator, &ValueInput::None)
            .expect("file-status rule extracts without a value");
        assert_eq!(extracted, rule);

        let (field, decomposed_operator, value) = decompose_rule(&rule);
        assert_eq!(field, EditorField::FileStatus);
        assert_eq!(decomposed_operator, operator);
        assert_eq!(value, ValueInput::None);
    }
}

#[test]
fn lyrics_bool_rule_round_trips_through_the_editor_model() {
    let field = EditorField::Bool(SmartPlaylistBoolField::HasLyrics);
    assert_eq!(operators_for_field(field), &[EditorOperator::BoolEqual]);

    for equals in [true, false] {
        let rule = SmartPlaylistRule::Bool(SmartPlaylistBoolRule {
            field: SmartPlaylistBoolField::HasLyrics,
            equals,
        });
        assert_eq!(
            extract_rule(field, EditorOperator::BoolEqual, &ValueInput::Bool(equals)),
            Ok(rule.clone())
        );
        assert_eq!(
            decompose_rule(&rule),
            (field, EditorOperator::BoolEqual, ValueInput::Bool(equals))
        );
    }
}

#[test]
fn rating_field_does_not_offer_empty_present_operators() {
    let rating_operators = operators_for_field(EditorField::Rating);
    assert!(!rating_operators.contains(&EditorOperator::NumberIsEmpty));
    assert!(!rating_operators.contains(&EditorOperator::NumberIsPresent));
    let number_operators = operators_for_field(EditorField::Number(SmartPlaylistNumberField::Year));
    assert!(number_operators.contains(&EditorOperator::NumberIsEmpty));
}

#[test]
fn extract_rule_rating_constructs_rating_variant_with_parsed_stars() {
    let rule = extract_rule(
        EditorField::Rating,
        EditorOperator::NumberGreaterThanOrEqual,
        &ValueInput::Rating(4),
    )
    .expect("extracts");
    assert_eq!(
        rule,
        SmartPlaylistRule::Rating {
            operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
            value: Rating::new(4).expect("valid"),
        }
    );
}

#[test]
fn extract_rule_rating_rejects_out_of_range_value() {
    let result = extract_rule(
        EditorField::Rating,
        EditorOperator::NumberEqual,
        &ValueInput::Rating(9),
    );
    assert_eq!(
        result,
        Err(RuleError::OutOfRangeRating {
            offending: "9".to_owned(),
        })
    );
}

#[test]
fn extract_rule_date_before_parses_iso_value() {
    let rule = extract_rule(
        EditorField::Date(SmartPlaylistDateField::DateAdded),
        EditorOperator::DateBefore,
        &ValueInput::Date("2024-05-23".to_owned()),
    )
    .expect("extracts");
    match rule {
        SmartPlaylistRule::DateBefore { field, date } => {
            assert_eq!(field, SmartPlaylistDateField::DateAdded);
            let expected = parse_iso_date("2024-05-23").expect("valid");
            assert_eq!(date, expected);
        }
        other => assert_eq!(
            other,
            SmartPlaylistRule::DateBefore {
                field: SmartPlaylistDateField::DateAdded,
                date: parse_iso_date("2024-05-23").expect("valid"),
            }
        ),
    }
}

#[test]
fn extract_rule_date_before_rejects_invalid_iso() {
    let result = extract_rule(
        EditorField::Date(SmartPlaylistDateField::DateAdded),
        EditorOperator::DateBefore,
        &ValueInput::Date("nope".to_owned()),
    );
    assert_eq!(
        result,
        Err(RuleError::InvalidDate {
            offending: "nope".to_owned(),
        })
    );
}

#[test]
fn extract_rule_date_in_last_uses_days() {
    let rule = extract_rule(
        EditorField::Date(SmartPlaylistDateField::LastPlayed),
        EditorOperator::DateInLast,
        &ValueInput::Days(14),
    )
    .expect("extracts");
    match rule {
        SmartPlaylistRule::DateInLast { field, days } => {
            assert_eq!(field, SmartPlaylistDateField::LastPlayed);
            assert_eq!(days.get(), 14);
        }
        other => assert_eq!(
            other,
            SmartPlaylistRule::DateInLast {
                field: SmartPlaylistDateField::LastPlayed,
                days: NonZeroU32::new(14).expect("positive day count"),
            }
        ),
    }
}

#[test]
fn automatic_name_for_single_text_rule_uses_trimmed_text_value() {
    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Artist,
            operator: SmartPlaylistTextOperator::Is,
            value: "  Radiohead  ".to_owned(),
        }],
        limit: None,
    };

    assert_eq!(
        automatic_name_for_single_text_rule(&rules),
        Some("Radiohead".to_owned())
    );
}

#[test]
fn automatic_name_for_single_text_rule_ignores_non_text_or_multi_rule_sets() {
    let multi = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![
            SmartPlaylistRule::Text {
                field: SmartPlaylistTextField::Artist,
                operator: SmartPlaylistTextOperator::Is,
                value: "Radiohead".to_owned(),
            },
            SmartPlaylistRule::TextIsPresent {
                field: SmartPlaylistTextField::Album,
            },
        ],
        limit: None,
    };
    let non_text = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::TextIsPresent {
            field: SmartPlaylistTextField::Artist,
        }],
        limit: None,
    };

    assert_eq!(automatic_name_for_single_text_rule(&multi), None);
    assert_eq!(automatic_name_for_single_text_rule(&non_text), None);
}

#[test]
fn extract_rule_date_in_last_rejects_zero_days() {
    let result = extract_rule(
        EditorField::Date(SmartPlaylistDateField::LastPlayed),
        EditorOperator::DateInLast,
        &ValueInput::Days(0),
    );
    assert_eq!(result, Err(RuleError::InvalidDays));
}

#[test]
fn effective_value_kind_uses_rating_for_rating_field() {
    assert_eq!(
        effective_value_kind(EditorField::Rating, EditorOperator::NumberEqual),
        ValueKind::Rating
    );
    assert_eq!(
        effective_value_kind(
            EditorField::Number(SmartPlaylistNumberField::Year),
            EditorOperator::NumberEqual,
        ),
        ValueKind::Number
    );
}
