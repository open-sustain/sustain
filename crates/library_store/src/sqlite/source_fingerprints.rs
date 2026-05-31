// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Disposable source SHA-256 cache for incremental device export.

use rusqlite::{OptionalExtension, params};
use sustain_domain::{SourceFileStat, SourceFingerprint, TrackContentHash, TrackId};

use super::*;

pub(super) fn source_fingerprint(
    connection: &Connection,
    track_id: TrackId,
) -> StoreResult<Option<SourceFingerprint>> {
    connection
        .query_row(
            r#"
            SELECT device, inode, size_bytes, modified_at_ns, changed_at_ns, sha256
            FROM source_fingerprint_cache
            WHERE track_id = ?1
            "#,
            params![track_id.get()],
            |row| {
                let sha256: String = row.get(5)?;
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, i64>(4)?,
                    sha256,
                ))
            },
        )
        .optional()
        .map_err(StoreError::from)?
        .map(
            |(device, inode, size_bytes, modified_at_ns, changed_at_ns, sha256)| {
                Ok(SourceFingerprint {
                    stat: SourceFileStat {
                        device: nonnegative_u64(device, "source fingerprint device")?,
                        inode: nonnegative_u64(inode, "source fingerprint inode")?,
                        size_bytes: nonnegative_u64(size_bytes, "source fingerprint size")?,
                        modified_at_ns,
                        changed_at_ns,
                    },
                    content_hash: TrackContentHash::new(sha256).ok_or_else(|| {
                        StoreError::InvalidStoredEnum("source fingerprint SHA-256".to_owned())
                    })?,
                })
            },
        )
        .transpose()
}

pub(super) fn save_source_fingerprint(
    connection: &Connection,
    track_id: TrackId,
    fingerprint: &SourceFingerprint,
) -> StoreResult<()> {
    connection
        .execute(
            r#"
            INSERT INTO source_fingerprint_cache (
                track_id, device, inode, size_bytes, modified_at_ns, changed_at_ns, sha256
            )
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
            ON CONFLICT(track_id) DO UPDATE SET
                device = excluded.device,
                inode = excluded.inode,
                size_bytes = excluded.size_bytes,
                modified_at_ns = excluded.modified_at_ns,
                changed_at_ns = excluded.changed_at_ns,
                sha256 = excluded.sha256
            "#,
            params![
                track_id.get(),
                signed_i64(fingerprint.stat.device, "source fingerprint device")?,
                signed_i64(fingerprint.stat.inode, "source fingerprint inode")?,
                signed_i64(fingerprint.stat.size_bytes, "source fingerprint size")?,
                fingerprint.stat.modified_at_ns,
                fingerprint.stat.changed_at_ns,
                fingerprint.content_hash.as_str(),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn invalidate_source_fingerprint(
    connection: &Connection,
    track_id: TrackId,
) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM source_fingerprint_cache WHERE track_id = ?1",
            params![track_id.get()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

fn signed_i64(value: u64, field: &str) -> StoreResult<i64> {
    i64::try_from(value)
        .map_err(|_| StoreError::Database(format!("{field} exceeds SQLite INTEGER")))
}

fn nonnegative_u64(value: i64, field: &str) -> StoreResult<u64> {
    u64::try_from(value).map_err(|_| StoreError::Database(format!("{field} is negative")))
}
