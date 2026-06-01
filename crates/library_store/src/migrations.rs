// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Ordered SQLite schema migrations.
//!
//! Migration 1 is the complete alpha baseline. Unversioned databases that
//! already contain application tables are rejected: pre-versioning
//! development databases must be deleted and rebuilt by scanning the library.

use rusqlite::Connection;

use crate::{StoreError, StoreResult, schema::SCHEMA_SQL};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    description: &'static str,
    sql: &'static str,
}

const MIGRATIONS: &[Migration] = &[Migration {
    version: 1,
    description: "initial alpha schema",
    sql: SCHEMA_SQL,
}];

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
        "sync_device_playlists",
        "sync_devices",
        "sync_manifest",
        "tag_mirror_outbox",
        "track_acoustics",
        "track_analysis",
        "track_column_layout_default",
        "track_column_layout_playlist_override",
        "track_column_layout_smart_playlist_override",
        "track_online_status",
        "track_synced_lyrics",
        "track_waveform",
        "tracks",
    ];

    #[test]
    fn fresh_database_applies_every_migration() {
        let connection = Connection::open_in_memory().expect("connection");

        apply_pending(&connection).expect("migrate");

        assert_eq!(user_version(&connection).expect("version"), 1);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
    }

    #[test]
    fn migration_runner_is_idempotent() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending(&connection).expect("first migrate");

        apply_pending(&connection).expect("second migrate");

        assert_eq!(user_version(&connection).expect("version"), 1);
        assert_eq!(tables(&connection), EXPECTED_TABLES);
    }

    #[test]
    fn database_from_a_newer_build_is_rejected() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .pragma_update(None, "user_version", 2)
            .expect("set version");

        assert_eq!(
            apply_pending(&connection),
            Err(StoreError::DatabaseAhead {
                current: 2,
                supported: 1,
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
