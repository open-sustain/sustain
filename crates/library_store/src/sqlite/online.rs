// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for online-enrichment scheduling.

use super::*;

pub(super) fn record_online_attempt(
    connection: &Connection,
    track_id: TrackId,
    capabilities: OnlineCapabilities,
    context: OnlineContext,
) -> StoreResult<()> {
    if capabilities.is_empty() {
        return Ok(());
    }
    let at = |covered: bool| covered.then_some(context.now_unix);
    let version = |covered: bool| covered.then_some(i64::from(context.provider_version));
    connection
        .execute(
            UPSERT_TRACK_ONLINE_STATUS_SQL,
            params![
                track_id.get(),
                at(capabilities.artwork),
                at(capabilities.tags),
                at(capabilities.lyrics),
                version(capabilities.artwork),
                version(capabilities.tags),
                version(capabilities.lyrics),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

pub(super) fn tracks_needing_online(
    connection: &Connection,
    capabilities: OnlineCapabilities,
    provider_version: u32,
    limit: usize,
) -> StoreResult<Vec<TrackId>> {
    if capabilities.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(SELECT_TRACKS_NEEDING_ONLINE_SQL)
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![
            i64::from(capabilities.artwork),
            i64::from(capabilities.tags),
            i64::from(capabilities.lyrics),
            i64::from(provider_version),
            limit as i64,
        ])
        .map_err(StoreError::from)?;
    let mut ids = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let raw: i64 = row.get(0).map_err(StoreError::from)?;
        let id = TrackId::new(raw).ok_or(StoreError::InvalidStoredId(raw))?;
        ids.push(id);
    }
    Ok(ids)
}

pub(super) fn filter_tracks_needing_online(
    connection: &Connection,
    track_ids: &[TrackId],
    capabilities: OnlineCapabilities,
    provider_version: u32,
) -> StoreResult<Vec<TrackId>> {
    if capabilities.is_empty() || track_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut needing: HashSet<TrackId> = HashSet::with_capacity(track_ids.len());
    for chunk in track_ids.chunks(FILTER_IN_LIST_CHUNK_SIZE) {
        let sql = build_filter_tracks_needing_online_sql(chunk.len());
        let mut statement = connection.prepare(&sql).map_err(StoreError::from)?;
        let mut params: Vec<SqlValue> =
            chunk.iter().map(|id| SqlValue::Integer(id.get())).collect();
        params.push(SqlValue::Integer(i64::from(capabilities.artwork)));
        params.push(SqlValue::Integer(i64::from(capabilities.tags)));
        params.push(SqlValue::Integer(i64::from(capabilities.lyrics)));
        params.push(SqlValue::Integer(i64::from(provider_version)));
        let mut rows = statement
            .query(params_from_iter(params.iter()))
            .map_err(StoreError::from)?;
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let raw: i64 = row.get(0).map_err(StoreError::from)?;
            let id = TrackId::new(raw).ok_or(StoreError::InvalidStoredId(raw))?;
            needing.insert(id);
        }
    }
    Ok(track_ids
        .iter()
        .copied()
        .filter(|id| needing.contains(id))
        .collect())
}

fn build_filter_tracks_needing_online_sql(id_count: usize) -> String {
    debug_assert!(id_count > 0);
    let id_placeholders = (1..=id_count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let artwork = id_count + 1;
    let tags = id_count + 2;
    let lyrics = id_count + 3;
    let version = id_count + 4;
    // Mirrors `SELECT_TRACKS_NEEDING_ONLINE_SQL`: the artwork branch
    // still excludes tracks with embedded artwork so we never fetch a
    // remote picture for a file that already carries one.
    format!(
        "SELECT t.id FROM tracks t
LEFT JOIN track_online_status s ON s.track_id = t.id
WHERE t.is_missing = 0
  AND t.id IN ({id_placeholders})
  AND (
        (?{artwork} = 1
            AND COALESCE(t.has_embedded_artwork, 0) = 0
            AND (s.artwork_attempted_at_unix IS NULL OR s.artwork_provider_version < ?{version}))
     OR (?{tags}    = 1 AND (s.tags_attempted_at_unix   IS NULL OR s.tags_provider_version   < ?{version}))
     OR (?{lyrics}  = 1 AND (s.lyrics_attempted_at_unix IS NULL OR s.lyrics_provider_version < ?{version}))
      )"
    )
}
