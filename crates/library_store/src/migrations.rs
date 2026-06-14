// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Ordered SQLite schema migrations.
//!
//! Migration 1 is the complete alpha baseline. Unversioned databases that
//! already contain application tables are rejected: pre-versioning
//! development databases must be deleted and rebuilt by scanning the library.

use rusqlite::Connection;

use crate::{
    StoreError, StoreResult,
    schema::{
        ADD_SYNC_DEVICE_ARTISTS_SQL, ADD_TRACK_COLUMN_SORT_SQL, BACKFILL_GENERATED_SORT_FIELDS_SQL,
        SCHEMA_SQL,
    },
};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 4;

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial alpha schema",
        sql: SCHEMA_SQL,
    },
    Migration {
        version: 2,
        description: "persist track table sort order per scope",
        sql: ADD_TRACK_COLUMN_SORT_SQL,
    },
    Migration {
        version: 3,
        description: "backfill generated sort fields",
        sql: BACKFILL_GENERATED_SORT_FIELDS_SQL,
    },
    Migration {
        version: 4,
        description: "persist per-device artist sync selections",
        sql: ADD_SYNC_DEVICE_ARTISTS_SQL,
    },
];

const _: () = {
    assert!(MIGRATIONS.len() == CURRENT_SCHEMA_VERSION as usize);
    let mut index = 0;
    while index < MIGRATIONS.len() {
        assert!(MIGRATIONS[index].version == index as u32 + 1);
        index += 1;
    }
};

pub(crate) fn apply_pending(connection: &Connection) -> StoreResult<()> {
    apply_pending_from_registry(connection, MIGRATIONS)
}

fn apply_pending_from_registry(
    connection: &Connection,
    migrations: &[Migration],
) -> StoreResult<()> {
    connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .map_err(StoreError::from)?;
    let current = user_version(connection)?;
    let supported = migrations.last().map_or(0, |migration| migration.version);
    if current > supported {
        return Err(StoreError::DatabaseAhead { current, supported });
    }
    if current == 0 && has_application_tables(connection)? {
        return Err(StoreError::UnversionedDatabaseNotEmpty);
    }

    for migration in migrations
        .iter()
        .filter(|migration| migration.version > current)
    {
        connection
            .execute_batch("BEGIN IMMEDIATE;")
            .map_err(StoreError::from)?;
        let outcome = connection
            .execute_batch(migration.sql)
            .and_then(|()| connection.pragma_update(None, "user_version", migration.version))
            .and_then(|()| connection.execute_batch("COMMIT;"));
        if let Err(error) = outcome {
            let _ = connection.execute_batch("ROLLBACK;");
            return Err(StoreError::MigrationFailed {
                version: migration.version,
                description: migration.description,
                detail: error.to_string(),
            });
        }
    }
    Ok(())
}

fn user_version(connection: &Connection) -> StoreResult<u32> {
    connection
        .pragma_query_value(None, "user_version", |row| row.get(0))
        .map_err(StoreError::from)
}

fn has_application_tables(connection: &Connection) -> StoreResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS( \
                SELECT 1 FROM sqlite_schema \
                WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
            )",
            [],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;

    const EXPECTED_TABLES: &[&str] = &[
        "playlist_entries",
        "playlist_folders",
        "playlists",
        "smart_playlist_rules",
        "smart_playlists",
        "smart_shuffle_index",
        "source_fingerprint_cache",
        "sync_device_artists",
        "sync_device_playlists",
        "sync_devices",
        "sync_manifest",
        "tag_mirror_outbox",
        "track_acoustics",
        "track_analysis",
        "track_column_layout_default",
        "track_column_layout_playlist_override",
        "track_column_layout_smart_playlist_override",
        "track_column_sort_default",
        "track_column_sort_playlist_override",
        "track_column_sort_smart_playlist_override",
        "track_online_status",
        "track_synced_lyrics",
        "track_waveform",
        "tracks",
    ];

    #[test]
    fn fresh_database_applies_every_migration() {
        let connection = Connection::open_in_memory().expect("connection");

        apply_pending(&connection).expect("migrate");

        assert_eq!(user_version(&connection).expect("version"), 4);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
    }

    #[test]
    fn migration_runner_is_idempotent() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending(&connection).expect("first migrate");

        apply_pending(&connection).expect("second migrate");

        assert_eq!(user_version(&connection).expect("version"), 4);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
    }

    #[test]
    fn upgrade_from_v1_adds_sort_tables_and_preserves_data() {
        let connection = Connection::open_in_memory().expect("connection");
        // Stand up an existing alpha library pinned at schema version 1.
        connection
            .execute_batch(SCHEMA_SQL)
            .expect("baseline schema");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("set version");
        connection
            .execute(
                "INSERT INTO track_column_layout_default (column_id, position, visible, width_px) \
                 VALUES ('date_added', 0, 1, 120)",
                [],
            )
            .expect("seed layout");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(user_version(&connection).expect("version"), 4);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
        // The pre-existing layout row survives the migration untouched.
        let column_id: String = connection
            .query_row(
                "SELECT column_id FROM track_column_layout_default",
                [],
                |row| row.get(0),
            )
            .expect("layout preserved");
        assert_eq!(column_id, "date_added");
    }

    #[test]
    fn upgrade_from_v2_backfills_generated_sort_fields() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .execute_batch(SCHEMA_SQL)
            .expect("baseline schema");
        connection
            .execute_batch(ADD_TRACK_COLUMN_SORT_SQL)
            .expect("schema version 2 migration");
        connection
            .execute_batch(
                r#"
                INSERT INTO tracks (
                    id, relative_path, title, artist, album, album_artist,
                    composer, title_sort, artist_sort, album_sort,
                    album_artist_sort, composer_sort
                )
                VALUES
                    (
                        1, x'61', 'The Song', 'The Artist', 'A Record',
                        'An Album Artist', 'The Composer', NULL, '   ',
                        NULL, NULL, NULL
                    ),
                    (
                        2, x'62', 'The Explicit Song', 'The Explicit Artist',
                        NULL, NULL, NULL, 'Song, Explicit The',
                        'Artist, Explicit The', NULL, NULL, NULL
                    );
                "#,
            )
            .expect("seed version 2 tracks");
        connection
            .pragma_update(None, "user_version", 2)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(user_version(&connection).expect("version"), 4);
        let generated: [Option<String>; 5] = connection
            .query_row(
                "SELECT title_sort, artist_sort, album_sort, album_artist_sort, composer_sort \
                 FROM tracks WHERE id = 1",
                [],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ])
                },
            )
            .expect("generated row");
        assert_eq!(
            generated,
            [
                Some("Song (The)".to_owned()),
                Some("Artist (The)".to_owned()),
                Some("Record (A)".to_owned()),
                Some("Album Artist (An)".to_owned()),
                Some("Composer (The)".to_owned()),
            ]
        );

        let explicit: (Option<String>, Option<String>) = connection
            .query_row(
                "SELECT title_sort, artist_sort FROM tracks WHERE id = 2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("explicit row");
        assert_eq!(
            explicit,
            (
                Some("Song, Explicit The".to_owned()),
                Some("Artist, Explicit The".to_owned()),
            )
        );
    }

    #[test]
    fn upgrade_from_v3_adds_artist_selection_table_and_preserves_devices() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .execute_batch(SCHEMA_SQL)
            .expect("baseline schema");
        connection
            .execute_batch(ADD_TRACK_COLUMN_SORT_SQL)
            .expect("schema version 2 migration");
        connection
            .execute_batch(BACKFILL_GENERATED_SORT_FIELDS_SQL)
            .expect("schema version 3 migration");
        connection
            .execute_batch("DROP TABLE sync_device_artists;")
            .expect("simulate pre-v4 schema");
        connection
            .execute(
                "INSERT INTO sync_devices \
                 (id, label, kind, layout, sub_path, files_per_folder_cap, volume_id) \
                 VALUES ('dev', 'USB', 0, 0, '', 0, NULL)",
                [],
            )
            .expect("seed device");
        connection
            .execute(
                "INSERT INTO sync_device_playlists \
                 (device_id, item_kind, item_id, position) VALUES ('dev', 0, 7, 0)",
                [],
            )
            .expect("seed playlist selection");
        connection
            .pragma_update(None, "user_version", 3)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(user_version(&connection).expect("version"), 4);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
        let selection: (String, i64) = connection
            .query_row(
                "SELECT device_id, item_id FROM sync_device_playlists",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("playlist selection preserved");
        assert_eq!(selection, ("dev".to_owned(), 7));
        let artist_count: i64 = connection
            .query_row("SELECT COUNT(*) FROM sync_device_artists", [], |row| {
                row.get(0)
            })
            .expect("artist table present");
        assert_eq!(artist_count, 0);
    }

    #[test]
    fn database_from_a_newer_build_is_rejected() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .pragma_update(None, "user_version", 5)
            .expect("set version");

        assert_eq!(
            apply_pending(&connection),
            Err(StoreError::DatabaseAhead {
                current: 5,
                supported: 4,
            })
        );
    }

    #[test]
    fn unversioned_database_with_application_tables_is_rejected() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .execute_batch("CREATE TABLE old_development_schema (id INTEGER PRIMARY KEY);")
            .expect("create table");

        assert_eq!(
            apply_pending(&connection),
            Err(StoreError::UnversionedDatabaseNotEmpty)
        );
        assert_eq!(user_version(&connection).expect("version"), 0);
    }

    #[test]
    fn failed_migration_rolls_back_schema_and_version() {
        let connection = Connection::open_in_memory().expect("connection");
        let migrations = [
            Migration {
                version: 1,
                description: "sentinel",
                sql: "CREATE TABLE sentinel (id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                description: "broken",
                sql: "CREATE TABLE should_roll_back (id INTEGER PRIMARY KEY); broken sql;",
            },
        ];

        assert!(matches!(
            apply_pending_from_registry(&connection, &migrations),
            Err(StoreError::MigrationFailed { version: 2, .. })
        ));
        assert_eq!(user_version(&connection).expect("version"), 1);
        assert_eq!(tables(&connection), ["sentinel"]);
    }

    #[test]
    fn only_pending_migrations_are_applied() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .execute_batch("CREATE TABLE existing (id INTEGER PRIMARY KEY);")
            .expect("create existing");
        connection
            .pragma_update(None, "user_version", 1)
            .expect("set version");
        let migrations = [
            Migration {
                version: 1,
                description: "must not run",
                sql: "CREATE TABLE must_not_exist (id INTEGER PRIMARY KEY);",
            },
            Migration {
                version: 2,
                description: "pending",
                sql: "CREATE TABLE pending (id INTEGER PRIMARY KEY);",
            },
        ];

        apply_pending_from_registry(&connection, &migrations).expect("migrate");

        assert_eq!(user_version(&connection).expect("version"), 2);
        assert_eq!(tables(&connection), ["existing", "pending"]);
    }

    fn tables(connection: &Connection) -> Vec<String> {
        let mut statement = connection
            .prepare(
                "SELECT name FROM sqlite_schema \
                 WHERE type = 'table' AND name NOT LIKE 'sqlite_%' \
                 ORDER BY name",
            )
            .expect("prepare");
        statement
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("tables")
    }
}
