// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for track column layouts.

use rusqlite::OptionalExtension;

use super::*;

pub(super) fn load_track_column_layout(
    connection: &Connection,
    scope: TrackColumnLayoutScope,
) -> StoreResult<Option<TrackColumnLayout>> {
    let entries = match scope {
        TrackColumnLayoutScope::Default => load_layout_rows(
            connection,
            "SELECT column_id, visible, width_px \
                 FROM track_column_layout_default \
                 ORDER BY position",
            params![],
        )?,
        TrackColumnLayoutScope::Playlist(playlist_id) => load_layout_rows(
            connection,
            "SELECT column_id, visible, width_px \
                 FROM track_column_layout_playlist_override \
                 WHERE playlist_id = ?1 \
                 ORDER BY position",
            params![playlist_id.get()],
        )?,
        TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => load_layout_rows(
            connection,
            "SELECT column_id, visible, width_px \
                 FROM track_column_layout_smart_playlist_override \
                 WHERE smart_playlist_id = ?1 \
                 ORDER BY position",
            params![smart_playlist_id.get()],
        )?,
    };

    if entries.is_empty() {
        Ok(None)
    } else {
        let sort = load_track_column_sort(connection, scope)?;
        Ok(Some(TrackColumnLayout { entries, sort }))
    }
}

fn load_track_column_sort(
    connection: &Connection,
    scope: TrackColumnLayoutScope,
) -> StoreResult<Option<TrackColumnSort>> {
    let map = |row: &rusqlite::Row<'_>| -> rusqlite::Result<TrackColumnSort> {
        Ok(TrackColumnSort {
            column_id: row.get(0)?,
            ascending: row.get::<_, i64>(1)? != 0,
        })
    };
    let result = match scope {
        TrackColumnLayoutScope::Default => connection.query_row(
            "SELECT column_id, ascending FROM track_column_sort_default WHERE id = 0",
            params![],
            map,
        ),
        TrackColumnLayoutScope::Playlist(playlist_id) => connection.query_row(
            "SELECT column_id, ascending \
                 FROM track_column_sort_playlist_override \
                 WHERE playlist_id = ?1",
            params![playlist_id.get()],
            map,
        ),
        TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => connection.query_row(
            "SELECT column_id, ascending \
                 FROM track_column_sort_smart_playlist_override \
                 WHERE smart_playlist_id = ?1",
            params![smart_playlist_id.get()],
            map,
        ),
    };
    result.optional().map_err(StoreError::from)
}

pub(super) fn save_track_column_layout(
    connection: &mut Connection,
    scope: TrackColumnLayoutScope,
    layout: &TrackColumnLayout,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;

    match scope {
        TrackColumnLayoutScope::Default => {
            transaction
                .execute("DELETE FROM track_column_layout_default", params![])
                .map_err(StoreError::from)?;
            for (position, entry) in layout.entries.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO track_column_layout_default \
                             (column_id, position, visible, width_px) \
                             VALUES (?1, ?2, ?3, ?4)",
                        params![
                            entry.column_id,
                            position as i64,
                            i64::from(entry.visible),
                            i64::from(entry.width_px),
                        ],
                    )
                    .map_err(StoreError::from)?;
            }
        }
        TrackColumnLayoutScope::Playlist(playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_layout_playlist_override \
                         WHERE playlist_id = ?1",
                    params![playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            for (position, entry) in layout.entries.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO track_column_layout_playlist_override \
                             (playlist_id, column_id, position, visible, width_px) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            playlist_id.get(),
                            entry.column_id,
                            position as i64,
                            i64::from(entry.visible),
                            i64::from(entry.width_px),
                        ],
                    )
                    .map_err(StoreError::from)?;
            }
        }
        TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_layout_smart_playlist_override \
                         WHERE smart_playlist_id = ?1",
                    params![smart_playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            for (position, entry) in layout.entries.iter().enumerate() {
                transaction
                    .execute(
                        "INSERT INTO track_column_layout_smart_playlist_override \
                             (smart_playlist_id, column_id, position, visible, width_px) \
                             VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            smart_playlist_id.get(),
                            entry.column_id,
                            position as i64,
                            i64::from(entry.visible),
                            i64::from(entry.width_px),
                        ],
                    )
                    .map_err(StoreError::from)?;
            }
        }
    }

    save_track_column_sort(&transaction, scope, layout.sort.as_ref())?;

    transaction.commit().map_err(StoreError::from)
}

/// Replace the single sort row for `scope` inside the caller's transaction.
/// `None` clears it, so toggling a view back to its natural order is durable.
fn save_track_column_sort(
    transaction: &rusqlite::Transaction<'_>,
    scope: TrackColumnLayoutScope,
    sort: Option<&TrackColumnSort>,
) -> StoreResult<()> {
    match scope {
        TrackColumnLayoutScope::Default => {
            transaction
                .execute("DELETE FROM track_column_sort_default", params![])
                .map_err(StoreError::from)?;
            if let Some(sort) = sort {
                transaction
                    .execute(
                        "INSERT INTO track_column_sort_default (id, column_id, ascending) \
                             VALUES (0, ?1, ?2)",
                        params![sort.column_id, i64::from(sort.ascending)],
                    )
                    .map_err(StoreError::from)?;
            }
        }
        TrackColumnLayoutScope::Playlist(playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_sort_playlist_override WHERE playlist_id = ?1",
                    params![playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            if let Some(sort) = sort {
                transaction
                    .execute(
                        "INSERT INTO track_column_sort_playlist_override \
                             (playlist_id, column_id, ascending) \
                             VALUES (?1, ?2, ?3)",
                        params![playlist_id.get(), sort.column_id, i64::from(sort.ascending)],
                    )
                    .map_err(StoreError::from)?;
            }
        }
        TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_sort_smart_playlist_override \
                         WHERE smart_playlist_id = ?1",
                    params![smart_playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            if let Some(sort) = sort {
                transaction
                    .execute(
                        "INSERT INTO track_column_sort_smart_playlist_override \
                             (smart_playlist_id, column_id, ascending) \
                             VALUES (?1, ?2, ?3)",
                        params![
                            smart_playlist_id.get(),
                            sort.column_id,
                            i64::from(sort.ascending)
                        ],
                    )
                    .map_err(StoreError::from)?;
            }
        }
    }
    Ok(())
}

pub(super) fn delete_track_column_layout(
    connection: &mut Connection,
    scope: TrackColumnLayoutScope,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    match scope {
        TrackColumnLayoutScope::Default => {
            transaction
                .execute("DELETE FROM track_column_layout_default", params![])
                .map_err(StoreError::from)?;
            transaction
                .execute("DELETE FROM track_column_sort_default", params![])
                .map_err(StoreError::from)?;
        }
        TrackColumnLayoutScope::Playlist(playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_layout_playlist_override WHERE playlist_id = ?1",
                    params![playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            transaction
                .execute(
                    "DELETE FROM track_column_sort_playlist_override WHERE playlist_id = ?1",
                    params![playlist_id.get()],
                )
                .map_err(StoreError::from)?;
        }
        TrackColumnLayoutScope::SmartPlaylist(smart_playlist_id) => {
            transaction
                .execute(
                    "DELETE FROM track_column_layout_smart_playlist_override \
                         WHERE smart_playlist_id = ?1",
                    params![smart_playlist_id.get()],
                )
                .map_err(StoreError::from)?;
            transaction
                .execute(
                    "DELETE FROM track_column_sort_smart_playlist_override \
                         WHERE smart_playlist_id = ?1",
                    params![smart_playlist_id.get()],
                )
                .map_err(StoreError::from)?;
        }
    }
    transaction.commit().map_err(StoreError::from)
}

fn load_layout_rows(
    connection: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> StoreResult<Vec<TrackColumnEntry>> {
    let mut statement = connection.prepare(sql).map_err(StoreError::from)?;
    let mut rows = statement.query(params).map_err(StoreError::from)?;
    let mut entries = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let column_id: String = row.get(0).map_err(StoreError::from)?;
        let visible_flag: i64 = row.get(1).map_err(StoreError::from)?;
        let width_px: i64 = row.get(2).map_err(StoreError::from)?;
        entries.push(TrackColumnEntry {
            column_id,
            visible: visible_flag != 0,
            width_px: width_px.max(0) as u32,
        });
    }
    Ok(entries)
}
