// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for the tracks table.

use super::*;
use sustain_domain::TrackMetadata;

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
                metadata_text_param(&metadata.title),
                metadata_text_param(&metadata.artist),
                metadata_text_param(&metadata.album),
                metadata_text_param(&metadata.album_artist),
                metadata_text_param(&metadata.composer),
                metadata_text_param(&metadata.genre),
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
                metadata_text_param(&metadata.grouping),
                metadata.track_total.map(i64::from),
                metadata.disc_total.map(i64::from),
                metadata.compilation,
                metadata.bpm.map(i64::from),
                metadata_text_param(&metadata.key),
                metadata_text_param(&metadata.comments),
                metadata.sample_rate_hz.map(i64::from),
                metadata.channels.map(i64::from),
                metadata_text_param(&metadata.lyrics),
                track.file_size_bytes.map(|size| size as i64),
                track.has_embedded_artwork.map(i64::from),
                metadata_text_param(&metadata.title_sort),
                metadata_text_param(&metadata.artist_sort),
                metadata_text_param(&metadata.album_sort),
                metadata_text_param(&metadata.album_artist_sort),
                metadata_text_param(&metadata.composer_sort),
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
    let Some(mut track) = track(connection, track_id)? else {
        return Ok(());
    };
    track.metadata.apply_change(change);
    update_track_metadata(connection, track_id, &track.metadata)
}

pub(super) fn fill_missing_track_metadata(
    connection: &Connection,
    track_id: TrackId,
    change: &MetadataChange,
) -> StoreResult<()> {
    let Some(mut track) = track(connection, track_id)? else {
        return Ok(());
    };
    track.metadata.fill_missing_from_change(change);
    update_track_metadata(connection, track_id, &track.metadata)
}

fn update_track_metadata(
    connection: &Connection,
    track_id: TrackId,
    metadata: &TrackMetadata,
) -> StoreResult<()> {
    connection
        .execute(
            r#"
            UPDATE tracks SET
                title = ?2,
                artist = ?3,
                album = ?4,
                album_artist = ?5,
                composer = ?6,
                grouping = ?7,
                genre = ?8,
                track_number = ?9,
                track_total = ?10,
                disc_number = ?11,
                disc_total = ?12,
                year = ?13,
                compilation = ?14,
                bpm = ?15,
                musical_key = ?16,
                comments = ?17,
                lyrics = ?18,
                title_sort = ?19,
                artist_sort = ?20,
                album_sort = ?21,
                album_artist_sort = ?22,
                composer_sort = ?23
            WHERE id = ?1
            "#,
            params![
                track_id.get(),
                metadata_text_param(&metadata.title),
                metadata_text_param(&metadata.artist),
                metadata_text_param(&metadata.album),
                metadata_text_param(&metadata.album_artist),
                metadata_text_param(&metadata.composer),
                metadata_text_param(&metadata.grouping),
                metadata_text_param(&metadata.genre),
                metadata.track_number.map(i64::from),
                metadata.track_total.map(i64::from),
                metadata.disc_number.map(i64::from),
                metadata.disc_total.map(i64::from),
                metadata.year.map(i64::from),
                metadata.compilation,
                metadata.bpm.map(i64::from),
                metadata_text_param(&metadata.key),
                metadata_text_param(&metadata.comments),
                metadata_text_param(&metadata.lyrics),
                metadata_text_param(&metadata.title_sort),
                metadata_text_param(&metadata.artist_sort),
                metadata_text_param(&metadata.album_sort),
                metadata_text_param(&metadata.album_artist_sort),
                metadata_text_param(&metadata.composer_sort),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
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
