// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for the tracks table.

use super::*;
use sustain_domain::FieldChange;

pub(super) fn save_track(connection: &Connection, track: &Track) -> StoreResult<()> {
    execute_full_track(connection, SAVE_TRACK_SQL.as_str(), track)
}

fn execute_full_track(connection: &Connection, sql: &str, track: &Track) -> StoreResult<()> {
    let metadata = &track.metadata;
    let statistics = &track.statistics;
    let relative_path = relative_path_bytes(&track.location.relative_path);
    connection
        .execute(
            sql,
            params![
                track.id.get(),
                relative_path,
                metadata.title.as_deref(),
                metadata.artist.as_deref(),
                metadata.album.as_deref(),
                metadata.album_artist.as_deref(),
                metadata.composer.as_deref(),
                metadata.genre.as_deref(),
                metadata.track_number.map(i64::from),
                metadata.disc_number.map(i64::from),
                metadata.year.map(i64::from),
                metadata.duration.map(duration_to_seconds),
                metadata.bitrate_kbps.map(i64::from),
                i64::from(track.rating.stars()),
                statistics.play_count as i64,
                statistics.skip_count as i64,
                statistics.last_played_at.and_then(system_time_to_unix),
                statistics.last_skipped_at.and_then(system_time_to_unix),
                statistics.date_added_at.and_then(system_time_to_unix),
                track.location.is_missing(),
                metadata.grouping.as_deref(),
                metadata.track_total.map(i64::from),
                metadata.disc_total.map(i64::from),
                metadata.compilation,
                metadata.bpm.map(i64::from),
                metadata.key.as_deref(),
                metadata.comments.as_deref(),
                metadata.sample_rate_hz.map(i64::from),
                metadata.channels.map(i64::from),
                metadata.lyrics.as_deref(),
                track.file_size_bytes.map(|size| size as i64),
                track.has_embedded_artwork.map(i64::from),
                metadata.title_sort.as_deref(),
                metadata.artist_sort.as_deref(),
                metadata.album_sort.as_deref(),
                metadata.album_artist_sort.as_deref(),
                metadata.composer_sort.as_deref(),
                track.file_modified_at.and_then(system_time_to_unix),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn save_tracks(connection: &mut Connection, tracks: &[Track]) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    for track in tracks {
        save_track(&transaction, track)?;
    }
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn reconcile_scanned_tracks(
    connection: &mut Connection,
    tracks: &[Track],
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    for track in tracks {
        execute_full_track(&transaction, RECONCILE_SCANNED_TRACK_SQL.as_str(), track)?;
    }
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn update_track_location(
    connection: &Connection,
    track_id: TrackId,
    location: &TrackLocation,
) -> StoreResult<()> {
    connection
        .execute(
            "UPDATE tracks SET relative_path = ?1, is_missing = ?2 WHERE id = ?3",
            params![
                relative_path_bytes(&location.relative_path),
                location.is_missing(),
                track_id.get(),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn update_track_availability_if_path_matches(
    connection: &Connection,
    track_id: TrackId,
    location: &TrackLocation,
) -> StoreResult<bool> {
    connection
        .execute(
            "UPDATE tracks SET is_missing = ?1 WHERE id = ?2 AND relative_path = ?3",
            params![
                location.is_missing(),
                track_id.get(),
                relative_path_bytes(&location.relative_path),
            ],
        )
        .map(|changed| changed > 0)
        .map_err(StoreError::from)
}

pub(super) fn update_track_locations(
    connection: &mut Connection,
    updates: &[(TrackId, TrackLocation)],
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    for (track_id, location) in updates {
        update_track_location(&transaction, *track_id, location)?;
    }
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn relocate_track(
    connection: &Connection,
    track_id: TrackId,
    location: &TrackLocation,
    file_size_bytes: u64,
) -> StoreResult<()> {
    connection
        .execute(
            r#"
            UPDATE tracks SET
                relative_path = ?1,
                is_missing = ?2,
                file_size_bytes = ?3,
                has_embedded_artwork = NULL,
                file_modified_at_unix = NULL
            WHERE id = ?4
            "#,
            params![
                relative_path_bytes(&location.relative_path),
                location.is_missing(),
                i64::try_from(file_size_bytes)
                    .map_err(|_| StoreError::Database("file size exceeds SQLite INTEGER".into()))?,
                track_id.get(),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn replace_audio(
    connection: &mut Connection,
    track_id: TrackId,
    location: &TrackLocation,
    audio_properties: TrackAudioProperties,
    file_size_bytes: u64,
    has_embedded_artwork: bool,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    let changed = transaction
        .execute(
            r#"
            UPDATE tracks SET
                relative_path = ?1,
                is_missing = ?2,
                duration_seconds = ?3,
                bitrate_kbps = ?4,
                sample_rate_hz = ?5,
                channels = ?6,
                file_size_bytes = ?7,
                has_embedded_artwork = ?8,
                file_modified_at_unix = NULL
            WHERE id = ?9
            "#,
            params![
                relative_path_bytes(&location.relative_path),
                location.is_missing(),
                audio_properties.duration.map(duration_to_seconds),
                audio_properties.bitrate_kbps.map(i64::from),
                audio_properties.sample_rate_hz.map(i64::from),
                audio_properties.channels.map(i64::from),
                i64::try_from(file_size_bytes)
                    .map_err(|_| StoreError::Database("file size exceeds SQLite INTEGER".into()))?,
                has_embedded_artwork,
                track_id.get(),
            ],
        )
        .map_err(StoreError::from)?;
    if changed == 0 {
        return Err(StoreError::Database(format!(
            "track {} does not exist",
            track_id.get()
        )));
    }
    for sql in [
        "DELETE FROM track_analysis WHERE track_id = ?1",
        "DELETE FROM track_acoustics WHERE track_id = ?1",
        "DELETE FROM track_waveform WHERE track_id = ?1",
        "DELETE FROM source_fingerprint_cache WHERE track_id = ?1",
    ] {
        transaction
            .execute(sql, params![track_id.get()])
            .map_err(StoreError::from)?;
    }
    transaction
        .execute("DELETE FROM smart_shuffle_index", [])
        .map_err(StoreError::from)?;
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn update_track_rating(
    connection: &Connection,
    track_id: TrackId,
    rating: Rating,
) -> StoreResult<()> {
    connection
        .execute(
            "UPDATE tracks SET rating = ?1 WHERE id = ?2",
            params![i64::from(rating.stars()), track_id.get()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn update_track_statistics(
    connection: &Connection,
    track_id: TrackId,
    statistics: &PlayStatistics,
) -> StoreResult<()> {
    connection
        .execute(
            r#"
            UPDATE tracks SET
                play_count = ?1,
                skip_count = ?2,
                last_played_at_unix = ?3,
                last_skipped_at_unix = ?4,
                date_added_at_unix = ?5
            WHERE id = ?6
            "#,
            params![
                statistics.play_count as i64,
                statistics.skip_count as i64,
                statistics.last_played_at.and_then(system_time_to_unix),
                statistics.last_skipped_at.and_then(system_time_to_unix),
                statistics.date_added_at.and_then(system_time_to_unix),
                track_id.get(),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn apply_track_metadata_change(
    connection: &Connection,
    track_id: TrackId,
    change: &MetadataChange,
) -> StoreResult<()> {
    let (title_action, title) = text_change_parts(&change.title);
    let (artist_action, artist) = text_change_parts(&change.artist);
    let (album_action, album) = text_change_parts(&change.album);
    let (album_artist_action, album_artist) = text_change_parts(&change.album_artist);
    let (composer_action, composer) = text_change_parts(&change.composer);
    let (grouping_action, grouping) = text_change_parts(&change.grouping);
    let (genre_action, genre) = text_change_parts(&change.genre);
    let (track_number_action, track_number) = copied_change_parts(&change.track_number);
    let (track_total_action, track_total) = copied_change_parts(&change.track_total);
    let (disc_number_action, disc_number) = copied_change_parts(&change.disc_number);
    let (disc_total_action, disc_total) = copied_change_parts(&change.disc_total);
    let (year_action, year) = copied_change_parts(&change.year);
    let (compilation_action, compilation) = copied_change_parts(&change.compilation);
    let (bpm_action, bpm) = copied_change_parts(&change.bpm);
    let (key_action, key) = text_change_parts(&change.key);
    let (comments_action, comments) = text_change_parts(&change.comments);
    let (lyrics_action, lyrics) = text_change_parts(&change.lyrics);
    connection
        .execute(
            APPLY_TRACK_METADATA_CHANGE_SQL,
            params![
                track_id.get(),
                title_action,
                title,
                artist_action,
                artist,
                album_action,
                album,
                album_artist_action,
                album_artist,
                composer_action,
                composer,
                grouping_action,
                grouping,
                genre_action,
                genre,
                track_number_action,
                track_number.map(i64::from),
                track_total_action,
                track_total.map(i64::from),
                disc_number_action,
                disc_number.map(i64::from),
                disc_total_action,
                disc_total.map(i64::from),
                year_action,
                year.map(i64::from),
                compilation_action,
                compilation,
                bpm_action,
                bpm.map(i64::from),
                key_action,
                key,
                comments_action,
                comments,
                lyrics_action,
                lyrics,
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn fill_missing_track_metadata(
    connection: &Connection,
    track_id: TrackId,
    change: &MetadataChange,
) -> StoreResult<()> {
    connection
        .execute(
            FILL_MISSING_TRACK_METADATA_SQL,
            params![
                track_id.get(),
                text_fill_value(&change.title),
                text_fill_value(&change.artist),
                text_fill_value(&change.album),
                text_fill_value(&change.album_artist),
                text_fill_value(&change.composer),
                text_fill_value(&change.grouping),
                text_fill_value(&change.genre),
                copied_fill_value(&change.track_number).map(i64::from),
                copied_fill_value(&change.track_total).map(i64::from),
                copied_fill_value(&change.disc_number).map(i64::from),
                copied_fill_value(&change.disc_total).map(i64::from),
                copied_fill_value(&change.year).map(i64::from),
                copied_fill_value(&change.compilation),
                copied_fill_value(&change.bpm).map(i64::from),
                text_fill_value(&change.key),
                text_fill_value(&change.comments),
                text_fill_value(&change.lyrics),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

fn text_change_parts(change: &FieldChange<String>) -> (i64, Option<&str>) {
    match change {
        FieldChange::Unchanged => (0, None),
        FieldChange::Set(value) => (1, Some(value)),
        FieldChange::Clear => (2, None),
    }
}

fn copied_change_parts<T: Copy>(change: &FieldChange<T>) -> (i64, Option<T>) {
    match change {
        FieldChange::Unchanged => (0, None),
        FieldChange::Set(value) => (1, Some(*value)),
        FieldChange::Clear => (2, None),
    }
}

fn text_fill_value(change: &FieldChange<String>) -> Option<&str> {
    match change {
        FieldChange::Set(value) => Some(value),
        FieldChange::Unchanged | FieldChange::Clear => None,
    }
}

fn copied_fill_value<T: Copy>(change: &FieldChange<T>) -> Option<T> {
    match change {
        FieldChange::Set(value) => Some(*value),
        FieldChange::Unchanged | FieldChange::Clear => None,
    }
}

pub(super) fn delete_track(connection: &Connection, track_id: TrackId) -> StoreResult<()> {
    connection
        .execute("DELETE FROM tracks WHERE id = ?1", params![track_id.get()])
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn track(connection: &Connection, track_id: TrackId) -> StoreResult<Option<Track>> {
    let mut statement = connection
        .prepare(SELECT_TRACK_BY_ID_SQL.as_str())
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![track_id.get()])
        .map_err(StoreError::from)?;

    rows.next()
        .map_err(StoreError::from)?
        .map(track_from_row)
        .transpose()
}

pub(super) fn tracks(connection: &Connection) -> StoreResult<Vec<Track>> {
    let mut statement = connection
        .prepare(SELECT_ALL_TRACKS_SQL.as_str())
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut tracks = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        tracks.push(track_from_row(row)?);
    }

    Ok(tracks)
}

pub(super) fn distinct_genres(connection: &Connection) -> StoreResult<Vec<String>> {
    let mut statement = connection
        .prepare(
            "SELECT DISTINCT genre FROM tracks \
                 WHERE genre IS NOT NULL AND TRIM(genre) <> '' \
                 ORDER BY genre",
        )
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut genres = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let value: String = row.get(0).map_err(StoreError::from)?;
        if !value.trim().is_empty() {
            genres.push(value);
        }
    }
    Ok(genres)
}
