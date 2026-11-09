// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The waveform cache: analysis waveforms, kept out of the library.
//!
//! Waveforms are derived data — about 600 bytes per second of audio,
//! gigabytes for a large library — so they live in their own SQLite file
//! in the cache directory instead of in the precious, migrated library
//! database. The file is attached to the library connection as the
//! [`SCHEMA`] schema, so analysis bookkeeping and waveforms are still
//! read and written through one connection.
//!
//! A cache row records the waveform *outcome* of one audio analysis: the
//! segments, or none when that analysis produced no waveform (a long
//! track, a decode failure). It counts only while its
//! `audio_attempt_token` equals the token the library holds for the
//! track's latest audio attempt. That one rule covers every way the two
//! files can disagree — a wiped cache, a library restored from a backup,
//! a crash between the two commits, a track id reused after a deletion,
//! replaced audio — and each resolves into analysing the track again.
//!
//! The cache owns no durable state, so it is never migrated: a schema
//! change or a corrupt file recreates it. When it cannot be opened at
//! all, the store attaches an in-memory stand-in and reports
//! [`WaveformCacheStatus::Unavailable`]: the library stays usable,
//! waveforms are not stored for the session, and analysis does not
//! chase the waveforms the stand-in lacks.

use std::{fs, path::Path};

use rusqlite::{Connection, ErrorCode};

/// File name of the cache in Sustain's cache directory.
pub(crate) const FILE_NAME: &str = "waveform-cache.sqlite";

/// Schema name the cache file is attached under.
pub(crate) const SCHEMA: &str = "waveform_cache";

const CACHE_SCHEMA_VERSION: i64 = 1;

const CREATE_SQL: &str = r#"
CREATE TABLE IF NOT EXISTS waveform_cache.track_waveform (
    track_id                    INTEGER PRIMARY KEY,
    audio_attempt_token         INTEGER NOT NULL,
    preview_segment_duration_ms REAL,
    preview_segments            BLOB,
    detail_segment_duration_ms  REAL,
    detail_segments             BLOB,
    CHECK (
        (preview_segment_duration_ms IS NULL) = (preview_segments IS NULL)
        AND (preview_segments IS NULL) = (detail_segment_duration_ms IS NULL)
        AND (detail_segment_duration_ms IS NULL) = (detail_segments IS NULL)
    )
);
"#;

// Foreign keys cannot reach into an attached database, so a TEMP trigger
// — the only kind that may span schemas — stands in for the cascade.
const FORGET_DELETED_TRACKS_SQL: &str = r#"
CREATE TEMP TRIGGER IF NOT EXISTS forget_deleted_track_waveform
AFTER DELETE ON main.tracks
BEGIN
    DELETE FROM waveform_cache.track_waveform WHERE track_id = OLD.id;
END;
"#;

/// Whether waveforms persist across sessions.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WaveformCacheStatus {
    /// The cache is attached; waveforms are stored and read back.
    Available,
    /// The cache file could not be opened; `reason` says why. Waveforms
    /// are not stored this session.
    Unavailable { reason: String },
}

impl WaveformCacheStatus {
    pub fn is_available(&self) -> bool {
        matches!(self, Self::Available)
    }
}

/// Attach the cache file at `path`, or the in-memory stand-in when it
/// cannot be opened.
pub(crate) fn attach_file(
    connection: &Connection,
    path: &Path,
) -> rusqlite::Result<WaveformCacheStatus> {
    let status = match open_file(connection, path) {
        Ok(()) => WaveformCacheStatus::Available,
        Err(reason) => {
            detach_if_attached(connection)?;
            attach_memory(connection)?;
            WaveformCacheStatus::Unavailable { reason }
        }
    };
    connection.execute_batch(FORGET_DELETED_TRACKS_SQL)?;
    Ok(status)
}

/// Attach an in-memory cache: a store that lives only as long as its
/// connection, such as an in-memory library.
pub(crate) fn attach_in_memory(connection: &Connection) -> rusqlite::Result<WaveformCacheStatus> {
    attach_memory(connection)?;
    connection.execute_batch(FORGET_DELETED_TRACKS_SQL)?;
    Ok(WaveformCacheStatus::Available)
}

fn attach_memory(connection: &Connection) -> rusqlite::Result<()> {
    attach(connection, ":memory:")?;
    connection.execute_batch(CREATE_SQL)
}

/// Attach the cache file, recreating it once if it is corrupt. The error
/// says why the file is unusable.
fn open_file(connection: &Connection, path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| format!("{}: {error}", parent.display()))?;
    }
    match attach_and_configure(connection, path) {
        Ok(()) => Ok(()),
        Err(error) if is_corruption(&error) => {
            detach_if_attached(connection).map_err(|error| error.to_string())?;
            remove_files(path).map_err(|error| format!("{}: {error}", path.display()))?;
            attach_and_configure(connection, path).map_err(|error| error.to_string())
        }
        Err(error) => Err(error.to_string()),
    }
}

fn attach_and_configure(connection: &Connection, path: &Path) -> rusqlite::Result<()> {
    let location = path
        .to_str()
        .ok_or_else(|| rusqlite::Error::InvalidPath(path.to_path_buf()))?;
    attach(connection, location)?;
    connection.execute_batch(
        "PRAGMA waveform_cache.journal_mode = WAL;
         PRAGMA waveform_cache.synchronous = NORMAL;",
    )?;
    let version: i64 =
        connection.pragma_query_value(Some(SCHEMA), "user_version", |row| row.get(0))?;
    // Another format is recreated, never migrated: the cache is derived.
    let other_format = version != CACHE_SCHEMA_VERSION;
    if other_format {
        connection.execute_batch("DROP TABLE IF EXISTS waveform_cache.track_waveform;")?;
    }
    connection.execute_batch(CREATE_SQL)?;
    if other_format {
        connection.pragma_update(Some(SCHEMA), "user_version", CACHE_SCHEMA_VERSION)?;
    }
    Ok(())
}

fn attach(connection: &Connection, location: &str) -> rusqlite::Result<()> {
    connection.execute("ATTACH DATABASE ?1 AS waveform_cache", [location])?;
    Ok(())
}

fn detach_if_attached(connection: &Connection) -> rusqlite::Result<()> {
    let attached: bool = connection.query_row(
        "SELECT EXISTS (SELECT 1 FROM pragma_database_list WHERE name = ?1)",
        [SCHEMA],
        |row| row.get(0),
    )?;
    if attached {
        connection.execute_batch("DETACH DATABASE waveform_cache;")?;
    }
    Ok(())
}

/// Whether `error` says the cache file is damaged, which is the one case
/// that warrants deleting it. Anything else — permissions, a full disk,
/// a locked file — leaves the file alone.
fn is_corruption(error: &rusqlite::Error) -> bool {
    matches!(
        error.sqlite_error_code(),
        Some(ErrorCode::NotADatabase | ErrorCode::DatabaseCorrupt)
    )
}

/// Delete the cache file and its WAL companions.
fn remove_files(path: &Path) -> std::io::Result<()> {
    for suffix in ["", "-wal", "-shm"] {
        let mut file = path.as_os_str().to_owned();
        file.push(suffix);
        match fs::remove_file(&file) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
    }
    Ok(())
}
