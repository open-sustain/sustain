// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for audio analysis, waveforms and acoustics.

use std::time::Duration;

use sustain_domain::Bpm;

use super::*;

pub(super) fn record_analysis(
    connection: &mut Connection,
    waveform_cache: &WaveformCacheStatus,
    track_id: TrackId,
    analysis: &TrackAnalysis,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
) -> StoreResult<()> {
    if capabilities.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction().map_err(StoreError::from)?;
    if !super::tracks::is_in_library(&transaction, track_id)? {
        return Ok(());
    }

    let waveform = (!analysis.waveform_detail.segments.is_empty())
        .then_some((&analysis.waveform_preview, &analysis.waveform_detail));
    record_attempt(
        &transaction,
        waveform_cache,
        track_id,
        capabilities,
        context,
        waveform,
    )?;

    if capabilities.bpm
        && let Some(bpm) = analysis.bpm
    {
        transaction
            .execute(
                REPLACE_TRACK_BPM_UNLESS_SET_BY_USER_SQL,
                params![i64::from(bpm.hundredths()), track_id.get()],
            )
            .map_err(StoreError::from)?;
    }

    if capabilities.bpm {
        match analysis.beat_grid {
            Some(grid) => transaction.execute(
                UPSERT_TRACK_BEAT_GRID_SQL,
                params![
                    track_id.get(),
                    i64::from(grid.tempo().hundredths()),
                    i64::try_from(grid.first_beat().as_micros()).map_err(|_| {
                        StoreError::Database("beat grid first beat exceeds INTEGER".into())
                    })?,
                    i64::from(grid.first_beat_number()),
                ],
            ),
            None => transaction.execute(DELETE_TRACK_BEAT_GRID_SQL, params![track_id.get()]),
        }
        .map_err(StoreError::from)?;
    }

    if capabilities.key
        && let Some(key) = analysis.key
    {
        transaction
            .execute(
                REPLACE_TRACK_MUSICAL_KEY_UNLESS_SET_BY_USER_SQL,
                params![key.short_code(), track_id.get()],
            )
            .map_err(StoreError::from)?;
    }

    if capabilities.audio
        && let Some(acoustics) = analysis.acoustics
        && acoustics.all_finite()
    {
        transaction
            .execute(
                UPSERT_TRACK_ACOUSTICS_SQL,
                params![
                    track_id.get(),
                    f64::from(acoustics.integrated_lufs),
                    f64::from(acoustics.short_term_lufs_max),
                    f64::from(acoustics.loudness_range_lu),
                    f64::from(acoustics.onset_rate_hz),
                    f64::from(acoustics.low_band_ratio),
                    f64::from(acoustics.mid_band_ratio),
                    f64::from(acoustics.high_band_ratio),
                    f64::from(acoustics.low_band_variation),
                    f64::from(acoustics.tonalness),
                ],
            )
            .map_err(StoreError::from)?;
    }

    transaction.commit().map_err(StoreError::from)
}

pub(super) fn record_analysis_attempt_failure(
    connection: &mut Connection,
    waveform_cache: &WaveformCacheStatus,
    track_id: TrackId,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
) -> StoreResult<()> {
    if capabilities.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction().map_err(StoreError::from)?;
    if !super::tracks::is_in_library(&transaction, track_id)? {
        return Ok(());
    }
    record_attempt(
        &transaction,
        waveform_cache,
        track_id,
        capabilities,
        context,
        None,
    )?;
    transaction.commit().map_err(StoreError::from)
}

/// Stamp an attempt at `capabilities` and, for an audio attempt, record
/// its waveform outcome under a fresh token — while the cache is
/// available: an in-memory stand-in would only grow for nothing.
fn record_attempt(
    connection: &Connection,
    waveform_cache: &WaveformCacheStatus,
    track_id: TrackId,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
    waveform: Option<(&WaveformSegments, &WaveformSegments)>,
) -> StoreResult<()> {
    let audio_attempt_token = if capabilities.audio {
        let token: i64 = connection
            .query_row("SELECT random()", [], |row| row.get(0))
            .map_err(StoreError::from)?;
        Some(token)
    } else {
        None
    };
    upsert_track_analysis(
        connection,
        track_id,
        capabilities,
        context,
        audio_attempt_token,
    )?;
    if let Some(token) = audio_attempt_token
        && waveform_cache.is_available()
    {
        let (preview, detail) = waveform.unzip();
        connection
            .execute(
                UPSERT_TRACK_WAVEFORM_SQL,
                params![
                    track_id.get(),
                    token,
                    preview.map(|preview| f64::from(preview.segment_duration_ms)),
                    preview.map(|preview| waveform_segments_to_blob(&preview.segments)),
                    detail.map(|detail| f64::from(detail.segment_duration_ms)),
                    detail.map(|detail| waveform_segments_to_blob(&detail.segments)),
                ],
            )
            .map_err(StoreError::from)?;
    }
    Ok(())
}

pub(super) fn tracks_needing_analysis(
    connection: &Connection,
    waveform_cache: &WaveformCacheStatus,
    capabilities: AnalysisCapabilities,
    versions: AnalyzerVersions,
    limit: usize,
) -> StoreResult<Vec<TrackId>> {
    if capabilities.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection
        .prepare(SELECT_TRACKS_NEEDING_ANALYSIS_SQL)
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![
            i64::from(capabilities.bpm),
            i64::from(capabilities.key),
            i64::from(capabilities.audio),
            i64::from(versions.bpm),
            i64::from(versions.key),
            i64::from(versions.audio),
            limit as i64,
            i64::from(waveform_cache.is_available()),
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

pub(super) fn filter_tracks_needing_analysis(
    connection: &Connection,
    waveform_cache: &WaveformCacheStatus,
    track_ids: &[TrackId],
    capabilities: AnalysisCapabilities,
    versions: AnalyzerVersions,
) -> StoreResult<Vec<TrackId>> {
    if capabilities.is_empty() || track_ids.is_empty() {
        return Ok(Vec::new());
    }
    let mut needing: HashSet<TrackId> = HashSet::with_capacity(track_ids.len());
    for chunk in track_ids.chunks(FILTER_IN_LIST_CHUNK_SIZE) {
        let sql = build_filter_tracks_needing_analysis_sql(chunk.len());
        let mut statement = connection.prepare(&sql).map_err(StoreError::from)?;
        let mut params: Vec<SqlValue> =
            chunk.iter().map(|id| SqlValue::Integer(id.get())).collect();
        params.push(SqlValue::Integer(i64::from(capabilities.bpm)));
        params.push(SqlValue::Integer(i64::from(capabilities.key)));
        params.push(SqlValue::Integer(i64::from(capabilities.audio)));
        params.push(SqlValue::Integer(i64::from(versions.bpm)));
        params.push(SqlValue::Integer(i64::from(versions.key)));
        params.push(SqlValue::Integer(i64::from(versions.audio)));
        params.push(SqlValue::Integer(i64::from(waveform_cache.is_available())));
        let mut rows = statement
            .query(params_from_iter(params.iter()))
            .map_err(StoreError::from)?;
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let raw: i64 = row.get(0).map_err(StoreError::from)?;
            let id = TrackId::new(raw).ok_or(StoreError::InvalidStoredId(raw))?;
            needing.insert(id);
        }
    }
    // Preserve caller order — playlist order is what the user
    // sees, and downstream FIFO dispatch carries that order
    // through to the scheduler.
    Ok(track_ids
        .iter()
        .copied()
        .filter(|id| needing.contains(id))
        .collect())
}

pub(super) fn load_waveform(
    connection: &Connection,
    track_id: TrackId,
) -> StoreResult<Option<StoredWaveform>> {
    let mut statement = connection
        .prepare(SELECT_TRACK_WAVEFORM_SQL)
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![track_id.get()])
        .map_err(StoreError::from)?;
    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    let preview_duration: f64 = row.get(0).map_err(StoreError::from)?;
    let preview_bytes: Vec<u8> = row.get(1).map_err(StoreError::from)?;
    let detail_duration: f64 = row.get(2).map_err(StoreError::from)?;
    let detail_bytes: Vec<u8> = row.get(3).map_err(StoreError::from)?;
    Ok(Some(StoredWaveform {
        preview: WaveformSegments {
            segment_duration_ms: preview_duration as f32,
            segments: blob_to_waveform_segments(&preview_bytes),
        },
        detail: WaveformSegments {
            segment_duration_ms: detail_duration as f32,
            segments: blob_to_waveform_segments(&detail_bytes),
        },
    }))
}

pub(super) fn load_beat_grid(
    connection: &Connection,
    track_id: TrackId,
) -> StoreResult<Option<BeatGrid>> {
    let mut statement = connection
        .prepare(SELECT_TRACK_BEAT_GRID_SQL)
        .map_err(StoreError::from)?;
    let mut rows = statement
        .query(params![track_id.get()])
        .map_err(StoreError::from)?;
    let Some(row) = rows.next().map_err(StoreError::from)? else {
        return Ok(None);
    };
    beat_grid_from_row(track_id, row, 0).map(Some)
}

pub(super) fn load_beat_grids(
    connection: &Connection,
    track_ids: &[TrackId],
) -> StoreResult<HashMap<TrackId, BeatGrid>> {
    let mut grids = HashMap::with_capacity(track_ids.len());
    for chunk in track_ids.chunks(FILTER_IN_LIST_CHUNK_SIZE) {
        let placeholders = (1..=chunk.len())
            .map(|index| format!("?{index}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!(
            "SELECT track_id, tempo_hundredths, first_beat_micros, first_beat_number
FROM track_beat_grid
WHERE track_id IN ({placeholders})"
        );
        let mut statement = connection.prepare(&sql).map_err(StoreError::from)?;
        let mut rows = statement
            .query(params_from_iter(chunk.iter().map(|id| id.get())))
            .map_err(StoreError::from)?;
        while let Some(row) = rows.next().map_err(StoreError::from)? {
            let raw: i64 = row.get(0).map_err(StoreError::from)?;
            let track_id = TrackId::new(raw).ok_or(StoreError::InvalidStoredId(raw))?;
            grids.insert(track_id, beat_grid_from_row(track_id, row, 1)?);
        }
    }
    Ok(grids)
}

/// Decode a `track_beat_grid` row whose tempo, first beat and first beat
/// number start at column `first`.
fn beat_grid_from_row(
    track_id: TrackId,
    row: &rusqlite::Row<'_>,
    first: usize,
) -> StoreResult<BeatGrid> {
    let tempo: i64 = row.get(first).map_err(StoreError::from)?;
    let first_beat_micros: i64 = row.get(first + 1).map_err(StoreError::from)?;
    let first_beat_number: i64 = row.get(first + 2).map_err(StoreError::from)?;
    let invalid = || {
        StoreError::Database(format!(
            "beat grid of track {}: tempo {tempo}, first beat {first_beat_micros} µs, number {first_beat_number}",
            track_id.get()
        ))
    };
    let tempo = u32::try_from(tempo)
        .ok()
        .and_then(Bpm::from_hundredths)
        .ok_or_else(invalid)?;
    let first_beat = u64::try_from(first_beat_micros)
        .map(Duration::from_micros)
        .map_err(|_| invalid())?;
    let first_beat_number = u8::try_from(first_beat_number).map_err(|_| invalid())?;
    BeatGrid::new(tempo, first_beat, first_beat_number).map_err(|_| invalid())
}

pub(super) fn load_all_acoustics(
    connection: &Connection,
) -> StoreResult<Vec<(TrackId, AcousticFeatures)>> {
    let mut statement = connection
        .prepare(SELECT_ALL_TRACK_ACOUSTICS_SQL)
        .map_err(StoreError::from)?;
    let mut rows = statement.query([]).map_err(StoreError::from)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(StoreError::from)? {
        let raw: i64 = row.get(0).map_err(StoreError::from)?;
        let Some(track_id) = TrackId::new(raw) else {
            continue;
        };
        let value = |index: usize| -> StoreResult<f32> {
            Ok(row.get::<_, f64>(index).map_err(StoreError::from)? as f32)
        };
        let features = AcousticFeatures {
            integrated_lufs: value(1)?,
            short_term_lufs_max: value(2)?,
            loudness_range_lu: value(3)?,
            onset_rate_hz: value(4)?,
            low_band_ratio: value(5)?,
            mid_band_ratio: value(6)?,
            high_band_ratio: value(7)?,
            low_band_variation: value(8)?,
            tonalness: value(9)?,
        };
        if features.all_finite() {
            out.push((track_id, features));
        }
    }
    Ok(out)
}

fn build_filter_tracks_needing_analysis_sql(id_count: usize) -> String {
    debug_assert!(id_count > 0);
    let id_placeholders = (1..=id_count)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");
    let bpm = id_count + 1;
    let key = id_count + 2;
    let audio = id_count + 3;
    let bpm_version = id_count + 4;
    let key_version = id_count + 5;
    let audio_version = id_count + 6;
    let cache_available = id_count + 7;
    format!(
        "SELECT t.id FROM tracks t
LEFT JOIN track_analysis ta ON ta.track_id = t.id
WHERE t.is_missing = 0
  AND t.id IN ({id_placeholders})
  AND (
        (?{bpm} = 1 AND (ta.bpm_attempted_at_unix   IS NULL OR ta.bpm_analyzer_version   < ?{bpm_version}))
     OR (?{key} = 1 AND (ta.key_attempted_at_unix   IS NULL OR ta.key_analyzer_version   < ?{key_version}))
     OR (?{audio} = 1 AND (ta.audio_attempted_at_unix IS NULL OR ta.audio_analyzer_version < ?{audio_version}
                     OR (?{cache_available} = 1 AND NOT EXISTS (
                            SELECT 1 FROM waveform_cache.track_waveform w
                            WHERE w.track_id = t.id
                              AND w.audio_attempt_token = ta.audio_attempt_token))))
      )"
    )
}

/// Shared upsert helper for [`SqliteLibraryStore::record_analysis`] and
/// [`SqliteLibraryStore::record_analysis_attempt_failure`]. NULL is
/// passed for any `*_attempted_at_unix` column the caller did not
/// request, so the SQL's `COALESCE` preserves the existing value.
fn upsert_track_analysis(
    connection: &Connection,
    track_id: TrackId,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
    audio_attempt_token: Option<i64>,
) -> StoreResult<()> {
    let at = |covered: bool| covered.then_some(context.now_unix);
    let version = |covered: bool, version: u32| covered.then_some(i64::from(version));
    connection
        .execute(
            UPSERT_TRACK_ANALYSIS_SQL,
            params![
                track_id.get(),
                at(capabilities.bpm),
                at(capabilities.key),
                at(capabilities.audio),
                audio_attempt_token,
                version(capabilities.bpm, context.versions.bpm),
                version(capabilities.key, context.versions.key),
                version(capabilities.audio, context.versions.audio),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}

#[cfg(test)]
mod tests {
    use rusqlite::{Connection, params};
    use sustain_domain::TrackId;

    use super::load_beat_grid;
    use crate::{StoreError, migrations};

    #[test]
    fn a_stored_grid_that_describes_no_grid_is_an_error() {
        let connection = Connection::open_in_memory().expect("connection");
        migrations::apply_pending(&connection).expect("schema");
        connection
            .execute(
                "INSERT INTO tracks (id, relative_path, title) VALUES (1, x'61', 'Title')",
                [],
            )
            .expect("seed track");
        // 120 BPM beats every 500 ms: a first beat at 500 ms is the second.
        connection
            .execute(
                "INSERT INTO track_beat_grid VALUES (1, ?1, ?2, 1)",
                params![12_000, 500_000],
            )
            .expect("seed grid");

        let track_id = TrackId::new(1).expect("track id");
        assert!(matches!(
            load_beat_grid(&connection, track_id),
            Err(StoreError::Database(_))
        ));
    }
}
