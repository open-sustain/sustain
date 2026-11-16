// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

use sustain_app_runtime::{
    CollationKey, Track, TrackId, TrackMetadata, compare_optional_text, effective_sort_key,
};
use sustain_i18n::gettext;

use crate::artwork_loader::ArtworkSource;
use crate::util::non_empty_text;

/// Stable identity for an album, derived from the normalized grouping
/// artist and title used to bucket tracks. For normal albums the grouping
/// artist is the album artist or track artist; for compilations without an
/// explicit album artist it is the shared compilation bucket. Equality
/// matches the bucketing logic, so two `AlbumViewModel`s produced from
/// different `group_albums` calls share a key iff they cover the same tracks.
#[derive(Clone, Debug, Eq, PartialEq, Hash, PartialOrd, Ord)]
pub(super) struct AlbumKey {
    artist: CollationKey,
    title: CollationKey,
}

#[cfg(test)]
impl AlbumKey {
    pub(super) fn for_test(artist: &str, title: &str) -> Self {
        Self {
            artist: CollationKey::new(artist),
            title: CollationKey::new(title),
        }
    }
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
    pub(super) artwork_source: Option<ArtworkSource>,
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
    /// The "sort as" tags the album files under, from the first of its
    /// tracks to carry each; `None` files it under the name it shows.
    sort_artist: Option<String>,
    sort_title: Option<String>,
}

impl AlbumBucket {
    /// Where the album files in the grid: by artist, then by title, each
    /// under its "sort as" tag when it has one, so "The Beatles" sits
    /// under B here as it does in the track table.
    fn filing_order(&self) -> (CollationKey, CollationKey, AlbumKey) {
        let filed_under = |sort_tag: &Option<String>, shown: &CollationKey| {
            sort_tag
                .as_deref()
                .map_or_else(|| shown.clone(), CollationKey::new)
        };
        (
            filed_under(&self.sort_artist, &self.key.artist),
            filed_under(&self.sort_title, &self.key.title),
            self.key.clone(),
        )
    }
}

pub(super) fn group_albums(
    tracks: &[Track],
    library_root: Option<&Path>,
    honor_sort_tags: bool,
) -> Vec<AlbumViewModel> {
    let mut albums = BTreeMap::<AlbumKey, AlbumBucket>::new();

    for track in tracks {
        let title = album_title(&track.metadata);
        let grouping = album_grouping(&track.metadata);
        let key = AlbumKey {
            artist: CollationKey::new(&grouping.key_artist),
            title: CollationKey::new(&title),
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
            sort_artist: None,
            sort_title: None,
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
        if bucket.sort_artist.is_none() {
            bucket.sort_artist =
                effective_sort_key(grouping.artist_sort_tag, None, honor_sort_tags)
                    .map(ToOwned::to_owned);
        }
        if bucket.sort_title.is_none() {
            bucket.sort_title =
                effective_sort_key(track.metadata.album_sort.as_deref(), None, honor_sort_tags)
                    .map(ToOwned::to_owned);
        }
        bucket.tracks.push(album_track(track));
    }

    let mut albums: Vec<AlbumBucket> = albums.into_values().collect();
    albums.sort_by_cached_key(AlbumBucket::filing_order);
    albums
        .into_iter()
        .map(|mut bucket| {
            bucket.tracks.sort_by(compare_album_tracks);
            let representative_track_path = bucket
                .tracks
                .iter()
                .find(|track| !track.is_missing)
                .or_else(|| bucket.tracks.first())
                .map(|track| track.file_path.clone());
            let artwork_source = representative_track_path_to_source(
                representative_track_path.as_ref(),
                library_root,
            );
            let artist = album_display_artist(&bucket);
            AlbumViewModel {
                key: bucket.key,
                title: bucket.title,
                artist,
                year: bucket.year,
                tracks: bucket.tracks,
                representative_track_path,
                artwork_source,
            }
        })
        .collect()
}

/// Replace the rows of `new_tracks` inside whichever albums hold them, in
/// one pass over every album. Returns the indices of the albums whose shown
/// tracks actually changed, so callers re-render exactly those rows; a
/// track no album holds, or whose shown fields did not change, is skipped.
///
/// A changed album's track list is re-sorted with [`compare_album_tracks`]
/// because a metadata update can move a track within its album (e.g. a
/// Tags retrieval populating a missing track number). Callers must regroup
/// instead when metadata changes can move a track to a different album
/// bucket.
pub(super) fn replace_tracks_in_albums(
    albums: &mut [AlbumViewModel],
    new_tracks: &HashMap<TrackId, AlbumTrackViewModel>,
    library_root: Option<&Path>,
) -> Vec<usize> {
    let mut updated = Vec::new();
    for (album_index, album) in albums.iter_mut().enumerate() {
        let mut changed = false;
        for slot in &mut album.tracks {
            if let Some(new_track) = new_tracks.get(&slot.id)
                && slot != new_track
            {
                slot.clone_from(new_track);
                changed = true;
            }
        }
        if changed {
            album.tracks.sort_by(compare_album_tracks);
            refresh_album_artwork_source(album, library_root);
            updated.push(album_index);
        }
    }
    updated
}

fn refresh_album_artwork_source(album: &mut AlbumViewModel, library_root: Option<&Path>) {
    album.representative_track_path = album
        .tracks
        .iter()
        .find(|track| !track.is_missing)
        .or_else(|| album.tracks.first())
        .map(|track| track.file_path.clone());
    album.artwork_source =
        representative_track_path_to_source(album.representative_track_path.as_ref(), library_root);
}

fn representative_track_path_to_source(
    path: Option<&PathBuf>,
    library_root: Option<&Path>,
) -> Option<ArtworkSource> {
    let relative = path?;
    if relative.is_absolute() {
        return Some(ArtworkSource::embedded_track(
            relative.clone(),
            relative.clone(),
        ));
    }
    Some(ArtworkSource::embedded_track(
        relative.clone(),
        library_root?.join(relative),
    ))
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
        .then_with(|| compare_optional_text(Some(&left.title), Some(&right.title)))
        .then_with(|| left.id.cmp(&right.id))
}

fn album_title(metadata: &TrackMetadata) -> String {
    non_empty_text(&metadata.album).unwrap_or_else(|| gettext("Unknown Album"))
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct AlbumGrouping<'a> {
    /// The "sort as" tag of the artist the album is keyed by, if any.
    artist_sort_tag: Option<&'a str>,
    key_artist: String,
    display_artist: String,
    track_artist: String,
    has_explicit_album_artist: bool,
    is_compilation: bool,
}

fn album_grouping(metadata: &TrackMetadata) -> AlbumGrouping<'_> {
    let track_artist = track_artist(metadata);
    if let Some(album_artist) = non_empty_text(&metadata.album_artist) {
        return AlbumGrouping {
            artist_sort_tag: metadata.album_artist_sort.as_deref(),
            key_artist: album_artist.clone(),
            display_artist: album_artist,
            track_artist,
            has_explicit_album_artist: true,
            is_compilation: metadata.compilation.unwrap_or(false),
        };
    }

    if metadata.compilation.unwrap_or(false) {
        return AlbumGrouping {
            artist_sort_tag: None,
            key_artist: gettext("Various Artists"),
            display_artist: gettext("Various Artists"),
            track_artist,
            has_explicit_album_artist: false,
            is_compilation: true,
        };
    }

    AlbumGrouping {
        artist_sort_tag: metadata.artist_sort.as_deref(),
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
    let key = CollationKey::new(&artist);
    if artists
        .iter()
        .any(|existing| CollationKey::new(existing) == key)
    {
        return;
    }
    artists.push(artist);
}

fn track_artist(metadata: &TrackMetadata) -> String {
    non_empty_text(&metadata.artist).unwrap_or_else(|| gettext("Unknown Artist"))
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

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use std::collections::HashMap;

    use sustain_app_runtime::{
        PlayStatistics, Rating, Track, TrackId, TrackLocation, TrackMetadata, TrackRelativePath,
    };

    use crate::artwork_loader::ArtworkSource;

    use super::{
        AlbumTrackViewModel, duration_text, group_albums, replace_tracks_in_albums,
        track_number_text,
    };

    #[test]
    fn groups_tracks_by_album_artist_and_album_title() {
        let tracks = vec![
            track(1, "b.flac", "Album", "Artist", Some(2), Some(200)),
            track(2, "a.flac", "Album", "Artist", Some(1), Some(200)),
            track(3, "other.flac", "Other", "Artist", Some(1), None),
        ];

        let albums = group_albums(&tracks, None, true);

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
    fn an_album_by_an_accented_artist_files_under_the_base_letter() {
        let tracks = vec![
            track(1, "a.flac", "Zoo", "Zebra", Some(1), None),
            track(2, "b.flac", "Olympia", "Édith Piaf", Some(1), None),
            track(3, "c.flac", "Eden", "Eurythmics", Some(1), None),
            track(4, "d.flac", "Debut", "Björk", Some(1), None),
        ];

        let albums = group_albums(&tracks, None, true);

        assert_eq!(
            albums
                .iter()
                .map(|album| album.artist.as_str())
                .collect::<Vec<_>>(),
            ["Björk", "Édith Piaf", "Eurythmics", "Zebra"]
        );
    }

    #[test]
    fn an_album_files_under_its_sort_tags_while_they_are_honored() {
        let mut beatles = track(1, "a.flac", "The White Album", "The Beatles", Some(1), None);
        beatles.metadata.artist_sort = Some("Beatles (The)".to_owned());
        beatles.metadata.album_sort = Some("White Album (The)".to_owned());
        let tracks = vec![
            beatles,
            track(2, "b.flac", "Mezzanine", "Massive Attack", Some(1), None),
            track(3, "c.flac", "Blue Lines", "Massive Attack", Some(1), None),
        ];
        let titles = |honor_sort_tags| {
            group_albums(&tracks, None, honor_sort_tags)
                .into_iter()
                .map(|album| album.title)
                .collect::<Vec<_>>()
        };

        assert_eq!(
            titles(true),
            ["The White Album", "Blue Lines", "Mezzanine"],
            "filed under B, and still shown as written"
        );
        assert_eq!(
            titles(false),
            ["Blue Lines", "Mezzanine", "The White Album"]
        );
    }

    #[test]
    fn non_compilation_tracks_with_same_album_and_different_artists_stay_separate() {
        let tracks = vec![
            track(1, "a.flac", "Shared", "Artist A", Some(1), None),
            track(2, "b.flac", "Shared", "Artist B", Some(2), None),
        ];

        let albums = group_albums(&tracks, None, true);

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

        let albums = group_albums(&tracks, None, true);

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

        let albums = group_albums(&tracks, None, true);

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

        let albums = group_albums(&tracks, None, true);

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
        let library_root = PathBuf::from("/music");

        let albums = group_albums(&tracks, Some(&library_root), true);

        assert_eq!(albums.len(), 1);
        assert_eq!(
            albums[0].representative_track_path.as_deref(),
            Some(std::path::Path::new("present.flac"))
        );
        assert_eq!(
            albums[0].artwork_source,
            Some(ArtworkSource::embedded_track(
                PathBuf::from("present.flac"),
                library_root.join("present.flac"),
            ))
        );
    }

    #[test]
    fn representative_track_falls_back_to_first_when_all_missing() {
        let tracks = vec![
            missing_track(1, "a.flac", "Album", "Artist"),
            missing_track(2, "b.flac", "Album", "Artist"),
        ];

        let albums = group_albums(&tracks, None, true);

        assert_eq!(albums.len(), 1);
        // Track ordering inside the bucket runs through `compare_album_tracks`,
        // which here ties on disc/track and falls back to title comparison; the
        // important contract is just "some path, not None".
        assert!(albums[0].representative_track_path.is_some());
        assert!(
            albums[0].artwork_source.is_none(),
            "relative album artwork source requires a library root"
        );
    }

    #[test]
    fn uses_unknown_album_and_artist_when_metadata_is_missing() {
        let albums = group_albums(
            &[Track {
                id: track_id(1),
                location: TrackLocation::available(relative_path("track.flac")),
                metadata: TrackMetadata::default(),
                rating: Rating::unrated(),
                statistics: PlayStatistics::default(),
                file_size_bytes: None,
                has_embedded_artwork: None,
                file_modified_at: None,
            }],
            None,
            true,
        );

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
    fn replacing_tracks_re_sorts_their_album_and_reports_it() {
        let mut albums = group_albums(
            &[
                track(1, "b.flac", "Album", "Artist", Some(2), Some(2000)),
                track(2, "a.flac", "Album", "Artist", Some(1), Some(2000)),
                track(3, "other.flac", "Other", "Artist", Some(1), None),
            ],
            None,
            true,
        );
        assert_eq!(
            albums[0]
                .tracks
                .iter()
                .map(|track| track.id.get())
                .collect::<Vec<_>>(),
            vec![2, 1],
        );

        // Promote track 1 from position 2 to position 0; the album's
        // track vector must re-sort and the album whose row needs
        // repainting must be reported.
        let mut replacement =
            super::album_track(&track(1, "b.flac", "Album", "Artist", Some(1), Some(2000)));
        replacement.track_number = Some(0);
        let updated = replace_tracks_in_albums(
            &mut albums,
            &HashMap::from([(track_id(1), replacement)]),
            None,
        );

        let updated_album_index = albums
            .iter()
            .position(|album| album.title == "Album")
            .expect("Album bucket present");
        assert_eq!(updated, vec![updated_album_index]);
        let updated_album = &albums[updated_album_index];
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
    fn replacing_tracks_of_several_albums_reports_each_once() {
        let mut albums = group_albums(
            &[
                track(1, "a.flac", "Album", "Artist", Some(1), None),
                track(2, "b.flac", "Album", "Artist", Some(2), None),
                track(3, "c.flac", "Other", "Artist", Some(1), None),
            ],
            None,
            true,
        );
        let renamed = |id, path, album, number| {
            let mut renamed = super::album_track(&track(id, path, album, "Artist", number, None));
            renamed.title = format!("Renamed {id}");
            renamed
        };

        let updated = replace_tracks_in_albums(
            &mut albums,
            &HashMap::from([
                (track_id(1), renamed(1, "a.flac", "Album", Some(1))),
                (track_id(2), renamed(2, "b.flac", "Album", Some(2))),
                (track_id(3), renamed(3, "c.flac", "Other", Some(1))),
            ]),
            None,
        );

        assert_eq!(updated, (0..albums.len()).collect::<Vec<_>>());
    }

    #[test]
    fn replacing_an_absent_or_unchanged_track_reports_nothing() {
        let mut albums = group_albums(
            &[track(1, "a.flac", "Album", "Artist", Some(1), None)],
            None,
            true,
        );
        let ghost = AlbumTrackViewModel {
            id: track_id(99),
            file_path: PathBuf::from("ghost.flac"),
            title: "Ghost".to_owned(),
            disc_number: None,
            track_number: None,
            duration_seconds: 0,
            is_missing: false,
        };
        let unchanged = super::album_track(&track(1, "a.flac", "Album", "Artist", Some(1), None));

        assert!(
            replace_tracks_in_albums(
                &mut albums,
                &HashMap::from([(track_id(99), ghost), (track_id(1), unchanged)]),
                None,
            )
            .is_empty()
        );
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
