// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::HashMap,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

use rusqlite::{Connection, OptionalExtension, params};
use sustain_app_runtime::MetadataService;
use sustain_artwork::{ArtworkDimensions, MAX_ENCODED_ARTWORK_BYTES, validate_dimensions};

use crate::artwork_color::{ArtworkPalette, ArtworkPaletteComponents};

use super::decode::{
    ArtworkVariant, DecodedArtwork, decode_artwork, palette_components_from_cache_row,
    texture_from_png,
};

const CACHE_SCHEMA_VERSION: i64 = 2;
const CACHE_SOURCE_KIND_EMBEDDED_TRACK: &str = "embedded-track";

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) enum ArtworkSource {
    EmbeddedTrack {
        /// Stable key for this embedded artwork source. Prefer the library
        /// relative track path so future disk-cache rows survive library-root
        /// moves; use the absolute path only when the model hands us one.
        cache_path: PathBuf,
        /// Absolute path to read on this machine.
        file_path: PathBuf,
    },
}

impl ArtworkSource {
    pub(crate) fn embedded_track(cache_path: PathBuf, file_path: PathBuf) -> Self {
        Self::EmbeddedTrack {
            cache_path,
            file_path,
        }
    }

    pub(super) fn cache_key(&self) -> (&'static str, Vec<u8>) {
        match self {
            ArtworkSource::EmbeddedTrack { cache_path, .. } => (
                CACHE_SOURCE_KIND_EMBEDDED_TRACK,
                cache_path.as_os_str().as_bytes().to_vec(),
            ),
        }
    }

    pub(super) fn file_fingerprint(&self) -> Option<ArtworkFileFingerprint> {
        let file_path = match self {
            ArtworkSource::EmbeddedTrack { file_path, .. } => file_path,
        };
        let metadata = fs::metadata(file_path).ok()?;
        let file_size = i64::try_from(metadata.len()).ok()?;
        let mtime_ns = metadata
            .mtime()
            .saturating_mul(1_000_000_000)
            .saturating_add(metadata.mtime_nsec());
        Some(ArtworkFileFingerprint {
            file_size,
            mtime_ns,
        })
    }
}

#[derive(Clone, Copy)]
pub(super) struct ArtworkFileFingerprint {
    pub(super) file_size: i64,
    pub(super) mtime_ns: i64,
}

pub(super) struct ArtworkRepository {
    metadata_service: Arc<dyn MetadataService>,
    disk_cache: Option<ArtworkDiskCache>,
    source_generations: Mutex<HashMap<ArtworkSource, u64>>,
}

impl ArtworkRepository {
    pub(super) fn new(metadata_service: Arc<dyn MetadataService>, cache_dir: PathBuf) -> Self {
        Self {
            metadata_service,
            disk_cache: ArtworkDiskCache::open(&cache_dir),
            source_generations: Mutex::new(HashMap::new()),
        }
    }

    pub(super) fn source_generation(&self, source: &ArtworkSource) -> u64 {
        self.source_generations
            .lock()
            .ok()
            .and_then(|generations| generations.get(source).copied())
            .unwrap_or_default()
    }

    pub(super) fn invalidate(&self, source: &ArtworkSource) {
        let Ok(mut generations) = self.source_generations.lock() else {
            return;
        };
        let generation = generations.entry(source.clone()).or_default();
        *generation = generation.saturating_add(1);
        if let Some(disk_cache) = &self.disk_cache {
            disk_cache.delete(source);
        }
    }

    pub(super) fn load(
        &self,
        source: &ArtworkSource,
        generation: u64,
        variant: ArtworkVariant,
    ) -> DecodedArtwork {
        let fingerprint = source.file_fingerprint();
        if let (Some(cache), Some(fingerprint)) = (&self.disk_cache, fingerprint)
            && let Some(decoded) = cache.load(source, fingerprint, variant)
        {
            return decoded;
        }

        match source {
            ArtworkSource::EmbeddedTrack { file_path, .. } => {
                let bytes = self.metadata_service.read_artwork(file_path).ok().flatten();
                let decoded = decode_artwork(bytes, variant);
                let generations = self.source_generations.lock().ok();
                if generations.as_ref().is_some_and(|generations| {
                    generations.get(source).copied().unwrap_or_default() == generation
                }) && let (Some(cache), Some(fingerprint)) = (&self.disk_cache, fingerprint)
                {
                    cache.store(source, fingerprint, &decoded.cache_entry);
                }
                decoded.artwork
            }
        }
    }
}

#[derive(Default)]
pub(super) struct CachedArtwork {
    pub(super) dimensions: Option<ArtworkDimensions>,
    pub(super) encoded_bytes_len: Option<usize>,
    pub(super) tile_png: Option<Vec<u8>>,
    pub(super) detail_png: Option<Vec<u8>>,
    pub(super) palette: Option<ArtworkPaletteComponents>,
}

pub(super) struct CachedArtworkRow {
    original_width: Option<i64>,
    original_height: Option<i64>,
    pub(super) encoded_bytes_len: Option<i64>,
    pub(super) tile_png: Option<Vec<u8>>,
    pub(super) detail_png: Option<Vec<u8>>,
    pub(super) palette: Option<ArtworkPaletteComponents>,
}

impl CachedArtworkRow {
    /// Reconstruct the in-memory artwork for one requested size.
    ///
    /// Only the requested variant's PNG is decoded into a `gdk::Texture`;
    /// the other size's payload stays on disk untouched. This is what keeps
    /// a tile load from materialising the much larger detail texture, so the
    /// disk path preserves the same memory discipline as a fresh decode.
    pub(super) fn decode(self, variant: ArtworkVariant) -> Option<DecodedArtwork> {
        let dimensions = match (self.original_width, self.original_height) {
            (Some(width), Some(height)) => Some(
                validate_dimensions(u64::try_from(width).ok()?, u64::try_from(height).ok()?)
                    .ok()?,
            ),
            (None, None) => None,
            _ => return None,
        };
        let encoded_bytes_len = self
            .encoded_bytes_len
            .map(usize::try_from)
            .transpose()
            .ok()?;
        if self.tile_png.is_none()
            && self.detail_png.is_none()
            && self.palette.is_none()
            && dimensions.is_none()
        {
            return Some(DecodedArtwork::default());
        }

        let (tile_texture, detail_texture) = match variant {
            ArtworkVariant::Tile => (
                Some(self.tile_png.as_deref().and_then(texture_from_png)?),
                None,
            ),
            ArtworkVariant::Detail => (
                None,
                Some(self.detail_png.as_deref().and_then(texture_from_png)?),
            ),
        };
        Some(DecodedArtwork {
            tile_texture,
            detail_texture,
            palette: self.palette.map(ArtworkPalette::from_components),
            dimensions,
            encoded_bytes_len,
        })
    }
}

pub(super) struct ArtworkDiskCache {
    connection: Mutex<Connection>,
}

impl ArtworkDiskCache {
    pub(super) fn open(cache_dir: &Path) -> Option<Self> {
        fs::create_dir_all(cache_dir).ok()?;
        let connection = Connection::open(cache_dir.join("artwork-cache.sqlite")).ok()?;
        Self::initialize(&connection).ok()?;
        Some(Self {
            connection: Mutex::new(connection),
        })
    }

    fn initialize(connection: &Connection) -> rusqlite::Result<()> {
        connection.execute_batch(
            r#"
                PRAGMA journal_mode = WAL;
                PRAGMA synchronous = NORMAL;
                "#,
        )?;

        let user_version: i64 =
            connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
        if user_version != CACHE_SCHEMA_VERSION {
            // This is a derived cache, not durable user data. Recreate on
            // schema changes instead of carrying cache migrations.
            connection.execute_batch("DROP TABLE IF EXISTS artwork_cache;")?;
        }

        connection.execute_batch(
            r#"
                CREATE TABLE IF NOT EXISTS artwork_cache (
                    source_kind      TEXT    NOT NULL,
                    source_key       BLOB    NOT NULL,
                    file_size        INTEGER NOT NULL,
                    mtime_ns         INTEGER NOT NULL,
                    format_version   INTEGER NOT NULL,
                    original_width   INTEGER,
                    original_height  INTEGER,
                    encoded_bytes    INTEGER,
                    tile_png         BLOB,
                    detail_png       BLOB,
                    background_red   INTEGER,
                    background_green INTEGER,
                    background_blue  INTEGER,
                    foreground_red   INTEGER,
                    foreground_green INTEGER,
                    foreground_blue  INTEGER,
                    secondary_red    INTEGER,
                    secondary_green  INTEGER,
                    secondary_blue   INTEGER,
                    updated_at_unix  INTEGER NOT NULL,
                    PRIMARY KEY (source_kind, source_key)
                ) WITHOUT ROWID;
                "#,
        )?;
        if user_version != CACHE_SCHEMA_VERSION {
            connection.pragma_update(None, "user_version", CACHE_SCHEMA_VERSION)?;
        }
        Ok(())
    }

    pub(super) fn load(
        &self,
        source: &ArtworkSource,
        fingerprint: ArtworkFileFingerprint,
        variant: ArtworkVariant,
    ) -> Option<DecodedArtwork> {
        let (source_kind, source_key) = source.cache_key();
        let cached = {
            let connection = self.connection.lock().ok()?;
            connection
                .query_row(
                    r#"
                    SELECT original_width,
                           original_height,
                           encoded_bytes,
                           tile_png,
                           detail_png,
                           background_red,
                           background_green,
                           background_blue,
                           foreground_red,
                           foreground_green,
                           foreground_blue,
                           secondary_red,
                           secondary_green,
                           secondary_blue
                      FROM artwork_cache
                     WHERE source_kind = ?1
                       AND source_key = ?2
                       AND file_size = ?3
                       AND mtime_ns = ?4
                       AND format_version = ?5
                       AND (tile_png IS NULL OR length(tile_png) <= ?6)
                       AND (detail_png IS NULL OR length(detail_png) <= ?6)
                    "#,
                    params![
                        source_kind,
                        source_key,
                        fingerprint.file_size,
                        fingerprint.mtime_ns,
                        CACHE_SCHEMA_VERSION,
                        MAX_ENCODED_ARTWORK_BYTES as i64,
                    ],
                    |row| {
                        Ok(CachedArtworkRow {
                            original_width: row.get(0)?,
                            original_height: row.get(1)?,
                            encoded_bytes_len: row.get(2)?,
                            tile_png: row.get(3)?,
                            detail_png: row.get(4)?,
                            palette: palette_components_from_cache_row(row)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()?
        };
        cached.decode(variant)
    }

    fn delete(&self, source: &ArtworkSource) {
        let (source_kind, source_key) = source.cache_key();
        let Ok(connection) = self.connection.lock() else {
            return;
        };
        let _ = connection.execute(
            r#"
            DELETE FROM artwork_cache
             WHERE source_kind = ?1
               AND source_key = ?2
            "#,
            params![source_kind, source_key],
        );
    }

    fn store(
        &self,
        source: &ArtworkSource,
        fingerprint: ArtworkFileFingerprint,
        cached: &CachedArtwork,
    ) {
        let (source_kind, source_key) = source.cache_key();
        let connection = match self.connection.lock() {
            Ok(connection) => connection,
            Err(_) => return,
        };
        let palette = cached.palette;
        let background = palette.map(|palette| palette.background);
        let foreground = palette.map(|palette| palette.foreground);
        let secondary = palette.map(|palette| palette.secondary);
        let _ = connection.execute(
            r#"
            INSERT INTO artwork_cache (
                source_kind,
                source_key,
                file_size,
                mtime_ns,
                format_version,
                original_width,
                original_height,
                encoded_bytes,
                tile_png,
                detail_png,
                background_red,
                background_green,
                background_blue,
                foreground_red,
                foreground_green,
                foreground_blue,
                secondary_red,
                secondary_green,
                secondary_blue,
                updated_at_unix
            )
            VALUES (
                ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                ?17, ?18, ?19,
                unixepoch()
            )
            ON CONFLICT(source_kind, source_key) DO UPDATE SET
                file_size = excluded.file_size,
                mtime_ns = excluded.mtime_ns,
                format_version = excluded.format_version,
                original_width = excluded.original_width,
                original_height = excluded.original_height,
                encoded_bytes = excluded.encoded_bytes,
                tile_png = excluded.tile_png,
                detail_png = excluded.detail_png,
                background_red = excluded.background_red,
                background_green = excluded.background_green,
                background_blue = excluded.background_blue,
                foreground_red = excluded.foreground_red,
                foreground_green = excluded.foreground_green,
                foreground_blue = excluded.foreground_blue,
                secondary_red = excluded.secondary_red,
                secondary_green = excluded.secondary_green,
                secondary_blue = excluded.secondary_blue,
                updated_at_unix = excluded.updated_at_unix
            "#,
            params![
                source_kind,
                source_key,
                fingerprint.file_size,
                fingerprint.mtime_ns,
                CACHE_SCHEMA_VERSION,
                cached
                    .dimensions
                    .map(|dimensions| i64::from(dimensions.width)),
                cached
                    .dimensions
                    .map(|dimensions| i64::from(dimensions.height)),
                cached
                    .encoded_bytes_len
                    .and_then(|bytes| i64::try_from(bytes).ok()),
                cached.tile_png.as_deref(),
                cached.detail_png.as_deref(),
                background.map(|color| i64::from(color.red)),
                background.map(|color| i64::from(color.green)),
                background.map(|color| i64::from(color.blue)),
                foreground.map(|color| i64::from(color.red)),
                foreground.map(|color| i64::from(color.green)),
                foreground.map(|color| i64::from(color.blue)),
                secondary.map(|color| i64::from(color.red)),
                secondary.map(|color| i64::from(color.green)),
                secondary.map(|color| i64::from(color.blue)),
            ],
        );
    }
}
