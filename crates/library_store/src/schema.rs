// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::sync::LazyLock;

// Sustain is in pre-release development: the SQLite schema is not yet stable.
// Schema changes are made by editing these CREATE TABLE statements; any
// existing local database is expected to be wiped and rebuilt from a library
// re-scan, not migrated. Do not add migration code for in-development schemas.
pub(super) const SCHEMA_SQL: &str = r#"
PRAGMA foreign_keys = ON;

CREATE TABLE IF NOT EXISTS tracks (
    id INTEGER PRIMARY KEY,
    -- Stored as the raw bytes of the Unix `OsStr`, not TEXT. Linux
    -- filenames are arbitrary byte sequences and need not be UTF-8;
    -- coercing them through `to_string_lossy` would replace invalid
    -- bytes with U+FFFD, making the file unreachable after restart and
    -- letting two distinct byte paths collapse onto one UNIQUE row. The
    -- store-layer codec round-trips this BLOB to a `PathBuf` exactly.
    relative_path BLOB NOT NULL UNIQUE,
    title TEXT,
    artist TEXT,
    album TEXT,
    album_artist TEXT,
    composer TEXT,
    genre TEXT,
    track_number INTEGER,
    disc_number INTEGER,
    year INTEGER,
    duration_seconds INTEGER,
    bitrate_kbps INTEGER,
    rating INTEGER NOT NULL DEFAULT 0,
    play_count INTEGER NOT NULL DEFAULT 0,
    skip_count INTEGER NOT NULL DEFAULT 0,
    last_played_at_unix INTEGER,
    last_skipped_at_unix INTEGER,
    date_added_at_unix INTEGER,
    is_missing INTEGER NOT NULL DEFAULT 0,
    grouping TEXT,
    track_total INTEGER,
    disc_total INTEGER,
    compilation INTEGER,
    bpm INTEGER,
    musical_key TEXT,
    comments TEXT,
    sample_rate_hz INTEGER,
    channels INTEGER,
    lyrics TEXT,
    -- No content_hash column: a stored SHA-256 went stale on every
    -- in-place tag/rating/artwork/enrichment write and was never set for
    -- scan-imported tracks, so dedup could not trust it. Import hashes
    -- bytes on disk transiently instead and persists nothing (#72).
    file_size_bytes INTEGER,
    -- Scan-time "does the file carry an embedded picture?" bit.
    -- NULL means the scanner has not observed this file yet (a row
    -- imported from an external source before any scan); 0/1 reflect
    -- the most recent scan. The online artwork scheduler reads this
    -- column directly in its candidate query and never re-probes the
    -- file at attempt time.
    has_embedded_artwork INTEGER,
    -- Tag-derived "sort as" names (issue #13). Captured at import
    -- alongside the display fields and used only for ordering; never
    -- mirrored back to files. NULL when the tag carried no sort field.
    title_sort TEXT,
    artist_sort TEXT,
    album_sort TEXT,
    album_artist_sort TEXT,
    composer_sort TEXT
);

CREATE TABLE IF NOT EXISTS playlist_folders (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    parent_folder_id INTEGER,
    position INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_folder_id) REFERENCES playlist_folders(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS playlists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    parent_folder_id INTEGER,
    position INTEGER NOT NULL DEFAULT 0,
    FOREIGN KEY (parent_folder_id) REFERENCES playlist_folders(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS playlist_entries (
    playlist_id INTEGER NOT NULL,
    track_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, track_id),
    FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE,
    FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS smart_playlists (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    parent_folder_id INTEGER,
    position INTEGER NOT NULL DEFAULT 0,
    match_kind TEXT NOT NULL,
    limit_count INTEGER,
    limit_selection TEXT,
    FOREIGN KEY (parent_folder_id) REFERENCES playlist_folders(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS smart_playlist_rules (
    smart_playlist_id INTEGER NOT NULL,
    position INTEGER NOT NULL,
    kind TEXT NOT NULL,
    field TEXT,
    text_operator TEXT,
    text_value TEXT,
    number_operator TEXT,
    number_value INTEGER,
    rating_stars INTEGER,
    date_unix INTEGER,
    days_value INTEGER,
    PRIMARY KEY (smart_playlist_id, position),
    FOREIGN KEY (smart_playlist_id) REFERENCES smart_playlists(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS track_column_layout_default (
    column_id TEXT PRIMARY KEY,
    position  INTEGER NOT NULL,
    visible   INTEGER NOT NULL,
    width_px  INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS track_column_layout_playlist_override (
    playlist_id INTEGER NOT NULL,
    column_id   TEXT    NOT NULL,
    position    INTEGER NOT NULL,
    visible     INTEGER NOT NULL,
    width_px    INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, column_id),
    FOREIGN KEY (playlist_id) REFERENCES playlists(id) ON DELETE CASCADE
);

CREATE TABLE IF NOT EXISTS track_column_layout_smart_playlist_override (
    smart_playlist_id INTEGER NOT NULL,
    column_id         TEXT    NOT NULL,
    position          INTEGER NOT NULL,
    visible           INTEGER NOT NULL,
    width_px          INTEGER NOT NULL,
    PRIMARY KEY (smart_playlist_id, column_id),
    FOREIGN KEY (smart_playlist_id) REFERENCES smart_playlists(id) ON DELETE CASCADE
);

-- Per-track analysis bookkeeping. Tiny row; never carries BLOB data.
-- The scheduler's "find tracks needing analysis" query LEFT JOINs against
-- this table and tests the *_attempted_at_unix columns to decide whether
-- a capability has been tried yet — distinguishing "not yet attempted"
-- (NULL) from "tried, no result" (timestamp set, tracks.bpm still NULL).
-- analyzer_version is bumped centrally when the DSP changes meaningfully,
-- so older rows are excluded from "fresh enough" checks without any
-- migration step.
CREATE TABLE IF NOT EXISTS track_analysis (
    track_id                  INTEGER PRIMARY KEY
                                REFERENCES tracks(id) ON DELETE CASCADE,
    bpm_attempted_at_unix     INTEGER,
    key_attempted_at_unix     INTEGER,
    audio_attempted_at_unix   INTEGER,
    analyzer_version          INTEGER NOT NULL
);

-- Per-track perceptual acoustic features for Smart Shuffle (loudness,
-- onset density, timbral band ratios, low-band variation, tonalness).
-- Split from track_analysis (which is bookkeeping-only) the same way
-- track_waveform is. Both this table and track_waveform are byproducts
-- of one heavy full-decode pass — the opt-in "audio analysis" — so they
-- share track_analysis.audio_attempted_at_unix. Absence of a row means
-- "not analysed", which the scorer masks.
CREATE TABLE IF NOT EXISTS track_acoustics (
    track_id             INTEGER PRIMARY KEY
                           REFERENCES tracks(id) ON DELETE CASCADE,
    integrated_lufs      REAL NOT NULL,
    short_term_lufs_max  REAL NOT NULL,
    loudness_range_lu    REAL NOT NULL,
    onset_rate_hz        REAL NOT NULL,
    low_band_ratio       REAL NOT NULL,
    mid_band_ratio       REAL NOT NULL,
    high_band_ratio      REAL NOT NULL,
    low_band_variation   REAL NOT NULL,
    tonalness            REAL NOT NULL
);

-- Waveform BLOBs only. Split from track_analysis so a future
-- ATTACH-based relocation of the bulk data to a sidecar database is
-- a schema edit, not a refactor. Each segments BLOB is `n * 4` bytes;
-- segment count is recovered as `blob.len() / 4`.
CREATE TABLE IF NOT EXISTS track_waveform (
    track_id                    INTEGER PRIMARY KEY
                                  REFERENCES tracks(id) ON DELETE CASCADE,
    preview_segment_duration_ms REAL    NOT NULL,
    preview_segments            BLOB    NOT NULL,
    detail_segment_duration_ms  REAL    NOT NULL,
    detail_segments             BLOB    NOT NULL
);

-- Time-coded lyrics. Plain lyrics live on tracks.lyrics (mirrored to
-- the file's standard Lyrics tag); synced lyrics are kept separately
-- because no cross-format tag exists for them and they are heavier
-- (a typical LRC parses to ~5 KB of JSON, larger for verbose songs).
-- Storing the parsed JSON form rather than the raw LRC string lets
-- the player iterate lines without re-parsing on every load.
CREATE TABLE IF NOT EXISTS track_synced_lyrics (
    track_id INTEGER PRIMARY KEY
                  REFERENCES tracks(id) ON DELETE CASCADE,
    lines_json TEXT NOT NULL,
    source     TEXT NOT NULL
);

-- The Smart Shuffle index: the prepared, library-dependent state the
-- picker needs — genre-token IDF and, later, normalization statistics
-- — serialised as an opaque blob in a singleton row (id = 1). There
-- is no trained model; see `sustain_smart_shuffle`. The bookkeeping
-- the preferences caption shows (indexed track count, analysis
-- coverage, build time) lives *inside* the blob; only `schema_version`
-- is broken out, so the runtime can discard a stale-shaped blob
-- without paying to deserialise it.
CREATE TABLE IF NOT EXISTS smart_shuffle_index (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    index_blob BLOB NOT NULL,
    schema_version INTEGER NOT NULL
);

-- Per-track bookkeeping for network-bound retrievals (artwork, tag
-- enrichment, lyrics). Same shape as track_analysis: a NULL
-- *_attempted_at_unix means "not yet tried at the current
-- provider_version", a non-NULL value means "tried, do not keep
-- retrying every cycle even if the field is still empty". The
-- scheduler's "find tracks needing online work" query LEFT JOINs
-- against this table to filter the candidate set without scanning the
-- whole library on every batch.
CREATE TABLE IF NOT EXISTS track_online_status (
    track_id                    INTEGER PRIMARY KEY
                                  REFERENCES tracks(id) ON DELETE CASCADE,
    artwork_attempted_at_unix   INTEGER,
    tags_attempted_at_unix      INTEGER,
    lyrics_attempted_at_unix    INTEGER,
    provider_version            INTEGER NOT NULL
);

-- Durable courtesy mirror work for editable file tags. SQLite remains
-- authoritative: every canonical edit and this compact per-track intent are
-- committed together. The worker resolves the track's current path and writes
-- the latest canonical row, so coalescing never replays stale snapshots.
-- Artwork bytes live in content-addressed external blobs; only their digest
-- and bounded length are stored here.
CREATE TABLE IF NOT EXISTS tag_mirror_outbox (
    track_id                   INTEGER PRIMARY KEY
                                 REFERENCES tracks(id) ON DELETE CASCADE,
    mirror_metadata            INTEGER NOT NULL DEFAULT 0,
    mirror_rating              INTEGER NOT NULL DEFAULT 0,
    artwork_action             INTEGER NOT NULL DEFAULT 0,
    artwork_digest             TEXT,
    artwork_size_bytes         INTEGER,
    generation                 INTEGER NOT NULL,
    attempt_count              INTEGER NOT NULL DEFAULT 0,
    next_attempt_at_unix       INTEGER NOT NULL DEFAULT 0,
    last_error                 TEXT,
    CHECK (mirror_metadata IN (0, 1)),
    CHECK (mirror_rating IN (0, 1)),
    CHECK (artwork_action IN (0, 1, 2)),
    CHECK (mirror_metadata = 1 OR mirror_rating = 1 OR artwork_action <> 0),
    CHECK (generation > 0),
    CHECK (attempt_count >= 0),
    CHECK (next_attempt_at_unix >= 0),
    CHECK (
        (artwork_action = 2 AND artwork_digest IS NOT NULL
                            AND artwork_size_bytes BETWEEN 1 AND 16777216)
        OR
        (artwork_action <> 2 AND artwork_digest IS NULL AND artwork_size_bytes IS NULL)
    )
);

-- Disposable source-content hash cache for incremental device export (#100).
-- Keyed by track, never authoritative: a cached SHA-256 is trusted only while
-- every stat(2) field still matches the live source, and Sustain's own
-- tag/rating/artwork rewrites drop the row so the next sync re-hashes and
-- re-copies. Safe to wipe and rebuild at any time.
CREATE TABLE IF NOT EXISTS source_fingerprint_cache (
    track_id       INTEGER PRIMARY KEY,
    device         INTEGER NOT NULL,
    inode          INTEGER NOT NULL,
    size_bytes     INTEGER NOT NULL,
    modified_at_ns INTEGER NOT NULL,
    changed_at_ns  INTEGER NOT NULL,
    sha256         TEXT NOT NULL,
    FOREIGN KEY (track_id) REFERENCES tracks(id) ON DELETE CASCADE
);

-- Device sync (#23/#24). Sustain owns the per-device configuration and
-- the saved playlist selection (the device only carries the files), all
-- keyed by the stable Sustain device id stored in the device's
-- `.sustain-device-id` marker.
CREATE TABLE IF NOT EXISTS sync_devices (
    id                   TEXT PRIMARY KEY,
    label                TEXT NOT NULL,
    kind                 INTEGER NOT NULL,
    layout               INTEGER NOT NULL,
    sub_path             TEXT NOT NULL,
    files_per_folder_cap INTEGER NOT NULL,
    volume_id            TEXT
);

-- Ticked playlists / smart playlists per device (item_kind 0 = playlist,
-- 1 = smart playlist), in display order.
CREATE TABLE IF NOT EXISTS sync_device_playlists (
    device_id TEXT NOT NULL,
    item_kind INTEGER NOT NULL,
    item_id   INTEGER NOT NULL,
    position  INTEGER NOT NULL,
    PRIMARY KEY (device_id, item_kind, item_id),
    FOREIGN KEY (device_id) REFERENCES sync_devices(id) ON DELETE CASCADE
);

-- What Sustain last wrote to a device: track -> on-device path + content
-- fingerprint. Drives the incremental differ. A track may have several
-- rows (the folder-per-playlist layout copies it once per playlist), so
-- the key is (device, on-device path).
CREATE TABLE IF NOT EXISTS sync_manifest (
    device_id      TEXT NOT NULL,
    track_id       INTEGER NOT NULL,
    on_device_path TEXT NOT NULL,
    fingerprint    TEXT NOT NULL,
    PRIMARY KEY (device_id, on_device_path),
    FOREIGN KEY (device_id) REFERENCES sync_devices(id) ON DELETE CASCADE
);
"#;

#[derive(Clone, Copy)]
struct TrackColumn {
    name: &'static str,
}

impl TrackColumn {
    const fn primary_key(name: &'static str) -> Self {
        Self { name }
    }

    const fn stored_value(name: &'static str) -> Self {
        Self { name }
    }
}

const TRACK_COLUMNS: &[TrackColumn] = &[
    TrackColumn::primary_key("id"),
    TrackColumn::stored_value("relative_path"),
    TrackColumn::stored_value("title"),
    TrackColumn::stored_value("artist"),
    TrackColumn::stored_value("album"),
    TrackColumn::stored_value("album_artist"),
    TrackColumn::stored_value("composer"),
    TrackColumn::stored_value("genre"),
    TrackColumn::stored_value("track_number"),
    TrackColumn::stored_value("disc_number"),
    TrackColumn::stored_value("year"),
    TrackColumn::stored_value("duration_seconds"),
    TrackColumn::stored_value("bitrate_kbps"),
    TrackColumn::stored_value("rating"),
    TrackColumn::stored_value("play_count"),
    TrackColumn::stored_value("skip_count"),
    TrackColumn::stored_value("last_played_at_unix"),
    TrackColumn::stored_value("last_skipped_at_unix"),
    TrackColumn::stored_value("date_added_at_unix"),
    TrackColumn::stored_value("is_missing"),
    TrackColumn::stored_value("grouping"),
    TrackColumn::stored_value("track_total"),
    TrackColumn::stored_value("disc_total"),
    TrackColumn::stored_value("compilation"),
    TrackColumn::stored_value("bpm"),
    TrackColumn::stored_value("musical_key"),
    TrackColumn::stored_value("comments"),
    TrackColumn::stored_value("sample_rate_hz"),
    TrackColumn::stored_value("channels"),
    TrackColumn::stored_value("lyrics"),
    TrackColumn::stored_value("file_size_bytes"),
    TrackColumn::stored_value("has_embedded_artwork"),
    TrackColumn::stored_value("title_sort"),
    TrackColumn::stored_value("artist_sort"),
    TrackColumn::stored_value("album_sort"),
    TrackColumn::stored_value("album_artist_sort"),
    TrackColumn::stored_value("composer_sort"),
];

pub(crate) mod track_column_index {
    pub(crate) const ID: usize = 0;
    pub(crate) const RELATIVE_PATH: usize = 1;
    pub(crate) const TITLE: usize = 2;
    pub(crate) const ARTIST: usize = 3;
    pub(crate) const ALBUM: usize = 4;
    pub(crate) const ALBUM_ARTIST: usize = 5;
    pub(crate) const COMPOSER: usize = 6;
    pub(crate) const GENRE: usize = 7;
    pub(crate) const TRACK_NUMBER: usize = 8;
    pub(crate) const DISC_NUMBER: usize = 9;
    pub(crate) const YEAR: usize = 10;
    pub(crate) const DURATION_SECONDS: usize = 11;
    pub(crate) const BITRATE_KBPS: usize = 12;
    pub(crate) const RATING: usize = 13;
    pub(crate) const PLAY_COUNT: usize = 14;
    pub(crate) const SKIP_COUNT: usize = 15;
    pub(crate) const LAST_PLAYED_AT_UNIX: usize = 16;
    pub(crate) const LAST_SKIPPED_AT_UNIX: usize = 17;
    pub(crate) const DATE_ADDED_AT_UNIX: usize = 18;
    pub(crate) const IS_MISSING: usize = 19;
    pub(crate) const GROUPING: usize = 20;
    pub(crate) const TRACK_TOTAL: usize = 21;
    pub(crate) const DISC_TOTAL: usize = 22;
    pub(crate) const COMPILATION: usize = 23;
    pub(crate) const BPM: usize = 24;
    pub(crate) const MUSICAL_KEY: usize = 25;
    pub(crate) const COMMENTS: usize = 26;
    pub(crate) const SAMPLE_RATE_HZ: usize = 27;
    pub(crate) const CHANNELS: usize = 28;
    pub(crate) const LYRICS: usize = 29;
    pub(crate) const FILE_SIZE_BYTES: usize = 30;
    pub(crate) const HAS_EMBEDDED_ARTWORK: usize = 31;
    pub(crate) const TITLE_SORT: usize = 32;
    pub(crate) const ARTIST_SORT: usize = 33;
    pub(crate) const ALBUM_SORT: usize = 34;
    pub(crate) const ALBUM_ARTIST_SORT: usize = 35;
    pub(crate) const COMPOSER_SORT: usize = 36;
}

pub(super) static SAVE_TRACK_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
INSERT INTO tracks (
{}
)
VALUES (
{}
)
"#,
        indented_track_column_names("    "),
        indented_insert_placeholders("    "),
    )
});

pub(super) static RECONCILE_SCANNED_TRACK_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
INSERT INTO tracks (
{}
)
VALUES (
{}
)
ON CONFLICT(id) DO UPDATE SET
    relative_path = excluded.relative_path,
    duration_seconds = excluded.duration_seconds,
    bitrate_kbps = excluded.bitrate_kbps,
    is_missing = excluded.is_missing,
    sample_rate_hz = excluded.sample_rate_hz,
    channels = excluded.channels,
    file_size_bytes = excluded.file_size_bytes,
    has_embedded_artwork = excluded.has_embedded_artwork
"#,
        indented_track_column_names("    "),
        indented_insert_placeholders("    "),
    )
});

pub(super) const APPLY_TRACK_METADATA_CHANGE_SQL: &str = r#"
UPDATE tracks SET
    title        = CASE ?2  WHEN 0 THEN title        WHEN 1 THEN ?3  ELSE NULL END,
    artist       = CASE ?4  WHEN 0 THEN artist       WHEN 1 THEN ?5  ELSE NULL END,
    album        = CASE ?6  WHEN 0 THEN album        WHEN 1 THEN ?7  ELSE NULL END,
    album_artist = CASE ?8  WHEN 0 THEN album_artist WHEN 1 THEN ?9  ELSE NULL END,
    composer     = CASE ?10 WHEN 0 THEN composer     WHEN 1 THEN ?11 ELSE NULL END,
    grouping     = CASE ?12 WHEN 0 THEN grouping     WHEN 1 THEN ?13 ELSE NULL END,
    genre        = CASE ?14 WHEN 0 THEN genre        WHEN 1 THEN ?15 ELSE NULL END,
    track_number = CASE ?16 WHEN 0 THEN track_number WHEN 1 THEN ?17 ELSE NULL END,
    track_total  = CASE ?18 WHEN 0 THEN track_total  WHEN 1 THEN ?19 ELSE NULL END,
    disc_number  = CASE ?20 WHEN 0 THEN disc_number  WHEN 1 THEN ?21 ELSE NULL END,
    disc_total   = CASE ?22 WHEN 0 THEN disc_total   WHEN 1 THEN ?23 ELSE NULL END,
    year         = CASE ?24 WHEN 0 THEN year         WHEN 1 THEN ?25 ELSE NULL END,
    compilation  = CASE ?26 WHEN 0 THEN compilation  WHEN 1 THEN ?27 ELSE NULL END,
    bpm          = CASE ?28 WHEN 0 THEN bpm          WHEN 1 THEN ?29 ELSE NULL END,
    musical_key  = CASE ?30 WHEN 0 THEN musical_key  WHEN 1 THEN ?31 ELSE NULL END,
    comments     = CASE ?32 WHEN 0 THEN comments     WHEN 1 THEN ?33 ELSE NULL END,
    lyrics       = CASE ?34 WHEN 0 THEN lyrics       WHEN 1 THEN ?35 ELSE NULL END
WHERE id = ?1
"#;

pub(super) const FILL_MISSING_TRACK_METADATA_SQL: &str = r#"
UPDATE tracks SET
    title        = COALESCE(title, ?2),
    artist       = COALESCE(artist, ?3),
    album        = COALESCE(album, ?4),
    album_artist = COALESCE(album_artist, ?5),
    composer     = COALESCE(composer, ?6),
    grouping     = COALESCE(grouping, ?7),
    genre        = CASE WHEN genre IS NULL OR TRIM(genre) = '' THEN COALESCE(?8, genre) ELSE genre END,
    track_number = COALESCE(track_number, ?9),
    track_total  = COALESCE(track_total, ?10),
    disc_number  = COALESCE(disc_number, ?11),
    disc_total   = COALESCE(disc_total, ?12),
    year         = COALESCE(year, ?13),
    compilation  = COALESCE(compilation, ?14),
    bpm          = COALESCE(bpm, ?15),
    musical_key  = COALESCE(musical_key, ?16),
    comments     = COALESCE(comments, ?17),
    lyrics       = CASE WHEN lyrics IS NULL OR TRIM(lyrics) = '' THEN COALESCE(?18, lyrics) ELSE lyrics END
WHERE id = ?1
"#;

pub(super) static SELECT_TRACK_BY_ID_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
SELECT
{}
FROM tracks
WHERE id = ?1
"#,
        indented_track_column_names("    "),
    )
});

pub(super) static SELECT_ALL_TRACKS_SQL: LazyLock<String> = LazyLock::new(|| {
    format!(
        r#"
SELECT
{}
FROM tracks
ORDER BY id
"#,
        indented_track_column_names("    "),
    )
});

/// Upsert into `track_analysis`. Each `*_attempted_at_unix` parameter
/// is either the analysis timestamp (if the capability ran this pass)
/// or `NULL` (if it did not) — `COALESCE` preserves whatever value
/// was already stored in that column, so a BPM-only re-analysis does
/// not clobber the waveform's "attempted" timestamp.
pub(super) const UPSERT_TRACK_ANALYSIS_SQL: &str = r#"
INSERT INTO track_analysis (
    track_id,
    bpm_attempted_at_unix,
    key_attempted_at_unix,
    audio_attempted_at_unix,
    analyzer_version
)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(track_id) DO UPDATE SET
    bpm_attempted_at_unix = COALESCE(excluded.bpm_attempted_at_unix, bpm_attempted_at_unix),
    key_attempted_at_unix = COALESCE(excluded.key_attempted_at_unix, key_attempted_at_unix),
    audio_attempted_at_unix = COALESCE(excluded.audio_attempted_at_unix, audio_attempted_at_unix),
    analyzer_version = excluded.analyzer_version
"#;

/// Upsert a track's acoustic features. Overwrites on re-analysis (the
/// feature set is a single-shot store-or-replace, like the waveform).
pub(super) const UPSERT_TRACK_ACOUSTICS_SQL: &str = r#"
INSERT INTO track_acoustics (
    track_id,
    integrated_lufs,
    short_term_lufs_max,
    loudness_range_lu,
    onset_rate_hz,
    low_band_ratio,
    mid_band_ratio,
    high_band_ratio,
    low_band_variation,
    tonalness
)
VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
ON CONFLICT(track_id) DO UPDATE SET
    integrated_lufs     = excluded.integrated_lufs,
    short_term_lufs_max = excluded.short_term_lufs_max,
    loudness_range_lu   = excluded.loudness_range_lu,
    onset_rate_hz       = excluded.onset_rate_hz,
    low_band_ratio      = excluded.low_band_ratio,
    mid_band_ratio      = excluded.mid_band_ratio,
    high_band_ratio     = excluded.high_band_ratio,
    low_band_variation  = excluded.low_band_variation,
    tonalness           = excluded.tonalness
"#;

/// Load every track's acoustic features for the Smart Shuffle index
/// rebuild. Column order matches `LibraryStore::load_all_acoustics`.
pub(super) const SELECT_ALL_TRACK_ACOUSTICS_SQL: &str = r#"
SELECT track_id, integrated_lufs, short_term_lufs_max, loudness_range_lu,
       onset_rate_hz, low_band_ratio, mid_band_ratio, high_band_ratio,
       low_band_variation, tonalness
FROM track_acoustics
"#;

pub(super) const UPSERT_TRACK_WAVEFORM_SQL: &str = r#"
INSERT INTO track_waveform (
    track_id,
    preview_segment_duration_ms,
    preview_segments,
    detail_segment_duration_ms,
    detail_segments
)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(track_id) DO UPDATE SET
    preview_segment_duration_ms = excluded.preview_segment_duration_ms,
    preview_segments = excluded.preview_segments,
    detail_segment_duration_ms = excluded.detail_segment_duration_ms,
    detail_segments = excluded.detail_segments
"#;

/// "Fill in `tracks.bpm` only if it is currently NULL." Honors the
/// rule that user-edited or tag-imported values win — the analyzer
/// supplies missing data, it never overrides existing data.
pub(super) const FILL_TRACK_BPM_IF_NULL_SQL: &str =
    r#"UPDATE tracks SET bpm = ?1 WHERE id = ?2 AND bpm IS NULL"#;

pub(super) const FILL_TRACK_MUSICAL_KEY_IF_NULL_SQL: &str =
    r#"UPDATE tracks SET musical_key = ?1 WHERE id = ?2 AND musical_key IS NULL"#;

pub(super) const SELECT_TRACK_WAVEFORM_SQL: &str = r#"
SELECT
    preview_segment_duration_ms,
    preview_segments,
    detail_segment_duration_ms,
    detail_segments
FROM track_waveform
WHERE track_id = ?1
"#;

/// Upsert the synced-lyrics JSON for a track. Source is a short
/// provider tag (e.g. `"lrclib"`) kept so a later diagnostic can
/// answer "where did this come from?" without consulting logs.
pub(super) const UPSERT_TRACK_SYNCED_LYRICS_SQL: &str = r#"
INSERT INTO track_synced_lyrics (track_id, lines_json, source)
VALUES (?1, ?2, ?3)
ON CONFLICT(track_id) DO UPDATE SET
    lines_json = excluded.lines_json,
    source     = excluded.source
"#;

pub(super) const SELECT_TRACK_SYNCED_LYRICS_SQL: &str = r#"
SELECT lines_json, source
FROM track_synced_lyrics
WHERE track_id = ?1
"#;

pub(super) const DELETE_TRACK_SYNCED_LYRICS_SQL: &str =
    r#"DELETE FROM track_synced_lyrics WHERE track_id = ?1"#;

/// Upsert into `track_online_status`. Each `*_attempted_at_unix`
/// parameter is either the timestamp (capability ran this pass) or
/// `NULL` (capability not requested this pass) — `COALESCE`
/// preserves the existing value so a lyrics-only pass does not
/// clobber the artwork attempt timestamp.
pub(super) const UPSERT_TRACK_ONLINE_STATUS_SQL: &str = r#"
INSERT INTO track_online_status (
    track_id,
    artwork_attempted_at_unix,
    tags_attempted_at_unix,
    lyrics_attempted_at_unix,
    provider_version
)
VALUES (?1, ?2, ?3, ?4, ?5)
ON CONFLICT(track_id) DO UPDATE SET
    artwork_attempted_at_unix = COALESCE(excluded.artwork_attempted_at_unix, artwork_attempted_at_unix),
    tags_attempted_at_unix    = COALESCE(excluded.tags_attempted_at_unix,    tags_attempted_at_unix),
    lyrics_attempted_at_unix  = COALESCE(excluded.lyrics_attempted_at_unix,  lyrics_attempted_at_unix),
    provider_version          = excluded.provider_version
"#;

/// "Find tracks needing online work." Returns track IDs not marked
/// missing AND having at least one of the requested capabilities
/// either un-attempted (NULL timestamp) or stamped by an older
/// provider_version.
///
/// The artwork capability has an extra non-destructive guard: tracks
/// whose most recent scan observed an embedded picture
/// (`has_embedded_artwork = 1`) are excluded entirely from the
/// artwork-needs clause. We will not touch a file that already has
/// its own cover, even on a fresh provider_version. `IS NULL` is
/// treated as "not yet scanned" → still a candidate.
///
/// Bound parameters mirror the analysis query:
///   ?1 = include_artwork   (1 or 0)
///   ?2 = include_tags      (1 or 0)
///   ?3 = include_lyrics    (1 or 0)
///   ?4 = current provider_version
///   ?5 = LIMIT
pub(super) const SELECT_TRACKS_NEEDING_ONLINE_SQL: &str = r#"
SELECT t.id
FROM tracks t
LEFT JOIN track_online_status s ON s.track_id = t.id
WHERE t.is_missing = 0
  AND (
        (?1 = 1
            AND COALESCE(t.has_embedded_artwork, 0) = 0
            AND (s.artwork_attempted_at_unix IS NULL OR s.provider_version < ?4))
     OR (?2 = 1 AND (s.tags_attempted_at_unix    IS NULL OR s.provider_version < ?4))
     OR (?3 = 1 AND (s.lyrics_attempted_at_unix  IS NULL OR s.provider_version < ?4))
      )
ORDER BY t.id
LIMIT ?5
"#;

pub(super) const UPSERT_SMART_SHUFFLE_INDEX_SQL: &str = r#"
INSERT INTO smart_shuffle_index (
    id,
    index_blob,
    schema_version
)
VALUES (1, ?1, ?2)
ON CONFLICT(id) DO UPDATE SET
    index_blob = excluded.index_blob,
    schema_version = excluded.schema_version
"#;

pub(super) const SELECT_SMART_SHUFFLE_INDEX_SQL: &str = r#"
SELECT index_blob, schema_version
FROM smart_shuffle_index
WHERE id = 1
"#;

pub(super) const DELETE_SMART_SHUFFLE_INDEX_SQL: &str =
    r#"DELETE FROM smart_shuffle_index WHERE id = 1"#;

/// "Find tracks needing analysis." Returns track IDs that are not
/// marked missing AND have at least one of the requested capabilities
/// either un-attempted (NULL timestamp) or stamped by an older
/// analyzer_version. Bound parameters in order:
///   ?1 = include_bpm        (1 or 0)
///   ?2 = include_key        (1 or 0)
///   ?3 = include_audio      (1 or 0)
///   ?4 = current analyzer_version
///   ?5 = LIMIT
pub(super) const SELECT_TRACKS_NEEDING_ANALYSIS_SQL: &str = r#"
SELECT t.id
FROM tracks t
LEFT JOIN track_analysis ta ON ta.track_id = t.id
WHERE t.is_missing = 0
  AND (
        (?1 = 1 AND (ta.bpm_attempted_at_unix   IS NULL OR ta.analyzer_version < ?4))
     OR (?2 = 1 AND (ta.key_attempted_at_unix   IS NULL OR ta.analyzer_version < ?4))
     OR (?3 = 1 AND (ta.audio_attempted_at_unix IS NULL OR ta.analyzer_version < ?4))
      )
ORDER BY t.id
LIMIT ?5
"#;

fn indented_track_column_names(indent: &str) -> String {
    TRACK_COLUMNS
        .iter()
        .map(|column| format!("{indent}{}", column.name))
        .collect::<Vec<_>>()
        .join(",\n")
}

fn indented_insert_placeholders(indent: &str) -> String {
    (1..=TRACK_COLUMNS.len())
        .map(|index| format!("{indent}?{index}"))
        .collect::<Vec<_>>()
        .join(",\n")
}

#[cfg(test)]
mod tests {
    use super::{TRACK_COLUMNS, track_column_index};

    #[test]
    fn track_column_indices_match_column_order() {
        let expected = [
            (track_column_index::ID, "id"),
            (track_column_index::RELATIVE_PATH, "relative_path"),
            (track_column_index::TITLE, "title"),
            (track_column_index::ARTIST, "artist"),
            (track_column_index::ALBUM, "album"),
            (track_column_index::ALBUM_ARTIST, "album_artist"),
            (track_column_index::COMPOSER, "composer"),
            (track_column_index::GENRE, "genre"),
            (track_column_index::TRACK_NUMBER, "track_number"),
            (track_column_index::DISC_NUMBER, "disc_number"),
            (track_column_index::YEAR, "year"),
            (track_column_index::DURATION_SECONDS, "duration_seconds"),
            (track_column_index::BITRATE_KBPS, "bitrate_kbps"),
            (track_column_index::RATING, "rating"),
            (track_column_index::PLAY_COUNT, "play_count"),
            (track_column_index::SKIP_COUNT, "skip_count"),
            (
                track_column_index::LAST_PLAYED_AT_UNIX,
                "last_played_at_unix",
            ),
            (
                track_column_index::LAST_SKIPPED_AT_UNIX,
                "last_skipped_at_unix",
            ),
            (track_column_index::DATE_ADDED_AT_UNIX, "date_added_at_unix"),
            (track_column_index::IS_MISSING, "is_missing"),
            (track_column_index::GROUPING, "grouping"),
            (track_column_index::TRACK_TOTAL, "track_total"),
            (track_column_index::DISC_TOTAL, "disc_total"),
            (track_column_index::COMPILATION, "compilation"),
            (track_column_index::BPM, "bpm"),
            (track_column_index::MUSICAL_KEY, "musical_key"),
            (track_column_index::COMMENTS, "comments"),
            (track_column_index::SAMPLE_RATE_HZ, "sample_rate_hz"),
            (track_column_index::CHANNELS, "channels"),
            (track_column_index::LYRICS, "lyrics"),
            (track_column_index::FILE_SIZE_BYTES, "file_size_bytes"),
            (
                track_column_index::HAS_EMBEDDED_ARTWORK,
                "has_embedded_artwork",
            ),
            (track_column_index::TITLE_SORT, "title_sort"),
            (track_column_index::ARTIST_SORT, "artist_sort"),
            (track_column_index::ALBUM_SORT, "album_sort"),
            (track_column_index::ALBUM_ARTIST_SORT, "album_artist_sort"),
            (track_column_index::COMPOSER_SORT, "composer_sort"),
        ];

        assert_eq!(TRACK_COLUMNS.len(), expected.len());
        for (index, name) in expected {
            assert_eq!(TRACK_COLUMNS[index].name, name);
        }
    }
}
