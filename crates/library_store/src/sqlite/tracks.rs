// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for the tracks table.

use super::*;

pub(super) fn save_track(connection: &Connection, track: &Track) -> StoreResult<()> {
    let metadata = &track.metadata;
    let statistics = &track.statistics;
    let relative_path = track.location.relative_path.as_path().to_string_lossy();
    connection
        .execute(
            SAVE_TRACK_SQL.as_str(),
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
                track.content_hash.as_ref().map(|hash| hash.as_str()),
                track.file_size_bytes.map(|size| size as i64),
                track.has_embedded_artwork.map(i64::from),
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
