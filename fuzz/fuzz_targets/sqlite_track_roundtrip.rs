// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![no_main]

use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf, time::Duration};

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sustain_domain::{
    PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
};
use sustain_library_store::{LibraryStore, SqliteLibraryStore};

const PATH_COMPONENT_BYTE_LIMIT: usize = 255;
const MAX_DECORATED_TEXT_BYTES: usize = 65_536;
const PATH_COMPONENT_LENGTHS: &[usize] = &[1, 2, 3, 119, 120, 121, 254, 255];
const HOSTILE_DECORATIONS: &[&str] = &[
    "",
    " \t\r\n ",
    "\0",
    "line one\nline two\r\n",
    "e\u{301}\u{327}",
    "\u{200f}\u{202e}עברית",
    "東京🧪",
    "/../\\:*?\"<>|",
];

#[derive(Arbitrary, Debug)]
struct SqliteFuzzInput {
    directory_bytes: Vec<u8>,
    file_name_bytes: Vec<u8>,
    directory_length_selector: u8,
    file_name_length_selector: u8,
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    composer: Option<String>,
    grouping: Option<String>,
    genre: Option<String>,
    key: Option<String>,
    comments: Option<String>,
    lyrics: Option<String>,
    title_sort: Option<String>,
    artist_sort: Option<String>,
    album_sort: Option<String>,
    album_artist_sort: Option<String>,
    composer_sort: Option<String>,
    track_number: Option<u32>,
    track_total: Option<u32>,
    disc_number: Option<u32>,
    disc_total: Option<u32>,
    year: Option<i32>,
    compilation: Option<bool>,
    bpm: Option<u32>,
    duration_seconds: Option<u32>,
    bitrate_kbps: Option<u32>,
    sample_rate_hz: Option<u32>,
    channels: Option<u8>,
    rating: u8,
    play_count: u64,
    skip_count: u64,
    file_size_bytes: Option<u64>,
    has_embedded_artwork: Option<bool>,
    missing: bool,
    decoration_seed: u8,
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(input) = SqliteFuzzInput::arbitrary(&mut unstructured) else {
        return;
    };
    exercise_sqlite_roundtrip(input);
});

fn exercise_sqlite_roundtrip(input: SqliteFuzzInput) {
    let relative_path = relative_path(
        input.directory_bytes,
        input.file_name_bytes,
        input.directory_length_selector,
        input.file_name_length_selector,
    );
    let mut metadata = TrackMetadata {
        title: decorated(input.title, input.decoration_seed),
        artist: decorated(input.artist, input.decoration_seed.wrapping_add(1)),
        album: decorated(input.album, input.decoration_seed.wrapping_add(2)),
        album_artist: decorated(input.album_artist, input.decoration_seed.wrapping_add(3)),
        composer: decorated(input.composer, input.decoration_seed.wrapping_add(4)),
        grouping: decorated(input.grouping, input.decoration_seed.wrapping_add(5)),
        genre: decorated(input.genre, input.decoration_seed.wrapping_add(6)),
        key: decorated(input.key, input.decoration_seed.wrapping_add(7)),
        comments: decorated(input.comments, input.decoration_seed.wrapping_add(8)),
        lyrics: decorated(input.lyrics, input.decoration_seed.wrapping_add(9)),
        title_sort: decorated(input.title_sort, input.decoration_seed.wrapping_add(10)),
        artist_sort: decorated(input.artist_sort, input.decoration_seed.wrapping_add(11)),
        album_sort: decorated(input.album_sort, input.decoration_seed.wrapping_add(12)),
        album_artist_sort: decorated(
            input.album_artist_sort,
            input.decoration_seed.wrapping_add(13),
        ),
        composer_sort: decorated(input.composer_sort, input.decoration_seed.wrapping_add(14)),
        track_number: input.track_number,
        track_total: input.track_total,
        disc_number: input.disc_number,
        disc_total: input.disc_total,
        year: input.year,
        compilation: input.compilation,
        bpm: input.bpm,
        duration: input
            .duration_seconds
            .map(|seconds| Duration::from_secs(u64::from(seconds))),
        bitrate_kbps: input.bitrate_kbps,
        sample_rate_hz: input.sample_rate_hz,
        channels: input.channels,
    };
    let original_metadata = metadata.clone();
    metadata.normalize_text_fields();
    assert_normalization_contract(&original_metadata, &metadata);
    let mut normalized_again = metadata.clone();
    normalized_again.normalize_text_fields();
    assert_eq!(
        metadata, normalized_again,
        "normalization must be idempotent"
    );

    let location = if input.missing {
        TrackLocation::missing(relative_path)
    } else {
        TrackLocation::available(relative_path)
    };
    let input_track = Track {
        id: TrackId::new(1).expect("fixed positive track id"),
        location,
        metadata: original_metadata,
        rating: Rating::new(input.rating % 6).expect("rating reduced to the valid range"),
        statistics: PlayStatistics {
            play_count: input.play_count,
            skip_count: input.skip_count,
            ..PlayStatistics::default()
        },
        file_size_bytes: input.file_size_bytes,
        has_embedded_artwork: input.has_embedded_artwork,
        file_modified_at: None,
    };
    let expected_track = Track {
        metadata,
        ..input_track.clone()
    };

    let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite must open");
    store
        .save_track(input_track)
        .expect("valid generated track must persist");
    let loaded = store
        .track(expected_track.id)
        .expect("persisted track query must succeed")
        .expect("persisted track must exist");
    assert_eq!(
        loaded, expected_track,
        "SQLite must round-trip the canonical track"
    );
    assert_eq!(
        store.tracks().expect("all-tracks query must succeed"),
        vec![expected_track]
    );
}

fn decorated(value: Option<String>, selector: u8) -> Option<String> {
    value.map(|mut value| {
        value.push_str(HOSTILE_DECORATIONS[usize::from(selector) % HOSTILE_DECORATIONS.len()]);
        amplify_text(&mut value, selector);
        value
    })
}

fn amplify_text(value: &mut String, selector: u8) {
    if value.is_empty() {
        return;
    }
    let requested_copies = match selector >> 5 {
        0..=4 => 1,
        5 => 8,
        6 => 64,
        _ => 512,
    };
    let copies = requested_copies.min((MAX_DECORATED_TEXT_BYTES / value.len()).max(1));
    if copies == 1 {
        return;
    }
    let unit = value.clone();
    value.reserve(unit.len() * (copies - 1));
    for _ in 1..copies {
        value.push_str(&unit);
    }
}

fn relative_path(
    directory_bytes: Vec<u8>,
    file_name_bytes: Vec<u8>,
    directory_length_selector: u8,
    file_name_length_selector: u8,
) -> TrackRelativePath {
    let directory = path_component(directory_bytes, b"artist", directory_length_selector);
    let file_name = path_component(file_name_bytes, b"track.flac", file_name_length_selector);
    TrackRelativePath::new(PathBuf::from(directory).join(file_name))
        .expect("sanitized byte components form a relative path")
}

fn path_component(bytes: Vec<u8>, fallback: &[u8], length_selector: u8) -> OsString {
    let mut bytes = bytes
        .into_iter()
        .take(PATH_COMPONENT_BYTE_LIMIT)
        .map(|byte| match byte {
            0 | b'/' => b'_',
            other => other,
        })
        .collect::<Vec<_>>();
    if bytes.is_empty() || bytes == b"." || bytes == b".." {
        bytes.clear();
        bytes.extend_from_slice(fallback);
    }
    let target_length =
        PATH_COMPONENT_LENGTHS[usize::from(length_selector) % PATH_COMPONENT_LENGTHS.len()];
    let unit = bytes.clone();
    while bytes.len() < target_length {
        let remaining = target_length - bytes.len();
        bytes.extend_from_slice(&unit[..unit.len().min(remaining)]);
    }
    bytes.truncate(target_length);
    if bytes == b"." || bytes == b".." {
        bytes[0] = b'x';
    }
    OsString::from_vec(bytes)
}

fn assert_normalization_contract(original: &TrackMetadata, normalized: &TrackMetadata) {
    assert_text_normalized(&original.title, &normalized.title);
    assert_text_normalized(&original.artist, &normalized.artist);
    assert_text_normalized(&original.album, &normalized.album);
    assert_text_normalized(&original.album_artist, &normalized.album_artist);
    assert_text_normalized(&original.composer, &normalized.composer);
    assert_text_normalized(&original.grouping, &normalized.grouping);
    assert_text_normalized(&original.genre, &normalized.genre);
    assert_text_normalized(&original.key, &normalized.key);
    assert_text_normalized(&original.comments, &normalized.comments);
    assert_text_normalized(&original.lyrics, &normalized.lyrics);
    assert_text_normalized(&original.title_sort, &normalized.title_sort);
    assert_text_normalized(&original.artist_sort, &normalized.artist_sort);
    assert_text_normalized(&original.album_sort, &normalized.album_sort);
    assert_text_normalized(&original.album_artist_sort, &normalized.album_artist_sort);
    assert_text_normalized(&original.composer_sort, &normalized.composer_sort);
}

fn assert_text_normalized(original: &Option<String>, normalized: &Option<String>) {
    if original
        .as_deref()
        .is_none_or(|value| value.trim().is_empty())
    {
        assert_eq!(normalized, &None);
    } else {
        assert_eq!(normalized, original);
    }
}
