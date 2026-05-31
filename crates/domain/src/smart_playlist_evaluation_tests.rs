// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::num::NonZeroU32;
use std::time::{Duration, SystemTime};

use crate::{
    PlayStatistics, Rating, SmartPlaylistDateField, SmartPlaylistLimit,
    SmartPlaylistLimitSelection, SmartPlaylistMatchKind, SmartPlaylistNumberField,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField,
    SmartPlaylistTextOperator, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
};

use super::{matching_tracks, track_matches_rule, track_matches_rule_set};

fn unix(seconds: u64) -> SystemTime {
    SystemTime::UNIX_EPOCH + Duration::from_secs(seconds)
}

fn relative(path: &str) -> TrackRelativePath {
    TrackRelativePath::new(path).expect("relative path is valid")
}

fn track(id: i64, genre: Option<&str>, play_count: u64, year: Option<i32>) -> Track {
    Track {
        id: TrackId::new(id).expect("positive id"),
        location: TrackLocation::available(relative(&format!("track-{id}.flac"))),
        metadata: TrackMetadata {
            title: Some(format!("Title {id}")),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            genre: genre.map(str::to_owned),
            year,
            duration: Some(Duration::from_secs(200)),
            bitrate_kbps: Some(320),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics {
            play_count,
            skip_count: 0,
            last_played_at: None,
            last_skipped_at: None,
            date_added_at: Some(unix(1_000)),
        },
        file_size_bytes: None,
        has_embedded_artwork: None,
    }
}

#[test]
fn text_contains_is_case_insensitive() {
    let track = track(1, Some("Trip-Hop"), 0, None);
    let rule = SmartPlaylistRule::Text {
        field: SmartPlaylistTextField::Genre,
        operator: SmartPlaylistTextOperator::Contains,
        value: "trip".to_owned(),
    };

    assert!(track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn text_is_not_rejects_exact_match() {
    let track = track(1, Some("Jazz"), 0, None);
    let rule = SmartPlaylistRule::Text {
        field: SmartPlaylistTextField::Genre,
        operator: SmartPlaylistTextOperator::IsNot,
        value: "Jazz".to_owned(),
    };

    assert!(!track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn missing_text_field_does_not_match_text_rule() {
    let track = track(1, None, 0, None);
    let rule = SmartPlaylistRule::Text {
        field: SmartPlaylistTextField::Genre,
        operator: SmartPlaylistTextOperator::Contains,
        value: "Rock".to_owned(),
    };

    assert!(!track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn number_greater_than_or_equal_matches_play_count() {
    let track = track(1, Some("Jazz"), 5, None);
    let rule = SmartPlaylistRule::Number {
        field: SmartPlaylistNumberField::PlayCount,
        operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
        value: 5,
    };

    assert!(track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn missing_number_field_does_not_match_number_rule() {
    let track = track(1, Some("Jazz"), 0, None);
    let rule = SmartPlaylistRule::Number {
        field: SmartPlaylistNumberField::Year,
        operator: SmartPlaylistNumberOperator::LessThan,
        value: 2000,
    };

    assert!(!track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn number_is_empty_matches_only_missing_numeric_field() {
    let with_year = track(1, Some("Jazz"), 0, Some(1999));
    let without_year = track(2, Some("Jazz"), 0, None);
    let rule = SmartPlaylistRule::NumberIsEmpty {
        field: SmartPlaylistNumberField::Year,
    };

    assert!(!track_matches_rule(&with_year, &rule, unix(2_000)));
    assert!(track_matches_rule(&without_year, &rule, unix(2_000)));
}

#[test]
fn number_is_present_matches_only_populated_numeric_field() {
    let with_year = track(1, Some("Jazz"), 0, Some(1999));
    let without_year = track(2, Some("Jazz"), 0, None);
    let rule = SmartPlaylistRule::NumberIsPresent {
        field: SmartPlaylistNumberField::Year,
    };

    assert!(track_matches_rule(&with_year, &rule, unix(2_000)));
    assert!(!track_matches_rule(&without_year, &rule, unix(2_000)));
}

#[test]
fn rating_rule_uses_rating_stars() {
    let mut track = track(1, Some("Jazz"), 0, None);
    track.rating = Rating::new(4).expect("valid rating");
    let rule = SmartPlaylistRule::Rating {
        operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
        value: Rating::new(4).expect("valid rating"),
    };

    assert!(track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn date_in_last_uses_injected_clock() {
    let mut track = track(1, Some("Jazz"), 0, None);
    track.statistics.last_played_at = Some(unix(900_000));
    let rule = SmartPlaylistRule::DateInLast {
        field: SmartPlaylistDateField::LastPlayed,
        days: NonZeroU32::new(2).expect("positive days"),
    };

    let now_inside_window = unix(900_000 + 86_400);
    let now_outside_window = unix(900_000 + 5 * 86_400);

    assert!(track_matches_rule(&track, &rule, now_inside_window));
    assert!(!track_matches_rule(&track, &rule, now_outside_window));
}

#[test]
fn date_not_in_last_treats_missing_date_as_matching() {
    let track = track(1, Some("Jazz"), 0, None);
    let rule = SmartPlaylistRule::DateNotInLast {
        field: SmartPlaylistDateField::LastPlayed,
        days: NonZeroU32::new(7).expect("positive days"),
    };

    assert!(track_matches_rule(&track, &rule, unix(2_000)));
}

#[test]
fn match_all_requires_every_rule_to_match() {
    let track = track(1, Some("Jazz"), 5, Some(1995));
    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![
            SmartPlaylistRule::Text {
                field: SmartPlaylistTextField::Genre,
                operator: SmartPlaylistTextOperator::Is,
                value: "Jazz".to_owned(),
            },
            SmartPlaylistRule::Number {
                field: SmartPlaylistNumberField::Year,
                operator: SmartPlaylistNumberOperator::LessThan,
                value: 2000,
            },
        ],
        limit: None,
    };

    assert!(track_matches_rule_set(&track, &rules, unix(2_000)));
}

#[test]
fn match_any_passes_when_one_rule_matches() {
    let track = track(1, Some("Jazz"), 0, Some(2010));
    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::Any,
        rules: vec![
            SmartPlaylistRule::Text {
                field: SmartPlaylistTextField::Genre,
                operator: SmartPlaylistTextOperator::Is,
                value: "Jazz".to_owned(),
            },
            SmartPlaylistRule::Number {
                field: SmartPlaylistNumberField::Year,
                operator: SmartPlaylistNumberOperator::LessThan,
                value: 2000,
            },
        ],
        limit: None,
    };

    assert!(track_matches_rule_set(&track, &rules, unix(2_000)));
}

#[test]
fn empty_rule_set_matches_nothing() {
    let track = track(1, Some("Jazz"), 0, None);
    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: Vec::new(),
        limit: None,
    };

    assert!(!track_matches_rule_set(&track, &rules, unix(2_000)));
}

#[test]
fn limit_truncates_to_count_after_sorting_by_play_count() {
    let mut tracks = vec![
        track(1, Some("Jazz"), 10, None),
        track(2, Some("Jazz"), 1, None),
        track(3, Some("Jazz"), 5, None),
        track(4, Some("Jazz"), 7, None),
    ];
    for (index, value) in [10_u64, 1, 5, 7].iter().enumerate() {
        tracks[index].statistics.play_count = *value;
    }

    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Genre,
            operator: SmartPlaylistTextOperator::Is,
            value: "Jazz".to_owned(),
        }],
        limit: Some(SmartPlaylistLimit {
            count: NonZeroU32::new(2).expect("positive count"),
            selection: SmartPlaylistLimitSelection::MostOftenPlayed,
        }),
    };

    let matched = matching_tracks(&tracks, &rules, unix(2_000));
    let matched_ids: Vec<i64> = matched.iter().map(|track| track.id.get()).collect();

    assert_eq!(matched_ids, vec![1, 4]);
}

#[test]
fn limit_sorted_by_most_recently_added_puts_newer_first() {
    let mut tracks = vec![
        track(1, Some("Jazz"), 0, None),
        track(2, Some("Jazz"), 0, None),
        track(3, Some("Jazz"), 0, None),
    ];
    tracks[0].statistics.date_added_at = Some(unix(100));
    tracks[1].statistics.date_added_at = Some(unix(300));
    tracks[2].statistics.date_added_at = Some(unix(200));

    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Genre,
            operator: SmartPlaylistTextOperator::Is,
            value: "Jazz".to_owned(),
        }],
        limit: Some(SmartPlaylistLimit {
            count: NonZeroU32::new(3).expect("positive count"),
            selection: SmartPlaylistLimitSelection::MostRecentlyAdded,
        }),
    };

    let matched = matching_tracks(&tracks, &rules, unix(2_000));
    let matched_ids: Vec<i64> = matched.iter().map(|track| track.id.get()).collect();

    assert_eq!(matched_ids, vec![2, 3, 1]);
}

#[test]
fn random_limit_order_is_seeded_by_the_injected_clock() {
    let tracks = vec![
        track(1, Some("Jazz"), 0, None),
        track(2, Some("Jazz"), 0, None),
        track(3, Some("Jazz"), 0, None),
        track(4, Some("Jazz"), 0, None),
        track(5, Some("Jazz"), 0, None),
    ];
    let rules = SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Genre,
            operator: SmartPlaylistTextOperator::Is,
            value: "Jazz".to_owned(),
        }],
        limit: Some(SmartPlaylistLimit {
            count: NonZeroU32::new(5).expect("positive count"),
            selection: SmartPlaylistLimitSelection::Random,
        }),
    };

    let first = matching_tracks(&tracks, &rules, unix(2_000));
    let second = matching_tracks(&tracks, &rules, unix(2_000));
    let later = matching_tracks(&tracks, &rules, unix(2_001));
    let first_ids: Vec<i64> = first.iter().map(|track| track.id.get()).collect();
    let second_ids: Vec<i64> = second.iter().map(|track| track.id.get()).collect();
    let later_ids: Vec<i64> = later.iter().map(|track| track.id.get()).collect();

    assert_eq!(first_ids, second_ids);
    assert_ne!(first_ids, vec![1, 2, 3, 4, 5]);
    assert_ne!(first_ids, later_ids);
}
