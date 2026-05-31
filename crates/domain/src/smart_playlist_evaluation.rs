// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cmp::Reverse,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    SmartPlaylistDateField, SmartPlaylistLimit, SmartPlaylistLimitSelection,
    SmartPlaylistMatchKind, SmartPlaylistNumberField, SmartPlaylistNumberOperator,
    SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField, SmartPlaylistTextOperator,
    Track,
};

const SECONDS_PER_DAY: u64 = 86_400;

pub fn matching_tracks<'a>(
    tracks: &'a [Track],
    rules: &SmartPlaylistRuleSet,
    now: SystemTime,
) -> Vec<&'a Track> {
    let mut matched: Vec<&Track> = tracks
        .iter()
        .filter(|track| track_matches_rule_set(track, rules, now))
        .collect();

    if let Some(limit) = rules.limit {
        apply_limit(&mut matched, limit, now);
    }

    matched
}

pub fn track_matches_rule_set(
    track: &Track,
    rules: &SmartPlaylistRuleSet,
    now: SystemTime,
) -> bool {
    if rules.rules.is_empty() {
        return false;
    }

    match rules.match_kind {
        SmartPlaylistMatchKind::All => rules
            .rules
            .iter()
            .all(|rule| track_matches_rule(track, rule, now)),
        SmartPlaylistMatchKind::Any => rules
            .rules
            .iter()
            .any(|rule| track_matches_rule(track, rule, now)),
    }
}

pub fn track_matches_rule(track: &Track, rule: &SmartPlaylistRule, now: SystemTime) -> bool {
    match rule {
        SmartPlaylistRule::Text {
            field,
            operator,
            value,
        } => evaluate_text(text_field_value(track, *field).as_deref(), *operator, value),
        SmartPlaylistRule::TextIsEmpty { field } => text_field_value(track, *field)
            .map(|value| value.trim().is_empty())
            .unwrap_or(true),
        SmartPlaylistRule::TextIsPresent { field } => text_field_value(track, *field)
            .map(|value| !value.trim().is_empty())
            .unwrap_or(false),
        SmartPlaylistRule::Number {
            field,
            operator,
            value,
        } => evaluate_number(number_field_value(track, *field), *operator, *value),
        SmartPlaylistRule::NumberIsEmpty { field } => number_field_value(track, *field).is_none(),
        SmartPlaylistRule::NumberIsPresent { field } => number_field_value(track, *field).is_some(),
        SmartPlaylistRule::Rating { operator, value } => evaluate_number(
            Some(i64::from(track.rating.stars())),
            *operator,
            i64::from(value.stars()),
        ),
        SmartPlaylistRule::DateBefore { field, date } => {
            date_field_value(track, *field).is_some_and(|track_date| track_date < *date)
        }
        SmartPlaylistRule::DateAfter { field, date } => {
            date_field_value(track, *field).is_some_and(|track_date| track_date > *date)
        }
        SmartPlaylistRule::DateInLast { field, days } => {
            let cutoff = now
                .checked_sub(Duration::from_secs(u64::from(days.get()) * SECONDS_PER_DAY))
                .unwrap_or(SystemTime::UNIX_EPOCH);
            date_field_value(track, *field).is_some_and(|track_date| track_date >= cutoff)
        }
        SmartPlaylistRule::DateNotInLast { field, days } => {
            let cutoff = now
                .checked_sub(Duration::from_secs(u64::from(days.get()) * SECONDS_PER_DAY))
                .unwrap_or(SystemTime::UNIX_EPOCH);
            match date_field_value(track, *field) {
                Some(track_date) => track_date < cutoff,
                None => true,
            }
        }
        SmartPlaylistRule::DateIsEmpty { field } => date_field_value(track, *field).is_none(),
        SmartPlaylistRule::DateIsPresent { field } => date_field_value(track, *field).is_some(),
        SmartPlaylistRule::FileIsMissing => track.location.is_missing(),
        SmartPlaylistRule::FileIsPresent => !track.location.is_missing(),
    }
}

fn text_field_value(track: &Track, field: SmartPlaylistTextField) -> Option<String> {
    match field {
        SmartPlaylistTextField::Title => track.metadata.title.clone(),
        SmartPlaylistTextField::Artist => track.metadata.artist.clone(),
        SmartPlaylistTextField::Album => track.metadata.album.clone(),
        SmartPlaylistTextField::AlbumArtist => track.metadata.album_artist.clone(),
        SmartPlaylistTextField::Composer => track.metadata.composer.clone(),
        SmartPlaylistTextField::Genre => track.metadata.genre.clone(),
        SmartPlaylistTextField::FileName => track
            .location
            .path()
            .file_name()
            .and_then(|os_str| os_str.to_str())
            .map(str::to_owned),
        SmartPlaylistTextField::MusicalKey => track.metadata.key.clone(),
    }
}

fn number_field_value(track: &Track, field: SmartPlaylistNumberField) -> Option<i64> {
    match field {
        SmartPlaylistNumberField::PlayCount => Some(track.statistics.play_count as i64),
        SmartPlaylistNumberField::SkipCount => Some(track.statistics.skip_count as i64),
        SmartPlaylistNumberField::TrackNumber => track.metadata.track_number.map(i64::from),
        SmartPlaylistNumberField::DiscNumber => track.metadata.disc_number.map(i64::from),
        SmartPlaylistNumberField::Year => track.metadata.year.map(i64::from),
        SmartPlaylistNumberField::DurationSeconds => track
            .metadata
            .duration
            .map(|duration| duration.as_secs() as i64),
        SmartPlaylistNumberField::BitrateKbps => track.metadata.bitrate_kbps.map(i64::from),
        SmartPlaylistNumberField::Bpm => track.metadata.bpm.map(i64::from),
    }
}

fn date_field_value(track: &Track, field: SmartPlaylistDateField) -> Option<SystemTime> {
    match field {
        SmartPlaylistDateField::DateAdded => track.statistics.date_added_at,
        SmartPlaylistDateField::LastPlayed => track.statistics.last_played_at,
        SmartPlaylistDateField::LastSkipped => track.statistics.last_skipped_at,
    }
}

fn evaluate_text(
    track_value: Option<&str>,
    operator: SmartPlaylistTextOperator,
    rule_value: &str,
) -> bool {
    let Some(track_value) = track_value else {
        return false;
    };
    let track = track_value.to_lowercase();
    let needle = rule_value.to_lowercase();
    match operator {
        SmartPlaylistTextOperator::Contains => track.contains(&needle),
        SmartPlaylistTextOperator::DoesNotContain => !track.contains(&needle),
        SmartPlaylistTextOperator::Is => track == needle,
        SmartPlaylistTextOperator::IsNot => track != needle,
        SmartPlaylistTextOperator::StartsWith => track.starts_with(&needle),
        SmartPlaylistTextOperator::EndsWith => track.ends_with(&needle),
    }
}

fn evaluate_number(
    track_value: Option<i64>,
    operator: SmartPlaylistNumberOperator,
    rule_value: i64,
) -> bool {
    let Some(track_value) = track_value else {
        return false;
    };
    match operator {
        SmartPlaylistNumberOperator::Equal => track_value == rule_value,
        SmartPlaylistNumberOperator::NotEqual => track_value != rule_value,
        SmartPlaylistNumberOperator::GreaterThan => track_value > rule_value,
        SmartPlaylistNumberOperator::GreaterThanOrEqual => track_value >= rule_value,
        SmartPlaylistNumberOperator::LessThan => track_value < rule_value,
        SmartPlaylistNumberOperator::LessThanOrEqual => track_value <= rule_value,
    }
}

fn apply_limit(tracks: &mut Vec<&Track>, limit: SmartPlaylistLimit, now: SystemTime) {
    sort_for_selection(tracks, limit.selection, now);
    tracks.truncate(limit.count.get() as usize);
}

fn sort_for_selection(
    tracks: &mut [&Track],
    selection: SmartPlaylistLimitSelection,
    now: SystemTime,
) {
    match selection {
        SmartPlaylistLimitSelection::Random => {
            let seed = random_seed(now);
            tracks.sort_by_key(|track| pseudo_random_key(track.id.get(), seed));
        }
        SmartPlaylistLimitSelection::TitleAscending => {
            tracks.sort_by(|left, right| {
                ci_string(left.metadata.title.as_deref())
                    .cmp(&ci_string(right.metadata.title.as_deref()))
            });
        }
        SmartPlaylistLimitSelection::ArtistAscending => {
            tracks.sort_by(|left, right| {
                ci_string(left.metadata.artist.as_deref())
                    .cmp(&ci_string(right.metadata.artist.as_deref()))
            });
        }
        SmartPlaylistLimitSelection::AlbumAscending => {
            tracks.sort_by(|left, right| {
                ci_string(left.metadata.album.as_deref())
                    .cmp(&ci_string(right.metadata.album.as_deref()))
            });
        }
        SmartPlaylistLimitSelection::GenreAscending => {
            tracks.sort_by(|left, right| {
                ci_string(left.metadata.genre.as_deref())
                    .cmp(&ci_string(right.metadata.genre.as_deref()))
            });
        }
        SmartPlaylistLimitSelection::HighestRating => {
            tracks.sort_by_key(|track| Reverse(track.rating.stars()));
        }
        SmartPlaylistLimitSelection::LowestRating => {
            tracks.sort_by_key(|track| track.rating.stars());
        }
        SmartPlaylistLimitSelection::MostRecentlyPlayed => {
            tracks.sort_by_key(|track| Reverse(track.statistics.last_played_at));
        }
        SmartPlaylistLimitSelection::LeastRecentlyPlayed => {
            tracks.sort_by_key(|track| track.statistics.last_played_at);
        }
        SmartPlaylistLimitSelection::MostOftenPlayed => {
            tracks.sort_by_key(|track| Reverse(track.statistics.play_count));
        }
        SmartPlaylistLimitSelection::LeastOftenPlayed => {
            tracks.sort_by_key(|track| track.statistics.play_count);
        }
        SmartPlaylistLimitSelection::MostRecentlyAdded => {
            tracks.sort_by_key(|track| Reverse(track.statistics.date_added_at));
        }
        SmartPlaylistLimitSelection::LeastRecentlyAdded => {
            tracks.sort_by_key(|track| track.statistics.date_added_at);
        }
    }
}

fn ci_string(value: Option<&str>) -> String {
    value.unwrap_or("").to_lowercase()
}

fn random_seed(now: SystemTime) -> u64 {
    let nanos = now
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    (nanos as u64) ^ ((nanos >> 64) as u64)
}

fn pseudo_random_key(track_id: i64, seed: u64) -> u64 {
    splitmix64((track_id as u64) ^ seed)
}

fn splitmix64(mut value: u64) -> u64 {
    value = value.wrapping_add(0x9e37_79b9_7f4a_7c15);
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

#[cfg(test)]
#[path = "smart_playlist_evaluation_tests.rs"]
mod tests;
