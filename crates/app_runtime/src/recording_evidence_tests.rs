// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{collections::BTreeMap, time::Duration};

use sustain_domain::{
    DuplicateGroup, DuplicateMatchMode, PlayStatistics, Rating, Track, TrackLocation,
    TrackMetadata, TrackRelativePath, duplicate_groups,
};
use sustain_library_store::SqliteLibraryStore;

use super::*;
use crate::test_waveforms::{analyse, music};

fn track(id: i64) -> Track {
    Track {
        id: TrackId::new(id).expect("valid id"),
        location: TrackLocation::available(
            TrackRelativePath::new(format!("{id}.flac")).expect("path"),
        ),
        metadata: TrackMetadata {
            artist: Some("Artist".to_owned()),
            title: Some("Title".to_owned()),
            album: Some("Album".to_owned()),
            duration: Some(Duration::from_secs(200)),
            ..TrackMetadata::default()
        },
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: Some(100),
        has_embedded_artwork: Some(false),
        file_modified_at: None,
    }
}

/// Two tracks the tags alone suggest as duplicates.
fn library() -> SqliteLibraryStore {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    store.save_track(track(1)).expect("save track");
    store.save_track(track(2)).expect("save track");
    store
}

fn scan(
    store: &SqliteLibraryStore,
    cache: &Arc<Mutex<RecordingIdentityCache>>,
) -> Vec<DuplicateGroup> {
    let tracks = store.tracks().expect("tracks");
    let mut evidence =
        WaveformRecordingEvidence::new(store, Arc::clone(cache)).expect("waveform versions");
    duplicate_groups(
        &tracks,
        DuplicateMatchMode::Loose,
        &BTreeMap::new(),
        &mut evidence,
    )
}

fn ids(ids: &[i64]) -> Vec<TrackId> {
    ids.iter()
        .map(|id| TrackId::new(*id).expect("valid id"))
        .collect()
}

#[test]
fn copies_of_one_recording_are_a_verified_group() {
    let store = library();
    analyse(&store, track(1).id, music(1, 200), 100);
    analyse(&store, track(2).id, music(1, 200), 100);

    assert_eq!(
        scan(&store, &Arc::default()),
        vec![DuplicateGroup {
            track_ids: ids(&[1, 2]),
            same_recording: true,
        }]
    );
}

#[test]
fn different_recordings_are_not_suggested() {
    let store = library();
    analyse(&store, track(1).id, music(1, 200), 100);
    analyse(&store, track(2).id, music(2, 200), 100);

    assert_eq!(scan(&store, &Arc::default()), Vec::new());
}

#[test]
fn a_track_without_a_waveform_leaves_the_group_unverified() {
    let store = library();
    analyse(&store, track(1).id, music(1, 200), 100);

    assert_eq!(
        scan(&store, &Arc::default()),
        vec![DuplicateGroup {
            track_ids: ids(&[1, 2]),
            same_recording: false,
        }]
    );
}

#[test]
fn reanalysing_a_track_replaces_its_cached_verdicts() {
    let store = library();
    let cache = Arc::default();
    analyse(&store, track(1).id, music(1, 200), 100);
    analyse(&store, track(2).id, music(1, 200), 100);
    assert_eq!(scan(&store, &cache).len(), 1);
    assert_eq!(lock(&cache).verdicts.len(), 1);

    // The file was replaced by another recording and analysed again.
    analyse(&store, track(2).id, music(2, 200), 200);
    assert_eq!(scan(&store, &cache), Vec::new());

    // A track that lost its waveform takes its verdicts with it.
    store.delete_tracks(&ids(&[2])).expect("remove track");
    scan(&store, &cache);
    assert!(lock(&cache).verdicts.is_empty());
}
