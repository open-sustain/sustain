// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::num::NonZeroU32;

use crate::{CalendarDate, PlaylistFolderId, Rating, SmartPlaylistId};

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmartPlaylist {
    pub id: SmartPlaylistId,
    pub name: String,
    pub parent_folder_id: Option<PlaylistFolderId>,
    pub position: u32,
    pub rules: SmartPlaylistRuleSet,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SmartPlaylistRuleSet {
    pub match_kind: SmartPlaylistMatchKind,
    pub rules: Vec<SmartPlaylistRule>,
    pub limit: Option<SmartPlaylistLimit>,
}

impl SmartPlaylistRuleSet {
    /// Whether what the rules select depends on the time, not only on the
    /// tracks: a relative date rule reads the present, and a calendar
    /// date rule reads dates in the user's time zone.
    pub fn reads_the_time(&self) -> bool {
        self.rules.iter().any(|rule| {
            matches!(
                rule,
                SmartPlaylistRule::DateInLast { .. }
                    | SmartPlaylistRule::DateNotInLast { .. }
                    | SmartPlaylistRule::DateBefore { .. }
                    | SmartPlaylistRule::DateAfter { .. }
            )
        })
    }

    pub fn is_empty(&self) -> bool {
        self.rules.is_empty()
    }
}

impl Default for SmartPlaylistRuleSet {
    fn default() -> Self {
        Self {
            match_kind: SmartPlaylistMatchKind::All,
            rules: Vec::new(),
            limit: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistMatchKind {
    All,
    Any,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmartPlaylistLimit {
    pub count: NonZeroU32,
    pub selection: SmartPlaylistLimitSelection,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistLimitSelection {
    Random,
    AlbumAscending,
    ArtistAscending,
    GenreAscending,
    TitleAscending,
    HighestRating,
    LowestRating,
    MostRecentlyPlayed,
    LeastRecentlyPlayed,
    MostOftenPlayed,
    LeastOftenPlayed,
    MostRecentlyAdded,
    LeastRecentlyAdded,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SmartPlaylistRule {
    Text {
        field: SmartPlaylistTextField,
        operator: SmartPlaylistTextOperator,
        value: String,
    },
    TextIsEmpty {
        field: SmartPlaylistTextField,
    },
    TextIsPresent {
        field: SmartPlaylistTextField,
    },
    Number {
        field: SmartPlaylistNumberField,
        operator: SmartPlaylistNumberOperator,
        value: i64,
    },
    /// Matches when a numeric field has no value at all (e.g. a track
    /// with no year tag). Distinct from `Number { .. Equal, 0 }`, which
    /// only matches a field explicitly set to zero.
    NumberIsEmpty {
        field: SmartPlaylistNumberField,
    },
    /// Matches when a numeric field has any value, the inverse of
    /// [`SmartPlaylistRule::NumberIsEmpty`].
    NumberIsPresent {
        field: SmartPlaylistNumberField,
    },
    Rating {
        operator: SmartPlaylistNumberOperator,
        value: Rating,
    },
    /// Matches a date that falls on a day before `date` in the user's
    /// calendar, the way the library displays its dates.
    DateBefore {
        field: SmartPlaylistDateField,
        date: CalendarDate,
    },
    /// Matches a date that falls on a day after `date` in the user's
    /// calendar.
    DateAfter {
        field: SmartPlaylistDateField,
        date: CalendarDate,
    },
    DateInLast {
        field: SmartPlaylistDateField,
        days: NonZeroU32,
    },
    DateNotInLast {
        field: SmartPlaylistDateField,
        days: NonZeroU32,
    },
    DateIsEmpty {
        field: SmartPlaylistDateField,
    },
    DateIsPresent {
        field: SmartPlaylistDateField,
    },
    Bool(SmartPlaylistBoolRule),
    /// Matches when the track's file is currently flagged missing — the
    /// last access could not find it on disk. Backs the seeded "Missing
    /// Files" smart playlist (#79). File availability is a whole-track
    /// attribute, so unlike the text/number/date rules it carries no
    /// field.
    FileIsMissing,
    /// Matches when the track's file is present (available) — the inverse
    /// of [`SmartPlaylistRule::FileIsMissing`].
    FileIsPresent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistTextField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Genre,
    FileName,
    MusicalKey,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistTextOperator {
    Contains,
    DoesNotContain,
    Is,
    IsNot,
    StartsWith,
    EndsWith,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistNumberField {
    PlayCount,
    SkipCount,
    TrackNumber,
    DiscNumber,
    Year,
    DurationSeconds,
    BitrateKbps,
    Bpm,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistNumberOperator {
    Equal,
    NotEqual,
    GreaterThan,
    GreaterThanOrEqual,
    LessThan,
    LessThanOrEqual,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistDateField {
    DateAdded,
    LastPlayed,
    LastSkipped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SmartPlaylistBoolRule {
    pub field: SmartPlaylistBoolField,
    pub equals: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SmartPlaylistBoolField {
    HasLyrics,
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU32;

    use super::{
        SmartPlaylistDateField, SmartPlaylistLimit, SmartPlaylistLimitSelection,
        SmartPlaylistMatchKind, SmartPlaylistRule, SmartPlaylistRuleSet,
    };
    use crate::CalendarDate;

    #[test]
    fn rule_sets_with_date_rules_read_the_time() {
        let field = SmartPlaylistDateField::LastPlayed;
        let days = NonZeroU32::new(7).expect("positive days");
        let date = CalendarDate::new(2024, 1, 1).expect("a valid date");
        let reading = [
            SmartPlaylistRule::DateInLast { field, days },
            SmartPlaylistRule::DateNotInLast { field, days },
            SmartPlaylistRule::DateBefore { field, date },
            SmartPlaylistRule::DateAfter { field, date },
        ];
        // Whether a date is there or not holds at any time.
        let timeless = [
            SmartPlaylistRule::DateIsEmpty { field },
            SmartPlaylistRule::DateIsPresent { field },
            SmartPlaylistRule::FileIsMissing,
        ];
        let rule_set = |rules: Vec<SmartPlaylistRule>| SmartPlaylistRuleSet {
            rules,
            ..SmartPlaylistRuleSet::default()
        };

        for rule in reading {
            assert!(
                rule_set(vec![timeless[0].clone(), rule.clone()]).reads_the_time(),
                "{rule:?} reads the time"
            );
        }
        assert!(!rule_set(timeless.to_vec()).reads_the_time());
    }

    #[test]
    fn rule_sets_default_to_matching_all_rules_without_a_limit() {
        let rule_set = SmartPlaylistRuleSet::default();

        assert_eq!(rule_set.match_kind, SmartPlaylistMatchKind::All);
        assert!(rule_set.is_empty());
        assert_eq!(rule_set.limit, None);
    }

    #[test]
    fn limits_pair_a_positive_count_with_a_selection_method() {
        let limit = SmartPlaylistLimit {
            count: NonZeroU32::new(25).expect("positive count"),
            selection: SmartPlaylistLimitSelection::MostRecentlyAdded,
        };

        assert_eq!(limit.count.get(), 25);
        assert_eq!(
            limit.selection,
            SmartPlaylistLimitSelection::MostRecentlyAdded
        );
    }
}
