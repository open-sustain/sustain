// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cmp::Reverse,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    SmartPlaylistBoolField, SmartPlaylistDateField, SmartPlaylistLimit,
    SmartPlaylistLimitSelection, SmartPlaylistMatchKind, SmartPlaylistNumberField,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField,
    SmartPlaylistTextOperator, Track, normalize_search_text,
};

const SECONDS_PER_DAY: u64 = 86_400;

pub fn matching_tracks<'a>(
    tracks: &'a [Track],
    rules: &SmartPlaylistRuleSet,
    now: SystemTime,
) -> Vec<&'a Track> {
    let prepared = PreparedRuleSet::new(rules, now);
    let mut matched: Vec<&Track> = tracks
        .iter()
        .filter(|track| prepared.matches(track))
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
    PreparedRuleSet::new(rules, now).matches(track)
}

pub fn track_matches_rule(track: &Track, rule: &SmartPlaylistRule, now: SystemTime) -> bool {
    rule_matches(track, rule, &rule_constants(rule, now))
}

/// A rule set with its per-rule constants resolved once for one
/// evaluation pass — text needles search-normalized up front,
/// relative-date windows anchored to a single cutoff — so evaluating a
/// 10,000-track library does not redo that work per track.
struct PreparedRuleSet<'rules> {
    match_kind: SmartPlaylistMatchKind,
    rules: Vec<(&'rules SmartPlaylistRule, RuleConstants)>,
}

impl<'rules> PreparedRuleSet<'rules> {
    fn new(rules: &'rules SmartPlaylistRuleSet, now: SystemTime) -> Self {
        Self {
            match_kind: rules.match_kind,
            rules: rules
                .rules
                .iter()
                .map(|rule| (rule, rule_constants(rule, now)))
                .collect(),
        }
    }

    /// Whether `track` matches the rule set. An empty rule set matches
    /// nothing.
    fn matches(&self, track: &Track) -> bool {
        if self.rules.is_empty() {
            return false;
        }

        match self.match_kind {
            SmartPlaylistMatchKind::All => self
                .rules
                .iter()
                .all(|(rule, constants)| rule_matches(track, rule, constants)),
            SmartPlaylistMatchKind::Any => self
                .rules
                .iter()
                .any(|(rule, constants)| rule_matches(track, rule, constants)),
        }
    }
}

/// One rule's evaluation-ready constants: whatever the rule can resolve
/// once per pass instead of once per track. Built by [`rule_constants`]
/// from the same rule it is later matched against.
enum RuleConstants {
    None,
    /// The text rule's value, already [`normalize_search_text`]-folded.
    TextNeedle(String),
    /// The absolute cutoff a relative-date rule compares against.
    DateCutoff(SystemTime),
}

fn rule_constants(rule: &SmartPlaylistRule, now: SystemTime) -> RuleConstants {
    match rule {
        SmartPlaylistRule::Text { value, .. } => {
            RuleConstants::TextNeedle(normalize_search_text(value))
        }
        SmartPlaylistRule::DateInLast { days, .. }
        | SmartPlaylistRule::DateNotInLast { days, .. } => {
            RuleConstants::DateCutoff(relative_date_cutoff(now, days.get()))
        }
        SmartPlaylistRule::TextIsEmpty { .. }
        | SmartPlaylistRule::TextIsPresent { .. }
        | SmartPlaylistRule::Number { .. }
        | SmartPlaylistRule::NumberIsEmpty { .. }
        | SmartPlaylistRule::NumberIsPresent { .. }
        | SmartPlaylistRule::Rating { .. }
        | SmartPlaylistRule::DateBefore { .. }
        | SmartPlaylistRule::DateAfter { .. }
        | SmartPlaylistRule::DateIsEmpty { .. }
        | SmartPlaylistRule::DateIsPresent { .. }
        | SmartPlaylistRule::Bool(_)
        | SmartPlaylistRule::FileIsMissing
        | SmartPlaylistRule::FileIsPresent => RuleConstants::None,
    }
}

fn relative_date_cutoff(now: SystemTime, days: u32) -> SystemTime {
    now.checked_sub(Duration::from_secs(u64::from(days) * SECONDS_PER_DAY))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// Evaluates one rule against one track. `constants` must come from
/// [`rule_constants`] for this same rule; the `unreachable!` arms guard
/// that pairing.
fn rule_matches(track: &Track, rule: &SmartPlaylistRule, constants: &RuleConstants) -> bool {
    match rule {
        SmartPlaylistRule::Text {
            field, operator, ..
        } => {
            let RuleConstants::TextNeedle(needle) = constants else {
                unreachable!("text rule prepared without a needle");
            };
            evaluate_text(text_field_value(track, *field), *operator, needle)
        }
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
        SmartPlaylistRule::DateInLast { field, .. } => {
            let RuleConstants::DateCutoff(cutoff) = constants else {
                unreachable!("relative-date rule prepared without a cutoff");
            };
            date_field_value(track, *field).is_some_and(|track_date| track_date >= *cutoff)
        }
        SmartPlaylistRule::DateNotInLast { field, .. } => {
            let RuleConstants::DateCutoff(cutoff) = constants else {
                unreachable!("relative-date rule prepared without a cutoff");
            };
            match date_field_value(track, *field) {
                Some(track_date) => track_date < *cutoff,
                None => true,
            }
        }
        SmartPlaylistRule::DateIsEmpty { field } => date_field_value(track, *field).is_none(),
        SmartPlaylistRule::DateIsPresent { field } => date_field_value(track, *field).is_some(),
        SmartPlaylistRule::Bool(rule) => bool_field_value(track, rule.field) == rule.equals,
        SmartPlaylistRule::FileIsMissing => track.location.is_missing(),
        SmartPlaylistRule::FileIsPresent => !track.location.is_missing(),
    }
}

fn bool_field_value(track: &Track, field: SmartPlaylistBoolField) -> bool {
    match field {
        SmartPlaylistBoolField::HasLyrics => track.has_lyrics(),
    }
}

fn text_field_value(track: &Track, field: SmartPlaylistTextField) -> Option<&str> {
    match field {
        SmartPlaylistTextField::Title => track.metadata.title.as_deref(),
        SmartPlaylistTextField::Artist => track.metadata.artist.as_deref(),
        SmartPlaylistTextField::Album => track.metadata.album.as_deref(),
        SmartPlaylistTextField::AlbumArtist => track.metadata.album_artist.as_deref(),
        SmartPlaylistTextField::Composer => track.metadata.composer.as_deref(),
        SmartPlaylistTextField::Genre => track.metadata.genre.as_deref(),
        SmartPlaylistTextField::FileName => track
            .location
            .path()
            .file_name()
            .and_then(|os_str| os_str.to_str()),
        SmartPlaylistTextField::MusicalKey => track.metadata.key.as_deref(),
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

/// `needle` must already be [`normalize_search_text`]-folded (prepared
/// once per evaluation pass); only the track side is normalized here.
fn evaluate_text(
    track_value: Option<&str>,
    operator: SmartPlaylistTextOperator,
    needle: &str,
) -> bool {
    let Some(track_value) = track_value else {
        return false;
    };
    let track = normalize_search_text(track_value);
    match operator {
        SmartPlaylistTextOperator::Contains => track.contains(needle),
        SmartPlaylistTextOperator::DoesNotContain => !track.contains(needle),
        SmartPlaylistTextOperator::Is => track == needle,
        SmartPlaylistTextOperator::IsNot => track != needle,
        SmartPlaylistTextOperator::StartsWith => track.starts_with(needle),
        SmartPlaylistTextOperator::EndsWith => track.ends_with(needle),
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
            tracks.sort_by_cached_key(|track| ci_string(track.metadata.title.as_deref()));
        }
        SmartPlaylistLimitSelection::ArtistAscending => {
            tracks.sort_by_cached_key(|track| ci_string(track.metadata.artist.as_deref()));
        }
        SmartPlaylistLimitSelection::AlbumAscending => {
            tracks.sort_by_cached_key(|track| ci_string(track.metadata.album.as_deref()));
        }
        SmartPlaylistLimitSelection::GenreAscending => {
            tracks.sort_by_cached_key(|track| ci_string(track.metadata.genre.as_deref()));
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

/// The case-insensitive key the limit sorts order by. Allocated once per
/// track via `sort_by_cached_key`, not once per comparison.
fn ci_string(value: Option<&str>) -> String {
    value.map(normalize_search_text).unwrap_or_default()
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
