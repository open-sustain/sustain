// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for smart playlists.

use super::*;

pub(super) fn save_smart_playlist(
    connection: &mut Connection,
    smart_playlist: SmartPlaylist,
) -> StoreResult<()> {
    let transaction = connection.transaction().map_err(StoreError::from)?;
    let (limit_count, limit_selection) = match smart_playlist.rules.limit {
        Some(limit) => (
            Some(i64::from(limit.count.get())),
            Some(limit_selection_name(limit.selection).to_owned()),
        ),
        None => (None, None),
    };
    transaction
        .execute(
            r#"
                INSERT INTO smart_playlists (
                    id, name, parent_folder_id, position, match_kind, limit_count, limit_selection
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                ON CONFLICT(id) DO UPDATE SET
                    name = excluded.name,
                    parent_folder_id = excluded.parent_folder_id,
                    position = excluded.position,
                    match_kind = excluded.match_kind,
                    limit_count = excluded.limit_count,
                    limit_selection = excluded.limit_selection
                "#,
            params![
                smart_playlist.id.get(),
                smart_playlist.name,
                smart_playlist.parent_folder_id.map(PlaylistFolderId::get),
                i64::from(smart_playlist.position),
                match_kind_name(smart_playlist.rules.match_kind),
                limit_count,
                limit_selection,
            ],
        )
        .map_err(StoreError::from)?;

    transaction
        .execute(
            "DELETE FROM smart_playlist_rules WHERE smart_playlist_id = ?1",
            params![smart_playlist.id.get()],
        )
        .map_err(StoreError::from)?;

    for (position, rule) in smart_playlist.rules.rules.iter().enumerate() {
        let row = rule_to_columns(rule);
        transaction
            .execute(
                r#"
                    INSERT INTO smart_playlist_rules (
                        smart_playlist_id, position, kind, field, text_operator, text_value,
                        number_operator, number_value, rating_stars, date_unix, days_value,
                        bool_value
                    )
                    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
                    "#,
                params![
                    smart_playlist.id.get(),
                    position as i64,
                    row.kind,
                    row.field,
                    row.text_operator,
                    row.text_value,
                    row.number_operator,
                    row.number_value,
                    row.rating_stars,
                    row.date_unix,
                    row.days_value,
                    row.bool_value,
                ],
            )
            .map_err(StoreError::from)?;
    }

    transaction.commit().map_err(StoreError::from)
}

pub(super) fn smart_playlist(
    connection: &Connection,
    smart_playlist_id: SmartPlaylistId,
) -> StoreResult<Option<SmartPlaylist>> {
    let mut statement = connection
            .prepare(
                r#"
                SELECT id, name, parent_folder_id, position, match_kind, limit_count, limit_selection
                FROM smart_playlists
                WHERE id = ?1
                "#,
            )
            .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![smart_playlist_id.get()])
        .map_err(StoreError::from)?;

    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    let id = smart_playlist_id_from_db(row.get(0).map_err(StoreError::from)?)?;
    let name = row.get(1).map_err(StoreError::from)?;
    let parent_folder_id = optional_playlist_folder_id_from_row(row, 2)?;
    let position = u32_from_row(row, 3)?;
    let match_kind = match_kind_from_name(&row.get::<_, String>(4).map_err(StoreError::from)?)?;
    let limit_count = optional_i64(row, 5)?;
    let limit_selection_name = optional_string(row, 6)?;
    let limit = build_limit(limit_count, limit_selection_name.as_deref())?;
    let rules = load_smart_playlist_rules(connection, id)?;

    Ok(Some(SmartPlaylist {
        id,
        name,
        parent_folder_id,
        position,
        rules: SmartPlaylistRuleSet {
            match_kind,
            rules,
            limit,
        },
    }))
}

pub(super) fn smart_playlists(connection: &Connection) -> StoreResult<Vec<SmartPlaylist>> {
    let mut statement = connection
            .prepare(
                r#"
                SELECT id, name, parent_folder_id, position, match_kind, limit_count, limit_selection
                FROM smart_playlists
                ORDER BY id
                "#,
            )
            .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut smart_playlists = Vec::new();

    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let id = smart_playlist_id_from_db(row.get(0).map_err(StoreError::from)?)?;
        let name = row.get(1).map_err(StoreError::from)?;
        let parent_folder_id = optional_playlist_folder_id_from_row(row, 2)?;
        let position = u32_from_row(row, 3)?;
        let match_kind = match_kind_from_name(&row.get::<_, String>(4).map_err(StoreError::from)?)?;
        let limit_count = optional_i64(row, 5)?;
        let limit_selection_name = optional_string(row, 6)?;
        let limit = build_limit(limit_count, limit_selection_name.as_deref())?;
        let rules = load_smart_playlist_rules(connection, id)?;
        smart_playlists.push(SmartPlaylist {
            id,
            name,
            parent_folder_id,
            position,
            rules: SmartPlaylistRuleSet {
                match_kind,
                rules,
                limit,
            },
        });
    }

    Ok(smart_playlists)
}

pub(super) fn delete_smart_playlist(
    connection: &Connection,
    smart_playlist_id: SmartPlaylistId,
) -> StoreResult<()> {
    connection
        .execute(
            "DELETE FROM smart_playlists WHERE id = ?1",
            params![smart_playlist_id.get()],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}
