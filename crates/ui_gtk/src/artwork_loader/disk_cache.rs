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
    requested_png: Option<Vec<u8>>,
    other_variant_present: bool,
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
        if self.requested_png.is_none()
            && !self.other_variant_present
            && self.palette.is_none()
            && dimensions.is_none()
        {
            return Some(DecodedArtwork::default());
        }
        let requested_png = self.requested_png?;

        let (tile_texture, detail_texture) = match variant {
            ArtworkVariant::Tile => (Some(texture_from_png(&requested_png)?), None),
            ArtworkVariant::Detail => (None, Some(texture_from_png(&requested_png)?)),
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
        let query = match variant {
            ArtworkVariant::Tile => {
                r#"
                    SELECT original_width,
                           original_height,
                           encoded_bytes,
                           tile_png,
                           detail_png IS NOT NULL,
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
                    "#
            }
            ArtworkVariant::Detail => {
                r#"
                    SELECT original_width,
                           original_height,
                           encoded_bytes,
                           detail_png,
                           tile_png IS NOT NULL,
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
                       AND (detail_png IS NULL OR length(detail_png) <= ?6)
                    "#
            }
        };
        let cached = {
            let connection = self.connection.lock().ok()?;
            connection
                .query_row(
                    query,
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
                            requested_png: row.get(3)?,
                            other_variant_present: row.get(4)?,
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
                original_width =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.original_width, artwork_cache.original_width)
                        ELSE excluded.original_width
                    END,
                original_height =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.original_height, artwork_cache.original_height)
                        ELSE excluded.original_height
                    END,
                encoded_bytes =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.encoded_bytes, artwork_cache.encoded_bytes)
                        ELSE excluded.encoded_bytes
                    END,
                tile_png =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.tile_png, artwork_cache.tile_png)
                        ELSE excluded.tile_png
                    END,
                detail_png =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.detail_png, artwork_cache.detail_png)
                        ELSE excluded.detail_png
                    END,
                background_red =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.background_red, artwork_cache.background_red)
                        ELSE excluded.background_red
                    END,
                background_green =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.background_green, artwork_cache.background_green)
                        ELSE excluded.background_green
                    END,
                background_blue =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.background_blue, artwork_cache.background_blue)
                        ELSE excluded.background_blue
                    END,
                foreground_red =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.foreground_red, artwork_cache.foreground_red)
                        ELSE excluded.foreground_red
                    END,
                foreground_green =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.foreground_green, artwork_cache.foreground_green)
                        ELSE excluded.foreground_green
                    END,
                foreground_blue =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.foreground_blue, artwork_cache.foreground_blue)
                        ELSE excluded.foreground_blue
                    END,
                secondary_red =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.secondary_red, artwork_cache.secondary_red)
                        ELSE excluded.secondary_red
                    END,
                secondary_green =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.secondary_green, artwork_cache.secondary_green)
                        ELSE excluded.secondary_green
                    END,
                secondary_blue =
                    CASE
                        WHEN artwork_cache.file_size = excluded.file_size
                         AND artwork_cache.mtime_ns = excluded.mtime_ns
                         AND artwork_cache.format_version = excluded.format_version
                        THEN COALESCE(excluded.secondary_blue, artwork_cache.secondary_blue)
                        ELSE excluded.secondary_blue
                    END,
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

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        sync::atomic::{AtomicUsize, Ordering},
    };

    use sustain_artwork::{ArtworkDimensions, MAX_ENCODED_ARTWORK_BYTES};

    use crate::artwork_color::{ArtworkPaletteComponents, RgbColorComponents};

    use super::{
        ArtworkDiskCache, ArtworkFileFingerprint, ArtworkSource, ArtworkVariant, CachedArtwork,
    };

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    #[test]
    fn cache_merges_tile_and_detail_variants_for_same_fingerprint() {
        let (_dir, cache) = test_cache();
        let source = test_source();
        let fingerprint = test_fingerprint();

        cache.store(
            &source,
            fingerprint,
            &CachedArtwork {
                tile_png: Some(VALID_PNG.to_vec()),
                ..cached_metadata()
            },
        );
        assert!(
            cache
                .load(&source, fingerprint, ArtworkVariant::Detail)
                .is_none(),
            "a tile-only cache row must not satisfy a detail request"
        );

        cache.store(
            &source,
            fingerprint,
            &CachedArtwork {
                detail_png: Some(VALID_PNG.to_vec()),
                ..cached_metadata()
            },
        );

        let tile = cache
            .load(&source, fingerprint, ArtworkVariant::Tile)
            .expect("tile cache hit survives detail merge");
        assert!(tile.tile_texture.is_some());
        assert!(tile.detail_texture.is_none());

        let detail = cache
            .load(&source, fingerprint, ArtworkVariant::Detail)
            .expect("detail cache hit after merge");
        assert!(detail.tile_texture.is_none());
        assert!(detail.detail_texture.is_some());
    }

    #[test]
    fn tile_load_ignores_invalid_detail_blob() {
        let (_dir, cache) = test_cache();
        let source = test_source();
        let fingerprint = test_fingerprint();

        cache.store(
            &source,
            fingerprint,
            &CachedArtwork {
                tile_png: Some(VALID_PNG.to_vec()),
                detail_png: Some(vec![0; MAX_ENCODED_ARTWORK_BYTES + 1]),
                ..cached_metadata()
            },
        );

        assert!(
            cache
                .load(&source, fingerprint, ArtworkVariant::Tile)
                .is_some(),
            "tile loads should not read or validate the unused detail PNG"
        );
        assert!(
            cache
                .load(&source, fingerprint, ArtworkVariant::Detail)
                .is_none(),
            "detail loads still validate the requested detail PNG"
        );
    }

    struct TestCacheDir(PathBuf);

    impl Drop for TestCacheDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn test_cache() -> (TestCacheDir, ArtworkDiskCache) {
        let index = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "sustain-ui-artwork-cache-test-{}-{index}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        let cache = ArtworkDiskCache::open(&dir).expect("open artwork disk cache");
        (TestCacheDir(dir), cache)
    }

    fn test_source() -> ArtworkSource {
        ArtworkSource::embedded_track(
            PathBuf::from("Artist/Album/01.flac"),
            PathBuf::from("/tmp/01.flac"),
        )
    }

    fn test_fingerprint() -> ArtworkFileFingerprint {
        ArtworkFileFingerprint {
            file_size: 1234,
            mtime_ns: 5678,
        }
    }

    fn cached_metadata() -> CachedArtwork {
        CachedArtwork {
            dimensions: Some(ArtworkDimensions {
                width: 1,
                height: 1,
                pixels: 1,
            }),
            encoded_bytes_len: Some(VALID_PNG.len()),
            palette: Some(ArtworkPaletteComponents {
                background: rgb(1, 2, 3),
                foreground: rgb(250, 251, 252),
                secondary: rgb(80, 90, 100),
            }),
            ..CachedArtwork::default()
        }
    }

    const fn rgb(red: u8, green: u8, blue: u8) -> RgbColorComponents {
        RgbColorComponents { red, green, blue }
    }
}
