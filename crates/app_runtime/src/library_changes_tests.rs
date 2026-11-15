// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeSet;

use super::{
    LibraryChangeOutbox, TrackChange, library_track_changes, playlists_with_changed_entries,
};
use crate::{
    PlayStatistics, Playlist, PlaylistEntry, PlaylistId, Rating, Track, TrackFields, TrackId,
    TrackLocation, TrackMetadata, TrackRelativePath,
};

fn track_id(id: i64) -> TrackId {
    TrackId::new(id).expect("positive track id")
}

fn playlist_id(id: i64) -> PlaylistId {
    PlaylistId::new(id).expect("positive playlist id")
}

fn track(id: i64, title: &str) -> Track {
    Track {
        id: track_id(id),
        location: TrackLocation::available(
            TrackRelativePath::new(format!("{id}.flac")).expect("relative path"),
        ),
        metadata: TrackMetadata {
            title: Some(title.to_owned()),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

fn playlist(id: i64, track_ids: &[i64]) -> Playlist {
    Playlist {
        id: playlist_id(id),
        name: format!("Playlist {id}"),
        parent_folder_id: None,
        position: 0,
        entries: track_ids
            .iter()
            .enumerate()
            .map(|(position, track)| PlaylistEntry {
                playlist_id: playlist_id(id),
                track_id: track_id(*track),
                position: position as u32,
            })
            .collect(),
    }
}

#[test]
fn only_the_first_pending_change_wakes_the_reader() {
    let mut outbox = LibraryChangeOutbox::default();

    assert!(outbox.record_track(track_id(1), TrackChange::fields(TrackFields::TITLE)));
    assert!(!outbox.record_track(track_id(2), TrackChange::fields(TrackFields::RATING)));
    assert!(!outbox.record_playlist_entries(playlist_id(1)));
    assert!(!outbox.record_track(track_id(1), TrackChange::EVERYTHING));
}

#[test]
fn a_playlist_change_alone_wakes_the_reader() {
    let mut outbox = LibraryChangeOutbox::default();

    assert!(outbox.record_playlist_entries(playlist_id(1)));
    assert!(!outbox.record_playlist_entries(playlist_id(1)));
    assert_eq!(
        outbox.take().playlist_entries,
        BTreeSet::from([playlist_id(1)])
    );
}

#[test]
fn a_change_recorded_after_a_take_wakes_the_reader_again() {
    let mut outbox = LibraryChangeOutbox::default();
    assert!(outbox.record_track(track_id(1), TrackChange::fields(TrackFields::TITLE)));

    assert_eq!(outbox.take().tracks.len(), 1);

    assert!(outbox.is_empty());
    assert!(outbox.record_track(track_id(1), TrackChange::fields(TrackFields::RATING)));
}

#[test]
fn changes_to_one_track_merge() {
    let mut outbox = LibraryChangeOutbox::default();
    outbox.record_track(track_id(1), TrackChange::fields(TrackFields::TITLE));
    outbox.record_track(
        track_id(1),
        TrackChange {
            fields: TrackFields::BPM,
            analysis: true,
        },
    );

    assert_eq!(
        outbox.take().tracks.get(&track_id(1)),
        Some(&TrackChange {
            fields: TrackFields::TITLE | TrackFields::BPM,
            analysis: true,
        })
    );
}

#[test]
fn an_empty_change_records_nothing() {
    let mut outbox = LibraryChangeOutbox::default();

    assert!(!outbox.record_track(track_id(1), TrackChange::default()));
    assert!(outbox.is_empty());
}

#[test]
fn an_analysis_change_alone_is_recorded() {
    let mut outbox = LibraryChangeOutbox::default();

    assert!(outbox.record_track(track_id(1), TrackChange::analysis()));
}

#[test]
fn a_track_that_joined_and_left_between_two_takes_is_still_touched() {
    let mut outbox = LibraryChangeOutbox::default();
    outbox.record_track(track_id(1), TrackChange::EVERYTHING);
    outbox.record_track(track_id(1), TrackChange::EVERYTHING);

    assert_eq!(
        outbox.take().tracks.get(&track_id(1)),
        Some(&TrackChange::EVERYTHING),
        "a view may have shown it in between"
    );
}

#[test]
fn comparing_two_libraries_finds_the_tracks_that_joined_left_or_changed() {
    let previous = vec![track(1, "Kept"), track(2, "Left"), track(4, "Old title")];
    let current = vec![
        track(1, "Kept"),
        track(3, "Joined"),
        track(4, "New title"),
        track(5, "Joined last"),
    ];

    assert_eq!(
        library_track_changes(&previous, &current),
        vec![
            (track_id(1), TrackChange::fields(TrackFields::NONE)),
            (track_id(2), TrackChange::EVERYTHING),
            (track_id(3), TrackChange::EVERYTHING),
            (track_id(4), TrackChange::fields(TrackFields::TITLE)),
            (track_id(5), TrackChange::EVERYTHING),
        ]
    );
}

#[test]
fn a_library_emptied_or_filled_changes_every_track() {
    let tracks = vec![track(1, "One"), track(2, "Two")];
    let everything = vec![
        (track_id(1), TrackChange::EVERYTHING),
        (track_id(2), TrackChange::EVERYTHING),
    ];

    assert_eq!(library_track_changes(&tracks, &[]), everything);
    assert_eq!(library_track_changes(&[], &tracks), everything);
}

#[test]
fn playlists_whose_entries_differ_are_found() {
    let previous = vec![
        playlist(1, &[1, 2]),
        playlist(2, &[1, 2]),
        playlist(3, &[1]),
        playlist(4, &[]),
    ];
    let mut renamed = playlist(1, &[1, 2]);
    renamed.name = "Renamed".to_owned();
    let current = vec![
        renamed,
        playlist(2, &[2, 1]),
        playlist(4, &[]),
        playlist(5, &[3]),
        playlist(6, &[]),
    ];

    assert_eq!(
        playlists_with_changed_entries(&previous, &current),
        BTreeSet::from([playlist_id(2), playlist_id(3), playlist_id(5)]),
        "a reorder, a deleted playlist and a new one with entries; \
         a rename and empty playlists coming or going are not entry changes"
    );
}
