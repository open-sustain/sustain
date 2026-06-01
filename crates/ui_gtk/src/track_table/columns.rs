// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::cmp::Ordering as CmpOrdering;
use std::time::SystemTime;

use sustain_app_runtime::compare_optional_text;

use super::inline_edit::EditableField;
use super::row::TrackTableRow;
use crate::date_format::format_system_time_short;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum TrackTableColumn {
    TrackName,
    Artist,
    Album,
    Genre,
    Lyrics,
    Year,
    Bpm,
    MusicKey,
    Bitrate,
    FileType,
    Duration,
    Rating,
    Plays,
    Skips,
    LastPlayed,
    LastSkipped,
    DateAdded,
    TrackNumber,
}

pub(super) const TRACK_TABLE_COLUMNS: &[TrackTableColumn] = &[
    TrackTableColumn::TrackName,
    TrackTableColumn::Artist,
    TrackTableColumn::Album,
    TrackTableColumn::Genre,
    TrackTableColumn::Lyrics,
    TrackTableColumn::Year,
    TrackTableColumn::Bpm,
    TrackTableColumn::MusicKey,
    TrackTableColumn::Bitrate,
    TrackTableColumn::FileType,
    TrackTableColumn::Duration,
    TrackTableColumn::Rating,
    TrackTableColumn::Plays,
    TrackTableColumn::Skips,
    TrackTableColumn::LastPlayed,
    TrackTableColumn::LastSkipped,
    TrackTableColumn::DateAdded,
    TrackTableColumn::TrackNumber,
];

impl TrackTableColumn {
    pub(super) fn title(self) -> &'static str {
        match self {
            Self::TrackName => "Track Name",
            Self::Artist => "Artist",
            Self::Album => "Album",
            Self::Genre => "Genre",
            Self::Lyrics => "Lyrics",
            Self::Year => "Year",
            Self::Bpm => "BPM",
            Self::MusicKey => "Key",
            Self::Bitrate => "Bitrate",
            Self::FileType => "Type",
            Self::Duration => "Duration",
            Self::Rating => "Rating",
            Self::Plays => "Plays",
            Self::Skips => "Skips",
            Self::LastPlayed => "Last Played",
            Self::LastSkipped => "Last Skipped",
            Self::DateAdded => "Date Added",
            Self::TrackNumber => "Track #",
        }
    }

    pub(super) fn action_name(self) -> &'static str {
        match self {
            Self::TrackName => "track_name",
            Self::Artist => "artist",
            Self::Album => "album",
            Self::Genre => "genre",
            Self::Lyrics => "lyrics",
            Self::Year => "year",
            Self::Bpm => "bpm",
            Self::MusicKey => "music_key",
            Self::Bitrate => "bitrate",
            Self::FileType => "file_type",
            Self::Duration => "duration",
            Self::Rating => "rating",
            Self::Plays => "plays",
            Self::Skips => "skips",
            Self::LastPlayed => "last_played",
            Self::LastSkipped => "last_skipped",
            Self::DateAdded => "date_added",
            Self::TrackNumber => "track_number",
        }
    }

    pub(super) fn default_width(self) -> i32 {
        match self {
            Self::TrackName => 220,
            Self::Artist => 150,
            Self::Album => 170,
            Self::Genre => 120,
            Self::Lyrics => 72,
            Self::Year => 72,
            Self::Bpm => 72,
            Self::MusicKey => 64,
            Self::Bitrate => 90,
            Self::FileType => 72,
            Self::Duration => 86,
            Self::Rating => 94,
            Self::Plays => 76,
            Self::Skips => 76,
            Self::LastPlayed => 120,
            Self::LastSkipped => 120,
            Self::DateAdded => 120,
            Self::TrackNumber => 86,
        }
    }

    pub(super) fn expands(self) -> bool {
        false
    }

    pub(super) fn default_visible(self) -> bool {
        // Skips and Last Skipped exist for users who care about the
        // signal but are noisier than most tracks need, so they ship
        // off by default. The user surfaces them through the column
        // selector menu like any other optional column. Music Key is
        // a niche analysis output most listeners don't think in terms
        // of, so it ships off by default too.
        !matches!(
            self,
            Self::Skips | Self::LastSkipped | Self::MusicKey | Self::Lyrics
        )
    }

    /// The metadata field this column edits inline, or `None` for columns
    /// that are derived, statistical, or otherwise read-only (Bitrate,
    /// Type, Duration, Rating — which has its own star widget — Plays,
    /// Skips, Last Played, Last Skipped, Date Added).
    pub(super) fn editable_field(self) -> Option<EditableField> {
        match self {
            Self::TrackName => Some(EditableField::Title),
            Self::Artist => Some(EditableField::Artist),
            Self::Album => Some(EditableField::Album),
            Self::Genre => Some(EditableField::Genre),
            Self::Year => Some(EditableField::Year),
            Self::Bpm => Some(EditableField::Bpm),
            Self::MusicKey => Some(EditableField::Key),
            Self::TrackNumber => Some(EditableField::TrackNumber),
            Self::Bitrate
            | Self::FileType
            | Self::Lyrics
            | Self::Duration
            | Self::Rating
            | Self::Plays
            | Self::Skips
            | Self::LastPlayed
            | Self::LastSkipped
            | Self::DateAdded => None,
        }
    }

    pub(super) fn xalign(self) -> f32 {
        match self {
            Self::TrackName
            | Self::Artist
            | Self::Album
            | Self::Genre
            | Self::Lyrics
            | Self::FileType
            | Self::LastPlayed
            | Self::LastSkipped
            | Self::DateAdded => 0.0,
            Self::MusicKey => 0.5,
            Self::Year
            | Self::Bpm
            | Self::Bitrate
            | Self::Duration
            | Self::Rating
            | Self::Plays
            | Self::Skips
            | Self::TrackNumber => 1.0,
        }
    }

    pub(super) fn text(self, row: &TrackTableRow) -> String {
        match self {
            Self::TrackName => row.track_name.clone(),
            Self::Artist => row.artist.clone(),
            Self::Album => row.album.clone(),
            Self::Genre => row.genre.clone(),
            Self::Lyrics => yes_no_text(row.has_lyrics).to_owned(),
            Self::Year => optional_number_text(row.year),
            Self::Bpm => optional_number_text(row.bpm),
            Self::MusicKey => row.music_key.clone().unwrap_or_default(),
            Self::Bitrate => row
                .bitrate_kbps
                .map(|bitrate| format!("{bitrate} kbps"))
                .unwrap_or_default(),
            Self::FileType => row.file_type.label().to_owned(),
            Self::Duration => track_duration_text(row.duration_seconds),
            Self::Rating => row.rating.to_string(),
            Self::Plays => row.plays.to_string(),
            Self::Skips => row.skips.to_string(),
            Self::LastPlayed => optional_date_text(row.last_played),
            Self::LastSkipped => optional_date_text(row.last_skipped),
            Self::DateAdded => optional_date_text(row.date_added),
            Self::TrackNumber => optional_number_text(row.track_number),
        }
    }

    pub(super) fn compare(self, left: &TrackTableRow, right: &TrackTableRow) -> CmpOrdering {
        match self {
            // Text columns with parallel "sort as" tags order by the
            // pre-resolved sort keys (issue #13), so "The Beatles" files
            // under B while still displaying as written.
            Self::TrackName => compare_optional_text(
                Some(&left.track_name_sort_key),
                Some(&right.track_name_sort_key),
            ),
            Self::Artist => {
                compare_optional_text(Some(&left.artist_sort_key), Some(&right.artist_sort_key))
            }
            Self::Album => {
                compare_optional_text(Some(&left.album_sort_key), Some(&right.album_sort_key))
            }
            Self::Genre => compare_optional_text(Some(&left.genre), Some(&right.genre)),
            Self::Lyrics => left.has_lyrics.cmp(&right.has_lyrics),
            Self::Year => left.year.cmp(&right.year),
            Self::Bpm => left.bpm.cmp(&right.bpm),
            Self::MusicKey => {
                compare_optional_text(left.music_key.as_deref(), right.music_key.as_deref())
            }
            Self::Bitrate => left.bitrate_kbps.cmp(&right.bitrate_kbps),
            Self::FileType => left.file_type.label().cmp(right.file_type.label()),
            Self::Duration => left.duration_seconds.cmp(&right.duration_seconds),
            Self::Rating => left.rating.cmp(&right.rating),
            Self::Plays => left.plays.cmp(&right.plays),
            Self::Skips => left.skips.cmp(&right.skips),
            Self::LastPlayed => left.last_played.cmp(&right.last_played),
            Self::LastSkipped => left.last_skipped.cmp(&right.last_skipped),
            Self::DateAdded => left.date_added.cmp(&right.date_added),
            Self::TrackNumber => left.track_number.cmp(&right.track_number),
        }
    }
}

fn optional_number_text<T: std::fmt::Display>(value: Option<T>) -> String {
    value.map(|value| value.to_string()).unwrap_or_default()
}

fn yes_no_text(value: bool) -> &'static str {
    if value { "Yes" } else { "No" }
}

fn optional_date_text(value: Option<SystemTime>) -> String {
    value.and_then(format_system_time_short).unwrap_or_default()
}

fn track_duration_text(duration_seconds: u64) -> String {
    let hours = duration_seconds / 3_600;
    let minutes = duration_seconds % 3_600 / 60;
    let seconds = duration_seconds % 60;

    if hours > 0 {
        format!("{hours}:{minutes:02}:{seconds:02}")
    } else {
        format!("{minutes}:{seconds:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn table_columns_match_the_product_contract() {
        let titles: Vec<&str> = TRACK_TABLE_COLUMNS
            .iter()
            .map(|column| column.title())
            .collect();

        assert_eq!(
            titles,
            vec![
                "Track Name",
                "Artist",
                "Album",
                "Genre",
                "Lyrics",
                "Year",
                "BPM",
                "Key",
                "Bitrate",
                "Type",
                "Duration",
                "Rating",
                "Plays",
                "Skips",
                "Last Played",
                "Last Skipped",
                "Date Added",
                "Track #",
            ]
        );
    }

    #[test]
    fn table_columns_have_stable_action_names() {
        let action_names: Vec<&str> = TRACK_TABLE_COLUMNS
            .iter()
            .map(|column| column.action_name())
            .collect();

        assert_eq!(
            action_names,
            vec![
                "track_name",
                "artist",
                "album",
                "genre",
                "lyrics",
                "year",
                "bpm",
                "music_key",
                "bitrate",
                "file_type",
                "duration",
                "rating",
                "plays",
                "skips",
                "last_played",
                "last_skipped",
                "date_added",
                "track_number",
            ]
        );
    }

    #[test]
    fn track_duration_text_uses_minutes_until_an_hour() {
        assert_eq!(track_duration_text(244), "4:04");
    }

    #[test]
    fn track_duration_text_uses_hours_when_needed() {
        assert_eq!(track_duration_text(3_904), "1:05:04");
    }

    #[test]
    fn lyrics_column_is_hidden_by_default_and_uses_yes_no_text() {
        assert!(!TrackTableColumn::Lyrics.default_visible());
        assert_eq!(yes_no_text(true), "Yes");
        assert_eq!(yes_no_text(false), "No");
    }
}
