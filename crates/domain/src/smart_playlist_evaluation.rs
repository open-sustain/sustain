// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cmp::Reverse,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use crate::{
    Calendar, Clock, CollationKey, MusicalKey, SmartPlaylistBoolField, SmartPlaylistDateField,
    SmartPlaylistLimit, SmartPlaylistLimitSelection, SmartPlaylistMatchKind,
    SmartPlaylistNumberField, SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet,
    SmartPlaylistTextField, SmartPlaylistTextOperator, Track, normalize_search_text,
};

const SECONDS_PER_DAY: u64 = 86_400;

/// The tracks of `tracks` that `rules` select, as `clock` reads the
/// current time and the user's calendar.
pub fn matching_tracks<'a>(
    tracks: &'a [Track],
    rules: &SmartPlaylistRuleSet,
    clock: &dyn Clock,
) -> Vec<&'a Track> {
    SmartPlaylistMatcher::new(rules, clock).select(tracks)
}

pub fn track_matches_rule(track: &Track, rule: &SmartPlaylistRule, clock: &dyn Clock) -> bool {
    rule_matches(
        track,
        rule,
        &rule_constants(rule, clock.now()),
        clock.calendar().as_ref(),
    )
}

/// A rule set ready to evaluate, for one evaluation pass: the present and
/// the user's calendar are read once, so every rule and the limit agree
/// on them, and each rule's constants — text needles search-normalized,
/// relative-date windows resolved to instants — are resolved once, so
/// evaluating a 10,000-track library does not redo that work per track.
pub struct SmartPlaylistMatcher<'rules> {
    rule_set: &'rules SmartPlaylistRuleSet,
    now: SystemTime,
    calendar: Arc<dyn Calendar>,
    constants: Vec<RuleConstants>,
}

impl<'rules> SmartPlaylistMatcher<'rules> {
    /// `rule_set`, as `clock` reads the present and the user's calendar
    /// now.
    pub fn new(rule_set: &'rules SmartPlaylistRuleSet, clock: &dyn Clock) -> Self {
        let now = clock.now();
        Self {
            rule_set,
            now,
            calendar: clock.calendar(),
            constants: rule_set
                .rules
                .iter()
                .map(|rule| rule_constants(rule, now))
                .collect(),
        }
    }

    /// Whether `track` matches the rules, regardless of any limit. An
    /// empty rule set matches nothing.
    pub fn matches(&self, track: &Track) -> bool {
        if self.constants.is_empty() {
            return false;
        }
        let calendar = self.calendar.as_ref();
        let mut rules = self.rule_set.rules.iter().zip(&self.constants);
        match self.rule_set.match_kind {
            SmartPlaylistMatchKind::All => {
                rules.all(|(rule, constants)| rule_matches(track, rule, constants, calendar))
            }
            SmartPlaylistMatchKind::Any => {
                rules.any(|(rule, constants)| rule_matches(track, rule, constants, calendar))
            }
        }
    }

    /// The tracks of `tracks` the rule set selects, its limit applied.
    pub fn select<'a>(&self, tracks: &'a [Track]) -> Vec<&'a Track> {
        let mut matched: Vec<&Track> = tracks.iter().filter(|track| self.matches(track)).collect();
        if let Some(limit) = self.rule_set.limit {
            apply_limit(&mut matched, limit, self.now);
        }
        matched
    }

    /// The earliest instant, after the present this matcher read, at
    /// which time passing alone can change what the rule set selects
    /// from `tracks`: the first time one of them leaves a relative date
    /// rule's window. `None` when the selection cannot change until a
    /// track does.
    pub fn selection_changes_at(&self, tracks: &[Track]) -> Option<SystemTime> {
        if !self
            .constants
            .iter()
            .any(|constants| matches!(constants, RuleConstants::WindowStart(_)))
        {
            return None;
        }
        tracks
            .iter()
            .filter_map(|track| self.verdict_changes_at(track))
            .min()
    }

    /// The earliest instant, after the present this matcher read, at
    /// which time passing alone can change a rule's verdict on `track`:
    /// when its date leaves a relative date rule's window, which it does
    /// once the window starts after it. `None` when no rule's verdict on
    /// it depends on the time. A date before the window has left it for
    /// good, and an empty date never enters it.
    pub fn verdict_changes_at(&self, track: &Track) -> Option<SystemTime> {
        self.rule_set
            .rules
            .iter()
            .zip(&self.constants)
            .filter_map(|(rule, constants)| {
                let (SmartPlaylistRule::DateInLast { field, days }
                | SmartPlaylistRule::DateNotInLast { field, days }) = rule
                else {
                    return None;
                };
                let date = date_field_value(track, *field)?;
                (date >= window_start(constants))
                    .then(|| date.checked_add(window_span(days.get()) + Duration::from_nanos(1)))
                    .flatten()
            })
            .min()
    }
}

/// One rule's evaluation-ready constants: whatever the rule can resolve
/// once per pass instead of once per track. Built by [`rule_constants`]
/// from the same rule it is later matched against.
enum RuleConstants {
    None,
    /// The text rule's value, already [`normalize_search_text`]-folded.
    TextNeedle(String),
    /// The start of a relative date rule's window.
    WindowStart(SystemTime),
}

fn rule_constants(rule: &SmartPlaylistRule, now: SystemTime) -> RuleConstants {
    match rule {
        SmartPlaylistRule::Text { value, .. } => {
            RuleConstants::TextNeedle(normalize_search_text(value))
        }
        SmartPlaylistRule::DateInLast { days, .. }
        | SmartPlaylistRule::DateNotInLast { days, .. } => {
            RuleConstants::WindowStart(relative_date_cutoff(now, days.get()))
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
    now.checked_sub(window_span(days))
        .unwrap_or(SystemTime::UNIX_EPOCH)
}

/// The length of a relative date rule's `days`-day window.
fn window_span(days: u32) -> Duration {
    Duration::from_secs(u64::from(days) * SECONDS_PER_DAY)
}

/// Evaluates one rule against one track, reading its dates in `calendar`.
/// `constants` must come from [`rule_constants`] for this same rule; the
/// `unreachable!` arms guard that pairing.
fn rule_matches(
    track: &Track,
    rule: &SmartPlaylistRule,
    constants: &RuleConstants,
    calendar: &dyn Calendar,
) -> bool {
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
        // A calendar rule compares the day the track's date falls on in
        // the user's calendar — the day the library shows — with its day.
        SmartPlaylistRule::DateBefore { field, date } => {
            date_field_value(track, *field).is_some_and(|instant| calendar.date_of(instant) < *date)
        }
        SmartPlaylistRule::DateAfter { field, date } => {
            date_field_value(track, *field).is_some_and(|instant| calendar.date_of(instant) > *date)
        }
        SmartPlaylistRule::DateInLast { field, .. } => {
            let window_start = window_start(constants);
            date_field_value(track, *field).is_some_and(|instant| instant >= window_start)
        }
        SmartPlaylistRule::DateNotInLast { field, .. } => {
            let window_start = window_start(constants);
            date_field_value(track, *field).is_none_or(|instant| instant < window_start)
        }
        SmartPlaylistRule::DateIsEmpty { field } => date_field_value(track, *field).is_none(),
        SmartPlaylistRule::DateIsPresent { field } => date_field_value(track, *field).is_some(),
        SmartPlaylistRule::Bool(rule) => bool_field_value(track, rule.field) == rule.equals,
        SmartPlaylistRule::FileIsMissing => track.location.is_missing(),
        SmartPlaylistRule::FileIsPresent => !track.location.is_missing(),
    }
}

fn window_start(constants: &RuleConstants) -> SystemTime {
    let RuleConstants::WindowStart(start) = constants else {
        unreachable!("relative date rule prepared without its window");
    };
    *start
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
        SmartPlaylistTextField::MusicalKey => track.metadata.key.map(MusicalKey::short_code),
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
        // Rule values are whole numbers, so a rule reads the tempo to the
        // nearest whole beat: "BPM is 128" matches 127.5 up to 128.49.
        SmartPlaylistNumberField::Bpm => track.metadata.bpm.map(|bpm| i64::from(bpm.whole())),
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
            tracks.sort_by_cached_key(|track| sort_key(track.metadata.title.as_deref()));
        }
        SmartPlaylistLimitSelection::ArtistAscending => {
            tracks.sort_by_cached_key(|track| sort_key(track.metadata.artist.as_deref()));
        }
        SmartPlaylistLimitSelection::AlbumAscending => {
            tracks.sort_by_cached_key(|track| sort_key(track.metadata.album.as_deref()));
        }
        SmartPlaylistLimitSelection::GenreAscending => {
            tracks.sort_by_cached_key(|track| sort_key(track.metadata.genre.as_deref()));
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
fn sort_key(value: Option<&str>) -> CollationKey {
    CollationKey::new(value.unwrap_or_default())
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
