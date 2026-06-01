// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for playlists and playlist folders.

use super::*;

pub(super) fn save_playlist(connection: &mut Connection, playlist: Playlist) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    transaction
        .execute(
            r#"
                INSERT INTO playlists (id, name, parent_folder_id, position)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    parent_folder_id = excluded.parent_folder_id,
                    position = excluded.position
                "#,
            params![
                playlist.id.get(),
                playlist.name,
                playlist.parent_folder_id.map(PlaylistFolderId::get),
                i64::from(playlist.position),
            ],
        )
        .map_err(StoreError::from)?;
    replace_playlist_entries(&transaction, &playlist)?;

    transaction.commit().map_err(StoreError::from)
}

pub(super) fn replace_playlist_entries(
    connection: &Connection,
    playlist: &Playlist,
) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM playlist_entries WHERE playlist_id = ?1",
            params![playlist.id.get()],
        )
        .map_err(StoreError::from)?;

    for entry in &playlist.entries {
        connection
            .execute(
                r#"
                    INSERT INTO playlist_entries (playlist_id, track_id, position)
                    VALUES (?1, ?2, ?3)
                    "#,
                params![
                    entry.playlist_id.get(),
                    entry.track_id.get(),
                    i64::from(entry.position),
                ],
            )
            .map_err(StoreError::from)?;
    }
    Ok(())
}

pub(super) fn playlist(
    connection: &Connection,
    playlist_id: PlaylistId,
) -> StoreResult<Option<Playlist>> {
    let mut statement = connection
        .prepare("SELECT id, name, parent_folder_id, position FROM playlists WHERE id = ?1")
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![playlist_id.get()])
        .map_err(StoreError::from)?;

    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    let id = playlist_id_from_db(row.get(0).map_err(StoreError::from)?)?;
    let name = row.get(1).map_err(StoreError::from)?;
    let parent_folder_id = optional_playlist_folder_id_from_row(row, 2)?;
    let position = u32_from_row(row, 3)?;
    let entries = playlist_entries(connection, id)?;

    Ok(Some(Playlist {
        id,
        name,
        parent_folder_id,
        position,
        entries,
    }))
}

pub(super) fn playlists(connection: &Connection) -> StoreResult<Vec<Playlist>> {
    let mut statement = connection
        .prepare("SELECT id, name, parent_folder_id, position FROM playlists ORDER BY id")
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut playlists = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let id = playlist_id_from_db(row.get(0).map_err(StoreError::from)?)?;
        let name = row.get(1).map_err(StoreError::from)?;
        let parent_folder_id = optional_playlist_folder_id_from_row(row, 2)?;
        let position = u32_from_row(row, 3)?;
        playlists.push(Playlist {
            id,
            name,
            parent_folder_id,
            position,
            entries: playlist_entries(connection, id)?,
        });
    }

    Ok(playlists)
}

pub(super) fn delete_playlist(connection: &Connection, playlist_id: PlaylistId) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM playlists WHERE id = ?1",
            params![playlist_id.get()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn save_playlist_folder(
    connection: &Connection,
    folder: PlaylistFolder,
) -> StoreResult<()> {
    connection
        .execute(
            r#"
                INSERT INTO playlist_folders (id, name, parent_folder_id, position)
                VALUES (?1, ?2, ?3, ?4)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    parent_folder_id = excluded.parent_folder_id,
                    position = excluded.position
                "#,
            params![
                folder.id.get(),
                folder.name,
                folder.parent_folder_id.map(PlaylistFolderId::get),
                i64::from(folder.position),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn playlist_folder(
    connection: &Connection,
    folder_id: PlaylistFolderId,
) -> StoreResult<Option<PlaylistFolder>> {
    let mut statement = connection
        .prepare("SELECT id, name, parent_folder_id, position FROM playlist_folders WHERE id = ?1")
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![folder_id.get()])
        .map_err(StoreError::from)?;

    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    Ok(Some(playlist_folder_from_row(row)?))
}

pub(super) fn playlist_folders(connection: &Connection) -> StoreResult<Vec<PlaylistFolder>> {
    let mut statement = connection
        .prepare("SELECT id, name, parent_folder_id, position FROM playlist_folders ORDER BY id")
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut folders = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        folders.push(playlist_folder_from_row(row)?);
    }

    Ok(folders)
}

pub(super) fn delete_playlist_folder(
    connection: &Connection,
    folder_id: PlaylistFolderId,
) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM playlist_folders WHERE id = ?1",
            params![folder_id.get()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}
