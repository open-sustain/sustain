// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cmp::Ordering;

use crate::PlaylistId;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibraryQuery {
    pub search_text: Option<String>,
    pub playlist_id: Option<PlaylistId>,
    pub sort: TrackSort,
    /// Whether text columns sort by the tag-derived "sort as" names
    /// when present (issue #13). Defaults to on; the library
    /// preferences expose a toggle for users who tag inconsistently.
    pub honor_sort_tags: bool,
}

impl Default for LibraryQuery {
    fn default() -> Self {
        Self {
            search_text: None,
            playlist_id: None,
            sort: TrackSort::default(),
            honor_sort_tags: true,
        }
    }
}

impl LibraryQuery {
    pub fn all() -> Self {
        Self::default()
    }

    pub fn honoring_sort_tags(mut self, honor_sort_tags: bool) -> Self {
        self.honor_sort_tags = honor_sort_tags;
        self
    }

    pub fn with_search_text(mut self, search_text: impl Into<String>) -> Self {
        let normalized = search_text.into().trim().to_owned();
        self.search_text = if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
        self
    }

    pub fn in_playlist(mut self, playlist_id: PlaylistId) -> Self {
        self.playlist_id = Some(playlist_id);
        self.sort = TrackSort {
            column: TrackSortColumn::PlaylistPosition,
            direction: SortDirection::Ascending,
        };
        self
    }

    pub fn sorted_by(mut self, sort: TrackSort) -> Self {
        self.sort = sort;
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TrackSort {
    pub column: TrackSortColumn,
    pub direction: SortDirection,
}

impl Default for TrackSort {
    fn default() -> Self {
        Self {
            column: TrackSortColumn::Title,
            direction: SortDirection::Ascending,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackSortColumn {
    PlaylistPosition,
    Title,
    Artist,
    Album,
    Genre,
    Rating,
    PlayCount,
    LastPlayed,
    Duration,
    DateAdded,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SortDirection {
    Ascending,
    Descending,
}

/// Compares two optional text fields for library sort order.
///
/// Each side is reduced to its trimmed, Unicode-lowercased form, and a missing
/// value (`None`) collates as the empty string. This is the single
/// normalization used everywhere the library sorts by a text column — the
/// library store, the search/filter pipeline, and the track-table column
/// headers — so the three always agree, including on non-ASCII text.
pub fn compare_optional_text(left: Option<&str>, right: Option<&str>) -> Ordering {
    fn normalized(value: Option<&str>) -> String {
        value.unwrap_or_default().trim().to_lowercase()
    }

    normalized(left).cmp(&normalized(right))
}

/// The text a column actually sorts by, honoring the "sort as" tag
/// fields (issue #13). When `honor_sort_tags` is on and `sort_field`
/// carries a non-blank value it wins; otherwise we fall back to the
/// display field. With the preference off the display field always
/// wins, giving strict alphabetic-as-shown ordering for libraries that
/// tag sort fields inconsistently. This is the single definition shared
/// by the library store's sort and the track-table column headers so
/// the two never disagree.
pub fn effective_sort_key<'a>(
    sort_field: Option<&'a str>,
    display_field: Option<&'a str>,
    honor_sort_tags: bool,
) -> Option<&'a str> {
    if honor_sort_tags
        && let Some(value) = sort_field
        && !value.trim().is_empty()
    {
        return Some(value);
    }
    display_field
}

#[cfg(test)]
mod tests {
    use std::cmp::Ordering;

    use super::{
        LibraryQuery, SortDirection, TrackSort, TrackSortColumn, compare_optional_text,
        effective_sort_key,
    };

    #[test]
    fn blank_search_text_is_treated_as_no_search() {
        let query = LibraryQuery::all().with_search_text("   ");

        assert_eq!(query.search_text, None);
    }

    #[test]
    fn search_text_is_trimmed() {
        let query = LibraryQuery::all().with_search_text("  Massive Attack  ");

        assert_eq!(query.search_text.as_deref(), Some("Massive Attack"));
    }

    #[test]
    fn default_sort_is_title_ascending() {
        assert_eq!(
            LibraryQuery::default().sort,
            TrackSort {
                column: TrackSortColumn::Title,
                direction: SortDirection::Ascending
            }
        );
    }

    #[test]
    fn playlist_queries_default_to_playlist_position_order() {
        let playlist_id = crate::PlaylistId::new(1).expect("valid playlist id");
        let query = LibraryQuery::all().in_playlist(playlist_id);

        assert_eq!(
            query.sort,
            TrackSort {
                column: TrackSortColumn::PlaylistPosition,
                direction: SortDirection::Ascending
            }
        );
    }

    #[test]
    fn optional_text_collation_trims_and_folds_unicode_case() {
        assert_eq!(
            compare_optional_text(Some("  Édith  "), Some("édith")),
            Ordering::Equal
        );
        assert_eq!(
            compare_optional_text(Some("ABBA"), Some("abba")),
            Ordering::Equal
        );
    }

    #[test]
    fn optional_text_collation_treats_none_as_empty() {
        assert_eq!(compare_optional_text(None, Some("")), Ordering::Equal);
        assert_eq!(compare_optional_text(None, Some("a")), Ordering::Less);
        assert_eq!(compare_optional_text(Some("a"), None), Ordering::Greater);
    }

    #[test]
    fn effective_sort_key_honors_sort_fields_with_display_fallback() {
        // Sort field present and honored -> it wins ("The Beatles" files under B).
        assert_eq!(
            effective_sort_key(Some("Beatles, The"), Some("The Beatles"), true),
            Some("Beatles, The")
        );
        // A blank sort field is treated as absent and falls back to display.
        assert_eq!(
            effective_sort_key(Some("   "), Some("The Beatles"), true),
            Some("The Beatles")
        );
        // No sort field -> display field.
        assert_eq!(
            effective_sort_key(None, Some("The Beatles"), true),
            Some("The Beatles")
        );
        // Preference off -> display field always, even when a sort field exists.
        assert_eq!(
            effective_sort_key(Some("Beatles, The"), Some("The Beatles"), false),
            Some("The Beatles")
        );
    }

    #[test]
    fn library_query_honors_sort_tags_by_default() {
        assert!(LibraryQuery::default().honor_sort_tags);
        assert!(
            !LibraryQuery::all()
                .honoring_sort_tags(false)
                .honor_sort_tags
        );
    }
}
