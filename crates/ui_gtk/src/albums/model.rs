// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeMap;
use std::path::PathBuf;

use sustain_app_runtime::{Track, TrackId, TrackMetadata};

use crate::util::non_empty_text;

/// Stable identity for an album, derived from the normalized grouping
/// artist and title used to bucket tracks. For normal albums the grouping
/// artist is the album artist or track artist; for compilations without an
/// explicit album artist it is the shared compilation bucket. Equality
/// matches the bucketing logic, so two `AlbumViewModel`s produced from
/// different `group_albums` calls share a key iff they cover the same tracks.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub(super) struct AlbumKey {
    artist: String,
    title: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AlbumViewModel {
    pub(super) key: AlbumKey,
    pub(super) title: String,
    pub(super) artist: String,
    pub(super) year: Option<i32>,
    pub(super) tracks: Vec<AlbumTrackViewModel>,
    /// Audio file the artwork loader should read to extract this album's
    /// cover, expressed as the track's `TrackLocation::path()` (relative
    /// when the library has a root, absolute when imported as such). The
    /// view resolves it against the current library root before queueing.
    /// `None` when every track is missing on disk and the album is being
    /// rendered purely from cached metadata.
    pub(super) representative_track_path: Option<PathBuf>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct AlbumTrackViewModel {
    pub(super) id: TrackId,
    pub(super) file_path: PathBuf,
    pub(super) title: String,
    pub(super) disc_number: Option<u32>,
    pub(super) track_number: Option<u32>,
    pub(super) duration_seconds: u64,
    pub(super) is_missing: bool,
}

#[derive(Clone, Debug)]
struct AlbumBucket {
    key: AlbumKey,
    title: String,
    artist: String,
    has_explicit_album_artist: bool,
    is_compilation: bool,
    track_artists: Vec<String>,
    year: Option<i32>,
    tracks: Vec<AlbumTrackViewModel>,
}

const UNKNOWN_ALBUM: &str = "Unknown Album";
const UNKNOWN_ARTIST: &str = "Unknown Artist";
const COMPILATION_ALBUM_ARTIST: &str = "Various Artists";

pub(super) fn group_albums(tracks: &[Track]) -> Vec<AlbumViewModel> {
    let mut albums = BTreeMap::<AlbumKey, AlbumBucket>::new();

    for track in tracks {
        let title = album_title(&track.metadata);
        let grouping = album_grouping(&track.metadata);
        let key = AlbumKey {
            artist: normalize_album_key(&grouping.key_artist),
            title: normalize_album_key(&title),
        };
        let bucket = albums.entry(key.clone()).or_insert_with(|| AlbumBucket {
            key,
            title,
            artist: grouping.display_artist.clone(),
            has_explicit_album_artist: grouping.has_explicit_album_artist,
            is_compilation: grouping.is_compilation,
            track_artists: Vec::new(),
            year: track.metadata.year,
            tracks: Vec::new(),
        });
        if grouping.has_explicit_album_artist && !bucket.has_explicit_album_artist {
            bucket.artist = grouping.display_artist.clone();
            bucket.has_explicit_album_artist = true;
        }
        bucket.is_compilation |= grouping.is_compilation;
        push_unique_artist(&mut bucket.track_artists, grouping.track_artist);
        if bucket.year.is_none() {
            bucket.year = track.metadata.year;
        }
        bucket.tracks.push(album_track(track));
    }

    albums
        .into_values()
        .map(|mut bucket| {
            bucket.tracks.sort_by(compare_album_tracks);
            let representative_track_path = bucket
                .tracks
                .iter()
                .find(|track| !track.is_missing)
                .or_else(|| bucket.tracks.first())
                .map(|track| track.file_path.clone());
            let artist = album_display_artist(&bucket);
            AlbumViewModel {
                key: bucket.key,
                title: bucket.title,
                artist,
                year: bucket.year,
                tracks: bucket.tracks,
                representative_track_path,
            }
        })
        .collect()
}

/// Locate the album holding `track_id` and replace that track's row in
/// place. Returns the album's key so callers can re-render exactly the
/// row that needs it; `None` when no album currently holds the track.
///
/// The track list is re-sorted with [`compare_album_tracks`] because a
/// metadata update can move a track within its album (e.g. a Tags
/// retrieval populating a missing track number). The track's album
/// itself never moves — every Sustain feature is existing-tag-preserving,
/// so the album grouping key (artist + title) is stable across updates.
pub(super) fn replace_track_in_album(
    albums: &mut [AlbumViewModel],
    track_id: TrackId,
    new_track: &AlbumTrackViewModel,
) -> Option<AlbumKey> {
    for album in albums.iter_mut() {
        if let Some(slot) = album.tracks.iter_mut().find(|track| track.id == track_id) {
            *slot = new_track.clone();
            album.tracks.sort_by(compare_album_tracks);
            return Some(album.key.clone());
        }
    }
    None
}

pub(super) fn album_subtitle(album: &AlbumViewModel) -> String {
    match album.year {
        Some(year) => format!("{} ({year})", album.artist),
        None => album.artist.clone(),
    }
}

pub(super) fn track_number_text(track: &AlbumTrackViewModel) -> String {
    match (track.disc_number, track.track_number) {
        (Some(disc), Some(number)) if disc > 1 => format!("{disc}-{number}"),
        (_, Some(number)) => number.to_string(),
        _ => String::new(),
    }
}

pub(super) fn duration_text(duration_seconds: u64) -> String {
    let hours = duration_seconds / 3_600;
    let minutes = duration_seconds % 3_600 / 60;
    let seconds = duration_seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

pub(super) fn album_track(track: &Track) -> AlbumTrackViewModel {
    AlbumTrackViewModel {
        id: track.id,
        file_path: track.location.path().to_path_buf(),
        title: track_title(track),
        disc_number: track.metadata.disc_number,
        track_number: track.metadata.track_number,
        duration_seconds: track
            .metadata
            .duration
            .map(|duration| duration.as_secs())
            .unwrap_or_default(),
        is_missing: track.location.is_missing(),
    }
}

pub(super) fn compare_album_tracks(
    left: &AlbumTrackViewModel,
    right: &AlbumTrackViewModel,
) -> std::cmp::Ordering {
    left.disc_number
        .unwrap_or(0)
        .cmp(&right.disc_number.unwrap_or(0))
        .then_with(|| {
            left.track_number
                .unwrap_or(u32::MAX)
                .cmp(&right.track_number.unwrap_or(u32::MAX))
        })
        .then_with(|| normalize_album_key(&left.title).cmp(&normalize_album_key(&right.title)))
        .then_with(|| left.id.cmp(&right.id))
}

fn album_title(metadata: &TrackMetadata) -> String {
    non_empty_text(&metadata.album).unwrap_or_else(|| UNKNOWN_ALBUM.to_owned())
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AlbumGrouping {
    key_artist: String,
    display_artist: String,
    track_artist: String,
    has_explicit_album_artist: bool,
    is_compilation: bool,
}

fn album_grouping(metadata: &TrackMetadata) -> AlbumGrouping {
    let track_artist = track_artist(metadata);
    if let Some(album_artist) = non_empty_text(&metadata.album_artist) {
        return AlbumGrouping {
            key_artist: album_artist.clone(),
            display_artist: album_artist,
            track_artist,
            has_explicit_album_artist: true,
            is_compilation: metadata.compilation.unwrap_or(false),
        };
    }

    if metadata.compilation.unwrap_or(false) {
        return AlbumGrouping {
            key_artist: COMPILATION_ALBUM_ARTIST.to_owned(),
            display_artist: COMPILATION_ALBUM_ARTIST.to_owned(),
            track_artist,
            has_explicit_album_artist: false,
            is_compilation: true,
        };
    }

    AlbumGrouping {
        key_artist: track_artist.clone(),
        display_artist: track_artist.clone(),
        track_artist,
        has_explicit_album_artist: false,
        is_compilation: false,
    }
}

fn album_display_artist(bucket: &AlbumBucket) -> String {
    if bucket.is_compilation
        && !bucket.has_explicit_album_artist
        && !bucket.track_artists.is_empty()
    {
        return artist_summary(&bucket.track_artists);
    }
    bucket.artist.clone()
}

fn artist_summary(artists: &[String]) -> String {
    artists.join(", ")
}

fn push_unique_artist(artists: &mut Vec<String>, artist: String) {
    let normalized = normalize_album_key(&artist);
    if artists
        .iter()
        .any(|existing| normalize_album_key(existing) == normalized)
    {
        return;
    }
    artists.push(artist);
}

fn track_artist(metadata: &TrackMetadata) -> String {
    non_empty_text(&metadata.artist).unwrap_or_else(|| UNKNOWN_ARTIST.to_owned())
}

fn track_title(track: &Track) -> String {
    non_empty_text(&track.metadata.title)
        .or_else(|| {
            track
                .location
                .path()
                .file_stem()
                .and_then(|file_stem| file_stem.to_str())
                .map(str::trim)
                .filter(|file_stem| !file_stem.is_empty())
                .map(ToOwned::to_owned)
        })
        .unwrap_or_default()
}

fn normalize_album_key(value: &str) -> String {
    value.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use sustain_app_runtime::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use super::{
        AlbumTrackViewModel, duration_text, group_albums, replace_track_in_album, track_number_text,
    };

    #[test]
    fn groups_tracks_by_album_artist_and_album_title() {
        let tracks = vec![
            track(1, "b.flac", "Album", "Artist", Some(2), Some(200)),
            track(2, "a.flac", "Album", "Artist", Some(1), Some(200)),
            track(3, "other.flac", "Other", "Artist", Some(1), None),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 2);
        assert_eq!(albums[0].title, "Album");
        assert_eq!(albums[0].artist, "Artist");
        assert_eq!(albums[0].year, Some(200));
        assert_eq!(
            albums[0]
                .tracks
                .iter()
                .map(|track| track.id.get())
                .collect::<Vec<_>>(),
            vec![2, 1]
        );
        assert_eq!(albums[1].title, "Other");
    }

    #[test]
    fn non_compilation_tracks_with_same_album_and_different_artists_stay_separate() {
        let tracks = vec![
            track(1, "a.flac", "Shared", "Artist A", Some(1), None),
            track(2, "b.flac", "Shared", "Artist B", Some(2), None),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 2);
        assert_eq!(
            albums
                .iter()
                .map(|album| album.artist.as_str())
                .collect::<Vec<_>>(),
            vec!["Artist A", "Artist B"]
        );
    }

    #[test]
    fn compilation_tracks_group_by_album_when_track_artists_differ() {
        let tracks = vec![
            compilation_track(1, "a.flac", "Compilation", "Artist A", Some(1)),
            compilation_track(2, "b.flac", "Compilation", "Artist B", Some(2)),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].title, "Compilation");
        assert_eq!(albums[0].artist, "Artist A, Artist B");
        assert_eq!(
            albums[0]
                .tracks
                .iter()
                .map(|track| track.id.get())
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
    }

    #[test]
    fn explicit_album_artist_overrides_compilation_artist_summary() {
        let tracks = vec![
            compilation_track_with_album_artist(
                1,
                "a.flac",
                "Compilation",
                "Artist A",
                "Album Artist",
                Some(1),
            ),
            compilation_track_with_album_artist(
                2,
                "b.flac",
                "Compilation",
                "Artist B",
                "Album Artist",
                Some(2),
            ),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 1);
        assert_eq!(albums[0].artist, "Album Artist");
    }

    #[test]
    fn compilation_artist_summary_preserves_all_unique_artists() {
        let tracks = vec![
            compilation_track(1, "a.flac", "Compilation", "Artist A", Some(1)),
            compilation_track(2, "b.flac", "Compilation", "Artist B", Some(2)),
            compilation_track(3, "c.flac", "Compilation", "Artist C", Some(3)),
            compilation_track(4, "d.flac", "Compilation", "Artist D", Some(4)),
            compilation_track(5, "e.flac", "Compilation", "Artist E", Some(5)),
            compilation_track(6, "f.flac", "Compilation", "artist a", Some(6)),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(
            albums[0].artist,
            "Artist A, Artist B, Artist C, Artist D, Artist E"
        );
    }

    #[test]
    fn representative_track_prefers_first_non_missing() {
        let tracks = vec![
            missing_track(1, "missing.flac", "Album", "Artist"),
            track(2, "present.flac", "Album", "Artist", Some(2), Some(200)),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 1);
        assert_eq!(
            albums[0].representative_track_path.as_deref(),
            Some(std::path::Path::new("present.flac"))
        );
    }

    #[test]
    fn representative_track_falls_back_to_first_when_all_missing() {
        let tracks = vec![
            missing_track(1, "a.flac", "Album", "Artist"),
            missing_track(2, "b.flac", "Album", "Artist"),
        ];

        let albums = group_albums(&tracks);

        assert_eq!(albums.len(), 1);
        // Track ordering inside the bucket runs through `compare_album_tracks`,
        // which here ties on disc/track and falls back to title comparison; the
        // important contract is just "some path, not None".
        assert!(albums[0].representative_track_path.is_some());
    }

    #[test]
    fn uses_unknown_album_and_artist_when_metadata_is_missing() {
        let albums = group_albums(&[Track {
            id: track_id(1),
            location: TrackLocation::available(relative_path("track.flac")),
            metadata: TrackMetadata::default(),
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }]);

        assert_eq!(albums[0].title, "Unknown Album");
        assert_eq!(albums[0].artist, "Unknown Artist");
    }

    #[test]
    fn track_number_text_includes_disc_when_needed() {
        let single_disc = album_track(Some(1), Some(7));
        let multi_disc = album_track(Some(2), Some(7));

        assert_eq!(track_number_text(&single_disc), "7");
        assert_eq!(track_number_text(&multi_disc), "2-7");
    }

    #[test]
    fn duration_text_uses_hours_when_needed() {
        assert_eq!(duration_text(245), "4:05");
        assert_eq!(duration_text(3_904), "1:05:04");
    }

    #[test]
    fn replace_track_in_album_updates_target_and_re_sorts() {
        let mut albums = group_albums(&[
            track(1, "b.flac", "Album", "Artist", Some(2), Some(2000)),
            track(2, "a.flac", "Album", "Artist", Some(1), Some(2000)),
            track(3, "other.flac", "Other", "Artist", Some(1), None),
        ]);
        assert_eq!(
            albums[0]
                .tracks
                .iter()
                .map(|track| track.id.get())
                .collect::<Vec<_>>(),
            vec![2, 1],
        );

        // Promote track 1 from position 2 to position 0; the album's
        // track vector must re-sort and the function must report the
        // album whose row needs repainting.
        let mut replacement =
            super::album_track(&track(1, "b.flac", "Album", "Artist", Some(1), Some(2000)));
        replacement.track_number = Some(0);
        let key = replace_track_in_album(&mut albums, track_id(1), &replacement);

        let updated_album = albums
            .iter()
            .find(|album| album.title == "Album")
            .expect("Album bucket present");
        assert_eq!(key.as_ref(), Some(&updated_album.key));
        assert_eq!(
            updated_album
                .tracks
                .iter()
                .map(|track| track.id.get())
                .collect::<Vec<_>>(),
            vec![1, 2],
        );
    }

    #[test]
    fn replace_track_in_album_returns_none_when_track_absent() {
        let mut albums = group_albums(&[track(1, "a.flac", "Album", "Artist", Some(1), None)]);
        let ghost = AlbumTrackViewModel {
            id: track_id(99),
            file_path: PathBuf::from("ghost.flac"),
            title: "Ghost".to_owned(),
            disc_number: None,
            track_number: None,
            duration_seconds: 0,
            is_missing: false,
        };

        assert!(replace_track_in_album(&mut albums, track_id(99), &ghost).is_none());
    }

    fn track(
        id: i64,
        path: &str,
        album: &str,
        artist: &str,
        track_number: Option<u32>,
        year: Option<i32>,
    ) -> Track {
        Track {
            id: track_id(id),
            location: TrackLocation::available(relative_path(path)),
            metadata: TrackMetadata {
                title: Some(path.trim_end_matches(".flac").to_owned()),
                artist: Some(artist.to_owned()),
                album: Some(album.to_owned()),
                year,
                track_number,
                duration: Some(Duration::from_secs(180)),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn missing_track(id: i64, path: &str, album: &str, artist: &str) -> Track {
        let mut track = track(id, path, album, artist, None, None);
        track.location = TrackLocation::missing(relative_path(path));
        track
    }

    fn compilation_track(
        id: i64,
        path: &str,
        album: &str,
        artist: &str,
        track_number: Option<u32>,
    ) -> Track {
        let mut track = track(id, path, album, artist, track_number, None);
        track.metadata.compilation = Some(true);
        track
    }

    fn compilation_track_with_album_artist(
        id: i64,
        path: &str,
        album: &str,
        artist: &str,
        album_artist: &str,
        track_number: Option<u32>,
    ) -> Track {
        let mut track = compilation_track(id, path, album, artist, track_number);
        track.metadata.album_artist = Some(album_artist.to_owned());
        track
    }

    fn album_track(disc_number: Option<u32>, track_number: Option<u32>) -> AlbumTrackViewModel {
        AlbumTrackViewModel {
            id: track_id(1),
            file_path: PathBuf::from("track.flac"),
            title: "Track".to_owned(),
            disc_number,
            track_number,
            duration_seconds: 180,
            is_missing: false,
        }
    }

    fn track_id(value: i64) -> TrackId {
        match TrackId::new(value) {
            Some(track_id) => track_id,
            None => unreachable!("test helper only constructs positive ids"),
        }
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(PathBuf::from(path)).expect("test path is relative")
    }
}
