// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    num::NonZeroU32,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use rusqlite::{Connection, Row, params};
use sustain_domain::{
    PlayStatistics, PlaylistEntry, SmartPlaylistDateField, SmartPlaylistLimit,
    SmartPlaylistLimitSelection, SmartPlaylistMatchKind, SmartPlaylistNumberField,
    SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistTextField,
    SmartPlaylistTextOperator, TrackLocation, TrackMetadata, TrackRelativePath, WaveformSegment,
};

use crate::{
    PlaylistFolder, PlaylistFolderId, PlaylistId, Rating, SmartPlaylistId, StoreError, StoreResult,
    Track, TrackId, schema::track_column_index as track_column,
};

pub(crate) fn track_from_row(row: &Row<'_>) -> StoreResult<Track> {
    let duration_seconds = optional_i64(row, track_column::DURATION_SECONDS)?;
    let rating_value = row
        .get::<_, i64>(track_column::RATING)
        .map_err(StoreError::from)?;

    Ok(Track {
        id: track_id_from_db(row.get(track_column::ID).map_err(StoreError::from)?)?,
        location: track_location_from_row(row)?,
        metadata: TrackMetadata {
            title: row.get(track_column::TITLE).map_err(StoreError::from)?,
            artist: row.get(track_column::ARTIST).map_err(StoreError::from)?,
            album: row.get(track_column::ALBUM).map_err(StoreError::from)?,
            album_artist: row
                .get(track_column::ALBUM_ARTIST)
                .map_err(StoreError::from)?,
            composer: row.get(track_column::COMPOSER).map_err(StoreError::from)?,
            grouping: row.get(track_column::GROUPING).map_err(StoreError::from)?,
            genre: row.get(track_column::GENRE).map_err(StoreError::from)?,
            track_number: optional_u32(row, track_column::TRACK_NUMBER)?,
            track_total: optional_u32(row, track_column::TRACK_TOTAL)?,
            disc_number: optional_u32(row, track_column::DISC_NUMBER)?,
            disc_total: optional_u32(row, track_column::DISC_TOTAL)?,
            year: optional_i64(row, track_column::YEAR)?.map(|value| value as i32),
            compilation: row
                .get(track_column::COMPILATION)
                .map_err(StoreError::from)?,
            bpm: optional_u32(row, track_column::BPM)?,
            key: row
                .get(track_column::MUSICAL_KEY)
                .map_err(StoreError::from)?,
            comments: row.get(track_column::COMMENTS).map_err(StoreError::from)?,
            duration: duration_seconds.map(seconds_to_duration),
            bitrate_kbps: optional_u32(row, track_column::BITRATE_KBPS)?,
            sample_rate_hz: optional_u32(row, track_column::SAMPLE_RATE_HZ)?,
            channels: optional_u8(row, track_column::CHANNELS)?,
            lyrics: row.get(track_column::LYRICS).map_err(StoreError::from)?,
            title_sort: row
                .get(track_column::TITLE_SORT)
                .map_err(StoreError::from)?,
            artist_sort: row
                .get(track_column::ARTIST_SORT)
                .map_err(StoreError::from)?,
            album_sort: row
                .get(track_column::ALBUM_SORT)
                .map_err(StoreError::from)?,
            album_artist_sort: row
                .get(track_column::ALBUM_ARTIST_SORT)
                .map_err(StoreError::from)?,
            composer_sort: row
                .get(track_column::COMPOSER_SORT)
                .map_err(StoreError::from)?,
        },
        rating: Rating::new(rating_value as u8).unwrap_or_else(Rating::unrated),
        statistics: PlayStatistics {
            play_count: row
                .get::<_, i64>(track_column::PLAY_COUNT)
                .map_err(StoreError::from)? as u64,
            skip_count: row
                .get::<_, i64>(track_column::SKIP_COUNT)
                .map_err(StoreError::from)? as u64,
            last_played_at: optional_i64(row, track_column::LAST_PLAYED_AT_UNIX)?
                .map(unix_to_system_time),
            last_skipped_at: optional_i64(row, track_column::LAST_SKIPPED_AT_UNIX)?
                .map(unix_to_system_time),
            date_added_at: optional_i64(row, track_column::DATE_ADDED_AT_UNIX)?
                .map(unix_to_system_time),
        },
        file_size_bytes: optional_i64(row, track_column::FILE_SIZE_BYTES)?
            .map(|value| value as u64),
        has_embedded_artwork: optional_bool(row, track_column::HAS_EMBEDDED_ARTWORK)?,
        file_modified_at: optional_i64(row, track_column::FILE_MODIFIED_AT_UNIX)?
            .map(unix_to_system_time),
    })
}

fn optional_bool(row: &Row<'_>, index: usize) -> StoreResult<Option<bool>> {
    optional_i64(row, index).map(|value| value.map(|value| value != 0))
}

fn track_location_from_row(row: &Row<'_>) -> StoreResult<TrackLocation> {
    let path_bytes = row
        .get::<_, Vec<u8>>(track_column::RELATIVE_PATH)
        .map_err(StoreError::from)?;
    let is_missing = row
        .get::<_, bool>(track_column::IS_MISSING)
        .map_err(StoreError::from)?;
    let relative_path = relative_path_from_bytes(path_bytes)?;

    if is_missing {
        Ok(TrackLocation::missing(relative_path))
    } else {
        Ok(TrackLocation::available(relative_path))
    }
}

/// The raw `OsStr` bytes of a track's relative path, for the
/// `tracks.relative_path` BLOB column. Linux filenames are arbitrary
/// byte sequences, so the path is stored and compared by exact bytes —
/// never coerced through `to_string_lossy`, which would replace invalid
/// UTF-8 with U+FFFD and could both lose the real file and collapse two
/// distinct paths onto one UNIQUE row.
pub(crate) fn relative_path_bytes(relative_path: &TrackRelativePath) -> &[u8] {
    use std::os::unix::ffi::OsStrExt;

    relative_path.as_path().as_os_str().as_bytes()
}

/// Rebuild a [`TrackRelativePath`] from the stored `OsStr` bytes,
/// byte-for-byte. Fails only when the bytes do not form a valid relative
/// path (absolute, parent-traversal, or empty) — which would mean the row
/// was written by something other than [`relative_path_bytes`].
pub(crate) fn relative_path_from_bytes(bytes: Vec<u8>) -> StoreResult<TrackRelativePath> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, path::PathBuf};

    let path = PathBuf::from(OsString::from_vec(bytes));
    TrackRelativePath::new(path.clone())
        .ok_or_else(|| StoreError::InvalidStoredPath(path.to_string_lossy().into_owned()))
}

pub(crate) fn playlist_entries(
    connection: &Connection,
    playlist_id: PlaylistId,
) -> StoreResult<Vec<PlaylistEntry>> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT playlist_id, track_id, position
            FROM playlist_entries
            WHERE playlist_id = ?1
            ORDER BY position
            "#,
        )
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![playlist_id.get()])
        .map_err(StoreError::from)?;
    let mut entries = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        entries.push(PlaylistEntry {
            playlist_id: playlist_id_from_db(row.get(0).map_err(StoreError::from)?)?,
            track_id: track_id_from_db(row.get(1).map_err(StoreError::from)?)?,
            position: row.get::<_, i64>(2).map_err(StoreError::from)? as u32,
        });
    }

    Ok(entries)
}

pub(crate) fn optional_i64(row: &Row<'_>, index: usize) -> StoreResult<Option<i64>> {
    row.get(index).map_err(StoreError::from)
}

fn optional_u32(row: &Row<'_>, index: usize) -> StoreResult<Option<u32>> {
    optional_i64(row, index).map(|value| value.map(|value| value as u32))
}

fn optional_u8(row: &Row<'_>, index: usize) -> StoreResult<Option<u8>> {
    optional_i64(row, index)
        .map(|value| value.map(|value| value.clamp(0, i64::from(u8::MAX)) as u8))
}

pub(crate) fn optional_string(row: &Row<'_>, index: usize) -> StoreResult<Option<String>> {
    row.get(index).map_err(StoreError::from)
}

pub(crate) fn u32_from_row(row: &Row<'_>, index: usize) -> StoreResult<u32> {
    Ok(row.get::<_, i64>(index).map_err(StoreError::from)?.max(0) as u32)
}

fn track_id_from_db(value: i64) -> StoreResult<TrackId> {
    TrackId::new(value).ok_or(StoreError::InvalidStoredId(value))
}

pub(crate) fn playlist_id_from_db(value: i64) -> StoreResult<PlaylistId> {
    PlaylistId::new(value).ok_or(StoreError::InvalidStoredId(value))
}

fn playlist_folder_id_from_db(value: i64) -> StoreResult<PlaylistFolderId> {
    PlaylistFolderId::new(value).ok_or(StoreError::InvalidStoredId(value))
}

pub(crate) fn smart_playlist_id_from_db(value: i64) -> StoreResult<SmartPlaylistId> {
    SmartPlaylistId::new(value).ok_or(StoreError::InvalidStoredId(value))
}

pub(crate) fn optional_playlist_folder_id_from_row(
    row: &Row<'_>,
    index: usize,
) -> StoreResult<Option<PlaylistFolderId>> {
    optional_i64(row, index)?
        .map(playlist_folder_id_from_db)
        .transpose()
}

pub(crate) fn playlist_folder_from_row(row: &Row<'_>) -> StoreResult<PlaylistFolder> {
    Ok(PlaylistFolder {
        id: playlist_folder_id_from_db(row.get(0).map_err(StoreError::from)?)?,
        name: row.get(1).map_err(StoreError::from)?,
        parent_folder_id: optional_playlist_folder_id_from_row(row, 2)?,
        position: u32_from_row(row, 3)?,
    })
}

pub(crate) fn duration_to_seconds(duration: Duration) -> i64 {
    duration.as_secs() as i64
}

fn seconds_to_duration(seconds: i64) -> Duration {
    Duration::from_secs(seconds.max(0) as u64)
}

pub(crate) fn system_time_to_unix(system_time: SystemTime) -> Option<i64> {
    system_time
        .duration_since(UNIX_EPOCH)
        .ok()
        .map(|duration| duration.as_secs() as i64)
}

fn unix_to_system_time(seconds: i64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(seconds.max(0) as u64)
}

pub(crate) fn match_kind_name(kind: SmartPlaylistMatchKind) -> &'static str {
    match kind {
        SmartPlaylistMatchKind::All => "All",
        SmartPlaylistMatchKind::Any => "Any",
    }
}

pub(crate) fn match_kind_from_name(name: &str) -> StoreResult<SmartPlaylistMatchKind> {
    match name {
        "All" => Ok(SmartPlaylistMatchKind::All),
        "Any" => Ok(SmartPlaylistMatchKind::Any),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

pub(crate) fn limit_selection_name(selection: SmartPlaylistLimitSelection) -> &'static str {
    match selection {
        SmartPlaylistLimitSelection::Random => "Random",
        SmartPlaylistLimitSelection::AlbumAscending => "AlbumAscending",
        SmartPlaylistLimitSelection::ArtistAscending => "ArtistAscending",
        SmartPlaylistLimitSelection::GenreAscending => "GenreAscending",
        SmartPlaylistLimitSelection::TitleAscending => "TitleAscending",
        SmartPlaylistLimitSelection::HighestRating => "HighestRating",
        SmartPlaylistLimitSelection::LowestRating => "LowestRating",
        SmartPlaylistLimitSelection::MostRecentlyPlayed => "MostRecentlyPlayed",
        SmartPlaylistLimitSelection::LeastRecentlyPlayed => "LeastRecentlyPlayed",
        SmartPlaylistLimitSelection::MostOftenPlayed => "MostOftenPlayed",
        SmartPlaylistLimitSelection::LeastOftenPlayed => "LeastOftenPlayed",
        SmartPlaylistLimitSelection::MostRecentlyAdded => "MostRecentlyAdded",
        SmartPlaylistLimitSelection::LeastRecentlyAdded => "LeastRecentlyAdded",
    }
}

fn limit_selection_from_name(name: &str) -> StoreResult<SmartPlaylistLimitSelection> {
    match name {
        "Random" => Ok(SmartPlaylistLimitSelection::Random),
        "AlbumAscending" => Ok(SmartPlaylistLimitSelection::AlbumAscending),
        "ArtistAscending" => Ok(SmartPlaylistLimitSelection::ArtistAscending),
        "GenreAscending" => Ok(SmartPlaylistLimitSelection::GenreAscending),
        "TitleAscending" => Ok(SmartPlaylistLimitSelection::TitleAscending),
        "HighestRating" => Ok(SmartPlaylistLimitSelection::HighestRating),
        "LowestRating" => Ok(SmartPlaylistLimitSelection::LowestRating),
        "MostRecentlyPlayed" => Ok(SmartPlaylistLimitSelection::MostRecentlyPlayed),
        "LeastRecentlyPlayed" => Ok(SmartPlaylistLimitSelection::LeastRecentlyPlayed),
        "MostOftenPlayed" => Ok(SmartPlaylistLimitSelection::MostOftenPlayed),
        "LeastOftenPlayed" => Ok(SmartPlaylistLimitSelection::LeastOftenPlayed),
        "MostRecentlyAdded" => Ok(SmartPlaylistLimitSelection::MostRecentlyAdded),
        "LeastRecentlyAdded" => Ok(SmartPlaylistLimitSelection::LeastRecentlyAdded),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

pub(crate) fn build_limit(
    count: Option<i64>,
    selection_name: Option<&str>,
) -> StoreResult<Option<SmartPlaylistLimit>> {
    match (count, selection_name) {
        (Some(count), Some(name)) => {
            let count = u32::try_from(count)
                .ok()
                .and_then(NonZeroU32::new)
                .ok_or_else(|| StoreError::InvalidStoredEnum(format!("limit_count={count}")))?;
            let selection = limit_selection_from_name(name)?;
            Ok(Some(SmartPlaylistLimit { count, selection }))
        }
        (None, None) => Ok(None),
        _ => Err(StoreError::InvalidStoredEnum(
            "limit_count and limit_selection must both be set or both be NULL".to_owned(),
        )),
    }
}

fn text_field_name(field: SmartPlaylistTextField) -> &'static str {
    match field {
        SmartPlaylistTextField::Title => "Title",
        SmartPlaylistTextField::Artist => "Artist",
        SmartPlaylistTextField::Album => "Album",
        SmartPlaylistTextField::AlbumArtist => "AlbumArtist",
        SmartPlaylistTextField::Composer => "Composer",
        SmartPlaylistTextField::Genre => "Genre",
        SmartPlaylistTextField::FileName => "FileName",
        SmartPlaylistTextField::MusicalKey => "MusicalKey",
    }
}

fn text_field_from_name(name: &str) -> StoreResult<SmartPlaylistTextField> {
    match name {
        "Title" => Ok(SmartPlaylistTextField::Title),
        "Artist" => Ok(SmartPlaylistTextField::Artist),
        "Album" => Ok(SmartPlaylistTextField::Album),
        "AlbumArtist" => Ok(SmartPlaylistTextField::AlbumArtist),
        "Composer" => Ok(SmartPlaylistTextField::Composer),
        "Genre" => Ok(SmartPlaylistTextField::Genre),
        "FileName" => Ok(SmartPlaylistTextField::FileName),
        "MusicalKey" => Ok(SmartPlaylistTextField::MusicalKey),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

fn text_operator_name(operator: SmartPlaylistTextOperator) -> &'static str {
    match operator {
        SmartPlaylistTextOperator::Contains => "Contains",
        SmartPlaylistTextOperator::DoesNotContain => "DoesNotContain",
        SmartPlaylistTextOperator::Is => "Is",
        SmartPlaylistTextOperator::IsNot => "IsNot",
        SmartPlaylistTextOperator::StartsWith => "StartsWith",
        SmartPlaylistTextOperator::EndsWith => "EndsWith",
    }
}

fn text_operator_from_name(name: &str) -> StoreResult<SmartPlaylistTextOperator> {
    match name {
        "Contains" => Ok(SmartPlaylistTextOperator::Contains),
        "DoesNotContain" => Ok(SmartPlaylistTextOperator::DoesNotContain),
        "Is" => Ok(SmartPlaylistTextOperator::Is),
        "IsNot" => Ok(SmartPlaylistTextOperator::IsNot),
        "StartsWith" => Ok(SmartPlaylistTextOperator::StartsWith),
        "EndsWith" => Ok(SmartPlaylistTextOperator::EndsWith),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

fn number_field_name(field: SmartPlaylistNumberField) -> &'static str {
    match field {
        SmartPlaylistNumberField::PlayCount => "PlayCount",
        SmartPlaylistNumberField::SkipCount => "SkipCount",
        SmartPlaylistNumberField::TrackNumber => "TrackNumber",
        SmartPlaylistNumberField::DiscNumber => "DiscNumber",
        SmartPlaylistNumberField::Year => "Year",
        SmartPlaylistNumberField::DurationSeconds => "DurationSeconds",
        SmartPlaylistNumberField::BitrateKbps => "BitrateKbps",
        SmartPlaylistNumberField::Bpm => "Bpm",
    }
}

fn number_field_from_name(name: &str) -> StoreResult<SmartPlaylistNumberField> {
    match name {
        "PlayCount" => Ok(SmartPlaylistNumberField::PlayCount),
        "SkipCount" => Ok(SmartPlaylistNumberField::SkipCount),
        "TrackNumber" => Ok(SmartPlaylistNumberField::TrackNumber),
        "DiscNumber" => Ok(SmartPlaylistNumberField::DiscNumber),
        "Year" => Ok(SmartPlaylistNumberField::Year),
        "DurationSeconds" => Ok(SmartPlaylistNumberField::DurationSeconds),
        "BitrateKbps" => Ok(SmartPlaylistNumberField::BitrateKbps),
        "Bpm" => Ok(SmartPlaylistNumberField::Bpm),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

fn number_operator_name(operator: SmartPlaylistNumberOperator) -> &'static str {
    match operator {
        SmartPlaylistNumberOperator::Equal => "Equal",
        SmartPlaylistNumberOperator::NotEqual => "NotEqual",
        SmartPlaylistNumberOperator::GreaterThan => "GreaterThan",
        SmartPlaylistNumberOperator::GreaterThanOrEqual => "GreaterThanOrEqual",
        SmartPlaylistNumberOperator::LessThan => "LessThan",
        SmartPlaylistNumberOperator::LessThanOrEqual => "LessThanOrEqual",
    }
}

fn number_operator_from_name(name: &str) -> StoreResult<SmartPlaylistNumberOperator> {
    match name {
        "Equal" => Ok(SmartPlaylistNumberOperator::Equal),
        "NotEqual" => Ok(SmartPlaylistNumberOperator::NotEqual),
        "GreaterThan" => Ok(SmartPlaylistNumberOperator::GreaterThan),
        "GreaterThanOrEqual" => Ok(SmartPlaylistNumberOperator::GreaterThanOrEqual),
        "LessThan" => Ok(SmartPlaylistNumberOperator::LessThan),
        "LessThanOrEqual" => Ok(SmartPlaylistNumberOperator::LessThanOrEqual),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

fn date_field_name(field: SmartPlaylistDateField) -> &'static str {
    match field {
        SmartPlaylistDateField::DateAdded => "DateAdded",
        SmartPlaylistDateField::LastPlayed => "LastPlayed",
        SmartPlaylistDateField::LastSkipped => "LastSkipped",
    }
}

fn date_field_from_name(name: &str) -> StoreResult<SmartPlaylistDateField> {
    match name {
        "DateAdded" => Ok(SmartPlaylistDateField::DateAdded),
        "LastPlayed" => Ok(SmartPlaylistDateField::LastPlayed),
        "LastSkipped" => Ok(SmartPlaylistDateField::LastSkipped),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

#[derive(Default)]
pub(crate) struct RuleColumns {
    pub(crate) kind: &'static str,
    pub(crate) field: Option<&'static str>,
    pub(crate) text_operator: Option<&'static str>,
    pub(crate) text_value: Option<String>,
    pub(crate) number_operator: Option<&'static str>,
    pub(crate) number_value: Option<i64>,
    pub(crate) rating_stars: Option<i64>,
    pub(crate) date_unix: Option<i64>,
    pub(crate) days_value: Option<i64>,
}

pub(crate) fn rule_to_columns(rule: &SmartPlaylistRule) -> RuleColumns {
    match rule {
        SmartPlaylistRule::Text {
            field,
            operator,
            value,
        } => RuleColumns {
            kind: "Text",
            field: Some(text_field_name(*field)),
            text_operator: Some(text_operator_name(*operator)),
            text_value: Some(value.clone()),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::TextIsEmpty { field } => RuleColumns {
            kind: "TextIsEmpty",
            field: Some(text_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::TextIsPresent { field } => RuleColumns {
            kind: "TextIsPresent",
            field: Some(text_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::Number {
            field,
            operator,
            value,
        } => RuleColumns {
            kind: "Number",
            field: Some(number_field_name(*field)),
            number_operator: Some(number_operator_name(*operator)),
            number_value: Some(*value),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::NumberIsEmpty { field } => RuleColumns {
            kind: "NumberIsEmpty",
            field: Some(number_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::NumberIsPresent { field } => RuleColumns {
            kind: "NumberIsPresent",
            field: Some(number_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::Rating { operator, value } => RuleColumns {
            kind: "Rating",
            number_operator: Some(number_operator_name(*operator)),
            rating_stars: Some(i64::from(value.stars())),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateBefore { field, date } => RuleColumns {
            kind: "DateBefore",
            field: Some(date_field_name(*field)),
            date_unix: system_time_to_unix(*date),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateAfter { field, date } => RuleColumns {
            kind: "DateAfter",
            field: Some(date_field_name(*field)),
            date_unix: system_time_to_unix(*date),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateInLast { field, days } => RuleColumns {
            kind: "DateInLast",
            field: Some(date_field_name(*field)),
            days_value: Some(i64::from(days.get())),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateNotInLast { field, days } => RuleColumns {
            kind: "DateNotInLast",
            field: Some(date_field_name(*field)),
            days_value: Some(i64::from(days.get())),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateIsEmpty { field } => RuleColumns {
            kind: "DateIsEmpty",
            field: Some(date_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::DateIsPresent { field } => RuleColumns {
            kind: "DateIsPresent",
            field: Some(date_field_name(*field)),
            ..RuleColumns::default()
        },
        SmartPlaylistRule::FileIsMissing => RuleColumns {
            kind: "FileIsMissing",
            ..RuleColumns::default()
        },
        SmartPlaylistRule::FileIsPresent => RuleColumns {
            kind: "FileIsPresent",
            ..RuleColumns::default()
        },
    }
}

pub(crate) fn load_smart_playlist_rules(
    connection: &Connection,
    smart_playlist_id: SmartPlaylistId,
) -> StoreResult<Vec<SmartPlaylistRule>> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT kind, field, text_operator, text_value, number_operator, number_value,
                   rating_stars, date_unix, days_value
            FROM smart_playlist_rules
            WHERE smart_playlist_id = ?1
            ORDER BY position
            "#,
        )
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![smart_playlist_id.get()])
        .map_err(StoreError::from)?;
    let mut rules = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        rules.push(rule_from_row(row)?);
    }

    Ok(rules)
}

fn rule_from_row(row: &Row<'_>) -> StoreResult<SmartPlaylistRule> {
    let kind = row.get::<_, String>(0).map_err(StoreError::from)?;
    let field_name = optional_string(row, 1)?;
    let text_operator_name_value = optional_string(row, 2)?;
    let text_value = optional_string(row, 3)?;
    let number_operator_name_value = optional_string(row, 4)?;
    let number_value = optional_i64(row, 5)?;
    let rating_stars = optional_i64(row, 6)?;
    let date_unix = optional_i64(row, 7)?;
    let days_value = optional_i64(row, 8)?;

    let rule_field_name = || -> StoreResult<&str> {
        field_name
            .as_deref()
            .ok_or_else(|| StoreError::InvalidStoredEnum(format!("{kind} rule missing field")))
    };
    let require_text_operator = || -> StoreResult<SmartPlaylistTextOperator> {
        text_operator_name_value
            .as_deref()
            .ok_or_else(|| {
                StoreError::InvalidStoredEnum(format!("{kind} rule missing text_operator"))
            })
            .and_then(text_operator_from_name)
    };
    let require_text_value = || -> StoreResult<String> {
        text_value
            .clone()
            .ok_or_else(|| StoreError::InvalidStoredEnum(format!("{kind} rule missing text_value")))
    };
    let require_number_operator = || -> StoreResult<SmartPlaylistNumberOperator> {
        number_operator_name_value
            .as_deref()
            .ok_or_else(|| {
                StoreError::InvalidStoredEnum(format!("{kind} rule missing number_operator"))
            })
            .and_then(number_operator_from_name)
    };
    let require_number_value = || -> StoreResult<i64> {
        number_value.ok_or_else(|| {
            StoreError::InvalidStoredEnum(format!("{kind} rule missing number_value"))
        })
    };
    let require_date_unix = || -> StoreResult<SystemTime> {
        date_unix
            .ok_or_else(|| StoreError::InvalidStoredEnum(format!("{kind} rule missing date_unix")))
            .map(unix_to_system_time)
    };
    let require_days_value = || -> StoreResult<NonZeroU32> {
        days_value
            .ok_or_else(|| StoreError::InvalidStoredEnum(format!("{kind} rule missing days_value")))
            .and_then(|days| {
                u32::try_from(days)
                    .ok()
                    .and_then(NonZeroU32::new)
                    .ok_or_else(|| {
                        StoreError::InvalidStoredEnum(format!("{kind} rule days={days}"))
                    })
            })
    };
    let require_rating = || -> StoreResult<Rating> {
        let stars = rating_stars
            .ok_or_else(|| StoreError::InvalidStoredEnum("Rating rule missing stars".to_owned()))?;
        let stars = u8::try_from(stars)
            .ok()
            .and_then(Rating::new)
            .ok_or_else(|| StoreError::InvalidStoredEnum(format!("Rating rule stars={stars}")))?;
        Ok(stars)
    };

    match kind.as_str() {
        "Text" => Ok(SmartPlaylistRule::Text {
            field: text_field_from_name(rule_field_name()?)?,
            operator: require_text_operator()?,
            value: require_text_value()?,
        }),
        "TextIsEmpty" => Ok(SmartPlaylistRule::TextIsEmpty {
            field: text_field_from_name(rule_field_name()?)?,
        }),
        "TextIsPresent" => Ok(SmartPlaylistRule::TextIsPresent {
            field: text_field_from_name(rule_field_name()?)?,
        }),
        "Number" => Ok(SmartPlaylistRule::Number {
            field: number_field_from_name(rule_field_name()?)?,
            operator: require_number_operator()?,
            value: require_number_value()?,
        }),
        "NumberIsEmpty" => Ok(SmartPlaylistRule::NumberIsEmpty {
            field: number_field_from_name(rule_field_name()?)?,
        }),
        "NumberIsPresent" => Ok(SmartPlaylistRule::NumberIsPresent {
            field: number_field_from_name(rule_field_name()?)?,
        }),
        "Rating" => Ok(SmartPlaylistRule::Rating {
            operator: require_number_operator()?,
            value: require_rating()?,
        }),
        "DateBefore" => Ok(SmartPlaylistRule::DateBefore {
            field: date_field_from_name(rule_field_name()?)?,
            date: require_date_unix()?,
        }),
        "DateAfter" => Ok(SmartPlaylistRule::DateAfter {
            field: date_field_from_name(rule_field_name()?)?,
            date: require_date_unix()?,
        }),
        "DateInLast" => Ok(SmartPlaylistRule::DateInLast {
            field: date_field_from_name(rule_field_name()?)?,
            days: require_days_value()?,
        }),
        "DateNotInLast" => Ok(SmartPlaylistRule::DateNotInLast {
            field: date_field_from_name(rule_field_name()?)?,
            days: require_days_value()?,
        }),
        "DateIsEmpty" => Ok(SmartPlaylistRule::DateIsEmpty {
            field: date_field_from_name(rule_field_name()?)?,
        }),
        "DateIsPresent" => Ok(SmartPlaylistRule::DateIsPresent {
            field: date_field_from_name(rule_field_name()?)?,
        }),
        "FileIsMissing" => Ok(SmartPlaylistRule::FileIsMissing),
        "FileIsPresent" => Ok(SmartPlaylistRule::FileIsPresent),
        other => Err(StoreError::InvalidStoredEnum(other.to_owned())),
    }
}

/// Serialize a sequence of [`WaveformSegment`]s to the on-disk BLOB
/// form: each segment becomes exactly four bytes
/// (amplitude, low, mid, high). No framing — the segment count is
/// recovered as `blob.len() / 4` on read.
pub(crate) fn waveform_segments_to_blob(segments: &[WaveformSegment]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(segments.len() * 4);
    for segment in segments {
        bytes.push(segment.amplitude);
        bytes.push(segment.low_band);
        bytes.push(segment.mid_band);
        bytes.push(segment.high_band);
    }
    bytes
}

/// Inverse of [`waveform_segments_to_blob`]. Trailing bytes that do
/// not form a complete 4-byte segment are silently dropped — the
/// writer only ever produces multiples of four, so anything else is
/// either a corruption to ignore or schema drift to investigate.
pub(crate) fn blob_to_waveform_segments(blob: &[u8]) -> Vec<WaveformSegment> {
    blob.chunks_exact(4)
        .map(|chunk| WaveformSegment {
            amplitude: chunk[0],
            low_band: chunk[1],
            mid_band: chunk[2],
            high_band: chunk[3],
        })
        .collect()
}
