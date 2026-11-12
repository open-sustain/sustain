// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Ordered SQLite schema migrations.
//!
//! Migration 1 is the complete alpha baseline. Unversioned databases that
//! already contain application tables are rejected: pre-versioning
//! development databases must be deleted and rebuilt by scanning the library.

use rusqlite::Connection;
use sustain_domain::MusicalKey;

use crate::{
    StoreError, StoreResult,
    schema::{
        ADD_SYNC_DEVICE_ARTISTS_SQL, ADD_SYNC_MANIFEST_ANLZ_SQL, ADD_TRACK_BEAT_GRID_SQL,
        ADD_TRACK_BIT_DEPTH_SQL, ADD_TRACK_BPM_SET_BY_USER_SQL, ADD_TRACK_COLUMN_SORT_SQL,
        ADD_TRACK_KEY_ORIGIN_SQL, BACKFILL_GENERATED_SORT_FIELDS_SQL,
        INDEX_PLAYLIST_ENTRIES_BY_TRACK_SQL, MOVE_WAVEFORMS_TO_CACHE_SQL,
        NORMALIZE_EMPTY_TRACK_METADATA_SQL, SCHEMA_SQL, STORE_BPM_IN_HUNDREDTHS_SQL,
        VERSION_ANALYSIS_PER_CAPABILITY_SQL, VERSION_ONLINE_ATTEMPTS_PER_CAPABILITY_SQL,
    },
};

pub(crate) const CURRENT_SCHEMA_VERSION: u32 = 16;

#[derive(Clone, Copy)]
struct Migration {
    version: u32,
    description: &'static str,
    step: MigrationStep,
}

/// What a migration does, run inside the migration's transaction.
#[derive(Clone, Copy)]
enum MigrationStep {
    /// A batch of SQL statements. The default: schema changes and value
    /// rewrites that SQL can express.
    Sql(&'static str),
    /// A data rewrite that has to apply a domain rule SQL cannot express,
    /// such as parsing free-form text. The rule must only ever produce
    /// values every later build reads as valid.
    Rust(fn(&Connection) -> rusqlite::Result<()>),
}

const MIGRATIONS: &[Migration] = &[
    Migration {
        version: 1,
        description: "initial alpha schema",
        step: MigrationStep::Sql(SCHEMA_SQL),
    },
    Migration {
        version: 2,
        description: "persist track table sort order per scope",
        step: MigrationStep::Sql(ADD_TRACK_COLUMN_SORT_SQL),
    },
    Migration {
        version: 3,
        description: "backfill generated sort fields",
        step: MigrationStep::Sql(BACKFILL_GENERATED_SORT_FIELDS_SQL),
    },
    Migration {
        version: 4,
        description: "persist per-device artist sync selections",
        step: MigrationStep::Sql(ADD_SYNC_DEVICE_ARTISTS_SQL),
    },
    Migration {
        version: 5,
        description: "normalize empty track metadata strings",
        step: MigrationStep::Sql(NORMALIZE_EMPTY_TRACK_METADATA_SQL),
    },
    Migration {
        version: 6,
        description: "record the Pioneer analysis pair each sync manifest entry owns",
        step: MigrationStep::Sql(ADD_SYNC_MANIFEST_ANLZ_SQL),
    },
    Migration {
        version: 7,
        description: "normalize stored musical keys and drop text that is not a key",
        step: MigrationStep::Rust(normalize_musical_keys),
    },
    Migration {
        version: 8,
        description: "store tempos to the hundredth of a beat",
        step: MigrationStep::Sql(STORE_BPM_IN_HUNDREDTHS_SQL),
    },
    Migration {
        version: 9,
        description: "store each track's beat grid",
        step: MigrationStep::Sql(ADD_TRACK_BEAT_GRID_SQL),
    },
    Migration {
        version: 10,
        description: "store each track's bit depth and rescan every file once to learn it",
        step: MigrationStep::Sql(ADD_TRACK_BIT_DEPTH_SQL),
    },
    Migration {
        version: 11,
        description: "remember which BPMs the user typed, and measure every other one again",
        step: MigrationStep::Sql(ADD_TRACK_BPM_SET_BY_USER_SQL),
    },
    Migration {
        version: 12,
        description: "move waveforms out of the library into the waveform cache",
        step: MigrationStep::Sql(MOVE_WAVEFORMS_TO_CACHE_SQL),
    },
    Migration {
        version: 13,
        description: "version analysis attempts per capability",
        step: MigrationStep::Sql(VERSION_ANALYSIS_PER_CAPABILITY_SQL),
    },
    Migration {
        version: 14,
        description: "index playlist entries by track",
        step: MigrationStep::Sql(INDEX_PLAYLIST_ENTRIES_BY_TRACK_SQL),
    },
    Migration {
        version: 15,
        description: "remember where each key came from",
        step: MigrationStep::Sql(ADD_TRACK_KEY_ORIGIN_SQL),
    },
    Migration {
        version: 16,
        description: "version online attempts per capability",
        step: MigrationStep::Sql(VERSION_ONLINE_ATTEMPTS_PER_CAPABILITY_SQL),
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
        let outcome = match migration.step {
            MigrationStep::Sql(sql) => connection.execute_batch(sql),
            MigrationStep::Rust(rewrite) => rewrite(connection),
        }
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

/// Migration 7. Keys used to be stored as whatever text the file's key tag
/// held: the same key in a dozen spellings (`Am`, `Amin`, `8A`, `A min`)
/// and, now and then, text that is no key at all (a URL a ripper left
/// behind). Every value is re-read with [`MusicalKey::parse`], the rule
/// import now applies, and stored as its canonical short code. Text that
/// is not a key is cleared together with the track's key-analysis
/// attempt, so analysis fills the key in (it never overwrites a stored
/// key, so an earlier attempt left the junk in place). Canonical
/// short codes parse to themselves, so the rewrite is idempotent and its
/// output is valid for every later build.
fn normalize_musical_keys(connection: &Connection) -> rusqlite::Result<()> {
    let mut select =
        connection.prepare("SELECT id, musical_key FROM tracks WHERE musical_key IS NOT NULL")?;
    let stored = select
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut update = connection.prepare("UPDATE tracks SET musical_key = ?1 WHERE id = ?2")?;
    let mut forget_attempt = connection
        .prepare("UPDATE track_analysis SET key_attempted_at_unix = NULL WHERE track_id = ?1")?;
    for (id, text) in stored {
        let canonical = MusicalKey::parse(&text).map(MusicalKey::short_code);
        if canonical != Some(text.as_str()) {
            update.execute(rusqlite::params![canonical, id])?;
        }
        if canonical.is_none() {
            forget_attempt.execute([id])?;
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
        "track_beat_grid",
        "track_column_layout_default",
        "track_column_layout_playlist_override",
        "track_column_layout_smart_playlist_override",
        "track_column_sort_default",
        "track_column_sort_playlist_override",
        "track_column_sort_smart_playlist_override",
        "track_online_status",
        "track_synced_lyrics",
        "tracks",
    ];

    #[test]
    fn fresh_database_applies_every_migration() {
        let connection = Connection::open_in_memory().expect("connection");

        apply_pending(&connection).expect("migrate");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(tables(&connection), EXPECTED_TABLES);
    }

    #[test]
    fn migration_runner_is_idempotent() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending(&connection).expect("first migrate");

        apply_pending(&connection).expect("second migrate");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
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

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
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

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
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

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
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
    fn upgrade_from_v4_normalizes_empty_track_metadata_strings() {
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
            .execute_batch(ADD_SYNC_DEVICE_ARTISTS_SQL)
            .expect("schema version 4 migration");
        connection
            .execute_batch(
                r#"
                INSERT INTO tracks (
                    id, relative_path, title, artist, album, album_artist,
                    composer, genre, grouping, musical_key, comments, lyrics,
                    title_sort, artist_sort, album_sort, album_artist_sort,
                    composer_sort
                )
                VALUES
                    (
                        1, x'61', '', ' ', char(9), char(10), char(11),
                        char(12), char(13), char(9) || char(10),
                        char(13) || char(32), char(32) || char(9),
                        '', ' ', char(10), char(13), char(9)
                    ),
                    (
                        2, x'62', '  Real title  ', NULL, 'Album',
                        'The Artist', 'Composer', 'Genre', 'Grouping',
                        '8A', '  Real comment  ', 'line one' || char(10),
                        ' Title Sort ', 'Artist Sort', 'Album Sort',
                        'Album Artist Sort', 'Composer Sort'
                    );
                "#,
            )
            .expect("seed version 4 tracks");
        connection
            .pragma_update(None, "user_version", 4)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        let normalized: [Option<String>; 15] = connection
            .query_row(
                "SELECT title, artist, album, album_artist, composer, genre, \
                        grouping, musical_key, comments, lyrics, title_sort, \
                        artist_sort, album_sort, album_artist_sort, composer_sort \
                 FROM tracks WHERE id = 1",
                [],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                    ])
                },
            )
            .expect("normalized row");
        let absent: [Option<String>; 15] = std::array::from_fn(|_| None);
        assert_eq!(normalized, absent);

        let preserved: [Option<String>; 15] = connection
            .query_row(
                "SELECT title, artist, album, album_artist, composer, genre, \
                        grouping, musical_key, comments, lyrics, title_sort, \
                        artist_sort, album_sort, album_artist_sort, composer_sort \
                 FROM tracks WHERE id = 2",
                [],
                |row| {
                    Ok([
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                        row.get(7)?,
                        row.get(8)?,
                        row.get(9)?,
                        row.get(10)?,
                        row.get(11)?,
                        row.get(12)?,
                        row.get(13)?,
                        row.get(14)?,
                    ])
                },
            )
            .expect("preserved row");
        assert_eq!(
            preserved,
            [
                Some("  Real title  ".to_owned()),
                None,
                Some("Album".to_owned()),
                Some("The Artist".to_owned()),
                Some("Composer".to_owned()),
                Some("Genre".to_owned()),
                Some("Grouping".to_owned()),
                // Kept by migration 5, then normalized by migration 7.
                Some("Am".to_owned()),
                Some("  Real comment  ".to_owned()),
                Some("line one\n".to_owned()),
                Some(" Title Sort ".to_owned()),
                Some("Artist Sort".to_owned()),
                Some("Album Sort".to_owned()),
                Some("Album Artist Sort".to_owned()),
                Some("Composer Sort".to_owned()),
            ]
        );
    }

    #[test]
    fn upgrade_from_v5_adds_manifest_anlz_record_and_preserves_entries() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 5");
        }
        connection
            .execute_batch(
                "INSERT INTO sync_devices \
                 (id, label, kind, layout, sub_path, files_per_folder_cap, volume_id) \
                 VALUES ('dev', 'USB', 0, 2, '', 0, NULL); \
                 INSERT INTO sync_manifest (device_id, track_id, on_device_path, fingerprint) \
                 VALUES ('dev', 7, 'Contents/A/B/1.mp3', 'fp');",
            )
            .expect("seed version 5 manifest");
        connection
            .pragma_update(None, "user_version", 5)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        assert_eq!(tables(&connection), EXPECTED_TABLES);
        let entry: (i64, String, String, Option<String>, Option<String>) = connection
            .query_row(
                "SELECT track_id, on_device_path, fingerprint, anlz_path, anlz_digest \
                 FROM sync_manifest",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("manifest entry preserved");
        assert_eq!(
            entry,
            (
                7,
                "Contents/A/B/1.mp3".to_owned(),
                "fp".to_owned(),
                None,
                None
            )
        );
        // A path without its digest (or the reverse) is rejected.
        for (path, digest) in [(Some("PIONEER/x.DAT"), None), (None, Some("ab"))] {
            let result = connection.execute(
                "INSERT INTO sync_manifest \
                 (device_id, track_id, on_device_path, fingerprint, anlz_path, anlz_digest) \
                 VALUES ('dev', 8, 'Contents/A/B/2.mp3', 'fp', ?1, ?2)",
                rusqlite::params![path, digest],
            );
            assert!(result.is_err(), "{path:?} / {digest:?}");
        }
    }

    #[test]
    fn upgrade_from_v6_normalizes_musical_keys_and_clears_text_that_is_not_a_key() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
            ADD_SYNC_MANIFEST_ANLZ_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 6");
        }
        // Spellings found in a real library, one per row.
        let stored = [
            ("Am", Some("Am")),
            ("Amin", Some("Am")),
            ("A#min", Some("Bbm")),
            ("G#m", Some("Abm")),
            ("D#", Some("Eb")),
            ("G m", Some("Gm")),
            ("Emaj", Some("E")),
            ("8A", Some("Am")),
            ("3B", Some("Db")),
            ("F#m", Some("F#m")),
            ("www.RnB4U.in", None),
        ];
        for (index, (text, _)) in stored.iter().enumerate() {
            connection
                .execute(
                    "INSERT INTO tracks (id, relative_path, title, musical_key) \
                     VALUES (?1, ?2, 'Title', ?3)",
                    rusqlite::params![index as i64 + 1, vec![b'a' + index as u8], text],
                )
                .expect("seed track");
        }
        connection
            .execute(
                "INSERT INTO tracks (id, relative_path, musical_key) VALUES (100, x'7a', NULL)",
                [],
            )
            .expect("seed keyless track");
        // Key analysis already ran on the junk track and on a keyed one.
        let junk_id = stored.len() as i64;
        for track_id in [1, junk_id] {
            connection
                .execute(
                    "INSERT INTO track_analysis \
                     (track_id, bpm_attempted_at_unix, key_attempted_at_unix, analyzer_version) \
                     VALUES (?1, 10, 20, 1)",
                    [track_id],
                )
                .expect("seed analysis attempt");
        }
        connection
            .pragma_update(None, "user_version", 6)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        let attempts = |track_id: i64| -> (Option<i64>, Option<i64>) {
            connection
                .query_row(
                    "SELECT bpm_attempted_at_unix, key_attempted_at_unix \
                     FROM track_analysis WHERE track_id = ?1",
                    [track_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("analysis row")
        };
        assert_eq!(attempts(1), (Some(10), Some(20)));
        assert_eq!(attempts(junk_id), (Some(10), None));
        for (index, (text, expected)) in stored.iter().enumerate() {
            let (key, title): (Option<String>, String) = connection
                .query_row(
                    "SELECT musical_key, title FROM tracks WHERE id = ?1",
                    [index as i64 + 1],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .expect("track preserved");
            assert_eq!(key.as_deref(), *expected, "{text:?}");
            assert_eq!(title, "Title");
        }
        let keyless: Option<String> = connection
            .query_row("SELECT musical_key FROM tracks WHERE id = 100", [], |row| {
                row.get(0)
            })
            .expect("keyless track preserved");
        assert_eq!(keyless, None);
    }

    #[test]
    fn upgrade_from_v7_stores_tempos_in_hundredths_and_clears_values_that_are_not_a_tempo() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
            ADD_SYNC_MANIFEST_ANLZ_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 6");
        }
        normalize_musical_keys(&connection).expect("schema version 7 migration");
        // Values found in a real library: whole tempos, zeros, and tempos a
        // tagger stored multiplied by 100.
        let stored: [(Option<i64>, Option<i64>); 6] = [
            (Some(128), Some(12_800)),
            (Some(1), Some(100)),
            (Some(655), Some(65_500)),
            (Some(0), None),
            (Some(12_488), None),
            (None, None),
        ];
        for (index, (bpm, _)) in stored.iter().enumerate() {
            let track_id = index as i64 + 1;
            connection
                .execute(
                    "INSERT INTO tracks (id, relative_path, title, bpm, musical_key) \
                     VALUES (?1, ?2, 'Title', ?3, 'Am')",
                    rusqlite::params![track_id, vec![b'a' + index as u8], bpm],
                )
                .expect("seed track");
            // Tempo analysis already ran on every track.
            connection
                .execute(
                    "INSERT INTO track_analysis \
                     (track_id, bpm_attempted_at_unix, key_attempted_at_unix, analyzer_version) \
                     VALUES (?1, 10, 20, 1)",
                    [track_id],
                )
                .expect("seed analysis attempt");
        }
        connection
            .pragma_update(None, "user_version", 7)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        for (index, (bpm, expected)) in stored.iter().enumerate() {
            let (hundredths, title, key, bpm_attempt, key_attempt): (
                Option<i64>,
                String,
                String,
                Option<i64>,
                Option<i64>,
            ) = connection
                .query_row(
                    "SELECT t.bpm_hundredths, t.title, t.musical_key, \
                            a.bpm_attempted_at_unix, a.key_attempted_at_unix \
                     FROM tracks t JOIN track_analysis a ON a.track_id = t.id \
                     WHERE t.id = ?1",
                    [index as i64 + 1],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                        ))
                    },
                )
                .expect("track preserved");
            assert_eq!(hundredths, *expected, "{bpm:?}");
            assert_eq!((title.as_str(), key.as_str()), ("Title", "Am"));
            // A cleared tempo is analyzed again, and so (version 11) is
            // every held one; only a track that never had a BPM keeps its
            // attempt.
            let expected_bpm_attempt = if bpm.is_none() { Some(10) } else { None };
            assert_eq!(bpm_attempt, expected_bpm_attempt, "{bpm:?}");
            assert_eq!(key_attempt, Some(20));
        }
        let columns: Vec<String> = connection
            .prepare("SELECT name FROM pragma_table_info('tracks')")
            .expect("table info")
            .query_map([], |row| row.get(0))
            .expect("columns")
            .collect::<Result<_, _>>()
            .expect("column names");
        assert!(!columns.iter().any(|column| column == "bpm"));
    }

    #[test]
    fn upgrade_from_v8_adds_an_empty_beat_grid_table_and_preserves_tracks() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
            ADD_SYNC_MANIFEST_ANLZ_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 6");
        }
        normalize_musical_keys(&connection).expect("schema version 7 migration");
        connection
            .execute_batch(STORE_BPM_IN_HUNDREDTHS_SQL)
            .expect("schema version 8 migration");
        connection
            .execute(
                "INSERT INTO tracks (id, relative_path, title, bpm_hundredths) \
                 VALUES (1, x'61', 'Title', 12886)",
                [],
            )
            .expect("seed track");
        connection
            .execute(
                "INSERT INTO track_analysis \
                 (track_id, bpm_attempted_at_unix, analyzer_version) VALUES (1, 10, 12)",
                [],
            )
            .expect("seed analysis attempt");
        connection
            .pragma_update(None, "user_version", 8)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        let (title, bpm, attempt): (String, i64, Option<i64>) = connection
            .query_row(
                "SELECT t.title, t.bpm_hundredths, a.bpm_attempted_at_unix \
                 FROM tracks t JOIN track_analysis a ON a.track_id = t.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("track preserved");
        // Version 11 forgets the attempt so the held BPM is measured again.
        assert_eq!((title.as_str(), bpm, attempt), ("Title", 12_886, None));
        let grids: i64 = connection
            .query_row("SELECT COUNT(*) FROM track_beat_grid", [], |row| row.get(0))
            .expect("beat grid table");
        assert_eq!(grids, 0);
        // The table only holds what describes a grid.
        for (tempo, first_beat, number) in [(0, 0, 1), (12_800, -1, 1), (12_800, 0, 5)] {
            assert!(
                connection
                    .execute(
                        "INSERT INTO track_beat_grid VALUES (1, ?1, ?2, ?3)",
                        [tempo, first_beat, number],
                    )
                    .is_err(),
                "{tempo} {first_beat} {number}"
            );
        }
    }

    #[test]
    fn upgrade_from_v9_adds_bit_depth_and_forgets_scan_fingerprints() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
            ADD_SYNC_MANIFEST_ANLZ_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 6");
        }
        normalize_musical_keys(&connection).expect("schema version 7 migration");
        connection
            .execute_batch(STORE_BPM_IN_HUNDREDTHS_SQL)
            .expect("schema version 8 migration");
        connection
            .execute_batch(ADD_TRACK_BEAT_GRID_SQL)
            .expect("schema version 9 migration");
        connection
            .execute(
                "INSERT INTO tracks \
                 (id, relative_path, title, sample_rate_hz, file_size_bytes, file_modified_at_unix) \
                 VALUES (1, x'61', 'Title', 96000, 1234, 1700000000)",
                [],
            )
            .expect("seed track");
        connection
            .pragma_update(None, "user_version", 9)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        let row: (String, i64, i64, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT title, sample_rate_hz, file_size_bytes, file_modified_at_unix, bit_depth \
                 FROM tracks WHERE id = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("track preserved");
        // The fingerprint is forgotten so the next scan reads the file
        // and learns its bit depth; everything else is kept.
        assert_eq!(row, ("Title".to_owned(), 96_000, 1234, None, None));
        for invalid in [0, 256] {
            assert!(
                connection
                    .execute("UPDATE tracks SET bit_depth = ?1 WHERE id = 1", [invalid])
                    .is_err(),
                "{invalid}"
            );
        }
    }

    #[test]
    fn upgrade_from_v10_marks_no_bpm_as_typed_and_measures_held_bpms_again() {
        let connection = Connection::open_in_memory().expect("connection");
        for sql in [
            SCHEMA_SQL,
            ADD_TRACK_COLUMN_SORT_SQL,
            BACKFILL_GENERATED_SORT_FIELDS_SQL,
            ADD_SYNC_DEVICE_ARTISTS_SQL,
            NORMALIZE_EMPTY_TRACK_METADATA_SQL,
            ADD_SYNC_MANIFEST_ANLZ_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema up to version 6");
        }
        normalize_musical_keys(&connection).expect("schema version 7 migration");
        for sql in [
            STORE_BPM_IN_HUNDREDTHS_SQL,
            ADD_TRACK_BEAT_GRID_SQL,
            ADD_TRACK_BIT_DEPTH_SQL,
        ] {
            connection
                .execute_batch(sql)
                .expect("schema versions 8 to 10");
        }
        // Track 1 holds a BPM; track 2 was measured and found none.
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path, title, bpm_hundredths) \
                     VALUES (1, x'61', 'Held', 6300), (2, x'62', 'Empty', NULL);
                 INSERT INTO track_analysis \
                     (track_id, bpm_attempted_at_unix, key_attempted_at_unix, analyzer_version) \
                     VALUES (1, 1700000000, 1700000000, 13), (2, 1700000000, 1700000000, 13);",
            )
            .expect("seed tracks");
        connection
            .pragma_update(None, "user_version", 10)
            .expect("set version");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        // Title, BPM, typed by the user, BPM attempt, key attempt.
        type Row = (String, Option<i64>, bool, Option<i64>, Option<i64>);
        let rows: Vec<Row> = connection
            .prepare(
                "SELECT t.title, t.bpm_hundredths, t.bpm_set_by_user, \
                     a.bpm_attempted_at_unix, a.key_attempted_at_unix \
                 FROM tracks t JOIN track_analysis a ON a.track_id = t.id ORDER BY t.id",
            )
            .expect("prepare")
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        // Values are kept; only the held BPM's attempt is forgotten, so
        // its tempo is measured again and the measurement replaces it.
        assert_eq!(
            rows,
            [
                (
                    "Held".to_owned(),
                    Some(6300),
                    false,
                    None,
                    Some(1_700_000_000)
                ),
                (
                    "Empty".to_owned(),
                    None,
                    false,
                    Some(1_700_000_000),
                    Some(1_700_000_000)
                ),
            ]
        );
        assert!(
            connection
                .execute("UPDATE tracks SET bpm_set_by_user = 2 WHERE id = 1", [])
                .is_err()
        );
    }

    #[test]
    fn upgrade_from_v11_drops_stored_waveforms_and_keeps_analysis_bookkeeping() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending_from_registry(&connection, &MIGRATIONS[..11]).expect("schema version 11");
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path, title) VALUES (1, x'61', 'Analysed');
                 INSERT INTO track_analysis \
                     (track_id, audio_attempted_at_unix, analyzer_version) \
                     VALUES (1, 1700000000, 15);
                 INSERT INTO track_waveform VALUES (1, 25.0, x'01020304', 6.6, x'01020304');",
            )
            .expect("seed an analysed track");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        let waveform_tables: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'track_waveform'",
                [],
                |row| row.get(0),
            )
            .expect("count");
        assert_eq!(waveform_tables, 0);
        // The attempt is kept; it holds no token yet, so no cached
        // waveform can count for it until its audio is analysed again.
        let (title, attempted, token): (String, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT t.title, a.audio_attempted_at_unix, a.audio_attempt_token \
                 FROM tracks t JOIN track_analysis a ON a.track_id = t.id",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("analysed track");
        assert_eq!(
            (title.as_str(), attempted, token),
            ("Analysed", Some(1_700_000_000), None)
        );
    }

    #[test]
    fn upgrade_from_v12_versions_each_attempted_capability() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending_from_registry(&connection, &MIGRATIONS[..12]).expect("schema version 12");
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path) VALUES (1, x'61');
                 INSERT INTO track_analysis \
                     (track_id, bpm_attempted_at_unix, audio_attempted_at_unix, analyzer_version) \
                     VALUES (1, 1700000000, 1700000000, 15);",
            )
            .expect("seed an analysed track");

        apply_pending(&connection).expect("upgrade");

        let versions: (Option<i64>, Option<i64>, Option<i64>) = connection
            .query_row(
                "SELECT bpm_analyzer_version, key_analyzer_version, audio_analyzer_version \
                 FROM track_analysis",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("versions");
        // Key was never attempted, so it has no version.
        assert_eq!(versions, (Some(15), None, Some(15)));
        assert!(
            connection
                .prepare("SELECT analyzer_version FROM track_analysis")
                .is_err()
        );
    }

    #[test]
    fn upgrade_from_v13_finds_a_tracks_playlist_entries_by_index() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending_from_registry(&connection, &MIGRATIONS[..13]).expect("schema version 13");
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path) VALUES (1, x'61'), (2, x'62');
                 INSERT INTO playlists (id, name) VALUES (1, 'Mix');
                 INSERT INTO playlist_entries (playlist_id, track_id, position) \
                     VALUES (1, 1, 0), (1, 2, 1);",
            )
            .expect("seed a playlist");

        apply_pending(&connection).expect("upgrade");

        let plan: String = connection
            .query_row(
                "EXPLAIN QUERY PLAN SELECT 1 FROM playlist_entries WHERE track_id = 1",
                [],
                |row| row.get(3),
            )
            .expect("query plan");
        assert!(plan.contains("playlist_entries_by_track"), "{plan}");
        connection
            .execute("DELETE FROM tracks WHERE id = 1", [])
            .expect("remove a track");
        let left: Vec<i64> = connection
            .prepare("SELECT track_id FROM playlist_entries ORDER BY position")
            .expect("prepare")
            .query_map([], |row| row.get(0))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("entries");
        assert_eq!(left, vec![2]);
    }

    #[test]
    fn upgrade_from_v14_keeps_every_held_key_as_imported() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending_from_registry(&connection, &MIGRATIONS[..14]).expect("schema version 14");
        // Track 1 holds a key; track 2 was measured and found none.
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path, title, musical_key) \
                     VALUES (1, x'61', 'Held', 'Am'), (2, x'62', 'Empty', NULL);
                 INSERT INTO track_analysis \
                     (track_id, bpm_attempted_at_unix, key_attempted_at_unix, \
                      bpm_analyzer_version, key_analyzer_version) \
                     VALUES (1, 1700000000, 1700000000, 15, 15), \
                            (2, 1700000000, 1700000000, 15, 15);",
            )
            .expect("seed tracks");

        apply_pending(&connection).expect("upgrade");

        assert_eq!(
            user_version(&connection).expect("version"),
            CURRENT_SCHEMA_VERSION
        );
        // Title, key, key origin, BPM attempt, key attempt.
        type Row = (String, Option<String>, i64, Option<i64>, Option<i64>);
        let rows: Vec<Row> = connection
            .prepare(
                "SELECT t.title, t.musical_key, t.key_origin, \
                     a.bpm_attempted_at_unix, a.key_attempted_at_unix \
                 FROM tracks t JOIN track_analysis a ON a.track_id = t.id ORDER BY t.id",
            )
            .expect("prepare")
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        // Values and attempts are kept, and the held key is imported:
        // analysis will not replace it.
        assert_eq!(
            rows,
            [
                (
                    "Held".to_owned(),
                    Some("Am".to_owned()),
                    0,
                    Some(1_700_000_000),
                    Some(1_700_000_000)
                ),
                (
                    "Empty".to_owned(),
                    None,
                    0,
                    Some(1_700_000_000),
                    Some(1_700_000_000)
                ),
            ]
        );
        assert!(
            connection
                .execute("UPDATE tracks SET key_origin = 3 WHERE id = 1", [])
                .is_err()
        );
    }

    #[test]
    fn upgrade_from_v15_versions_each_attempted_online_capability() {
        let connection = Connection::open_in_memory().expect("connection");
        apply_pending_from_registry(&connection, &MIGRATIONS[..15]).expect("schema version 15");
        connection
            .execute_batch(
                "INSERT INTO tracks (id, relative_path) VALUES (1, x'61');
                 INSERT INTO track_online_status \
                     (track_id, tags_attempted_at_unix, lyrics_attempted_at_unix, \
                      provider_version) \
                     VALUES (1, 1700000000, 1700000001, 2);",
            )
            .expect("seed an attempted track");

        apply_pending(&connection).expect("upgrade");

        // Artwork attempt, artwork / tags / lyrics versions, tags and
        // lyrics attempts.
        type Row = (
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
            Option<i64>,
        );
        let row: Row = connection
            .query_row(
                "SELECT artwork_attempted_at_unix, artwork_provider_version, \
                     tags_provider_version, lyrics_provider_version, \
                     tags_attempted_at_unix, lyrics_attempted_at_unix \
                 FROM track_online_status",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .expect("status");
        // Artwork was never attempted, so it has no version; the attempts
        // themselves are kept.
        assert_eq!(
            row,
            (
                None,
                None,
                Some(2),
                Some(2),
                Some(1_700_000_000),
                Some(1_700_000_001)
            )
        );
        assert!(
            connection
                .prepare("SELECT provider_version FROM track_online_status")
                .is_err()
        );
    }

    #[test]
    fn failed_rust_migration_rolls_back_its_writes_and_version() {
        fn write_then_fail(connection: &Connection) -> rusqlite::Result<()> {
            connection.execute("INSERT INTO sentinel (id) VALUES (1)", [])?;
            Err(rusqlite::Error::InvalidQuery)
        }
        let connection = Connection::open_in_memory().expect("connection");
        let migrations = [
            Migration {
                version: 1,
                description: "sentinel",
                step: MigrationStep::Sql("CREATE TABLE sentinel (id INTEGER PRIMARY KEY);"),
            },
            Migration {
                version: 2,
                description: "broken rewrite",
                step: MigrationStep::Rust(write_then_fail),
            },
        ];

        assert!(matches!(
            apply_pending_from_registry(&connection, &migrations),
            Err(StoreError::MigrationFailed { version: 2, .. })
        ));
        assert_eq!(user_version(&connection).expect("version"), 1);
        let rows: i64 = connection
            .query_row("SELECT COUNT(*) FROM sentinel", [], |row| row.get(0))
            .expect("count");
        assert_eq!(rows, 0);
    }

    #[test]
    fn database_from_a_newer_build_is_rejected() {
        let connection = Connection::open_in_memory().expect("connection");
        connection
            .pragma_update(None, "user_version", CURRENT_SCHEMA_VERSION + 1)
            .expect("set version");

        assert_eq!(
            apply_pending(&connection),
            Err(StoreError::DatabaseAhead {
                current: CURRENT_SCHEMA_VERSION + 1,
                supported: CURRENT_SCHEMA_VERSION,
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
                step: MigrationStep::Sql("CREATE TABLE sentinel (id INTEGER PRIMARY KEY);"),
            },
            Migration {
                version: 2,
                description: "broken",
                step: MigrationStep::Sql(
                    "CREATE TABLE should_roll_back (id INTEGER PRIMARY KEY); broken sql;",
                ),
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
                step: MigrationStep::Sql("CREATE TABLE must_not_exist (id INTEGER PRIMARY KEY);"),
            },
            Migration {
                version: 2,
                description: "pending",
                step: MigrationStep::Sql("CREATE TABLE pending (id INTEGER PRIMARY KEY);"),
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
