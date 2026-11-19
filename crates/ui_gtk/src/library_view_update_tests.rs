// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeSet;

use sustain_app_runtime::{LibraryChanges, PlaylistId, TrackChange, TrackFields, TrackId};

use super::{AlbumsUpdate, DuplicatesUpdate, LibraryViewUpdate};

fn track_id(id: i64) -> TrackId {
    TrackId::new(id).expect("positive track id")
}

fn changed(fields: TrackFields) -> TrackChange {
    TrackChange::fields(fields)
}

fn update(changes: &[(i64, TrackChange)]) -> LibraryViewUpdate {
    LibraryViewUpdate::for_changes(&LibraryChanges {
        tracks: changes
            .iter()
            .map(|(id, change)| (track_id(*id), *change))
            .collect(),
        playlist_entries: BTreeSet::new(),
    })
}

#[test]
fn an_album_grouping_field_regroups_albums() {
    for field in [
        TrackFields::ARTIST,
        TrackFields::ALBUM,
        TrackFields::ALBUM_ARTIST,
        TrackFields::YEAR,
        TrackFields::COMPILATION,
    ] {
        assert_eq!(
            update(&[(1, changed(field))]).albums,
            AlbumsUpdate::Regroup,
            "{field:?}"
        );
    }
}

#[test]
fn a_shown_track_field_patches_albums_in_place() {
    for field in [
        TrackFields::TITLE,
        TrackFields::DISC_NUMBER,
        TrackFields::DISC_TOTAL,
        TrackFields::TRACK_NUMBER,
        TrackFields::TRACK_TOTAL,
        TrackFields::DURATION,
        TrackFields::LOCATION,
    ] {
        assert_eq!(
            update(&[(1, changed(field))]).albums,
            AlbumsUpdate::Patch(vec![track_id(1)]),
            "{field:?}"
        );
    }
}

#[test]
fn only_the_tracks_with_a_shown_field_are_patched() {
    let update = update(&[
        (1, changed(TrackFields::TITLE | TrackFields::GENRE)),
        (2, changed(TrackFields::BPM | TrackFields::KEY)),
        (3, changed(TrackFields::DURATION)),
    ]);

    assert_eq!(
        update.albums,
        AlbumsUpdate::Patch(vec![track_id(1), track_id(3)]),
        "an analysed BPM and key leave Albums alone"
    );
    assert_eq!(
        update.track_ids,
        vec![track_id(1), track_id(2), track_id(3)]
    );
}

#[test]
fn fields_albums_does_not_show_leave_albums_and_duplicates_alone() {
    let update = update(&[(
        1,
        changed(
            TrackFields::RATING
                | TrackFields::STATISTICS
                | TrackFields::GENRE
                | TrackFields::LYRICS
                | TrackFields::BPM,
        ),
    )]);

    assert_eq!(update.albums, AlbumsUpdate::Unaffected);
    assert_eq!(update.duplicates, DuplicatesUpdate::Rows);
}

#[test]
fn a_duplicate_identity_field_regroups_duplicates() {
    for field in [
        TrackFields::TITLE,
        TrackFields::ARTIST,
        TrackFields::ALBUM,
        TrackFields::DURATION,
    ] {
        assert_eq!(
            update(&[(1, changed(field))]).duplicates,
            DuplicatesUpdate::Regroup,
            "{field:?}"
        );
    }
    assert_eq!(
        update(&[(1, changed(TrackFields::GENRE | TrackFields::YEAR))]).duplicates,
        DuplicatesUpdate::Rows
    );
}

#[test]
fn an_analysis_change_regroups_duplicates() {
    let analysed = TrackChange::analysis();

    assert_eq!(
        update(&[(1, analysed)]).duplicates,
        DuplicatesUpdate::Regroup,
        "analysis verifies, separates and, once lost, rejoins duplicates"
    );
    assert_eq!(
        update(&[(1, analysed), (2, changed(TrackFields::GENRE))]).albums,
        AlbumsUpdate::Unaffected,
        "analysis changes nothing Albums shows"
    );
}

#[test]
fn artwork_reloads_album_artwork_and_repaints_rows() {
    let update = LibraryViewUpdate::for_artwork(&[track_id(4)]);

    assert_eq!(update.track_ids, vec![track_id(4)]);
    assert_eq!(update.albums, AlbumsUpdate::Artwork(vec![track_id(4)]));
    assert_eq!(update.duplicates, DuplicatesUpdate::Rows);
    assert!(!update.statistics);
}

#[test]
fn a_track_that_joined_or_left_regroups_every_aggregate_view() {
    let update = update(&[(1, TrackChange::EVERYTHING)]);

    assert_eq!(update.albums, AlbumsUpdate::Regroup);
    assert_eq!(update.duplicates, DuplicatesUpdate::Regroup);
    assert!(update.statistics);
}

#[test]
fn only_what_the_statistics_page_aggregates_refreshes_it() {
    for field in [
        TrackFields::GENRE,
        TrackFields::AUDIO_FORMAT,
        TrackFields::YEAR,
        TrackFields::RATING,
        TrackFields::STATISTICS,
    ] {
        assert!(update(&[(1, changed(field))]).statistics, "{field:?}");
    }
    assert!(
        !update(&[(1, changed(TrackFields::BPM | TrackFields::TITLE))]).statistics,
        "an analysed BPM or an edited title leaves it alone"
    );
}

#[test]
fn the_playlists_whose_entries_changed_are_passed_on() {
    let playlist_id = PlaylistId::new(7).expect("positive playlist id");
    let update = LibraryViewUpdate::for_changes(&LibraryChanges {
        tracks: Default::default(),
        playlist_entries: BTreeSet::from([playlist_id]),
    });

    assert_eq!(update.playlist_entries, BTreeSet::from([playlist_id]));
    assert!(update.track_ids.is_empty());
    assert_eq!(update.albums, AlbumsUpdate::Unaffected);
}

#[test]
fn the_tracks_whose_analysis_changed_are_named() {
    let update = update(&[
        (1, TrackChange::analysis()),
        (2, changed(TrackFields::TITLE)),
        (3, TrackChange::EVERYTHING),
    ]);

    assert_eq!(update.analysed, BTreeSet::from([track_id(1), track_id(3)]));
}
