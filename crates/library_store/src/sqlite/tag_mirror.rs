// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite operations for the durable file-tag mirror outbox.

use std::collections::BTreeSet;

use rusqlite::{OptionalExtension, params};

use super::*;
use crate::{PendingTagMirror, StoredTagMirrorArtwork, TagMirrorArtwork, TagMirrorKinds};

pub(super) fn update_track_rating_and_enqueue(
    connection: &mut Connection,
    track_id: TrackId,
    rating: Rating,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    tracks::update_track_rating(&transaction, track_id, rating)?;
    enqueue(
        &transaction,
        track_id,
        TagMirrorKinds {
            rating: true,
            ..TagMirrorKinds::default()
        },
        TagMirrorArtwork::Unchanged,
    )?;
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn apply_track_metadata_change_and_enqueue(
    connection: &mut Connection,
    track_id: TrackId,
    change: &MetadataChange,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    tracks::apply_track_metadata_change(&transaction, track_id, change)?;
    enqueue(
        &transaction,
        track_id,
        TagMirrorKinds {
            metadata: true,
            ..TagMirrorKinds::default()
        },
        TagMirrorArtwork::Unchanged,
    )?;
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn apply_track_metadata_change_and_location_and_enqueue(
    connection: &mut Connection,
    track_id: TrackId,
    change: &MetadataChange,
    location: &TrackLocation,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    tracks::apply_track_metadata_change(&transaction, track_id, change)?;
    tracks::update_track_location(&transaction, track_id, location)?;
    enqueue(
        &transaction,
        track_id,
        TagMirrorKinds {
            metadata: true,
            ..TagMirrorKinds::default()
        },
        TagMirrorArtwork::Unchanged,
    )?;
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn relocate_track_and_enqueue(
    connection: &mut Connection,
    track_id: TrackId,
    location: &TrackLocation,
    file_size_bytes: u64,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    tracks::relocate_track(&transaction, track_id, location, file_size_bytes)?;
    source_fingerprints::invalidate_source_fingerprint(&transaction, track_id)?;
    enqueue(
        &transaction,
        track_id,
        TagMirrorKinds {
            metadata: true,
            rating: true,
            ..TagMirrorKinds::default()
        },
        TagMirrorArtwork::Unchanged,
    )?;
    transaction.commit().map_err(StoreError::from)
}

pub(super) fn fill_missing_track_metadata_and_enqueue(
    connection: &mut Connection,
    track_id: TrackId,
    change: &MetadataChange,
) -> StoreResult<bool> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    let before = tracks::track(&transaction, track_id)?;
    tracks::fill_missing_track_metadata(&transaction, track_id, change)?;
    let after = tracks::track(&transaction, track_id)?;
    let changed = before.map(|track| track.metadata) != after.map(|track| track.metadata);
    if changed {
        enqueue(
            &transaction,
            track_id,
            TagMirrorKinds {
                metadata: true,
                ..TagMirrorKinds::default()
            },
            TagMirrorArtwork::Unchanged,
        )?;
    }
    transaction.commit().map_err(StoreError::from)?;
    Ok(changed)
}

/// The user's set/clear decision. The track is recorded as carrying (or not
/// carrying) embedded artwork in the same transaction.
pub(super) fn enqueue_user_artwork(
    connection: &mut Connection,
    track_id: TrackId,
    artwork: TagMirrorArtwork,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    enqueue_artwork(&transaction, track_id, artwork)?;
    transaction.commit().map_err(StoreError::from)
}

/// Retrieved artwork, retained only for a track that has no embedded
/// artwork and no pending artwork decision.
pub(super) fn enqueue_retrieved_artwork(
    connection: &mut Connection,
    track_id: TrackId,
    artwork: StoredTagMirrorArtwork,
) -> StoreResult<bool> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    let lacks_artwork: bool = transaction
        .query_row(
            r#"
            SELECT COALESCE(t.has_embedded_artwork, 0) = 0
                AND NOT EXISTS (
                    SELECT 1 FROM tag_mirror_outbox o
                    WHERE o.track_id = t.id AND o.artwork_action <> 0
                )
            FROM tracks t
            WHERE t.id = ?1
            "#,
            params![track_id.get()],
            |row| row.get(0),
        )
        .optional()
        .map_err(StoreError::from)?
        .unwrap_or(false);
    if !lacks_artwork {
        return Ok(false);
    }
    enqueue_artwork(&transaction, track_id, TagMirrorArtwork::Set(artwork))?;
    transaction.commit().map_err(StoreError::from)?;
    Ok(true)
}

fn enqueue_artwork(
    connection: &Connection,
    track_id: TrackId,
    artwork: TagMirrorArtwork,
) -> StoreResult<()> {
    let carries_cover = match artwork {
        TagMirrorArtwork::Unchanged => None,
        TagMirrorArtwork::Clear => Some(false),
        TagMirrorArtwork::Set(_) => Some(true),
    };
    enqueue(
        connection,
        track_id,
        TagMirrorKinds {
            artwork: true,
            ..TagMirrorKinds::default()
        },
        artwork,
    )?;
    if let Some(carries_cover) = carries_cover {
        connection
            .execute(
                "UPDATE tracks SET has_embedded_artwork = ?2 WHERE id = ?1",
                params![track_id.get(), carries_cover],
            )
            .map_err(StoreError::from)?;
    }
    Ok(())
}

fn enqueue(
    connection: &Connection,
    track_id: TrackId,
    kinds: TagMirrorKinds,
    artwork: TagMirrorArtwork,
) -> StoreResult<()> {
    let (artwork_action, artwork_digest, artwork_size_bytes) = artwork_parts(&artwork);
    connection
        .execute(
            r#"
            INSERT INTO tag_mirror_outbox (
                track_id, mirror_metadata, mirror_rating, artwork_action,
                artwork_digest, artwork_size_bytes, generation
            )
            SELECT ?1, ?2, ?3, ?4, ?5, ?6, 1
            WHERE EXISTS (SELECT 1 FROM tracks WHERE id = ?1)
            ON CONFLICT(track_id) DO UPDATE SET
                mirror_metadata = MAX(tag_mirror_outbox.mirror_metadata, excluded.mirror_metadata),
                mirror_rating = MAX(tag_mirror_outbox.mirror_rating, excluded.mirror_rating),
                artwork_action = CASE
                    WHEN excluded.artwork_action <> 0 THEN excluded.artwork_action
                    ELSE tag_mirror_outbox.artwork_action
                END,
                artwork_digest = CASE
                    WHEN excluded.artwork_action <> 0 THEN excluded.artwork_digest
                    ELSE tag_mirror_outbox.artwork_digest
                END,
                artwork_size_bytes = CASE
                    WHEN excluded.artwork_action <> 0 THEN excluded.artwork_size_bytes
                    ELSE tag_mirror_outbox.artwork_size_bytes
                END,
                generation = tag_mirror_outbox.generation + 1,
                attempt_count = 0,
                next_attempt_at_unix = 0,
                last_error = NULL
            "#,
            params![
                track_id.get(),
                kinds.metadata,
                kinds.rating,
                artwork_action,
                artwork_digest,
                artwork_size_bytes,
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn due(
    connection: &Connection,
    now_unix: i64,
    limit: usize,
) -> StoreResult<Vec<PendingTagMirror>> {
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut statement = connection
        .prepare(
            r#"
            SELECT track_id, generation, mirror_metadata, mirror_rating,
                   artwork_action, artwork_digest, artwork_size_bytes,
                   attempt_count, next_attempt_at_unix, last_error
            FROM tag_mirror_outbox
            WHERE next_attempt_at_unix <= ?1
            ORDER BY next_attempt_at_unix, track_id
            LIMIT ?2
            "#,
        )
        .map_err(StoreError::from)?;
    let rows = statement
        .query_map(params![now_unix, limit], pending_from_row)
        .map_err(StoreError::from)?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StoreError::from)
}

pub(super) fn next_attempt_at(connection: &Connection) -> StoreResult<Option<i64>> {
    connection
        .query_row(
            "SELECT MIN(next_attempt_at_unix) FROM tag_mirror_outbox",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

/// Settles the intent the writer has just applied to the file. An applied
/// artwork intent also records what the file now carries: a scan between
/// enqueue and completion read the file before the write and may have
/// recorded the old state, which would otherwise outlive the intent.
pub(super) fn complete(
    connection: &mut Connection,
    track_id: TrackId,
    generation: u64,
) -> StoreResult<bool> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    transaction
        .execute(
            r#"
            UPDATE tracks
            SET has_embedded_artwork = (
                SELECT artwork_action = 2 FROM tag_mirror_outbox
                WHERE track_id = ?1 AND generation = ?2
            )
            WHERE id = ?1 AND EXISTS (
                SELECT 1 FROM tag_mirror_outbox
                WHERE track_id = ?1 AND generation = ?2 AND artwork_action <> 0
            )
            "#,
            params![track_id.get(), generation as i64],
        )
        .map_err(StoreError::from)?;
    let completed = transaction
        .execute(
            "DELETE FROM tag_mirror_outbox WHERE track_id = ?1 AND generation = ?2",
            params![track_id.get(), generation as i64],
        )
        .map_err(StoreError::from)?
        > 0;
    transaction.commit().map_err(StoreError::from)?;
    Ok(completed)
}

pub(super) fn record_failure(
    connection: &Connection,
    track_id: TrackId,
    generation: u64,
    next_attempt_at_unix: i64,
    error: &str,
) -> StoreResult<bool> {
    connection
        .execute(
            r#"
            UPDATE tag_mirror_outbox
            SET attempt_count = attempt_count + 1,
                next_attempt_at_unix = ?3,
                last_error = ?4
            WHERE track_id = ?1 AND generation = ?2
            "#,
            params![
                track_id.get(),
                generation as i64,
                next_attempt_at_unix,
                error
            ],
        )
        .map(|changed| changed > 0)
        .map_err(StoreError::from)
}

pub(super) fn referenced_artwork(connection: &Connection) -> StoreResult<BTreeSet<String>> {
    let mut statement = connection
        .prepare(
            "SELECT artwork_digest FROM tag_mirror_outbox \
             WHERE artwork_action = 2 AND artwork_digest IS NOT NULL",
        )
        .map_err(StoreError::from)?;
    let rows = statement
        .query_map([], |row| row.get(0))
        .map_err(StoreError::from)?;
    rows.collect::<Result<BTreeSet<_>, _>>()
        .map_err(StoreError::from)
}

fn artwork_parts(artwork: &TagMirrorArtwork) -> (i64, Option<&str>, Option<i64>) {
    match artwork {
        TagMirrorArtwork::Unchanged => (0, None, None),
        TagMirrorArtwork::Clear => (1, None, None),
        TagMirrorArtwork::Set(artwork) => {
            (2, Some(artwork.digest()), Some(artwork.size_bytes() as i64))
        }
    }
}

fn pending_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PendingTagMirror> {
    let track_id = row.get::<_, i64>(0)?;
    let track_id = TrackId::new(track_id)
        .ok_or_else(|| conversion_error(0, rusqlite::types::Type::Integer, "invalid track id"))?;
    let generation = row.get::<_, i64>(1)?;
    if generation <= 0 {
        return Err(conversion_error(
            1,
            rusqlite::types::Type::Integer,
            "invalid outbox generation",
        ));
    }
    let artwork_action = row.get::<_, i64>(4)?;
    let artwork = match artwork_action {
        0 => TagMirrorArtwork::Unchanged,
        1 => TagMirrorArtwork::Clear,
        2 => {
            let digest = row.get::<_, String>(5)?;
            let size_bytes = row.get::<_, i64>(6)?;
            if size_bytes <= 0 {
                return Err(conversion_error(
                    6,
                    rusqlite::types::Type::Integer,
                    "invalid stored artwork size",
                ));
            }
            let stored = StoredTagMirrorArtwork::from_stored_parts(digest, size_bytes as u64)
                .map_err(|_| {
                    conversion_error(
                        5,
                        rusqlite::types::Type::Text,
                        "invalid stored artwork reference",
                    )
                })?;
            TagMirrorArtwork::Set(stored)
        }
        _ => {
            return Err(conversion_error(
                4,
                rusqlite::types::Type::Integer,
                "invalid artwork action",
            ));
        }
    };
    let attempt_count = row.get::<_, i64>(7)?;
    if !(0..=i64::from(u32::MAX)).contains(&attempt_count) {
        return Err(conversion_error(
            7,
            rusqlite::types::Type::Integer,
            "invalid attempt count",
        ));
    }
    Ok(PendingTagMirror {
        track_id,
        generation: generation as u64,
        kinds: TagMirrorKinds {
            metadata: row.get(2)?,
            rating: row.get(3)?,
            artwork: artwork_action != 0,
        },
        artwork,
        attempt_count: attempt_count as u32,
        next_attempt_at_unix: row.get(8)?,
        last_error: row.get(9)?,
    })
}

fn conversion_error(
    index: usize,
    stored_type: rusqlite::types::Type,
    message: &'static str,
) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(
        index,
        stored_type,
        std::io::Error::new(std::io::ErrorKind::InvalidData, message).into(),
    )
}
