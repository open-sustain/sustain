// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{path::Path, time::SystemTime};

use sustain_app_runtime::{Track, TrackId, effective_sort_key, normalize_sort_text};

use crate::util::non_empty_text;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum AudioFileType {
    Flac,
    M4a,
    Mp4,
    Mp3,
    Ogg,
    Wav,
    Unknown,
}

impl AudioFileType {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::Flac => "FLAC",
            Self::M4a => "M4A",
            Self::Mp4 => "MP4",
            Self::Mp3 => "MP3",
            Self::Ogg => "OGG",
            Self::Wav => "WAV",
            Self::Unknown => "",
        }
    }

    fn from_path(path: &Path) -> Self {
        match path
            .extension()
            .and_then(|extension| extension.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("flac") => Self::Flac,
            Some("m4a") | Some("m4b") => Self::M4a,
            Some("mp4") => Self::Mp4,
            Some("mp3") => Self::Mp3,
            Some("ogg") | Some("oga") | Some("opus") => Self::Ogg,
            Some("wav") => Self::Wav,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct TrackTableRow {
    pub(crate) track_id: Option<TrackId>,
    pub(crate) track_name: String,
    pub(crate) artist: String,
    pub(crate) album: String,
    pub(crate) genre: String,
    pub(crate) has_lyrics: bool,
    /// Effective sort keys for the text columns, resolved at row-build
    /// time from the track's "sort as" tags and the live
    /// honor-sort-tags preference (issue #13), then pre-collated with
    /// [`normalize_sort_text`] so the column comparators are plain
    /// `str::cmp`. The keys order "The Beatles" under B while the row
    /// still displays as written.
    pub(super) track_name_sort_key: String,
    pub(super) artist_sort_key: String,
    pub(super) album_sort_key: String,
    pub(crate) year: Option<i32>,
    pub(crate) bpm: Option<u32>,
    pub(crate) music_key: Option<String>,
    pub(crate) bitrate_kbps: Option<u32>,
    pub(super) file_type: AudioFileType,
    pub(crate) duration_seconds: u64,
    pub(crate) rating: u8,
    pub(crate) plays: u64,
    pub(crate) skips: u64,
    pub(crate) last_played: Option<SystemTime>,
    pub(crate) last_skipped: Option<SystemTime>,
    pub(crate) date_added: Option<SystemTime>,
    pub(crate) track_number: Option<u32>,
    pub(crate) file_size_bytes: u64,
    pub(crate) is_missing: bool,
    /// Authoritative position of this track inside the currently-displayed
    /// regular playlist, mirrored straight from
    /// [`sustain_app_runtime::PlaylistEntry::position`]. `None` for any row
    /// not sourced from a regular playlist (Songs view, Albums view's track
    /// list, Library / Smart Playlist selections, etc.). The status column
    /// sorts by this field, so its non-None value defines the "play order"
    /// the user can click back to after sorting by another column.
    pub(crate) playlist_position: Option<u32>,
    /// Optional stable band for derived grouped views such as Duplicates.
    /// `None` keeps ordinary alternating row striping.
    pub(crate) group_band: Option<bool>,
}

impl TrackTableRow {
    pub(crate) fn from_track(track: &Track, honor_sort_tags: bool) -> Self {
        let track_name = non_empty_text(&track.metadata.title)
            .or_else(|| file_stem_text(track.location.path()))
            .unwrap_or_default();
        let artist = non_empty_text(&track.metadata.artist).unwrap_or_default();
        let album = non_empty_text(&track.metadata.album).unwrap_or_default();
        let track_name_sort_key = sort_key(
            track.metadata.title_sort.as_deref(),
            &track_name,
            honor_sort_tags,
        );
        let artist_sort_key = sort_key(
            track.metadata.artist_sort.as_deref(),
            &artist,
            honor_sort_tags,
        );
        let album_sort_key = sort_key(
            track.metadata.album_sort.as_deref(),
            &album,
            honor_sort_tags,
        );
        Self {
            track_id: Some(track.id),
            track_name,
            artist,
            album,
            genre: non_empty_text(&track.metadata.genre).unwrap_or_default(),
            has_lyrics: track.has_lyrics(),
            track_name_sort_key,
            artist_sort_key,
            album_sort_key,
            year: track.metadata.year,
            bpm: track.metadata.bpm,
            music_key: non_empty_text(&track.metadata.key),
            bitrate_kbps: track.metadata.bitrate_kbps,
            file_type: AudioFileType::from_path(track.location.path()),
            duration_seconds: track
                .metadata
                .duration
                .map(|duration| duration.as_secs())
                .unwrap_or_default(),
            rating: track.rating.stars(),
            plays: track.statistics.play_count,
            skips: track.statistics.skip_count,
            last_played: track.statistics.last_played_at,
            last_skipped: track.statistics.last_skipped_at,
            date_added: track.statistics.date_added_at,
            track_number: track.metadata.track_number,
            file_size_bytes: track.file_size_bytes.unwrap_or(0),
            is_missing: track.location.is_missing(),
            playlist_position: None,
            group_band: None,
        }
    }

    pub(crate) fn with_playlist_position(mut self, playlist_position: Option<u32>) -> Self {
        self.playlist_position = playlist_position;
        self
    }

    pub(crate) fn with_group_band(mut self, group_band: bool) -> Self {
        self.group_band = Some(group_band);
        self
    }
}

/// The effective sort key for a text column: the tag-derived "sort as"
/// value when the preference is on and one is present, otherwise the
/// already-resolved display string — pre-collated so sorting compares
/// stored keys with `str::cmp` instead of re-normalizing per comparison.
/// Shares [`sustain_app_runtime::effective_sort_key`] and
/// [`normalize_sort_text`] with the library store so the table headers
/// and the store sort agree (issue #13).
fn sort_key(sort_field: Option<&str>, display: &str, honor_sort_tags: bool) -> String {
    normalize_sort_text(
        effective_sort_key(sort_field, Some(display), honor_sort_tags).unwrap_or(display),
    )
}

fn file_stem_text(path: &Path) -> Option<String> {
    path.file_stem()
        .and_then(|file_stem| file_stem.to_str())
        .map(str::trim)
        .filter(|file_stem| !file_stem.is_empty())
        .map(ToOwned::to_owned)
}
