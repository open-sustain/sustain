// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::num::NonZeroU32;

use crate::{
    Rating, SmartPlaylist, SmartPlaylistDateField, SmartPlaylistId, SmartPlaylistLimit,
    SmartPlaylistLimitSelection, SmartPlaylistMatchKind, SmartPlaylistNumberField,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet, SmartPlaylistTextField,
};

// Mirrors iTunes 11's starter set as far as Sustain's domain vocabulary
// allows. Entries outside Sustain's pure-local-music scope (Music Videos,
// Purchased, podcast/audiobook buckets) are deliberately omitted. "Missing
// Tags" is a Sustain-native addition for library consolidation.
pub fn default_smart_playlists(starting_id: i64) -> Vec<SmartPlaylist> {
    let templates: [(&str, SmartPlaylistRuleSet); 7] = [
        ("Recently Added", recently_added_rules()),
        ("Recently Played", recently_played_rules()),
        ("Top 25 Most Played", top_25_most_played_rules()),
        ("4+ Stars", four_plus_stars_rules()),
        ("Unplayed", unplayed_rules()),
        ("Missing Tags", missing_tags_rules()),
        ("Missing Files", missing_files_rules()),
    ];

    templates
        .into_iter()
        .enumerate()
        .map(|(offset, (name, rules))| {
            let raw_id = starting_id
                .checked_add(offset as i64)
                .expect("default smart-playlist id must not overflow i64");
            SmartPlaylist {
                id: SmartPlaylistId::new(raw_id)
                    .expect("default smart-playlist id must be positive"),
                name: name.to_owned(),
                parent_folder_id: None,
                position: 0,
                rules,
            }
        })
        .collect()
}

fn recently_added_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::DateInLast {
            field: SmartPlaylistDateField::DateAdded,
            days: days(14),
        }],
        limit: None,
    }
}

fn recently_played_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::DateInLast {
            field: SmartPlaylistDateField::LastPlayed,
            days: days(14),
        }],
        limit: None,
    }
}

fn top_25_most_played_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Number {
            field: SmartPlaylistNumberField::PlayCount,
            operator: SmartPlaylistNumberOperator::GreaterThan,
            value: 0,
        }],
        limit: Some(SmartPlaylistLimit {
            count: NonZeroU32::new(25).expect("25 is positive"),
            selection: SmartPlaylistLimitSelection::MostOftenPlayed,
        }),
    }
}

fn four_plus_stars_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Rating {
            operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
            value: Rating::new(4).expect("4-star rating is in range"),
        }],
        limit: None,
    }
}

fn unplayed_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Number {
            field: SmartPlaylistNumberField::PlayCount,
            operator: SmartPlaylistNumberOperator::Equal,
            value: 0,
        }],
        limit: None,
    }
}

/// Tracks that are missing at least one of the core descriptive tags —
/// album, artist, genre, year — or that have never been rated. A
/// match-any rule set so a track surfaces the moment any one field is
/// blank, making this the working list for library backfill and
/// consolidation. Year uses `NumberIsEmpty` (no year tag at all) rather
/// than `Number == 0`, and an unrated track is one at zero stars.
fn missing_tags_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::Any,
        rules: vec![
            SmartPlaylistRule::TextIsEmpty {
                field: SmartPlaylistTextField::Album,
            },
            SmartPlaylistRule::TextIsEmpty {
                field: SmartPlaylistTextField::Artist,
            },
            SmartPlaylistRule::TextIsEmpty {
                field: SmartPlaylistTextField::Genre,
            },
            SmartPlaylistRule::NumberIsEmpty {
                field: SmartPlaylistNumberField::Year,
            },
            SmartPlaylistRule::Rating {
                operator: SmartPlaylistNumberOperator::Equal,
                value: Rating::unrated(),
            },
        ],
        limit: None,
    }
}

/// Tracks whose file the last access could not find on disk. A single
/// `FileIsMissing` rule, so the list is exactly the library's broken
/// links — the working set for relocating or pruning them (#79).
fn missing_files_rules() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::FileIsMissing],
        limit: None,
    }
}

fn days(value: u32) -> NonZeroU32 {
    NonZeroU32::new(value).expect("default smart-playlist day window must be positive")
}

#[cfg(test)]
mod tests {
    use super::default_smart_playlists;

    #[test]
    fn seeds_named_defaults_with_sequential_ids() {
        let playlists = default_smart_playlists(1);

        let names: Vec<&str> = playlists.iter().map(|smart| smart.name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "Recently Added",
                "Recently Played",
                "Top 25 Most Played",
                "4+ Stars",
                "Unplayed",
                "Missing Tags",
                "Missing Files",
            ]
        );

        let ids: Vec<i64> = playlists.iter().map(|smart| smart.id.get()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn starting_id_is_honored() {
        let playlists = default_smart_playlists(42);

        assert_eq!(playlists.first().expect("non-empty").id.get(), 42);
        assert_eq!(playlists.last().expect("non-empty").id.get(), 48);
    }

    #[test]
    fn missing_files_is_a_single_file_is_missing_rule() {
        let missing_files = default_smart_playlists(1)
            .into_iter()
            .find(|smart| smart.name == "Missing Files")
            .expect("Missing Files default present");

        assert_eq!(
            missing_files.rules.rules.as_slice(),
            [crate::SmartPlaylistRule::FileIsMissing]
        );
        assert_eq!(missing_files.rules.limit, None);
    }

    #[test]
    fn missing_tags_matches_any_of_the_descriptive_tags() {
        let missing_tags = default_smart_playlists(1)
            .into_iter()
            .find(|smart| smart.name == "Missing Tags")
            .expect("Missing Tags default present");

        // Match-any: a track surfaces the instant any one descriptive tag
        // is blank, not only when every one of them is.
        assert_eq!(
            missing_tags.rules.match_kind,
            crate::SmartPlaylistMatchKind::Any
        );
        assert_eq!(missing_tags.rules.rules.len(), 5);
        assert_eq!(missing_tags.rules.limit, None);
    }

    #[test]
    fn every_default_carries_at_least_one_rule() {
        for smart in default_smart_playlists(1) {
            assert!(
                !smart.rules.is_empty(),
                "default '{}' must carry at least one rule",
                smart.name
            );
        }
    }
}
