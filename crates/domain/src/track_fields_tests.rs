// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::Duration;

use super::TrackFields;
use crate::{
    Bpm, PlayStatistics, Rating, Track, TrackAvailability, TrackId, TrackLocation, TrackMetadata,
    TrackRelativePath,
};

fn track() -> Track {
    Track {
        id: TrackId::new(1).expect("positive track id"),
        location: TrackLocation::available(TrackRelativePath::new("a.flac").expect("path")),
        metadata: TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Artist".to_owned()),
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

#[test]
fn identical_versions_differ_in_nothing() {
    assert!(TrackFields::between(&track(), &track()).is_empty());
}

#[test]
fn each_changed_field_is_named() {
    let old = track();
    let mut new = track();
    new.metadata.artist = Some("Other".to_owned());
    new.metadata.year = Some(1999);
    new.rating = Rating::new(4).expect("valid rating");

    assert_eq!(
        TrackFields::between(&old, &new),
        TrackFields::ARTIST | TrackFields::YEAR | TrackFields::RATING
    );
}

#[test]
fn companion_fields_report_their_shared_member() {
    let old = track();
    let mut new = track();
    new.metadata.track_total = Some(12);
    new.metadata.bpm_set_by_user = true;
    new.metadata.album_artist_sort = Some("Band, The".to_owned());
    new.metadata.channels = Some(2);
    new.file_modified_at = Some(std::time::SystemTime::UNIX_EPOCH);

    assert_eq!(
        TrackFields::between(&old, &new),
        TrackFields::TRACK_NUMBER
            | TrackFields::BPM
            | TrackFields::SORT_NAMES
            | TrackFields::AUDIO_FORMAT
            | TrackFields::FILE
    );
}

#[test]
fn file_state_is_told_apart_from_metadata() {
    let old = track();
    let mut new = track();
    new.location = new.location.with_availability(TrackAvailability::Missing);
    new.has_embedded_artwork = Some(true);
    new.statistics.play_count = 3;

    assert_eq!(
        TrackFields::between(&old, &new),
        TrackFields::LOCATION | TrackFields::EMBEDDED_ARTWORK | TrackFields::STATISTICS
    );
}

#[test]
fn analysis_values_are_fields_like_any_other() {
    let old = track();
    let mut new = track();
    new.metadata.bpm = Bpm::from_whole(120);
    new.metadata.duration = Some(Duration::from_secs(201));

    let fields = TrackFields::between(&old, &new);
    assert!(fields.contains(TrackFields::BPM | TrackFields::DURATION));
    assert!(!fields.intersects(TrackFields::KEY | TrackFields::TITLE));
}

#[test]
fn set_operations_agree() {
    let grouping = TrackFields::ARTIST | TrackFields::ALBUM;

    assert!(grouping.contains(TrackFields::ARTIST));
    assert!(!grouping.contains(TrackFields::ARTIST | TrackFields::RATING));
    assert!(grouping.intersects(TrackFields::ALBUM | TrackFields::RATING));
    assert!(!grouping.intersects(TrackFields::RATING));
    assert!(TrackFields::ALL.contains(grouping));
    assert!(grouping.contains(TrackFields::NONE));
}
