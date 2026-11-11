// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::PathBuf;

use std::{
    num::NonZeroU32,
    time::{Duration, SystemTime},
};

use sustain_domain::{
    AcousticFeatures, BeatGrid, Bpm, FieldChange, MetadataChange, PlayStatistics, PlaylistEntry,
    Rating, SmartPlaylistBoolField, SmartPlaylistBoolRule, SmartPlaylistDateField,
    SmartPlaylistLimit, SmartPlaylistLimitSelection, SmartPlaylistMatchKind,
    SmartPlaylistNumberField, SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet,
    SmartPlaylistTextField, SmartPlaylistTextOperator, SourceFileStat, SourceFingerprint,
    TrackAudioProperties, TrackContentHash, TrackLocation, TrackMetadata, TrackRelativePath,
};

use sustain_domain::{
    DETAIL_SEGMENTS_PER_SECOND, MusicalKey, PREVIEW_SEGMENT_COUNT, TrackAnalysis, WaveformSegment,
    WaveformSegments,
};

use super::{
    AnalysisCapabilities, AnalysisContext, DuplicateConsolidationPlan, LibraryStore,
    OnlineCapabilities, OnlineContext, Playlist, PlaylistFolder, PlaylistFolderId, SmartPlaylist,
    SmartPlaylistId, SqliteLibraryStore, StoredSmartShuffleIndex, StoredSyncedLyrics,
    StoredWaveform, SyncedLyrics, TagMirrorArtwork, Track, TrackColumnEntry, TrackColumnLayout,
    TrackColumnLayoutScope, TrackColumnSort,
};
use crate::{PlaylistId, StoreResult, TrackId};
use sustain_domain::SyncedLyricsLine;

#[test]
fn a_new_store_is_empty() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");

    assert_eq!(store.tracks(), Ok(Vec::new()));
    assert_eq!(store.playlists(), Ok(Vec::new()));
}

#[test]
fn sqlite_store_rejects_whole_row_replacement_by_id() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let first = track(1, "old.flac");
    let replacement = track(1, "new.flac");

    assert_eq!(store.save_track(first.clone()), Ok(()));
    assert!(store.save_track(replacement).is_err());

    assert_eq!(store.track(first.id), Ok(Some(first)));
}

#[test]
fn sqlite_save_track_normalizes_empty_text_metadata() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let mut track = track(1, "blank.flac");
    track.metadata.title = Some(" \n\t".to_owned());
    track.metadata.artist = Some("  Artist  ".to_owned());
    track.metadata.grouping = Some(String::new());
    track.metadata.comments = Some("\r".to_owned());

    store.save_track(track.clone()).expect("save track");

    let loaded = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(loaded.metadata.title, None);
    assert_eq!(loaded.metadata.artist.as_deref(), Some("  Artist  "));
    assert_eq!(loaded.metadata.grouping, None);
    assert_eq!(loaded.metadata.comments, None);
}

#[test]
fn sqlite_track_key_round_trips_as_its_short_code() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let mut track = track(1, "keyed.flac");
    track.metadata.key = Some(MusicalKey::FsMinor);
    store.save_track(track.clone()).expect("save track");

    let stored: String = store
        .connection_guard()
        .expect("connection")
        .query_row("SELECT musical_key FROM tracks WHERE id = 1", [], |row| {
            row.get(0)
        })
        .expect("stored key");
    assert_eq!(stored, "F#m");
    assert_eq!(
        store
            .track(track.id)
            .expect("load")
            .expect("exists")
            .metadata
            .key,
        Some(MusicalKey::FsMinor)
    );
}

#[test]
fn sqlite_stored_key_that_is_not_a_short_code_is_rejected() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let track = track(1, "corrupt.flac");
    store.save_track(track.clone()).expect("save track");
    store
        .connection_guard()
        .expect("connection")
        .execute("UPDATE tracks SET musical_key = 'Amin' WHERE id = 1", [])
        .expect("inject non-canonical key");

    assert_eq!(
        store.track(track.id),
        Err(super::StoreError::InvalidStoredEnum(
            "musical key: Amin".to_owned()
        ))
    );
}

#[test]
fn sqlite_bpm_round_trips_to_the_hundredth() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let mut track = track(1, "tempo.flac");
    track.metadata.bpm = Bpm::from_hundredths(11_886);
    store.save_track(track.clone()).expect("save track");

    assert_eq!(
        store
            .track(track.id)
            .expect("load")
            .expect("exists")
            .metadata
            .bpm,
        Bpm::from_hundredths(11_886)
    );
}

#[test]
fn sqlite_stored_bpm_that_is_not_a_tempo_is_rejected() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let track = track(1, "corrupt.flac");
    store.save_track(track.clone()).expect("save track");
    store
        .connection_guard()
        .expect("connection")
        .execute("UPDATE tracks SET bpm_hundredths = 0 WHERE id = 1", [])
        .expect("inject zero tempo");

    assert_eq!(
        store.track(track.id),
        Err(super::StoreError::InvalidStoredEnum(
            "bpm hundredths: 0".to_owned()
        ))
    );
}

#[test]
fn availability_update_requires_matching_path() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let original = track(1, "original.flac");
    store.save_track(original.clone()).expect("save track");

    let relocated =
        TrackLocation::available(TrackRelativePath::new("relocated.flac").expect("relative path"));
    store
        .update_track_location(original.id, &relocated)
        .expect("relocate track");

    let stale_missing = original
        .location
        .clone()
        .with_availability(sustain_domain::TrackAvailability::Missing);
    assert!(
        !store
            .update_track_availability_if_path_matches(original.id, &stale_missing)
            .expect("reject stale availability update")
    );
    assert_eq!(
        store
            .track(original.id)
            .expect("load relocated track")
            .expect("track")
            .location,
        relocated
    );

    let relocated_missing = relocated
        .clone()
        .with_availability(sustain_domain::TrackAvailability::Missing);
    assert!(
        store
            .update_track_availability_if_path_matches(original.id, &relocated_missing)
            .expect("accept matching availability update")
    );
    assert_eq!(
        store
            .track(original.id)
            .expect("load missing track")
            .expect("track")
            .location,
        relocated_missing
    );
}

#[test]
fn scanner_merge_preserves_authoritative_fields() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut authoritative = track(1, "a.flac");
    authoritative.metadata.title = Some("User title".to_owned());
    authoritative.metadata.duration = Some(Duration::from_secs(60));
    authoritative.rating = Rating::new(5).expect("rating");
    authoritative.statistics.play_count = 12;
    store
        .save_track(authoritative.clone())
        .expect("seed authoritative row");

    let mut scanned = authoritative.clone();
    scanned.metadata.title = Some("Tag title".to_owned());
    scanned.metadata.duration = Some(Duration::from_secs(90));
    scanned.metadata.bit_depth = Some(24);
    scanned.rating = Rating::unrated();
    scanned.statistics.play_count = 0;
    scanned.file_size_bytes = Some(1234);
    scanned.has_embedded_artwork = Some(true);
    store
        .reconcile_scanned_tracks(&[scanned])
        .expect("merge scanner observations");

    let loaded = store
        .track(authoritative.id)
        .expect("load")
        .expect("track exists");
    assert_eq!(loaded.metadata.title.as_deref(), Some("User title"));
    assert_eq!(loaded.metadata.duration, Some(Duration::from_secs(90)));
    assert_eq!(loaded.metadata.bit_depth, Some(24));
    assert_eq!(loaded.rating, Rating::new(5).expect("rating"));
    assert_eq!(loaded.statistics.play_count, 12);
    assert_eq!(loaded.file_size_bytes, Some(1234));
    assert_eq!(loaded.has_embedded_artwork, Some(true));
}

#[test]
fn online_fill_preserves_existing_values() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut authoritative = track(1, "a.flac");
    authoritative.metadata.title = Some("User title".to_owned());
    authoritative.rating = Rating::new(4).expect("rating");
    store
        .save_track(authoritative.clone())
        .expect("seed authoritative row");

    store
        .fill_missing_track_metadata(
            authoritative.id,
            &MetadataChange {
                title: FieldChange::Set("Remote title".to_owned()),
                artist: FieldChange::Set("Remote artist".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("fill missing metadata");

    let loaded = store
        .track(authoritative.id)
        .expect("load")
        .expect("track exists");
    assert_eq!(loaded.metadata.title.as_deref(), Some("User title"));
    assert_eq!(loaded.metadata.artist.as_deref(), Some("Remote artist"));
    assert_eq!(loaded.rating, Rating::new(4).expect("rating"));
}

#[test]
fn online_fill_generates_missing_sort_fields() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("seed row");

    store
        .fill_missing_track_metadata(
            track.id,
            &MetadataChange {
                artist: FieldChange::Set("The Remote Artist".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("fill missing metadata");

    let loaded = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(loaded.metadata.artist.as_deref(), Some("The Remote Artist"));
    assert_eq!(
        loaded.metadata.artist_sort.as_deref(),
        Some("Remote Artist (The)")
    );
}

#[test]
fn metadata_edit_refreshes_generated_sort_fields() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut track = track(1, "a.flac");
    track.metadata.artist = Some("The Beatles".to_owned());
    track.metadata.fill_missing_generated_sort_fields();
    store.save_track(track.clone()).expect("seed row");

    store
        .apply_track_metadata_change(
            track.id,
            &MetadataChange {
                artist: FieldChange::Set("The Beatles".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("apply edit");

    let loaded = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(loaded.metadata.artist.as_deref(), Some("The Beatles"));
    assert_eq!(
        loaded.metadata.artist_sort.as_deref(),
        Some("Beatles (The)")
    );
}

#[test]
fn metadata_edit_preserves_explicit_sort_fields() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut track = track(1, "a.flac");
    track.metadata.artist = Some("The Beatles".to_owned());
    track.metadata.artist_sort = Some("Beatles, The".to_owned());
    store.save_track(track.clone()).expect("seed row");

    store
        .apply_track_metadata_change(
            track.id,
            &MetadataChange {
                artist: FieldChange::Set("The Beach Boys".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("apply edit");

    let loaded = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(loaded.metadata.artist.as_deref(), Some("The Beach Boys"));
    assert_eq!(loaded.metadata.artist_sort.as_deref(), Some("Beatles, The"));
}

#[test]
fn sqlite_store_round_trips_repeated_playlist_entries() {
    let store = SqliteLibraryStore::open_in_memory().expect("store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");
    let playlist = playlist(1, "Repeat", vec![entry(1, 1, 0), entry(1, 1, 1)]);

    store
        .save_playlist(playlist.clone())
        .expect("save repeated entries");

    assert_eq!(store.playlist(playlist.id), Ok(Some(playlist)));
}

#[test]
fn sqlite_store_reports_freshly_created_only_on_first_open() {
    let dir = std::env::temp_dir().join(format!(
        "sustain_freshness_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).expect("create test directory");
    let path = dir.join("library.sqlite");

    let first =
        SqliteLibraryStore::open(&path, dir.join("cache")).expect("open creates the database file");
    assert!(first.was_freshly_created());
    drop(first);

    let second =
        SqliteLibraryStore::open(&path, dir.join("cache")).expect("reopen existing database");
    assert!(!second.was_freshly_created());
    drop(second);

    std::fs::remove_dir_all(&dir).expect("clean up test directory");
}

#[test]
fn sqlite_store_saves_and_loads_tracks() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let mut track = track(1, "a.flac");
    track.metadata.title = Some("Track".to_owned());
    track.metadata.artist = Some("Artist".to_owned());
    track.metadata.bitrate_kbps = Some(1411);
    track.metadata.duration = Some(std::time::Duration::from_secs(245));
    track.rating = Rating::new(4).expect("valid test rating");

    assert_eq!(store.save_track(track.clone()), Ok(()));

    assert_eq!(store.track(track.id), Ok(Some(track.clone())));
    assert_eq!(store.tracks(), Ok(vec![track]));
}

#[test]
fn sqlite_store_round_trips_file_modified_at_truncated_to_seconds() {
    // The scan fingerprint compares at one-second resolution because the
    // column stores Unix seconds; a sub-second mtime must come back
    // truncated so a freshly stat'd file and its stored row match (#71).
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let mut track = track(1, "fingerprinted.flac");
    track.file_size_bytes = Some(4096);
    track.file_modified_at = Some(std::time::UNIX_EPOCH + Duration::from_millis(1_700_000_000_750));

    assert_eq!(store.save_track(track.clone()), Ok(()));

    let loaded = store
        .track(track.id)
        .expect("query stored track")
        .expect("track is present");
    assert_eq!(
        loaded.file_modified_at,
        Some(std::time::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
    );
    assert_eq!(loaded.file_size_bytes, Some(4096));
}

#[test]
fn sqlite_store_rolls_back_batch_track_save_on_failure() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let first = track(1, "same.flac");
    let duplicate_relative_path = track(2, "same.flac");

    assert!(
        store
            .save_tracks(&[first, duplicate_relative_path])
            .is_err()
    );
    assert_eq!(store.tracks(), Ok(Vec::new()));
}

#[test]
fn sqlite_store_preserves_missing_track_location_state() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let mut track = track(1, "missing.flac");
    track.location = TrackLocation::missing(relative_path("missing.flac"));

    assert_eq!(store.save_track(track.clone()), Ok(()));

    assert_eq!(store.track(track.id), Ok(Some(track.clone())));
    assert_eq!(store.tracks(), Ok(vec![track]));
}

#[test]
fn sqlite_store_round_trips_non_utf8_relative_paths_byte_for_byte() {
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::{ffi::OsString, time::SystemTime};

    // Two distinct Linux filenames differing only by an invalid UTF-8
    // byte (0xFF vs 0xFE). Both render lossily to the same "track�.mp3",
    // so a `to_string_lossy` round-trip would collapse them onto one
    // UNIQUE row and resolve the wrong file after restart. We require
    // byte-exact storage and two distinct rows.
    let first_bytes = b"artist/track\xFF.mp3".to_vec();
    let second_bytes = b"artist/track\xFE.mp3".to_vec();
    let first_path = TrackRelativePath::new(PathBuf::from(OsString::from_vec(first_bytes.clone())))
        .expect("first non-utf8 path is a valid relative path");
    let second_path =
        TrackRelativePath::new(PathBuf::from(OsString::from_vec(second_bytes.clone())))
            .expect("second non-utf8 path is a valid relative path");

    // A real library root with both files on disk, so the reloaded paths
    // can be proven to resolve to the actual files after a restart.
    let root = std::env::temp_dir().join(format!(
        "sustain_nonutf8_paths_{}",
        SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos()
    ));
    std::fs::create_dir_all(root.join("artist")).expect("create library artist directory");
    std::fs::write(first_path.resolve(&root), b"one").expect("write first file");
    std::fs::write(second_path.resolve(&root), b"two").expect("write second file");
    let db_path = root.join("library.sqlite");

    let first_track = Track {
        location: TrackLocation::available(first_path),
        ..track(1, "ignored.mp3")
    };
    let second_track = Track {
        location: TrackLocation::available(second_path),
        ..track(2, "ignored.mp3")
    };

    {
        let store =
            SqliteLibraryStore::open(&db_path, root.join("cache")).expect("open library database");
        assert_eq!(store.save_track(first_track.clone()), Ok(()));
        // Distinct bytes must not collide on the UNIQUE constraint.
        assert_eq!(store.save_track(second_track.clone()), Ok(()));
    }

    // Restart: reopen the database and reload from disk.
    let store =
        SqliteLibraryStore::open(&db_path, root.join("cache")).expect("reopen library database");
    let loaded = store.tracks().expect("reload tracks");
    assert_eq!(loaded.len(), 2, "two distinct non-utf8 paths stay two rows");
    assert_eq!(loaded, vec![first_track, second_track]);

    // Byte-exact round trip, and each reloaded path resolves to its file.
    assert_eq!(
        loaded[0].location.path().as_os_str().as_bytes(),
        first_bytes.as_slice()
    );
    assert_eq!(
        loaded[1].location.path().as_os_str().as_bytes(),
        second_bytes.as_slice()
    );
    assert!(loaded[0].location.absolute_path(&root).exists());
    assert!(loaded[1].location.absolute_path(&root).exists());

    std::fs::remove_dir_all(&root).expect("clean up test library");
}

#[test]
fn sqlite_store_deletes_tracks_and_cascades_to_playlist_entries() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let first_track = track(1, "a.flac");
    let second_track = track(2, "b.flac");
    let stored_playlist = playlist(1, "Favorites", vec![entry(1, 1, 0), entry(1, 2, 1)]);

    assert_eq!(store.save_track(first_track.clone()), Ok(()));
    assert_eq!(store.save_track(second_track.clone()), Ok(()));
    assert_eq!(store.save_playlist(stored_playlist.clone()), Ok(()));

    assert_eq!(store.delete_track(first_track.id), Ok(()));
    assert_eq!(store.track(first_track.id), Ok(None));
    assert_eq!(store.tracks(), Ok(vec![second_track]));

    let stored = store
        .playlist(stored_playlist.id)
        .expect("playlist loads")
        .expect("playlist exists");
    assert_eq!(stored.entries.len(), 1);
    assert_eq!(stored.entries[0].track_id, track_id(2));
}

#[test]
fn deleting_a_missing_track_is_a_no_op() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");

    assert_eq!(store.delete_track(track_id(42)), Ok(()));
}

#[test]
fn deleting_tracks_removes_the_batch_and_its_playlist_entries() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let kept = track(2, "b.flac");
    store
        .save_tracks(&[track(1, "a.flac"), kept.clone(), track(3, "c.flac")])
        .expect("save tracks");
    let stored_playlist = playlist(
        1,
        "Favorites",
        vec![entry(1, 1, 0), entry(1, 2, 1), entry(1, 3, 2)],
    );
    store
        .save_playlist(stored_playlist.clone())
        .expect("save playlist");

    assert_eq!(store.delete_tracks(&[track_id(1), track_id(3)]), Ok(()));

    assert_eq!(store.tracks(), Ok(vec![kept]));
    let stored = store
        .playlist(stored_playlist.id)
        .expect("playlist loads")
        .expect("playlist exists");
    let entries = stored
        .entries
        .iter()
        .map(|entry| entry.track_id)
        .collect::<Vec<_>>();
    assert_eq!(entries, [track_id(2)]);
}

#[test]
fn track_id_at_finds_the_track_stored_at_a_path() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    store.save_track(track(1, "a.flac")).expect("save");
    store.save_track(track(2, "b.flac")).expect("save");

    assert_eq!(
        store.track_id_at(&relative_path("b.flac")),
        Ok(Some(track_id(2)))
    );
    assert_eq!(store.track_id_at(&relative_path("c.flac")), Ok(None));

    store
        .update_track_location(
            track_id(2),
            &TrackLocation::available(relative_path("c.flac")),
        )
        .expect("move");
    assert_eq!(store.track_id_at(&relative_path("b.flac")), Ok(None));
    assert_eq!(
        store.track_id_at(&relative_path("c.flac")),
        Ok(Some(track_id(2)))
    );
}

#[test]
fn sqlite_store_saves_and_loads_playlists() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let track = track(2, "a.flac");
    let playlist = playlist(1, "Favorites", vec![entry(1, 2, 0)]);

    assert_eq!(store.save_track(track), Ok(()));
    assert_eq!(store.save_playlist(playlist.clone()), Ok(()));

    assert_eq!(store.playlist(playlist.id), Ok(Some(playlist.clone())));
    assert_eq!(store.playlists(), Ok(vec![playlist]));
}

#[test]
fn sqlite_store_deletes_playlists() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let playlist = playlist(1, "Favorites", Vec::new());

    assert_eq!(store.save_playlist(playlist.clone()), Ok(()));
    assert_eq!(store.delete_playlist(playlist.id), Ok(()));

    assert_eq!(store.playlist(playlist.id), Ok(None));
    assert_eq!(store.playlists(), Ok(Vec::new()));
}

#[test]
fn sqlite_store_persists_playlist_folder_membership_and_position() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let folder = folder(1, "Mixes", None, 0);
    let mut stored_playlist = playlist(1, "Favorites", Vec::new());
    stored_playlist.parent_folder_id = Some(folder.id);
    stored_playlist.position = 3;

    assert_eq!(store.save_playlist_folder(folder.clone()), Ok(()));
    assert_eq!(store.save_playlist(stored_playlist.clone()), Ok(()));

    let loaded = store
        .playlist(stored_playlist.id)
        .expect("load succeeds")
        .expect("playlist exists");
    assert_eq!(loaded.parent_folder_id, Some(folder.id));
    assert_eq!(loaded.position, 3);
}

#[test]
fn sqlite_store_saves_and_loads_folders() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let folder = folder(1, "Mixes", None, 2);

    assert_eq!(store.save_playlist_folder(folder.clone()), Ok(()));

    assert_eq!(store.playlist_folder(folder.id), Ok(Some(folder.clone())));
    assert_eq!(store.playlist_folders(), Ok(vec![folder]));
}

#[test]
fn sqlite_store_persists_nested_folder_parent() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let parent = folder(1, "Mixes", None, 0);
    let child = folder(2, "Long Drives", Some(parent.id), 0);

    assert_eq!(store.save_playlist_folder(parent.clone()), Ok(()));
    assert_eq!(store.save_playlist_folder(child.clone()), Ok(()));

    let loaded = store
        .playlist_folder(child.id)
        .expect("load succeeds")
        .expect("child exists");
    assert_eq!(loaded.parent_folder_id, Some(parent.id));
}

#[test]
fn sqlite_store_cascade_deletes_folder_and_contents() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let folder = folder(1, "Mixes", None, 0);
    let child_folder = folder_with_id(2, "Long Drives", Some(folder.id), 0);
    let mut child_playlist = playlist(1, "Late Night", Vec::new());
    child_playlist.parent_folder_id = Some(folder.id);
    let child_smart = smart_playlist_with_rules(
        1,
        "Recently Added",
        Some(folder.id),
        0,
        SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::All,
            rules: vec![SmartPlaylistRule::DateInLast {
                field: SmartPlaylistDateField::DateAdded,
                days: NonZeroU32::new(7).expect("positive day count"),
            }],
            limit: None,
        },
    );

    assert_eq!(store.save_playlist_folder(folder.clone()), Ok(()));
    assert_eq!(store.save_playlist_folder(child_folder.clone()), Ok(()));
    assert_eq!(store.save_playlist(child_playlist.clone()), Ok(()));
    assert_eq!(store.save_smart_playlist(child_smart.clone()), Ok(()));

    assert_eq!(store.delete_playlist_folder(folder.id), Ok(()));

    assert_eq!(store.playlist_folder(folder.id), Ok(None));
    assert_eq!(store.playlist_folder(child_folder.id), Ok(None));
    assert_eq!(store.playlist(child_playlist.id), Ok(None));
    assert_eq!(store.smart_playlist(child_smart.id), Ok(None));
}

#[test]
fn sqlite_store_saves_and_loads_smart_playlists() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let smart = smart_playlist_with_rules(1, "Recent", None, 0, simple_text_rule_set());

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));

    assert_eq!(store.smart_playlist(smart.id), Ok(Some(smart.clone())));
    assert_eq!(store.smart_playlists(), Ok(vec![smart]));
}

#[test]
fn sqlite_store_round_trips_every_rule_variant() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let smart = smart_playlist_with_rules(
        1,
        "Variants",
        None,
        0,
        SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::Any,
            limit: None,
            rules: vec![
                SmartPlaylistRule::Text {
                    field: SmartPlaylistTextField::Artist,
                    operator: SmartPlaylistTextOperator::Contains,
                    value: "Massive Attack".to_owned(),
                },
                SmartPlaylistRule::TextIsEmpty {
                    field: SmartPlaylistTextField::Composer,
                },
                SmartPlaylistRule::TextIsPresent {
                    field: SmartPlaylistTextField::Album,
                },
                SmartPlaylistRule::Number {
                    field: SmartPlaylistNumberField::PlayCount,
                    operator: SmartPlaylistNumberOperator::GreaterThan,
                    value: 5,
                },
                SmartPlaylistRule::NumberIsEmpty {
                    field: SmartPlaylistNumberField::Year,
                },
                SmartPlaylistRule::NumberIsPresent {
                    field: SmartPlaylistNumberField::Bpm,
                },
                SmartPlaylistRule::Rating {
                    operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
                    value: Rating::new(4).expect("valid rating"),
                },
                SmartPlaylistRule::DateBefore {
                    field: SmartPlaylistDateField::LastPlayed,
                    date: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000),
                },
                SmartPlaylistRule::DateAfter {
                    field: SmartPlaylistDateField::DateAdded,
                    date: SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000),
                },
                SmartPlaylistRule::DateInLast {
                    field: SmartPlaylistDateField::LastPlayed,
                    days: NonZeroU32::new(30).expect("positive day count"),
                },
                SmartPlaylistRule::DateNotInLast {
                    field: SmartPlaylistDateField::LastSkipped,
                    days: NonZeroU32::new(90).expect("positive day count"),
                },
                SmartPlaylistRule::DateIsEmpty {
                    field: SmartPlaylistDateField::LastPlayed,
                },
                SmartPlaylistRule::DateIsPresent {
                    field: SmartPlaylistDateField::DateAdded,
                },
                SmartPlaylistRule::Bool(SmartPlaylistBoolRule {
                    field: SmartPlaylistBoolField::HasLyrics,
                    equals: true,
                }),
                SmartPlaylistRule::FileIsMissing,
                SmartPlaylistRule::FileIsPresent,
            ],
        },
    );

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    assert_eq!(store.smart_playlist(smart.id), Ok(Some(smart)));
}

#[test]
fn sqlite_store_persists_rule_order() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let rules = vec![
        SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Genre,
            operator: SmartPlaylistTextOperator::Is,
            value: "Trip-Hop".to_owned(),
        },
        SmartPlaylistRule::Number {
            field: SmartPlaylistNumberField::Year,
            operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
            value: 1995,
        },
        SmartPlaylistRule::Rating {
            operator: SmartPlaylistNumberOperator::Equal,
            value: Rating::new(5).expect("valid rating"),
        },
    ];
    let smart = smart_playlist_with_rules(
        1,
        "Mix",
        None,
        0,
        SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::All,
            limit: None,
            rules: rules.clone(),
        },
    );

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    let loaded = store
        .smart_playlist(smart.id)
        .expect("load succeeds")
        .expect("exists");
    assert_eq!(loaded.rules.rules, rules);
}

#[test]
fn sqlite_store_persists_smart_playlist_limit() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let smart = smart_playlist_with_rules(
        1,
        "Top 25",
        None,
        0,
        SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::All,
            limit: Some(SmartPlaylistLimit {
                count: NonZeroU32::new(25).expect("positive limit"),
                selection: SmartPlaylistLimitSelection::MostRecentlyAdded,
            }),
            rules: vec![SmartPlaylistRule::Rating {
                operator: SmartPlaylistNumberOperator::GreaterThanOrEqual,
                value: Rating::new(4).expect("valid rating"),
            }],
        },
    );

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    assert_eq!(store.smart_playlist(smart.id), Ok(Some(smart)));
}

#[test]
fn sqlite_store_persists_match_kind_any() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let smart = smart_playlist_with_rules(
        1,
        "Either Or",
        None,
        0,
        SmartPlaylistRuleSet {
            match_kind: SmartPlaylistMatchKind::Any,
            limit: None,
            rules: vec![SmartPlaylistRule::Text {
                field: SmartPlaylistTextField::Artist,
                operator: SmartPlaylistTextOperator::Is,
                value: "Portishead".to_owned(),
            }],
        },
    );

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    let loaded = store
        .smart_playlist(smart.id)
        .expect("load succeeds")
        .expect("exists");
    assert_eq!(loaded.rules.match_kind, SmartPlaylistMatchKind::Any);
}

#[test]
fn sqlite_store_cascade_deletes_rules_when_smart_playlist_deleted() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let smart = smart_playlist_with_rules(1, "Recent", None, 0, simple_text_rule_set());

    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    assert_eq!(store.delete_smart_playlist(smart.id), Ok(()));
    assert_eq!(store.smart_playlist(smart.id), Ok(None));

    let resaved = smart_playlist_with_rules(1, "Recent", None, 0, simple_text_rule_set());
    assert_eq!(store.save_smart_playlist(resaved.clone()), Ok(()));
    let loaded = store
        .smart_playlist(resaved.id)
        .expect("load succeeds")
        .expect("exists");
    assert_eq!(loaded.rules.rules.len(), resaved.rules.rules.len());
}

// -------- analysis storage --------

/// Build a non-trivial `TrackAnalysis` with a handful of segments
/// in each tier so blob round-trip and the BPM/key write rules
/// both have real data to act on.
/// An analysis whose tempo, when it has one, comes with a grid of that
/// tempo.
fn sample_analysis(bpm: Option<f64>, key: Option<MusicalKey>) -> TrackAnalysis {
    let bpm = bpm.map(|bpm| Bpm::from_f64(bpm).expect("valid tempo"));
    let preview = WaveformSegments {
        segment_duration_ms: 25.0,
        segments: (0..PREVIEW_SEGMENT_COUNT)
            .map(|i| WaveformSegment {
                amplitude: (i % 256) as u8,
                low_band: ((i * 3) % 256) as u8,
                mid_band: ((i * 5) % 256) as u8,
                high_band: ((i * 7) % 256) as u8,
            })
            .collect(),
    };
    let detail = WaveformSegments {
        segment_duration_ms: 1_000.0 / DETAIL_SEGMENTS_PER_SECOND as f32,
        segments: (0..512)
            .map(|i| WaveformSegment {
                amplitude: (i % 256) as u8,
                low_band: ((i + 1) % 256) as u8,
                mid_band: ((i + 2) % 256) as u8,
                high_band: ((i + 3) % 256) as u8,
            })
            .collect(),
    };
    TrackAnalysis {
        bpm,
        key,
        beat_grid: bpm.map(|tempo| {
            BeatGrid::new(tempo, Duration::from_micros(123_456), 2).expect("valid grid")
        }),
        waveform_preview: preview,
        waveform_detail: detail,
        acoustics: None,
    }
}

fn finite_acoustics() -> AcousticFeatures {
    AcousticFeatures {
        integrated_lufs: -14.0,
        short_term_lufs_max: -10.0,
        loudness_range_lu: 5.0,
        onset_rate_hz: 2.0,
        low_band_ratio: 0.4,
        mid_band_ratio: 0.4,
        high_band_ratio: 0.2,
        low_band_variation: 0.5,
        tonalness: 0.8,
    }
}

/// Standard context used by analysis tests: analyzer_version 1
/// plus caller-supplied wall-clock.
fn ctx(now_unix: i64) -> AnalysisContext {
    AnalysisContext {
        analyzer_version: 1,
        now_unix,
    }
}

#[test]
fn record_analysis_round_trips_waveform_bytes() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");

    let analysis = sample_analysis(Some(126.0), Some(MusicalKey::DMinor));
    store
        .record_analysis(
            track.id,
            &analysis,
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record analysis");

    let stored = store
        .load_waveform(track.id)
        .expect("load")
        .expect("waveform exists");
    assert_eq!(stored.preview.segments, analysis.waveform_preview.segments);
    assert_eq!(stored.detail.segments, analysis.waveform_detail.segments);
    assert!(
        (stored.preview.segment_duration_ms - analysis.waveform_preview.segment_duration_ms).abs()
            < 1e-3
    );
}

#[test]
fn record_analysis_ignores_non_finite_acoustics() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");
    let mut analysis = sample_analysis(Some(126.0), Some(MusicalKey::DMinor));
    analysis.acoustics = Some(AcousticFeatures {
        integrated_lufs: f32::NAN,
        ..finite_acoustics()
    });

    store
        .record_analysis(
            track.id,
            &analysis,
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record analysis");

    assert!(
        store
            .load_all_acoustics()
            .expect("load acoustics")
            .is_empty()
    );
}

#[test]
fn record_analysis_keeps_the_latest_beat_grid() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");
    assert_eq!(store.load_beat_grid(track.id), Ok(None));

    let analysis = sample_analysis(Some(126.37), None);
    store
        .record_analysis(
            track.id,
            &analysis,
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record analysis");
    assert_eq!(store.load_beat_grid(track.id), Ok(analysis.beat_grid));

    // An analysis without the tempo capability leaves the grid alone.
    let without_tempo = AnalysisCapabilities {
        bpm: false,
        key: true,
        audio: true,
    };
    store
        .record_analysis(
            track.id,
            &sample_analysis(None, None),
            without_tempo,
            ctx(1_700_000_001),
        )
        .expect("record analysis without tempo");
    assert_eq!(store.load_beat_grid(track.id), Ok(analysis.beat_grid));

    // A tempo analysis that finds no grid drops the stale one.
    store
        .record_analysis(
            track.id,
            &sample_analysis(None, None),
            AnalysisCapabilities::all(),
            ctx(1_700_000_002),
        )
        .expect("record analysis without grid");
    assert_eq!(store.load_beat_grid(track.id), Ok(None));
}

#[test]
fn load_beat_grids_returns_the_grids_of_the_asked_tracks() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let gridded = track(1, "gridded.flac");
    let other = track(2, "other.flac");
    let bare = track(3, "bare.flac");
    for track in [&gridded, &other, &bare] {
        store.save_track(track.clone()).expect("save track");
    }
    let tempo_a = sample_analysis(Some(126.37), None);
    let tempo_b = sample_analysis(Some(98.0), None);
    for (track, analysis) in [(&gridded, &tempo_a), (&other, &tempo_b)] {
        store
            .record_analysis(
                track.id,
                analysis,
                AnalysisCapabilities::all(),
                ctx(1_700_000_000),
            )
            .expect("record analysis");
    }

    let grids = store
        .load_beat_grids(&[gridded.id, bare.id])
        .expect("load grids");
    assert_eq!(grids.len(), 1);
    assert_eq!(grids.get(&gridded.id).copied(), tempo_a.beat_grid);
    assert!(store.load_beat_grids(&[]).expect("load none").is_empty());
}

#[test]
fn record_analysis_replaces_bpms_the_user_did_not_type() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    // No BPM or key yet: analysis fills both.
    let blank = track(1, "blank.flac");
    store.save_track(blank.clone()).expect("save blank");

    // BPM and key read from file tags: the measured tempo replaces the
    // tag's BPM; the tag's key stays.
    let mut tagged = track(2, "tagged.flac");
    tagged.metadata.bpm = Bpm::from_whole(63);
    tagged.metadata.key = Some(MusicalKey::AMinor);
    store.save_track(tagged.clone()).expect("save tagged");

    // A BPM the user typed: analysis leaves it.
    let mut typed = track(3, "typed.flac");
    typed.metadata.bpm = Bpm::from_whole(95);
    typed.metadata.bpm_set_by_user = true;
    store.save_track(typed.clone()).expect("save typed");

    // A measured tempo keeps its hundredths.
    let analysis = sample_analysis(Some(126.37), Some(MusicalKey::DMinor));
    for id in [blank.id, tagged.id, typed.id] {
        store
            .record_analysis(
                id,
                &analysis,
                AnalysisCapabilities::all(),
                ctx(1_700_000_000),
            )
            .expect("record");
    }

    let loaded_blank = store.track(blank.id).expect("load blank").expect("exists");
    assert_eq!(loaded_blank.metadata.bpm, Bpm::from_hundredths(12_637));
    assert_eq!(loaded_blank.metadata.key, Some(MusicalKey::DMinor));

    let loaded_tagged = store
        .track(tagged.id)
        .expect("load tagged")
        .expect("exists");
    assert_eq!(loaded_tagged.metadata.bpm, Bpm::from_hundredths(12_637));
    assert!(!loaded_tagged.metadata.bpm_set_by_user);
    assert_eq!(loaded_tagged.metadata.key, Some(MusicalKey::AMinor));

    let loaded_typed = store.track(typed.id).expect("load typed").expect("exists");
    assert_eq!(loaded_typed.metadata.bpm, Bpm::from_whole(95));
    assert!(loaded_typed.metadata.bpm_set_by_user);
}

#[test]
fn a_bpm_edit_survives_analysis_until_the_user_clears_it() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut tagged = track(1, "tagged.flac");
    tagged.metadata.bpm = Bpm::from_whole(63);
    store.save_track(tagged.clone()).expect("save");
    let analysis = sample_analysis(Some(126.37), None);
    let analyze = || {
        store
            .record_analysis(
                tagged.id,
                &analysis,
                AnalysisCapabilities::all(),
                ctx(1_700_000_000),
            )
            .expect("record");
        store
            .track(tagged.id)
            .expect("load")
            .expect("exists")
            .metadata
    };
    let needs = |capabilities| {
        store
            .filter_tracks_needing_analysis(&[tagged.id], capabilities, 1)
            .expect("filter")
    };
    let tempo_only = AnalysisCapabilities {
        bpm: true,
        key: false,
        audio: false,
    };
    let key_and_audio = AnalysisCapabilities {
        bpm: false,
        key: true,
        audio: true,
    };
    let bpm_change = |bpm| MetadataChange {
        bpm,
        ..MetadataChange::default()
    };

    analyze();
    store
        .apply_track_metadata_change(
            tagged.id,
            &bpm_change(FieldChange::Set(Bpm::from_whole(128).expect("tempo"))),
        )
        .expect("edit");
    assert!(
        needs(tempo_only).is_empty(),
        "a typed BPM needs no measuring"
    );
    let after_edit = analyze();
    assert_eq!(after_edit.bpm, Bpm::from_whole(128));
    assert!(after_edit.bpm_set_by_user);

    // Clearing goes through the Get Info path, which mirrors tags too.
    store
        .apply_track_metadata_change_and_enqueue_mirror(tagged.id, &bpm_change(FieldChange::Clear))
        .expect("clear");
    assert_eq!(needs(tempo_only), vec![tagged.id]);
    assert!(needs(key_and_audio).is_empty());
    let after_clear = analyze();
    assert_eq!(after_clear.bpm, Bpm::from_hundredths(12_637));
    assert!(!after_clear.bpm_set_by_user);
}

#[test]
fn tracks_needing_analysis_lists_only_un_attempted() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    // 3 tracks, 1 missing, 1 already waveform-analyzed.
    let alpha = track(1, "alpha.flac");
    let mut beta = track(2, "beta.flac");
    beta.location = beta
        .location
        .with_availability(sustain_domain::TrackAvailability::Missing);
    let gamma = track(3, "gamma.flac");
    for t in [&alpha, &beta, &gamma] {
        store.save_track(t.clone()).expect("save");
    }

    // Mark gamma's waveform as attempted.
    store
        .record_analysis(
            gamma.id,
            &sample_analysis(None, None),
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            ctx(1_700_000_000),
        )
        .expect("record waveform for gamma");

    let needs = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            1,
            100,
        )
        .expect("query");
    // Only alpha qualifies: beta is missing, gamma is attempted at version 1.
    assert_eq!(needs, vec![alpha.id]);
}

#[test]
fn filter_tracks_needing_analysis_drops_cached_and_missing() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    // Same setup as `run_tracks_needing_analysis_lists_only_un_attempted`
    // but exercised through the bulk-filter path: caller passes the
    // full set of ids it cares about (mirroring a per-playlist
    // explicit run) and the store returns only the ones that still
    // need at least one of the requested capabilities.
    let alpha = track(1, "alpha.flac");
    let mut beta = track(2, "beta.flac");
    beta.location = beta
        .location
        .with_availability(sustain_domain::TrackAvailability::Missing);
    let gamma = track(3, "gamma.flac");
    for t in [&alpha, &beta, &gamma] {
        store.save_track(t.clone()).expect("save");
    }

    store
        .record_analysis(
            gamma.id,
            &sample_analysis(None, None),
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            ctx(1_700_000_000),
        )
        .expect("record waveform for gamma");

    let all_ids = vec![alpha.id, beta.id, gamma.id];

    // Only waveform requested: alpha needs it (never attempted),
    // beta is missing, gamma is already attempted at v1. Filter
    // returns only alpha.
    let filtered = store
        .filter_tracks_needing_analysis(
            &all_ids,
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            1,
        )
        .expect("filter");
    assert_eq!(filtered, vec![alpha.id]);

    // BPM requested: alpha and gamma both qualify (neither was
    // BPM-attempted), beta is still missing. Order matches input
    // order so playlist sequencing survives downstream.
    let filtered_bpm = store
        .filter_tracks_needing_analysis(
            &all_ids,
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            1,
        )
        .expect("filter");
    assert_eq!(filtered_bpm, vec![alpha.id, gamma.id]);

    // Empty capability mask -> empty result, regardless of input.
    let filtered_empty = store
        .filter_tracks_needing_analysis(&all_ids, AnalysisCapabilities::default(), 1)
        .expect("filter");
    assert!(filtered_empty.is_empty());

    // Empty input id list -> empty result.
    let filtered_no_ids = store
        .filter_tracks_needing_analysis(
            &[],
            AnalysisCapabilities {
                bpm: true,
                key: true,
                audio: true,
            },
            1,
        )
        .expect("filter");
    assert!(filtered_no_ids.is_empty());

    // Version bump re-enrolls cached tracks: gamma now needs
    // waveform again (its stamp is at version 1).
    let filtered_v2 = store
        .filter_tracks_needing_analysis(
            &all_ids,
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            2,
        )
        .expect("filter");
    assert_eq!(filtered_v2, vec![alpha.id, gamma.id]);
}

#[test]
fn failed_attempt_prevents_immediate_retry() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save");

    store
        .record_analysis_attempt_failure(
            track.id,
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            ctx(1_700_000_000),
        )
        .expect("record failure");

    let needs = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            1,
            100,
        )
        .expect("query");
    assert!(
        needs.is_empty(),
        "failed attempts should not requeue at same analyzer_version"
    );

    // But a version bump re-enrolls the track.
    let needs_v2 = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            2,
            100,
        )
        .expect("query v2");
    assert_eq!(needs_v2, vec![track.id]);
}

#[test]
fn sqlite_cascade_delete_clears_analysis_rows() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(120.0), Some(MusicalKey::CMajor)),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record");
    assert!(matches!(store.load_waveform(t.id), Ok(Some(_))));

    store.delete_track(t.id).expect("delete");
    assert_eq!(store.load_waveform(t.id), Ok(None));
    // Re-saving the same id should not trip a unique-violation
    // because the cascade dropped the analysis/waveform rows too.
    store.save_track(t.clone()).expect("re-save");
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(130.0), Some(MusicalKey::EMinor)),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record again");
    assert!(matches!(store.load_waveform(t.id), Ok(Some(_))));
}

/// A file-backed store in `dir`, its waveform cache in `dir/cache`.
fn open_store_in(dir: &std::path::Path) -> SqliteLibraryStore {
    SqliteLibraryStore::open(dir.join("library.sqlite"), dir.join("cache")).expect("open store")
}

fn waveform_cache_file(dir: &std::path::Path) -> PathBuf {
    dir.join("cache").join("waveform-cache.sqlite")
}

fn audio_only() -> AnalysisCapabilities {
    AnalysisCapabilities {
        bpm: false,
        key: false,
        audio: true,
    }
}

fn needs_audio_analysis(store: &dyn LibraryStore, track_id: TrackId) -> bool {
    let needing = store
        .tracks_needing_analysis(audio_only(), 1, 10)
        .expect("query");
    let filtered = store
        .filter_tracks_needing_analysis(&[track_id], audio_only(), 1)
        .expect("filter");
    assert_eq!(needing.contains(&track_id), filtered.contains(&track_id));
    filtered.contains(&track_id)
}

/// A store in a fresh directory holding track 1, analysed once.
fn analysed_store_in(label: &str) -> (PathBuf, TrackId) {
    let dir = unique_store_test_directory(label);
    std::fs::create_dir_all(&dir).expect("create test directory");
    let store = open_store_in(&dir);
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(120.0), Some(MusicalKey::CMajor)),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record");
    (dir, t.id)
}

#[test]
fn sqlite_waveforms_persist_in_the_waveform_cache() {
    let (dir, id) = analysed_store_in("waveform_cache_persist");
    assert!(waveform_cache_file(&dir).is_file());

    let store = open_store_in(&dir);
    assert!(store.waveform_cache_status().is_available());
    assert!(matches!(store.load_waveform(id), Ok(Some(_))));
    assert!(!needs_audio_analysis(&store, id));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_lost_waveform_cache_analyses_audio_again() {
    let (dir, id) = analysed_store_in("waveform_cache_lost");
    std::fs::remove_dir_all(dir.join("cache")).expect("wipe the cache");

    let store = open_store_in(&dir);
    assert_eq!(store.load_waveform(id), Ok(None));
    assert!(needs_audio_analysis(&store, id));
    // Only audio analysis produces waveforms.
    let bpm_and_key = AnalysisCapabilities {
        audio: false,
        ..AnalysisCapabilities::all()
    };
    assert_eq!(
        store.tracks_needing_analysis(bpm_and_key, 1, 10),
        Ok(Vec::new())
    );
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_restored_library_ignores_waveforms_of_later_attempts() {
    let (dir, id) = analysed_store_in("waveform_cache_restore");
    let library = dir.join("library.sqlite");
    let backup = dir.join("backup.sqlite");
    std::fs::copy(&library, &backup).expect("back the library up");
    {
        let store = open_store_in(&dir);
        store
            .record_analysis(
                id,
                &sample_analysis(Some(121.0), Some(MusicalKey::CMajor)),
                AnalysisCapabilities::all(),
                ctx(1_700_000_000),
            )
            .expect("analyse again");
    }
    std::fs::copy(&backup, &library).expect("restore the backup");

    let store = open_store_in(&dir);
    assert_eq!(store.load_waveform(id), Ok(None));
    assert!(needs_audio_analysis(&store, id));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_audio_attempts_without_a_waveform_are_not_retried() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory");
    let long = track(1, "long.flac");
    let broken = track(2, "broken.flac");
    store.save_track(long.clone()).expect("save");
    store.save_track(broken.clone()).expect("save");
    let mut analysis = sample_analysis(Some(120.0), None);
    analysis.waveform_preview.segments.clear();
    analysis.waveform_detail.segments.clear();

    store
        .record_analysis(long.id, &analysis, audio_only(), ctx(1_700_000_000))
        .expect("record");
    store
        .record_analysis_attempt_failure(broken.id, audio_only(), ctx(1_700_000_000))
        .expect("record failure");

    for id in [long.id, broken.id] {
        assert_eq!(store.load_waveform(id), Ok(None));
        assert!(!needs_audio_analysis(&store, id));
    }
}

#[test]
fn sqlite_corrupt_waveform_cache_is_recreated() {
    let (dir, id) = analysed_store_in("waveform_cache_corrupt");
    std::fs::write(waveform_cache_file(&dir), vec![0x5a; 8192]).expect("corrupt the cache");

    let store = open_store_in(&dir);
    assert!(store.waveform_cache_status().is_available());
    assert!(needs_audio_analysis(&store, id));
    store
        .record_analysis(
            id,
            &sample_analysis(Some(120.0), None),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record");
    assert!(matches!(store.load_waveform(id), Ok(Some(_))));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_waveform_cache_of_another_format_is_emptied() {
    let (dir, id) = analysed_store_in("waveform_cache_format");
    {
        let cache = rusqlite::Connection::open(waveform_cache_file(&dir)).expect("open cache");
        cache
            .pragma_update(None, "user_version", 999)
            .expect("set version");
    }

    let store = open_store_in(&dir);
    assert!(store.waveform_cache_status().is_available());
    assert_eq!(store.load_waveform(id), Ok(None));
    assert!(needs_audio_analysis(&store, id));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn sqlite_unopenable_waveform_cache_leaves_the_library_usable() {
    let dir = unique_store_test_directory("waveform_cache_unopenable");
    std::fs::create_dir_all(&dir).expect("create test directory");
    // A file where the cache directory should be.
    std::fs::write(dir.join("cache"), b"").expect("block the cache directory");

    let store = open_store_in(&dir);
    assert!(!store.waveform_cache_status().is_available());
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    assert!(needs_audio_analysis(&store, t.id));
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(120.0), None),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record");
    // Nothing is kept, and analysis does not chase what cannot be kept.
    assert_eq!(store.load_waveform(t.id), Ok(None));
    assert!(!needs_audio_analysis(&store, t.id));
    drop(store);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn single_capability_attempt_leaves_the_others_stale() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(120.0), Some(MusicalKey::CMajor)),
            AnalysisCapabilities::all(),
            ctx(1_000),
        )
        .expect("analyse everything at version 1");
    let bpm_only = AnalysisCapabilities {
        bpm: true,
        key: false,
        audio: false,
    };
    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(121.0), None),
            bpm_only,
            AnalysisContext {
                analyzer_version: 2,
                now_unix: 2_000,
            },
        )
        .expect("measure the tempo again at version 2");

    let needing = |capabilities| {
        store
            .tracks_needing_analysis(capabilities, 2, 10)
            .expect("query")
    };
    assert_eq!(needing(bpm_only), Vec::new());
    for stale in [
        AnalysisCapabilities {
            key: true,
            ..AnalysisCapabilities::none()
        },
        audio_only(),
    ] {
        assert_eq!(needing(stale), vec![t.id]);
        assert_eq!(
            store.filter_tracks_needing_analysis(&[t.id], stale, 2),
            Ok(vec![t.id])
        );
    }
}

#[test]
fn partial_capability_record_preserves_other_attempts() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");

    // First, record only BPM.
    store
        .record_analysis_attempt_failure(
            t.id,
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            ctx(1_000),
        )
        .expect("record bpm");

    // Then, separately, record only waveform.
    store
        .record_analysis(
            t.id,
            &sample_analysis(None, None),
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            ctx(2_000),
        )
        .expect("record waveform");

    // Both capabilities should now be marked attempted; only key
    // remains pending.
    let needs_bpm = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: true,
                key: false,
                audio: false,
            },
            1,
            10,
        )
        .expect("q bpm");
    let needs_wave = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: false,
                key: false,
                audio: true,
            },
            1,
            10,
        )
        .expect("q wave");
    let needs_key = store
        .tracks_needing_analysis(
            AnalysisCapabilities {
                bpm: false,
                key: true,
                audio: false,
            },
            1,
            10,
        )
        .expect("q key");
    assert!(needs_bpm.is_empty(), "bpm should be marked attempted");
    assert!(needs_wave.is_empty(), "waveform should be marked attempted");
    assert_eq!(needs_key, vec![t.id], "key still pending");
}

#[test]
fn empty_capabilities_record_is_no_op() {
    let store = SqliteLibraryStore::open_in_memory().expect("open");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");

    store
        .record_analysis(
            t.id,
            &sample_analysis(Some(120.0), Some(MusicalKey::CMajor)),
            AnalysisCapabilities::none(),
            ctx(1_000),
        )
        .expect("no-op");
    // No bookkeeping row → track is still listed as needing analysis.
    let needs = store
        .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 10)
        .expect("query");
    assert_eq!(needs, vec![t.id]);
    assert_eq!(store.load_waveform(t.id), Ok(None));
}

#[test]
fn stored_waveform_equality_is_well_defined() {
    // Sanity check that the public StoredWaveform exposes PartialEq
    // so call sites can do simple assertions in tests.
    let stored = StoredWaveform {
        preview: WaveformSegments {
            segment_duration_ms: 25.0,
            segments: vec![WaveformSegment::silent()],
        },
        detail: WaveformSegments {
            segment_duration_ms: 6.0,
            segments: vec![WaveformSegment::silent()],
        },
    };
    assert_eq!(stored.clone(), stored);
}

// -------- end analysis storage --------

// -------- synced lyrics storage --------

fn sample_synced_lyrics() -> SyncedLyrics {
    SyncedLyrics {
        lines: vec![
            SyncedLyricsLine {
                at_ms: 1_000,
                text: "Hello".to_owned(),
            },
            SyncedLyricsLine {
                at_ms: 3_500,
                text: "World".to_owned(),
            },
        ],
    }
}

#[test]
fn record_and_load_synced_lyrics_round_trips() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    let lyrics = sample_synced_lyrics();
    store
        .record_synced_lyrics(t.id, &lyrics, "lrclib")
        .expect("record");

    let loaded = store
        .load_synced_lyrics(t.id)
        .expect("load")
        .expect("present");
    assert_eq!(loaded.lyrics, lyrics);
    assert_eq!(loaded.source, "lrclib");
}

#[test]
fn record_synced_lyrics_replaces_previous() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_synced_lyrics(t.id, &sample_synced_lyrics(), "lrclib")
        .expect("first");
    let replacement = SyncedLyrics {
        lines: vec![SyncedLyricsLine {
            at_ms: 500,
            text: "Only".to_owned(),
        }],
    };
    store
        .record_synced_lyrics(t.id, &replacement, "user")
        .expect("second");

    let loaded = store
        .load_synced_lyrics(t.id)
        .expect("load")
        .expect("present");
    assert_eq!(loaded.lyrics, replacement);
    assert_eq!(loaded.source, "user");
}

#[test]
fn record_synced_lyrics_empty_is_no_op() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_synced_lyrics(t.id, &sample_synced_lyrics(), "lrclib")
        .expect("seed");
    store
        .record_synced_lyrics(t.id, &SyncedLyrics::default(), "noop")
        .expect("no-op write");

    let loaded = store
        .load_synced_lyrics(t.id)
        .expect("load")
        .expect("present");
    assert_eq!(loaded.source, "lrclib");
    assert_eq!(loaded.lyrics, sample_synced_lyrics());
}

#[test]
fn clear_synced_lyrics_removes_row() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_synced_lyrics(t.id, &sample_synced_lyrics(), "lrclib")
        .expect("seed");
    store.clear_synced_lyrics(t.id).expect("clear");
    assert_eq!(store.load_synced_lyrics(t.id), Ok(None));
    // Clearing again is a no-op.
    store.clear_synced_lyrics(t.id).expect("clear again");
}

#[test]
fn sqlite_cascade_delete_clears_synced_lyrics() {
    let store = SqliteLibraryStore::open_in_memory().expect("open");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_synced_lyrics(t.id, &sample_synced_lyrics(), "lrclib")
        .expect("seed");
    store.delete_track(t.id).expect("delete");
    assert_eq!(store.load_synced_lyrics(t.id), Ok(None));
}

#[test]
fn stored_synced_lyrics_equality_is_well_defined() {
    let s = StoredSyncedLyrics {
        lyrics: sample_synced_lyrics(),
        source: "lrclib".to_owned(),
    };
    assert_eq!(s.clone(), s);
}

// -------- end synced lyrics storage --------

// -------- online status storage --------

fn online_ctx(now_unix: i64) -> OnlineContext {
    OnlineContext {
        provider_version: 1,
        now_unix,
    }
}

#[test]
fn record_online_attempt_marks_only_requested_capabilities() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");

    // Lyrics-only attempt.
    store
        .record_online_attempt(
            t.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            online_ctx(1_700_000_000),
        )
        .expect("record lyrics");

    // Should drop out of "needs lyrics" but still appear in
    // "needs artwork" — artwork was never stamped.
    let needs_lyrics = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            10,
        )
        .expect("query");
    assert!(needs_lyrics.is_empty(), "lyrics attempt should be recorded");

    let needs_artwork = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: true,
                tags: false,
                lyrics: false,
            },
            1,
            10,
        )
        .expect("query");
    assert_eq!(
        needs_artwork,
        vec![t.id],
        "artwork attempt was not requested"
    );
}

#[test]
fn filter_tracks_needing_online_drops_attempted_missing_and_embedded() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    // 4 tracks:
    //   alpha   - never attempted, no embedded artwork
    //   beta    - missing on disk
    //   gamma   - has embedded artwork (artwork branch must skip)
    //   delta   - already lyrics-attempted at v1
    let alpha = track(1, "alpha.flac");
    let mut beta = track(2, "beta.flac");
    beta.location = beta
        .location
        .with_availability(sustain_domain::TrackAvailability::Missing);
    let mut gamma = track(3, "gamma.flac");
    gamma.has_embedded_artwork = Some(true);
    let delta = track(4, "delta.flac");
    for t in [&alpha, &beta, &gamma, &delta] {
        store.save_track(t.clone()).expect("save");
    }

    store
        .record_online_attempt(
            delta.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            online_ctx(1_000),
        )
        .expect("record lyrics for delta");

    let all_ids = vec![alpha.id, beta.id, gamma.id, delta.id];

    // Lyrics only: alpha + gamma still need it (delta was
    // attempted, beta is missing).
    let needs_lyrics = store
        .filter_tracks_needing_online(
            &all_ids,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
        )
        .expect("filter");
    assert_eq!(needs_lyrics, vec![alpha.id, gamma.id]);

    // Artwork only: gamma is excluded by the embedded-artwork
    // guard; beta by the missing guard; delta never attempted
    // artwork so it still needs it. Result preserves input order.
    let needs_artwork = store
        .filter_tracks_needing_online(
            &all_ids,
            OnlineCapabilities {
                artwork: true,
                tags: false,
                lyrics: false,
            },
            1,
        )
        .expect("filter");
    assert_eq!(needs_artwork, vec![alpha.id, delta.id]);

    // Empty capability mask -> empty result.
    let none = store
        .filter_tracks_needing_online(&all_ids, OnlineCapabilities::default(), 1)
        .expect("filter");
    assert!(none.is_empty());

    // Empty input -> empty result.
    let none = store
        .filter_tracks_needing_online(&[], OnlineCapabilities::all(), 1)
        .expect("filter");
    assert!(none.is_empty());
}

#[test]
fn online_attempts_partial_capability_preserves_other() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");

    store
        .record_online_attempt(
            t.id,
            OnlineCapabilities {
                artwork: true,
                tags: false,
                lyrics: false,
            },
            online_ctx(1_000),
        )
        .expect("record artwork");
    store
        .record_online_attempt(
            t.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            online_ctx(2_000),
        )
        .expect("record lyrics");

    // Both artwork + lyrics attempts must be recorded; tags is
    // still pending.
    let needs = store
        .tracks_needing_online(OnlineCapabilities::all(), 1, 10)
        .expect("query");
    assert_eq!(needs, vec![t.id], "tags is still un-attempted");

    let needs_artwork = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: true,
                tags: false,
                lyrics: false,
            },
            1,
            10,
        )
        .expect("query");
    let needs_lyrics = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            10,
        )
        .expect("query");
    assert!(needs_artwork.is_empty());
    assert!(needs_lyrics.is_empty());
}

#[test]
fn online_query_skips_missing_tracks() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let present = track(1, "present.flac");
    let mut missing = track(2, "missing.flac");
    missing.location = missing
        .location
        .with_availability(sustain_domain::TrackAvailability::Missing);
    for t in [&present, &missing] {
        store.save_track(t.clone()).expect("save");
    }
    let needs = store
        .tracks_needing_online(OnlineCapabilities::all(), 1, 10)
        .expect("query");
    assert_eq!(needs, vec![present.id]);
}

#[test]
fn online_query_invalidates_stale_provider_version() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_online_attempt(
            t.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            OnlineContext {
                provider_version: 1,
                now_unix: 1_000,
            },
        )
        .expect("record");

    // Same version: track is satisfied.
    assert!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: false,
                    tags: false,
                    lyrics: true,
                },
                1,
                10,
            )
            .expect("query")
            .is_empty()
    );

    // Newer version: track re-qualifies.
    assert_eq!(
        store
            .tracks_needing_online(
                OnlineCapabilities {
                    artwork: false,
                    tags: false,
                    lyrics: true,
                },
                2,
                10,
            )
            .expect("query"),
        vec![t.id]
    );
}

#[test]
fn online_query_excludes_tracks_with_embedded_artwork() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut with_art = track(1, "with_art.flac");
    with_art.has_embedded_artwork = Some(true);
    let mut without_art = track(2, "without_art.flac");
    without_art.has_embedded_artwork = Some(false);
    let unknown = track(3, "unknown.flac"); // has_embedded_artwork = None
    for t in [&with_art, &without_art, &unknown] {
        store.save_track(t.clone()).expect("save");
    }
    // Artwork-only request: the seeded picture excludes id 1; ids 2 and 3
    // (false and "never scanned") remain candidates.
    let mut needs = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: true,
                tags: false,
                lyrics: false,
            },
            1,
            10,
        )
        .expect("query");
    needs.sort();
    assert_eq!(needs, vec![without_art.id, unknown.id]);

    // Lyrics-only request: the artwork bit is irrelevant, so all three
    // tracks remain candidates.
    let mut needs_lyrics = store
        .tracks_needing_online(
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            1,
            10,
        )
        .expect("query");
    needs_lyrics.sort();
    assert_eq!(needs_lyrics, vec![with_art.id, without_art.id, unknown.id]);
}

#[test]
fn empty_online_capabilities_record_is_no_op() {
    let store = SqliteLibraryStore::open_in_memory().expect("open");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_online_attempt(t.id, OnlineCapabilities::none(), online_ctx(1_000))
        .expect("no-op");
    // No row → track still appears in any non-empty query.
    let needs = store
        .tracks_needing_online(OnlineCapabilities::all(), 1, 10)
        .expect("query");
    assert_eq!(needs, vec![t.id]);
}

#[test]
fn sqlite_cascade_delete_clears_online_status() {
    let store = SqliteLibraryStore::open_in_memory().expect("open");
    let t = track(1, "a.flac");
    store.save_track(t.clone()).expect("save");
    store
        .record_online_attempt(
            t.id,
            OnlineCapabilities {
                artwork: false,
                tags: false,
                lyrics: true,
            },
            online_ctx(1_000),
        )
        .expect("record");
    store.delete_track(t.id).expect("delete");
    // Re-add and query — the cascading delete must have dropped
    // the bookkeeping row, so the track qualifies again.
    store.save_track(t.clone()).expect("re-save");
    let needs = store
        .tracks_needing_online(OnlineCapabilities::all(), 1, 10)
        .expect("query");
    assert_eq!(needs, vec![t.id]);
}

// -------- end online status storage --------

fn track(id: i64, path: &str) -> Track {
    Track {
        id: track_id(id),
        location: TrackLocation::available(relative_path(path)),
        metadata: TrackMetadata::default(),
        rating: Rating::unrated(),
        statistics: PlayStatistics::default(),
        file_size_bytes: None,
        has_embedded_artwork: None,
        file_modified_at: None,
    }
}

fn relative_path(path: &str) -> TrackRelativePath {
    TrackRelativePath::new(PathBuf::from(path)).expect("test path is relative")
}

fn playlist(id: i64, name: &str, entries: Vec<PlaylistEntry>) -> Playlist {
    Playlist {
        id: playlist_id(id),
        name: name.to_owned(),
        parent_folder_id: None,
        position: 0,
        entries,
    }
}

fn entry(playlist_id_value: i64, track_id_value: i64, position: u32) -> PlaylistEntry {
    PlaylistEntry {
        playlist_id: playlist_id(playlist_id_value),
        track_id: track_id(track_id_value),
        position,
    }
}

fn track_id(value: i64) -> TrackId {
    positive_id(TrackId::new(value))
}

fn playlist_id(value: i64) -> PlaylistId {
    positive_id(PlaylistId::new(value))
}

fn folder_id(value: i64) -> PlaylistFolderId {
    positive_id(PlaylistFolderId::new(value))
}

fn smart_id(value: i64) -> SmartPlaylistId {
    positive_id(SmartPlaylistId::new(value))
}

fn folder(
    id: i64,
    name: &str,
    parent_folder_id: Option<PlaylistFolderId>,
    position: u32,
) -> PlaylistFolder {
    PlaylistFolder {
        id: folder_id(id),
        name: name.to_owned(),
        parent_folder_id,
        position,
    }
}

fn folder_with_id(
    id: i64,
    name: &str,
    parent_folder_id: Option<PlaylistFolderId>,
    position: u32,
) -> PlaylistFolder {
    folder(id, name, parent_folder_id, position)
}

fn smart_playlist_with_rules(
    id: i64,
    name: &str,
    parent_folder_id: Option<PlaylistFolderId>,
    position: u32,
    rules: SmartPlaylistRuleSet,
) -> SmartPlaylist {
    SmartPlaylist {
        id: smart_id(id),
        name: name.to_owned(),
        parent_folder_id,
        position,
        rules,
    }
}

fn simple_text_rule_set() -> SmartPlaylistRuleSet {
    SmartPlaylistRuleSet {
        match_kind: SmartPlaylistMatchKind::All,
        rules: vec![SmartPlaylistRule::Text {
            field: SmartPlaylistTextField::Artist,
            operator: SmartPlaylistTextOperator::Contains,
            value: "Portishead".to_owned(),
        }],
        limit: None,
    }
}

fn positive_id<T>(id: Option<T>) -> T {
    match id {
        Some(id) => id,
        None => unreachable!("test helper only constructs positive ids"),
    }
}

fn _assert_store_result_is_public<T>(result: StoreResult<T>) -> StoreResult<T> {
    result
}

fn sample_layout() -> TrackColumnLayout {
    TrackColumnLayout::new(vec![
        TrackColumnEntry {
            column_id: "track_name".to_owned(),
            visible: true,
            width_px: 240,
        },
        TrackColumnEntry {
            column_id: "artist".to_owned(),
            visible: false,
            width_px: 160,
        },
        TrackColumnEntry {
            column_id: "rating".to_owned(),
            visible: true,
            width_px: 100,
        },
    ])
}

#[test]
fn sqlite_store_layout_round_trips_for_each_scope() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let playlist = playlist(1, "Favorites", Vec::new());
    assert_eq!(store.save_playlist(playlist.clone()), Ok(()));
    let smart = smart_playlist_with_rules(7, "Top Rated", None, 0, simple_text_rule_set());
    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));

    let layout = sample_layout();
    for scope in [
        TrackColumnLayoutScope::Default,
        TrackColumnLayoutScope::Playlist(playlist.id),
        TrackColumnLayoutScope::SmartPlaylist(smart.id),
    ] {
        assert_eq!(store.load_track_column_layout(scope), Ok(None));
        assert_eq!(store.save_track_column_layout(scope, &layout), Ok(()));
        assert_eq!(
            store.load_track_column_layout(scope),
            Ok(Some(layout.clone()))
        );
        assert_eq!(store.delete_track_column_layout(scope), Ok(()));
        assert_eq!(store.load_track_column_layout(scope), Ok(None));
    }
}

#[test]
fn sqlite_store_layout_round_trips_sort_for_each_scope() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let playlist = playlist(1, "Favorites", Vec::new());
    assert_eq!(store.save_playlist(playlist.clone()), Ok(()));
    let smart = smart_playlist_with_rules(7, "Top Rated", None, 0, simple_text_rule_set());
    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));

    let mut layout = sample_layout();
    layout.sort = Some(TrackColumnSort {
        column_id: "date_added".to_owned(),
        ascending: false,
    });

    for scope in [
        TrackColumnLayoutScope::Default,
        TrackColumnLayoutScope::Playlist(playlist.id),
        TrackColumnLayoutScope::SmartPlaylist(smart.id),
    ] {
        assert_eq!(store.save_track_column_layout(scope, &layout), Ok(()));
        assert_eq!(
            store.load_track_column_layout(scope),
            Ok(Some(layout.clone()))
        );

        // Re-saving without a sort must durably clear the stored sort row,
        // not leave the previous one behind.
        let cleared = sample_layout();
        assert_eq!(store.save_track_column_layout(scope, &cleared), Ok(()));
        assert_eq!(
            store.load_track_column_layout(scope),
            Ok(Some(cleared.clone()))
        );
    }
}

#[test]
fn sqlite_store_layout_save_replaces_existing_rows() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let scope = TrackColumnLayoutScope::Default;
    let initial = sample_layout();
    assert_eq!(store.save_track_column_layout(scope, &initial), Ok(()));

    let replacement = TrackColumnLayout::new(vec![TrackColumnEntry {
        column_id: "album".to_owned(),
        visible: true,
        width_px: 200,
    }]);
    assert_eq!(store.save_track_column_layout(scope, &replacement), Ok(()));

    assert_eq!(store.load_track_column_layout(scope), Ok(Some(replacement)));
}

#[test]
fn sqlite_store_playlist_layout_cascades_on_playlist_delete() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let playlist = playlist(1, "Favorites", Vec::new());
    assert_eq!(store.save_playlist(playlist.clone()), Ok(()));
    let scope = TrackColumnLayoutScope::Playlist(playlist.id);
    assert_eq!(
        store.save_track_column_layout(scope, &sample_layout()),
        Ok(())
    );

    assert_eq!(store.delete_playlist(playlist.id), Ok(()));

    assert_eq!(store.load_track_column_layout(scope), Ok(None));
}

#[test]
fn sqlite_store_smart_playlist_layout_cascades_on_smart_playlist_delete() {
    let store = SqliteLibraryStore::open_in_memory().expect("open in-memory sqlite store");
    let smart = smart_playlist_with_rules(3, "Recent", None, 0, simple_text_rule_set());
    assert_eq!(store.save_smart_playlist(smart.clone()), Ok(()));
    let scope = TrackColumnLayoutScope::SmartPlaylist(smart.id);
    assert_eq!(
        store.save_track_column_layout(scope, &sample_layout()),
        Ok(())
    );

    assert_eq!(store.delete_smart_playlist(smart.id), Ok(()));

    assert_eq!(store.load_track_column_layout(scope), Ok(None));
}

#[test]
fn duplicate_consolidation_commits_survivor_playlist_and_cache_changes() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut survivor = track(1, "keep.flac");
    survivor.metadata.title = Some("Old".to_owned());
    let removed = track(2, "remove.flac");
    store.save_track(survivor.clone()).expect("save survivor");
    store.save_track(removed.clone()).expect("save removed");
    let original_playlist = playlist(
        1,
        "Mix",
        vec![entry(1, 2, 0), entry(1, 1, 1), entry(1, 2, 2)],
    );
    store
        .save_playlist(original_playlist)
        .expect("save playlist");
    store
        .update_track_rating_and_enqueue_mirror(survivor.id, Rating::new(3).expect("rating"))
        .expect("enqueue survivor mirror");
    store
        .update_track_rating_and_enqueue_mirror(removed.id, Rating::new(4).expect("rating"))
        .expect("enqueue removed mirror");
    for track_id in [survivor.id, removed.id] {
        store
            .save_source_fingerprint(
                track_id,
                &SourceFingerprint {
                    stat: SourceFileStat {
                        device: 1,
                        inode: track_id.get() as u64,
                        size_bytes: 9,
                        modified_at_ns: 3,
                        changed_at_ns: 4,
                    },
                    content_hash: TrackContentHash::new("a".repeat(64)).expect("hash"),
                },
            )
            .expect("save source fingerprint");
    }

    survivor.metadata.title = Some("Chosen".to_owned());
    survivor.rating = Rating::new(5).expect("rating");
    survivor.statistics.play_count = 12;
    survivor.file_size_bytes = Some(42);
    survivor.has_embedded_artwork = Some(true);
    let rewritten_playlist = playlist(1, "Mix", vec![entry(1, 1, 0)]);
    store
        .commit_duplicate_consolidation(&DuplicateConsolidationPlan {
            survivor: survivor.clone(),
            removed_track_ids: vec![removed.id],
            rewritten_playlists: vec![rewritten_playlist.clone()],
        })
        .expect("commit consolidation");

    assert_eq!(store.track(survivor.id), Ok(Some(survivor)));
    assert_eq!(store.track(removed.id), Ok(None));
    assert_eq!(
        store.playlist(rewritten_playlist.id),
        Ok(Some(rewritten_playlist))
    );
    assert!(store.tag_mirrors_due(0, 10).expect("outbox").is_empty());
    assert_eq!(store.source_fingerprint(track_id(1)), Ok(None));
    assert_eq!(store.source_fingerprint(track_id(2)), Ok(None));
}

#[test]
fn a_source_fingerprint_for_a_track_no_longer_in_the_library_is_dropped() {
    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");

    assert_eq!(
        store.save_source_fingerprint(
            track_id(7),
            &SourceFingerprint {
                stat: SourceFileStat {
                    device: 1,
                    inode: 2,
                    size_bytes: 9,
                    modified_at_ns: 3,
                    changed_at_ns: 4,
                },
                content_hash: TrackContentHash::new("a".repeat(64)).expect("hash"),
            },
        ),
        Ok(())
    );
    assert_eq!(store.source_fingerprint(track_id(7)), Ok(None));
}

#[test]
fn background_results_for_a_track_no_longer_in_the_library_are_dropped() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let gone = track_id(7);

    assert_eq!(
        store.record_analysis(
            gone,
            &sample_analysis(Some(126.0), Some(MusicalKey::DMinor)),
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        ),
        Ok(())
    );
    assert_eq!(
        store.record_analysis_attempt_failure(
            gone,
            AnalysisCapabilities::all(),
            ctx(1_700_000_000)
        ),
        Ok(())
    );
    assert_eq!(
        store.record_online_attempt(gone, OnlineCapabilities::all(), online_ctx(1_700_000_000)),
        Ok(())
    );
    assert_eq!(
        store.record_synced_lyrics(gone, &sample_synced_lyrics(), "lrclib"),
        Ok(())
    );
    assert_eq!(
        store.enqueue_tag_mirror_artwork(gone, TagMirrorArtwork::Clear),
        Ok(())
    );

    assert_eq!(store.load_waveform(gone), Ok(None));
    assert_eq!(store.load_beat_grid(gone), Ok(None));
    assert_eq!(store.load_all_acoustics(), Ok(Vec::new()));
    assert_eq!(store.load_synced_lyrics(gone), Ok(None));
    assert_eq!(store.tag_mirrors_due(i64::MAX, 10), Ok(Vec::new()));
}

#[test]
fn managed_retarget_commits_metadata_location_and_outbox_together() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "loose.flac");
    store.save_track(track.clone()).expect("save track");
    store
        .update_track_rating_and_enqueue_mirror(track.id, Rating::new(4).expect("rating"))
        .expect("enqueue pre-existing rating mirror");

    let change = MetadataChange {
        title: FieldChange::Set("Canonical title".to_owned()),
        ..MetadataChange::default()
    };
    let location = TrackLocation::available(relative_path("Artist/Album/01 Canonical title.flac"));
    store
        .apply_track_metadata_change_and_location_and_enqueue_mirror(track.id, &change, &location)
        .expect("managed retarget commit");

    let stored = store
        .track(track.id)
        .expect("load track")
        .expect("track exists");
    assert_eq!(stored.location, location);
    assert_eq!(stored.metadata.title.as_deref(), Some("Canonical title"));
    let pending = store.tag_mirrors_due(0, 10).expect("load due rows");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].track_id, track.id);
    assert_eq!(pending[0].generation, 2);
    assert!(pending[0].kinds.metadata);
    assert!(pending[0].kinds.rating);
}

#[test]
fn relocation_resets_source_observations_and_queues_canonical_mirrors() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut track = track(1, "missing.flac");
    track.location = TrackLocation::missing(relative_path("missing.flac"));
    track.file_size_bytes = Some(9);
    track.has_embedded_artwork = Some(true);
    track.file_modified_at = Some(SystemTime::UNIX_EPOCH);
    store.save_track(track.clone()).expect("save track");
    store
        .save_source_fingerprint(
            track.id,
            &SourceFingerprint {
                stat: SourceFileStat {
                    device: 1,
                    inode: 2,
                    size_bytes: 9,
                    modified_at_ns: 3,
                    changed_at_ns: 4,
                },
                content_hash: TrackContentHash::new("a".repeat(64)).expect("hash"),
            },
        )
        .expect("save source fingerprint");
    let location = TrackLocation::available(relative_path("replacement.flac"));

    store
        .relocate_track_and_enqueue_mirror(track.id, &location, 27)
        .expect("relocate");

    let stored = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(stored.location, location);
    assert_eq!(stored.file_size_bytes, Some(27));
    assert_eq!(stored.has_embedded_artwork, None);
    assert_eq!(stored.file_modified_at, None);
    assert_eq!(store.source_fingerprint(track.id), Ok(None));
    let mirrors = store.tag_mirrors_due(0, 10).expect("load mirrors");
    assert_eq!(mirrors.len(), 1);
    assert!(mirrors[0].kinds.metadata);
    assert!(mirrors[0].kinds.rating);
}

#[test]
fn audio_replacement_preserves_library_fields_and_invalidates_derived_state() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let mut track = track(1, "old.mp3");
    track.metadata.title = Some("Precious title".to_owned());
    track.metadata.duration = Some(Duration::from_secs(180));
    track.metadata.bitrate_kbps = Some(96);
    track.rating = Rating::new(5).expect("rating");
    track.statistics.play_count = 42;
    track.file_modified_at = Some(SystemTime::UNIX_EPOCH);
    store.save_track(track.clone()).expect("save track");

    let mut analysis = sample_analysis(Some(120.0), Some(MusicalKey::AMinor));
    analysis.acoustics = Some(AcousticFeatures {
        integrated_lufs: -14.0,
        short_term_lufs_max: -10.0,
        loudness_range_lu: 5.0,
        onset_rate_hz: 2.0,
        low_band_ratio: 0.4,
        mid_band_ratio: 0.4,
        high_band_ratio: 0.2,
        low_band_variation: 0.5,
        tonalness: 0.8,
    });
    store
        .record_analysis(
            track.id,
            &analysis,
            AnalysisCapabilities::all(),
            ctx(1_700_000_000),
        )
        .expect("record analysis");
    store
        .save_source_fingerprint(
            track.id,
            &SourceFingerprint {
                stat: SourceFileStat {
                    device: 1,
                    inode: 2,
                    size_bytes: 9,
                    modified_at_ns: 3,
                    changed_at_ns: 4,
                },
                content_hash: TrackContentHash::new("a".repeat(64)).expect("hash"),
            },
        )
        .expect("save source fingerprint");
    store
        .save_smart_shuffle_index(&StoredSmartShuffleIndex {
            index_blob: vec![1, 2, 3],
            schema_version: 1,
        })
        .expect("save shuffle index");

    let replacement = TrackLocation::available(relative_path("old (YouTube).opus"));
    let properties = TrackAudioProperties {
        duration: Some(Duration::from_secs(181)),
        bitrate_kbps: Some(128),
        sample_rate_hz: Some(48_000),
        bit_depth: Some(16),
        channels: Some(2),
    };
    store
        .replace_track_audio(track.id, &replacement, properties, 27, true)
        .expect("replace audio");

    let stored = store.track(track.id).expect("load").expect("track exists");
    assert_eq!(stored.location, replacement);
    assert_eq!(stored.metadata.title.as_deref(), Some("Precious title"));
    assert_eq!(stored.metadata.audio_properties(), properties);
    assert_eq!(stored.rating, track.rating);
    assert_eq!(stored.statistics, track.statistics);
    assert_eq!(stored.file_size_bytes, Some(27));
    assert_eq!(stored.has_embedded_artwork, Some(true));
    assert_eq!(stored.file_modified_at, None);
    assert_eq!(store.load_waveform(track.id), Ok(None));
    assert_eq!(store.load_beat_grid(track.id), Ok(None));
    assert!(
        store
            .load_all_acoustics()
            .expect("load acoustics")
            .is_empty()
    );
    assert_eq!(store.source_fingerprint(track.id), Ok(None));
    assert_eq!(store.load_smart_shuffle_index(), Ok(None));
    assert_eq!(
        store
            .tracks_needing_analysis(AnalysisCapabilities::all(), 1, 10)
            .expect("tracks needing analysis"),
        vec![track.id]
    );
}

#[test]
fn track_delete_cascades_tag_mirror_outbox() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");
    store
        .update_track_rating_and_enqueue_mirror(track.id, Rating::new(5).expect("rating"))
        .expect("enqueue rating mirror");
    assert_eq!(store.tag_mirrors_due(0, 10).expect("pending").len(), 1);
    store.delete_track(track.id).expect("delete track");
    assert!(store.tag_mirrors_due(0, 10).expect("cascade").is_empty());
}

#[test]
fn tag_mirror_outbox_coalesces_latest_canonical_projection() {
    let store = &SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
    let track = track(1, "a.flac");
    store.save_track(track.clone()).expect("save track");
    store
        .update_track_rating_and_enqueue_mirror(track.id, Rating::new(4).expect("rating"))
        .expect("rating and outbox commit");
    let change = MetadataChange {
        title: FieldChange::Set("Canonical title".to_owned()),
        ..MetadataChange::default()
    };
    store
        .apply_track_metadata_change_and_enqueue_mirror(track.id, &change)
        .expect("metadata and outbox commit");

    let pending = store.tag_mirrors_due(0, 10).expect("load due rows");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].track_id, track.id);
    assert_eq!(pending[0].generation, 2);
    assert!(pending[0].kinds.metadata);
    assert!(pending[0].kinds.rating);
    assert!(!pending[0].kinds.artwork);
    assert_eq!(
        store
            .track(track.id)
            .expect("load canonical track")
            .expect("track exists")
            .metadata
            .title
            .as_deref(),
        Some("Canonical title")
    );

    assert!(
        !store
            .complete_tag_mirror(track.id, 1)
            .expect("stale completion rejected")
    );
    assert!(
        store
            .record_tag_mirror_failure(track.id, 2, 60, "injected failure")
            .expect("record failure")
    );
    assert!(store.tag_mirrors_due(59, 10).expect("not due").is_empty());

    store
        .apply_track_metadata_change_and_enqueue_mirror(
            track.id,
            &MetadataChange {
                artist: FieldChange::Set("Newer artist".to_owned()),
                ..MetadataChange::default()
            },
        )
        .expect("newer generation resets retry state");
    let pending = store.tag_mirrors_due(0, 10).expect("new generation due");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].generation, 3);
    assert_eq!(pending[0].attempt_count, 0);
    assert_eq!(pending[0].last_error, None);
    assert!(
        store
            .complete_tag_mirror(track.id, 3)
            .expect("complete latest generation")
    );
    assert!(
        store
            .tag_mirrors_due(i64::MAX, 10)
            .expect("empty")
            .is_empty()
    );
}

#[test]
fn sqlite_tag_mirror_artwork_survives_reopen_and_is_collected_after_completion() {
    let dir = unique_store_test_directory("tag_outbox_artwork");
    std::fs::create_dir_all(&dir).expect("create test directory");
    let database = dir.join("library.sqlite");
    let track = track(1, "a.flac");
    let bytes = valid_test_artwork();

    let artwork = {
        let store = SqliteLibraryStore::open(&database, dir.join("cache")).expect("open store");
        store.save_track(track.clone()).expect("save track");
        let artwork = store
            .publish_tag_mirror_artwork(&bytes)
            .expect("publish artwork blob");
        store
            .enqueue_tag_mirror_artwork(track.id, TagMirrorArtwork::Set(artwork.clone()))
            .expect("enqueue artwork mirror");
        artwork
    };

    let blob_path = dir
        .join("tag-outbox")
        .join("blobs")
        .join(format!("sha256-{}", artwork.digest()));
    assert!(blob_path.is_file());

    let store = SqliteLibraryStore::open(&database, dir.join("cache")).expect("reopen store");
    let pending = store.tag_mirrors_due(0, 10).expect("reload outbox");
    assert_eq!(pending.len(), 1);
    assert_eq!(pending[0].artwork, TagMirrorArtwork::Set(artwork.clone()));
    assert_eq!(
        store
            .read_tag_mirror_artwork(&artwork)
            .expect("reload artwork bytes"),
        bytes
    );
    assert!(
        store
            .complete_tag_mirror(track.id, pending[0].generation)
            .expect("complete artwork mirror")
    );
    store
        .garbage_collect_tag_mirror_artwork()
        .expect("collect unreferenced blob");
    assert!(!blob_path.exists());
    drop(store);
    std::fs::remove_dir_all(dir).expect("remove test directory");
}

fn unique_store_test_directory(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "sustain_{label}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos()
    ))
}

fn valid_test_artwork() -> Vec<u8> {
    vec![
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ]
}
