// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{Playlist, PlaylistEntry, Track, TrackId, TrackMetadata};

const STRICT_DURATION_TOLERANCE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DuplicateMatchMode {
    #[default]
    Loose,
    Strict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationRequest {
    pub track_ids: Vec<TrackId>,
    pub audio_track_id: TrackId,
    pub metadata_track_id: TrackId,
    pub artwork_track_id: TrackId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationPlan {
    pub survivor: Track,
    pub removed_track_ids: Vec<TrackId>,
    pub rewritten_playlists: Vec<Playlist>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplicateConsolidationError {
    NeedsMultipleTracks,
    RepeatedTrack,
    TrackNotFound,
    TrackUnavailable,
    ReferenceNotSelected,
    CountOverflow,
}

pub fn duplicate_groups(tracks: &[Track], mode: DuplicateMatchMode) -> Vec<Vec<TrackId>> {
    let mut loose_groups: BTreeMap<(String, String), Vec<&Track>> = BTreeMap::new();
    for track in tracks {
        let title = normalized_duplicate_text(track.metadata.title.as_deref().unwrap_or_default());
        if title.is_empty() {
            continue;
        }
        let artist =
            normalized_duplicate_text(track.metadata.artist.as_deref().unwrap_or_default());
        loose_groups.entry((artist, title)).or_default().push(track);
    }

    let mut groups = Vec::new();
    for tracks in loose_groups.into_values().filter(|tracks| tracks.len() > 1) {
        match mode {
            DuplicateMatchMode::Loose => groups.push(sorted_ids(tracks)),
            DuplicateMatchMode::Strict => append_strict_groups(&mut groups, tracks),
        }
    }
    groups
}

pub fn plan_duplicate_consolidation(
    tracks: &[Track],
    playlists: &[Playlist],
    request: &DuplicateConsolidationRequest,
    resulting_file_size_bytes: u64,
    resulting_has_embedded_artwork: bool,
) -> Result<DuplicateConsolidationPlan, DuplicateConsolidationError> {
    let selected_ids = request.track_ids.iter().copied().collect::<BTreeSet<_>>();
    if selected_ids.len() < 2 {
        return Err(DuplicateConsolidationError::NeedsMultipleTracks);
    }
    if selected_ids.len() != request.track_ids.len() {
        return Err(DuplicateConsolidationError::RepeatedTrack);
    }
    for reference in [
        request.audio_track_id,
        request.metadata_track_id,
        request.artwork_track_id,
    ] {
        if !selected_ids.contains(&reference) {
            return Err(DuplicateConsolidationError::ReferenceNotSelected);
        }
    }

    let selected_tracks = selected_ids
        .iter()
        .map(|id| {
            tracks
                .iter()
                .find(|track| track.id == *id)
                .ok_or(DuplicateConsolidationError::TrackNotFound)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if selected_tracks
        .iter()
        .any(|track| track.location.is_missing())
    {
        return Err(DuplicateConsolidationError::TrackUnavailable);
    }
    let audio_track = selected_tracks
        .iter()
        .find(|track| track.id == request.audio_track_id)
        .expect("validated audio reference");
    let metadata_track = selected_tracks
        .iter()
        .find(|track| track.id == request.metadata_track_id)
        .expect("validated metadata reference");

    let mut survivor = (*audio_track).clone();
    copy_editable_metadata(&mut survivor.metadata, &metadata_track.metadata);
    survivor.rating = selected_tracks
        .iter()
        .map(|track| track.rating)
        .max()
        .expect("validated non-empty selection");
    survivor.statistics.play_count = selected_tracks
        .iter()
        .try_fold(0_u64, |total, track| {
            total.checked_add(track.statistics.play_count)
        })
        .ok_or(DuplicateConsolidationError::CountOverflow)?;
    survivor.statistics.skip_count = selected_tracks
        .iter()
        .try_fold(0_u64, |total, track| {
            total.checked_add(track.statistics.skip_count)
        })
        .ok_or(DuplicateConsolidationError::CountOverflow)?;
    survivor.statistics.last_played_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.last_played_at)
        .max();
    survivor.statistics.last_skipped_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.last_skipped_at)
        .max();
    survivor.statistics.date_added_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.date_added_at)
        .min();
    survivor.file_size_bytes = Some(resulting_file_size_bytes);
    survivor.has_embedded_artwork = Some(resulting_has_embedded_artwork);
    survivor.file_modified_at = None;

    let removed_track_ids = selected_ids
        .iter()
        .copied()
        .filter(|track_id| *track_id != survivor.id)
        .collect::<Vec<_>>();
    let rewritten_playlists = playlists
        .iter()
        .filter_map(|playlist| {
            rewrite_playlist(playlist, &removed_track_ids)
                .then_some(())
                .map(|()| rewrite_playlist_entries(playlist, survivor.id, &removed_track_ids))
        })
        .collect();

    Ok(DuplicateConsolidationPlan {
        survivor,
        removed_track_ids,
        rewritten_playlists,
    })
}

fn append_strict_groups(groups: &mut Vec<Vec<TrackId>>, tracks: Vec<&Track>) {
    let mut strict_groups: Vec<Vec<&Track>> = Vec::new();
    for track in tracks {
        let album = normalized_duplicate_text(track.metadata.album.as_deref().unwrap_or_default());
        if let Some(group) = strict_groups.iter_mut().find(|group| {
            let reference = group[0];
            normalized_duplicate_text(reference.metadata.album.as_deref().unwrap_or_default())
                == album
                && durations_match(reference.metadata.duration, track.metadata.duration)
        }) {
            group.push(track);
        } else {
            strict_groups.push(vec![track]);
        }
    }
    groups.extend(
        strict_groups
            .into_iter()
            .filter(|group| group.len() > 1)
            .map(sorted_ids),
    );
}

fn sorted_ids(mut tracks: Vec<&Track>) -> Vec<TrackId> {
    tracks.sort_by_key(|track| track.id);
    tracks.into_iter().map(|track| track.id).collect()
}

fn durations_match(left: Option<Duration>, right: Option<Duration>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.abs_diff(right) <= STRICT_DURATION_TOLERANCE,
        (None, None) => true,
        _ => false,
    }
}

fn normalized_duplicate_text(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn copy_editable_metadata(target: &mut TrackMetadata, source: &TrackMetadata) {
    target.title.clone_from(&source.title);
    target.artist.clone_from(&source.artist);
    target.album.clone_from(&source.album);
    target.album_artist.clone_from(&source.album_artist);
    target.composer.clone_from(&source.composer);
    target.grouping.clone_from(&source.grouping);
    target.genre.clone_from(&source.genre);
    target.track_number = source.track_number;
    target.track_total = source.track_total;
    target.disc_number = source.disc_number;
    target.disc_total = source.disc_total;
    target.year = source.year;
    target.compilation = source.compilation;
    target.bpm = source.bpm;
    target.key.clone_from(&source.key);
    target.comments.clone_from(&source.comments);
    target.lyrics.clone_from(&source.lyrics);
}

fn rewrite_playlist(playlist: &Playlist, removed_ids: &[TrackId]) -> bool {
    playlist
        .entries
        .iter()
        .any(|entry| removed_ids.contains(&entry.track_id))
}

fn rewrite_playlist_entries(
    playlist: &Playlist,
    survivor_id: TrackId,
    removed_ids: &[TrackId],
) -> Playlist {
    let mut entries = playlist.entries.clone();
    entries.sort_by_key(|entry| entry.position);
    let mut rewritten = Vec::with_capacity(entries.len());
    for entry in entries {
        let track_id = if removed_ids.contains(&entry.track_id) {
            survivor_id
        } else {
            entry.track_id
        };
        if rewritten
            .last()
            .is_some_and(|previous: &PlaylistEntry| previous.track_id == track_id)
        {
            continue;
        }
        rewritten.push(PlaylistEntry {
            playlist_id: playlist.id,
            track_id,
            position: rewritten.len() as u32,
        });
    }
    Playlist {
        entries: rewritten,
        ..playlist.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::{PlayStatistics, PlaylistId, Rating, TrackLocation, TrackRelativePath};

    use super::*;

    #[test]
    fn loose_groups_fold_case_diacritics_and_whitespace() {
        let tracks = vec![
            track(1, "Beyoncé", " Déjà   Vu ", "Album", 200),
            track(2, "BEYONCE", "deja vu", "Other", 240),
            track(3, "Beyoncé", "Halo", "Album", 200),
        ];

        assert_eq!(
            duplicate_groups(&tracks, DuplicateMatchMode::Loose),
            vec![vec![track_id(1), track_id(2)]]
        );
    }

    #[test]
    fn strict_groups_require_album_and_near_duration() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "artist", "song", "album", 202),
            track(3, "artist", "song", "album", 204),
            track(4, "artist", "song", "other", 201),
        ];

        assert_eq!(
            duplicate_groups(&tracks, DuplicateMatchMode::Strict),
            vec![vec![track_id(1), track_id(2)]]
        );
    }

    #[test]
    fn plan_keeps_audio_identity_takes_metadata_and_aggregates_library_values() {
        let mut first = track(1, "Artist", "First", "Album", 200);
        first.rating = Rating::new(3).expect("rating");
        first.statistics = statistics(2, 1, 200, 100);
        let mut second = track(2, "Artist", "Chosen", "Album", 220);
        second.rating = Rating::new(5).expect("rating");
        second.statistics = statistics(7, 4, 300, 150);
        let playlist_id = PlaylistId::new(1).expect("playlist");
        let playlists = vec![Playlist {
            id: playlist_id,
            name: "Mix".to_owned(),
            parent_folder_id: None,
            position: 0,
            entries: vec![
                entry(playlist_id, first.id, 0),
                entry(playlist_id, second.id, 1),
                entry(playlist_id, track_id(3), 2),
            ],
        }];

        let plan = plan_duplicate_consolidation(
            &[first, second],
            &playlists,
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata_track_id: track_id(2),
                artwork_track_id: track_id(2),
            },
            1234,
            true,
        )
        .expect("plan");

        assert_eq!(plan.survivor.id, track_id(1));
        assert_eq!(plan.survivor.metadata.title.as_deref(), Some("Chosen"));
        assert_eq!(
            plan.survivor.metadata.duration,
            Some(Duration::from_secs(200))
        );
        assert_eq!(plan.survivor.rating, Rating::new(5).expect("rating"));
        assert_eq!(plan.survivor.statistics.play_count, 9);
        assert_eq!(plan.survivor.statistics.skip_count, 5);
        assert_eq!(
            plan.survivor.statistics.last_played_at,
            Some(UNIX_EPOCH + Duration::from_secs(300))
        );
        assert_eq!(
            plan.survivor.statistics.date_added_at,
            Some(UNIX_EPOCH + Duration::from_secs(100))
        );
        assert_eq!(plan.removed_track_ids, vec![track_id(2)]);
        assert_eq!(
            plan.rewritten_playlists[0]
                .entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![track_id(1), track_id(3)]
        );
    }

    fn track(id: i64, artist: &str, title: &str, album: &str, duration: u64) -> Track {
        Track {
            id: track_id(id),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("{id}.flac")).expect("path"),
            ),
            metadata: TrackMetadata {
                artist: Some(artist.to_owned()),
                title: Some(title.to_owned()),
                album: Some(album.to_owned()),
                duration: Some(Duration::from_secs(duration)),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: Some(100),
            has_embedded_artwork: Some(false),
            file_modified_at: None,
        }
    }

    fn statistics(
        play_count: u64,
        skip_count: u64,
        last_played: u64,
        date_added: u64,
    ) -> PlayStatistics {
        PlayStatistics {
            play_count,
            skip_count,
            last_played_at: Some(UNIX_EPOCH + Duration::from_secs(last_played)),
            last_skipped_at: None,
            date_added_at: Some(UNIX_EPOCH + Duration::from_secs(date_added)),
        }
    }

    fn entry(playlist_id: PlaylistId, track_id: TrackId, position: u32) -> PlaylistEntry {
        PlaylistEntry {
            playlist_id,
            track_id,
            position,
        }
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("track")
    }
}
