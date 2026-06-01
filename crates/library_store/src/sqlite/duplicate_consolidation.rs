// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Atomic SQLite half of user-confirmed duplicate consolidation.

use super::*;

pub(super) fn commit(
    connection: &mut Connection,
    plan: &DuplicateConsolidationPlan,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    update_survivor(&transaction, &plan.survivor)?;
    for playlist in &plan.rewritten_playlists {
        playlists::replace_playlist_entries(&transaction, playlist)?;
    }
    source_fingerprints::invalidate_source_fingerprint(&transaction, plan.survivor.id)?;
    transaction
        .execute(
            "DELETE FROM tag_mirror_outbox WHERE track_id = ?1",
            params![plan.survivor.id.get()],
        )
        .map_err(StoreError::from)?;
    for removed_track_id in &plan.removed_track_ids {
        tracks::delete_track(&transaction, *removed_track_id)?;
    }
    transaction.commit().map_err(StoreError::from)
}

fn update_survivor(connection: &Connection, track: &Track) -> StoreResult<()> {
    let metadata = &track.metadata;
    let statistics = &track.statistics;
    let changed = connection
        .execute(
            r#"
            UPDATE tracks SET
                title = ?1,
                artist = ?2,
                album = ?3,
                album_artist = ?4,
                composer = ?5,
                genre = ?6,
                track_number = ?7,
                disc_number = ?8,
                year = ?9,
                rating = ?10,
                play_count = ?11,
                skip_count = ?12,
                last_played_at_unix = ?13,
                last_skipped_at_unix = ?14,
                date_added_at_unix = ?15,
                is_missing = 0,
                grouping = ?16,
                track_total = ?17,
                disc_total = ?18,
                compilation = ?19,
                bpm = ?20,
                musical_key = ?21,
                comments = ?22,
                lyrics = ?23,
                file_size_bytes = ?24,
                has_embedded_artwork = ?25,
                file_modified_at_unix = NULL
            WHERE id = ?26
            "#,
            params![
                metadata.title.as_deref(),
                metadata.artist.as_deref(),
                metadata.album.as_deref(),
                metadata.album_artist.as_deref(),
                metadata.composer.as_deref(),
                metadata.genre.as_deref(),
                metadata.track_number.map(i64::from),
                metadata.disc_number.map(i64::from),
                metadata.year.map(i64::from),
                i64::from(track.rating.stars()),
                sqlite_integer(statistics.play_count, "play count")?,
                sqlite_integer(statistics.skip_count, "skip count")?,
                statistics.last_played_at.and_then(system_time_to_unix),
                statistics.last_skipped_at.and_then(system_time_to_unix),
                statistics.date_added_at.and_then(system_time_to_unix),
                metadata.grouping.as_deref(),
                metadata.track_total.map(i64::from),
                metadata.disc_total.map(i64::from),
                metadata.compilation,
                metadata.bpm.map(i64::from),
                metadata.key.as_deref(),
                metadata.comments.as_deref(),
                metadata.lyrics.as_deref(),
                track
                    .file_size_bytes
                    .map(|size| sqlite_integer(size, "file size"))
                    .transpose()?,
                track.has_embedded_artwork,
                track.id.get(),
            ],
        )
        .map_err(StoreError::from)?;
    if changed == 1 {
        Ok(())
    } else {
        Err(StoreError::Database(
            "duplicate consolidation survivor does not exist".to_owned(),
        ))
    }
}

fn sqlite_integer(value: u64, field: &str) -> StoreResult<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Database(format!("{field} exceeds SQLite INTEGER")))
}
