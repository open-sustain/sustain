// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Display order of a track table.
//!
//! A track table keeps its rows in display order in its own store: no
//! `GtkSortListModel` sits between the store and the selection model. GTK's
//! sort model answers every structural change — a search narrowing the
//! library, a playlist switch — with an O(n) reconciliation, and every
//! payload swap with a full re-sort that rebinds each realized cell. On a
//! 10,000-track library that cost 150–450 ms per search. Rows are instead
//! put in order here, comparing plain pre-collated fields, before they
//! reach the store.
//!
//! The `ColumnViewSorter` stays the authority on *which* order is active:
//! header clicks, the persisted sort, and the playlist play-order sort all
//! set it. A [`RowOrder`] mirrors its whole chain of sort columns, so ties
//! on the primary column resolve by the previously clicked ones exactly as
//! GTK's sorter resolves them. Sorting is stable: rows equal under every
//! key keep the order they arrived in.

use std::cmp::Ordering;

use super::{columns::TrackTableColumn, row::TrackTableRow};

/// One sortable dimension of a track table.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SortKey {
    /// The status column's play-order sort: the row's position in the
    /// displayed regular playlist. Rows without a position sort last.
    PlaylistPosition,
    /// A managed track column.
    Column(TrackTableColumn),
}

impl SortKey {
    fn compare(self, left: &TrackTableRow, right: &TrackTableRow) -> Ordering {
        match self {
            Self::PlaylistPosition => {
                compare_playlist_positions(left.playlist_position, right.playlist_position)
            }
            Self::Column(column) => column.compare(left, right),
        }
    }
}

/// Play order: positioned rows first, by position.
pub(super) fn compare_playlist_positions(left: Option<u32>, right: Option<u32>) -> Ordering {
    match (left, right) {
        (Some(left), Some(right)) => left.cmp(&right),
        (Some(_), None) => Ordering::Less,
        (None, Some(_)) => Ordering::Greater,
        (None, None) => Ordering::Equal,
    }
}

/// The active sort of a track table: its sort keys, primary first.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct RowOrder {
    keys: Vec<(SortKey, bool)>,
}

impl RowOrder {
    /// Add the next tie-breaking key, ascending or descending.
    pub(super) fn push(&mut self, key: SortKey, ascending: bool) {
        self.keys.push((key, ascending));
    }

    /// Whether no sort is active, so rows keep the order they arrived in.
    pub(super) fn is_unsorted(&self) -> bool {
        self.keys.is_empty()
    }

    pub(super) fn compare(&self, left: &TrackTableRow, right: &TrackTableRow) -> Ordering {
        self.keys
            .iter()
            .map(|&(key, ascending)| {
                let ordering = key.compare(left, right);
                if ascending {
                    ordering
                } else {
                    ordering.reverse()
                }
            })
            .find(|ordering| ordering.is_ne())
            .unwrap_or(Ordering::Equal)
    }

    pub(super) fn sort(&self, rows: &mut [TrackTableRow]) {
        if !self.is_unsorted() {
            rows.sort_by(|left, right| self.compare(left, right));
        }
    }

    /// The stable ordering of `rows` as a permutation: entry `i` is the
    /// index of the row that belongs at position `i`.
    pub(super) fn permutation<R>(&self, rows: &[R]) -> Vec<usize>
    where
        R: std::ops::Deref<Target = TrackTableRow>,
    {
        let mut permutation: Vec<usize> = (0..rows.len()).collect();
        if !self.is_unsorted() {
            permutation.sort_by(|&left, &right| self.compare(&rows[left], &rows[right]));
        }
        permutation
    }
}

/// Rearrange a sequence so position `i` receives the item at
/// `permutation[i]`, through `swap` calls only. Each cycle of the
/// permutation costs one swap per item but its last, so no item is copied
/// or held aside.
pub(super) fn apply_permutation(permutation: &[usize], mut swap: impl FnMut(usize, usize)) {
    let mut placed = vec![false; permutation.len()];
    for start in 0..permutation.len() {
        if placed[start] {
            continue;
        }
        let mut position = start;
        loop {
            placed[position] = true;
            let source = permutation[position];
            if source == start {
                break;
            }
            swap(position, source);
            position = source;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_permutation_moves_every_item_to_its_position() {
        let permutations: [&[usize]; 5] = [
            &[],
            &[0, 1, 2],
            &[1, 2, 0],
            &[2, 0, 1, 4, 3],
            &[3, 0, 4, 1, 2, 6, 5],
        ];
        for permutation in permutations {
            let mut items: Vec<usize> = (0..permutation.len()).collect();
            apply_permutation(permutation, |left, right| items.swap(left, right));
            assert_eq!(items, permutation, "permutation {permutation:?}");
        }
    }

    #[test]
    fn later_keys_break_ties_and_equal_rows_keep_their_order() {
        let mut order = RowOrder::default();
        order.push(SortKey::Column(TrackTableColumn::Year), false);
        order.push(SortKey::Column(TrackTableColumn::TrackName), true);
        let mut rows = vec![
            row(1, Some(1990), "a"),
            row(2, Some(2000), "b"),
            row(3, Some(2000), "a"),
            row(4, Some(1990), "a"),
        ];

        order.sort(&mut rows);

        let ids: Vec<i64> = rows
            .iter()
            .filter_map(|row| row.track_id.map(|id| id.get()))
            .collect();
        assert_eq!(ids, [3, 2, 1, 4]);
    }

    #[test]
    fn permutation_matches_sorting_the_rows() {
        let mut order = RowOrder::default();
        order.push(SortKey::Column(TrackTableColumn::TrackName), true);
        let rows = [row(1, None, "c"), row(2, None, "a"), row(3, None, "b")];
        let borrowed: Vec<&TrackTableRow> = rows.iter().collect();

        assert_eq!(order.permutation(&borrowed), [1, 2, 0]);
        assert_eq!(
            RowOrder::default().permutation(&borrowed),
            [0, 1, 2],
            "no sort keeps the arrival order"
        );
    }

    #[test]
    fn playlist_positions_order_positioned_rows_first() {
        assert_eq!(compare_playlist_positions(Some(1), Some(2)), Ordering::Less);
        assert_eq!(compare_playlist_positions(Some(9), None), Ordering::Less);
        assert_eq!(compare_playlist_positions(None, Some(0)), Ordering::Greater);
        assert_eq!(compare_playlist_positions(None, None), Ordering::Equal);
    }

    fn row(id: i64, year: Option<i32>, track_name: &str) -> TrackTableRow {
        let mut row = TrackTableRow::test_row(id);
        row.year = year;
        row.track_name_sort_key = track_name.to_owned();
        row
    }
}
