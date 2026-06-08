// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! SQLite `LibraryStore` operations for audio analysis, waveforms and acoustics.

use super::*;

pub(super) fn record_analysis(
    connection: &mut Connection,
    track_id: TrackId,
    analysis: &TrackAnalysis,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
) -> StoreResult<()> {
    if capabilities.is_empty() {
        return Ok(());
    }
    let transaction = connection.transaction().map_err(StoreError::from)?;

    upsert_track_analysis(&transaction, track_id, capabilities, context)?;

    if capabilities.audio && !analysis.waveform_detail.segments.is_empty() {
        transaction
            .execute(
                UPSERT_TRACK_WAVEFORM_SQL,
                params![
                    track_id.get(),
                    f64::from(analysis.waveform_preview.segment_duration_ms),
                    waveform_segments_to_blob(&analysis.waveform_preview.segments),
                    f64::from(analysis.waveform_detail.segment_duration_ms),
                    waveform_segments_to_blob(&analysis.waveform_detail.segments),
                ],
            )
            .map_err(StoreError::from)?;
    }

    if capabilities.bpm
        && let Some(bpm) = analysis.bpm.and_then(sustain_domain::validate_analysis_bpm)
    {
        transaction
            .execute(
                FILL_TRACK_BPM_IF_NULL_SQL,
                params![i64::from(bpm), track_id.get()],
            )
            .map_err(StoreError::from)?;
    }

    if capabilities.key
        && let Some(key) = analysis.key
    {
        transaction
            .execute(
                FILL_TRACK_MUSICAL_KEY_IF_NULL_SQL,
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
    connection: &Connection,
    track_id: TrackId,
    capabilities: AnalysisCapabilities,
    context: AnalysisContext,
) -> StoreResult<()> {
    if capabilities.is_empty() {
        return Ok(());
    }
    upsert_track_analysis(connection, track_id, capabilities, context)
}

pub(super) fn tracks_needing_analysis(
    connection: &Connection,
    capabilities: AnalysisCapabilities,
    analyzer_version: u32,
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
            i64::from(analyzer_version),
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

pub(super) fn filter_tracks_needing_analysis(
    connection: &Connection,
    track_ids: &[TrackId],
    capabilities: AnalysisCapabilities,
    analyzer_version: u32,
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
        params.push(SqlValue::Integer(i64::from(analyzer_version)));
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
    let version = id_count + 4;
    format!(
        "SELECT t.id FROM tracks t
LEFT JOIN track_analysis ta ON ta.track_id = t.id
WHERE t.is_missing = 0
  AND t.id IN ({id_placeholders})
  AND (
        (?{bpm} = 1 AND (ta.bpm_attempted_at_unix   IS NULL OR ta.analyzer_version < ?{version}))
     OR (?{key} = 1 AND (ta.key_attempted_at_unix   IS NULL OR ta.analyzer_version < ?{version}))
     OR (?{audio} = 1 AND (ta.audio_attempted_at_unix IS NULL OR ta.analyzer_version < ?{version}))
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
) -> StoreResult<()> {
    let bpm_at = capabilities.bpm.then_some(context.now_unix);
    let key_at = capabilities.key.then_some(context.now_unix);
    let audio_at = capabilities.audio.then_some(context.now_unix);
    connection
        .execute(
            UPSERT_TRACK_ANALYSIS_SQL,
            params![
                track_id.get(),
                bpm_at,
                key_at,
                audio_at,
                i64::from(context.analyzer_version),
            ],
        )
        .map(|_| ())
        .map_err(StoreError::from)
}
