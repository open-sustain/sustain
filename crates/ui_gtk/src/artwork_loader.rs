// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Shared background loader for cover artwork.
//!
//! Reading artwork from an audio file is a synchronous, disk- and
//! CPU-bound operation (a `lofty` tag parse plus a pixbuf decode plus a
//! palette derivation). Doing any part of it inline on the GTK main
//! thread freezes the UI on large libraries — for the Albums grid the
//! freeze used to be several seconds; for the now-playing tile it
//! manifests as a hitch on every track change.
//!
//! `ArtworkLoader` separates that work and is shared by every view
//! that needs an artwork texture or palette (Albums grid, album-detail
//! panel, integrated top bar's now-playing tile, future zoom modal).
//!
//! * A small pool of worker threads consumes `ArtworkSource` requests from a
//!   shared queue and runs the **entire** decode pipeline off the main thread:
//!   source resolution, `MetadataService::read_artwork` for embedded-track
//!   artwork, scaled pixbuf decode, `ArtworkPalette::from_pixbuf`, and bounded
//!   `gdk::Texture`s for the tile and detail sizes. The pixbuf itself is
//!   dropped on the worker; only the finished `DecodedArtwork` is handed back.
//!   (This relies on `gdk::Texture` being `Send + Sync` in gtk-rs — it is,
//!   because GdkTexture is documented as immutable after construction.)
//! * A GTK main-loop poller drains the result channel under a strict
//!   per-tick budget (small max batch + short wall-clock cap) so even a
//!   burst of completions can't monopolise the main thread, places each
//!   result in the source-keyed cache, and fires every callback that was
//!   waiting for that source.
//! * Staleness — discarding callbacks whose target widget is no longer
//!   relevant (Albums grid rebuilt, now-playing track changed) — is the
//!   caller's concern. Each view tracks its own per-view generation
//!   counter and checks it inside the callback closure before touching
//!   widgets. Keeping that policy with the caller lets independent
//!   views share one loader without one view's rebuild invalidating
//!   another view's in-flight requests.
//! * The repository checks a small SQLite cache before touching the audio file.
//!   Cache rows are keyed by source plus the representative file fingerprint,
//!   and store already-scaled tile/detail PNG payloads plus the derived palette.
//!   Today the only source is embedded artwork from an audio file; the explicit
//!   source boundary is where the missing-artwork downloader should plug in.

use std::{
    cell::RefCell,
    collections::HashMap,
    fs,
    os::unix::{ffi::OsStrExt, fs::MetadataExt},
    path::{Path, PathBuf},
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::Duration,
};

use gtk::{gdk, gdk_pixbuf, gio, glib};
use rusqlite::{Connection, OptionalExtension, params};
use sustain_app_runtime::MetadataService;
use sustain_artwork::{ArtworkDimensions, MAX_ENCODED_ARTWORK_BYTES, validate_encoded_artwork};

use crate::artwork_color::{ArtworkPalette, ArtworkPaletteComponents, RgbColorComponents};

/// Number of worker threads pulling from the request queue. Artwork
/// extraction is dominated by file I/O, tag parsing, and pixbuf decode;
/// a small fixed pool keeps the disk busy and uses a couple of cores
/// for decode without making us a noticeable burden on other processes.
/// Not user-configurable: the right value lives in a narrow band and
/// the wrong one is unlikely to surprise anyone.
const WORKER_COUNT: usize = 4;

/// Poll interval for delivering completed loads back to the main loop.
/// 50ms is fast enough that artwork pops in smoothly while keeping the
/// idle GTK loop quiet when nothing is being decoded.
const RESULT_POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Hard cap on the number of completed results the poller hands to
/// widget callbacks in a single tick. Even when each callback is cheap
/// (texture swap on a single image widget), the cumulative GTK redraw
/// cost can stall the frame if a flood of results lands at once. The
/// cap and the time budget below act together: whichever fires first
/// returns control to the main loop and lets GTK paint a frame before
/// the next batch is delivered.
const RESULT_BATCH_MAX: usize = 8;

/// Wall-clock budget for a single poller tick. Backstops the batch cap
/// against pathological per-callback work — if a single callback ends
/// up doing more than expected, we still relinquish the main thread on
/// schedule rather than running through the whole batch.
const RESULT_TICK_BUDGET: Duration = Duration::from_millis(4);

/// Maximum side length of the smaller cached texture. Sized to cover
/// the Albums grid tile (132px) and the now-playing tile (72px) without
/// either having to upscale. Bigger consumers (album-detail panel,
/// zoom modal) use the detail texture below.
const TILE_TEXTURE_MAX_SIDE: i32 = 132;

/// Maximum side length of the larger cached texture. Sized to cover
/// the album-detail panel (3× the grid tile). The cache stores PNG
/// payloads at this size; views downscale further at paint time.
const DETAIL_TEXTURE_MAX_SIDE: i32 = TILE_TEXTURE_MAX_SIDE * 3;

const CACHE_SCHEMA_VERSION: i64 = 1;
const CACHE_SOURCE_KIND_EMBEDDED_TRACK: &str = "embedded-track";

/// Decoded artwork shared between tile rendering (needs only the
/// texture) and detail-panel rendering (also needs the palette to tint
/// the panel background/text). Both are computed once per file and
/// cached.
#[derive(Clone, Default)]
pub(crate) struct DecodedArtwork {
    pub(crate) tile_texture: Option<gdk::Texture>,
    pub(crate) detail_texture: Option<gdk::Texture>,
    pub(crate) palette: Option<ArtworkPalette>,
}

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

    fn cache_key(&self) -> (&'static str, Vec<u8>) {
        match self {
            ArtworkSource::EmbeddedTrack { cache_path, .. } => (
                CACHE_SOURCE_KIND_EMBEDDED_TRACK,
                cache_path.as_os_str().as_bytes().to_vec(),
            ),
        }
    }

    fn file_fingerprint(&self) -> Option<ArtworkFileFingerprint> {
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
struct ArtworkFileFingerprint {
    file_size: i64,
    mtime_ns: i64,
}

pub(crate) type ArtworkCallback = Box<dyn FnOnce(DecodedArtwork) + 'static>;

#[derive(Clone)]
pub(crate) struct ArtworkLoader {
    inner: Rc<LoaderInner>,
}

struct LoaderInner {
    repository: Arc<ArtworkRepository>,
    request_tx: mpsc::Sender<WorkerRequest>,
    cache: RefCell<HashMap<ArtworkSource, DecodedArtwork>>,
    pending: RefCell<HashMap<ArtworkSource, Vec<ArtworkCallback>>>,
}

struct WorkerRequest {
    source: ArtworkSource,
    generation: u64,
}

struct WorkerResult {
    source: ArtworkSource,
    generation: u64,
    decoded: DecodedArtwork,
}

impl ArtworkLoader {
    pub(crate) fn new(metadata_service: Arc<dyn MetadataService>, cache_dir: PathBuf) -> Self {
        let repository = Arc::new(ArtworkRepository::new(metadata_service, cache_dir));
        let (request_tx, request_rx) = mpsc::channel::<WorkerRequest>();
        let request_rx = Arc::new(Mutex::new(request_rx));
        let (result_tx, result_rx) = mpsc::channel::<WorkerResult>();

        for index in 0..WORKER_COUNT {
            let request_rx = Arc::clone(&request_rx);
            let result_tx = result_tx.clone();
            let repository = Arc::clone(&repository);
            thread::Builder::new()
                .name(format!("sustain-artwork-{index}"))
                .spawn(move || worker_loop(request_rx, result_tx, repository))
                .expect("spawn artwork worker thread");
        }
        // Workers each keep their own clone of the result sender; drop
        // the original so the poller's `Disconnected` actually fires
        // when every worker has exited.
        drop(result_tx);

        let inner = Rc::new(LoaderInner {
            repository,
            request_tx,
            cache: RefCell::new(HashMap::new()),
            pending: RefCell::new(HashMap::new()),
        });

        install_result_poller(Rc::clone(&inner), result_rx);

        Self { inner }
    }

    /// Returns the decoded entry for `source`, if any. Lets a view
    /// reuse what another view already produced rather than reading the
    /// file a second time.
    pub(crate) fn cached(&self, source: &ArtworkSource) -> Option<DecodedArtwork> {
        self.inner.cache.borrow().get(source).cloned()
    }

    /// Request the decoded artwork for `source`. The callback fires on
    /// the main thread when the artwork becomes available, or
    /// synchronously when the in-memory cache already holds the entry —
    /// so a tile whose neighbour just resolved the same file never
    /// schedules redundant disk work.
    ///
    /// The loader has no notion of staleness; callbacks always fire.
    /// Each caller is responsible for checking, inside its closure,
    /// whether the result still applies to the widget it would update
    /// (e.g. via a per-view generation counter). Keeping that policy
    /// with the caller is what lets one shared loader serve multiple
    /// independent views without their rebuilds invalidating each
    /// other's in-flight requests.
    pub(crate) fn request(&self, source: ArtworkSource, callback: ArtworkCallback) {
        if let Some(cached) = self.inner.cache.borrow().get(&source) {
            callback(cached.clone());
            return;
        }
        let mut pending = self.inner.pending.borrow_mut();
        let needs_queue = !pending.contains_key(&source);
        pending.entry(source.clone()).or_default().push(callback);
        if needs_queue {
            // Send only fails if every worker has exited, which happens
            // exclusively at shutdown. Drop the callback silently in
            // that case — there is no view left to update.
            let generation = self.inner.repository.source_generation(&source);
            let _ = self
                .inner
                .request_tx
                .send(WorkerRequest { source, generation });
        }
    }

    /// Drop the cached entry (in-memory and on-disk) for `source`.
    ///
    /// Used after a write changes the underlying artwork — e.g. when
    /// the user accepts a fetched cover for the now-playing track. A
    /// fresh request after invalidation re-reads the source through
    /// the worker pool, so any view holding a stale texture redraws
    /// with the new bytes the next time it asks for them.
    ///
    /// We do not proactively repaint anything from here: views that
    /// care about the change are expected to re-issue their request
    /// (typically via their existing track-row-changed callback).
    /// That keeps the invalidation hook narrowly responsible and
    /// avoids reaching across the UI tree from a model-layer cache.
    pub(crate) fn invalidate(&self, source: &ArtworkSource) {
        // Forget the decoded value and callbacks before advancing the source
        // generation. Results already in flight are discarded by the poller;
        // advancing the repository generation also prevents those workers
        // from repopulating the on-disk cache after its matching row is
        // evicted.
        self.inner.cache.borrow_mut().remove(source);
        let _ = self.inner.pending.borrow_mut().remove(source);
        self.inner.repository.invalidate(source);
    }

    /// Insert decoded artwork built from already-in-memory bytes.
    ///
    /// Used after a remote fetch lands: the tag-write that persists
    /// the bytes is asynchronous, so a naive "invalidate then
    /// re-request" path would race the writer and briefly display
    /// the missing-artwork state. Priming the in-memory cache
    /// directly with the freshly-decoded artwork makes the new cover
    /// visible on the very next [`Self::cached`] / [`Self::request`]
    /// call without depending on disk write ordering.
    ///
    /// Only the in-memory cache is touched: the disk cache row was
    /// dropped by [`Self::invalidate`] and will be repopulated by
    /// the next miss-driven worker load once the tag write has
    /// landed and the file fingerprint has updated.
    pub(crate) fn prime(&self, source: ArtworkSource, bytes: Vec<u8>) {
        let decoded = decode_artwork(Some(bytes));
        self.inner
            .cache
            .borrow_mut()
            .insert(source, decoded.artwork);
    }
}

fn worker_loop(
    request_rx: Arc<Mutex<mpsc::Receiver<WorkerRequest>>>,
    result_tx: mpsc::Sender<WorkerResult>,
    repository: Arc<ArtworkRepository>,
) {
    loop {
        let request = {
            // Hold the lock only long enough to take one item; the
            // expensive read and decode happen unlocked so the other
            // workers can pull from the queue concurrently.
            let Ok(rx) = request_rx.lock() else {
                return;
            };
            match rx.recv() {
                Ok(request) => request,
                Err(_) => return,
            }
        };
        let decoded = repository.load(&request.source, request.generation);
        if result_tx
            .send(WorkerResult {
                source: request.source,
                generation: request.generation,
                decoded,
            })
            .is_err()
        {
            return;
        }
    }
}

fn install_result_poller(inner: Rc<LoaderInner>, rx: mpsc::Receiver<WorkerResult>) {
    glib::timeout_add_local(RESULT_POLL_INTERVAL, move || {
        // Bound per-tick work so a burst of completed loads doesn't
        // monopolise the main thread. Stop on whichever fires first:
        // the batch cap, the wall-clock budget, an empty queue, or all
        // workers having exited.
        let started = std::time::Instant::now();
        let mut processed = 0;
        loop {
            if processed >= RESULT_BATCH_MAX || started.elapsed() >= RESULT_TICK_BUDGET {
                return glib::ControlFlow::Continue;
            }
            match rx.try_recv() {
                Ok(result) => {
                    if inner.repository.source_generation(&result.source) != result.generation {
                        processed += 1;
                        continue;
                    }
                    inner
                        .cache
                        .borrow_mut()
                        .insert(result.source.clone(), result.decoded.clone());
                    let callbacks = inner
                        .pending
                        .borrow_mut()
                        .remove(&result.source)
                        .unwrap_or_default();
                    for callback in callbacks {
                        callback(result.decoded.clone());
                    }
                    processed += 1;
                }
                Err(mpsc::TryRecvError::Empty) => return glib::ControlFlow::Continue,
                Err(mpsc::TryRecvError::Disconnected) => return glib::ControlFlow::Break,
            }
        }
    });
}

struct ArtworkRepository {
    metadata_service: Arc<dyn MetadataService>,
    disk_cache: Option<ArtworkDiskCache>,
    source_generations: Mutex<HashMap<ArtworkSource, u64>>,
}

impl ArtworkRepository {
    fn new(metadata_service: Arc<dyn MetadataService>, cache_dir: PathBuf) -> Self {
        Self {
            metadata_service,
            disk_cache: ArtworkDiskCache::open(&cache_dir),
            source_generations: Mutex::new(HashMap::new()),
        }
    }

    fn source_generation(&self, source: &ArtworkSource) -> u64 {
        self.source_generations
            .lock()
            .ok()
            .and_then(|generations| generations.get(source).copied())
            .unwrap_or_default()
    }

    fn invalidate(&self, source: &ArtworkSource) {
        let Ok(mut generations) = self.source_generations.lock() else {
            return;
        };
        let generation = generations.entry(source.clone()).or_default();
        *generation = generation.saturating_add(1);
        if let Some(disk_cache) = &self.disk_cache {
            disk_cache.delete(source);
        }
    }

    fn load(&self, source: &ArtworkSource, generation: u64) -> DecodedArtwork {
        let fingerprint = source.file_fingerprint();
        if let (Some(cache), Some(fingerprint)) = (&self.disk_cache, fingerprint)
            && let Some(decoded) = cache.load(source, fingerprint)
        {
            return decoded;
        }

        match source {
            ArtworkSource::EmbeddedTrack { file_path, .. } => {
                let bytes = self.metadata_service.read_artwork(file_path).ok().flatten();
                let decoded = decode_artwork(bytes);
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

struct ArtworkDiskCache {
    connection: Mutex<Connection>,
}

impl ArtworkDiskCache {
    fn open(cache_dir: &Path) -> Option<Self> {
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

    fn load(
        &self,
        source: &ArtworkSource,
        fingerprint: ArtworkFileFingerprint,
    ) -> Option<DecodedArtwork> {
        let (source_kind, source_key) = source.cache_key();
        let cached = {
            let connection = self.connection.lock().ok()?;
            connection
                .query_row(
                    r#"
                    SELECT tile_png,
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
                            tile_png: row.get(0)?,
                            detail_png: row.get(1)?,
                            palette: palette_components_from_cache_row(row)?,
                        })
                    },
                )
                .optional()
                .ok()
                .flatten()?
        };
        cached.decode()
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
                unixepoch()
            )
            ON CONFLICT(source_kind, source_key) DO UPDATE SET
                file_size = excluded.file_size,
                mtime_ns = excluded.mtime_ns,
                format_version = excluded.format_version,
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

#[derive(Default)]
struct DecodedArtworkRecord {
    artwork: DecodedArtwork,
    cache_entry: CachedArtwork,
}

#[derive(Default)]
struct CachedArtwork {
    tile_png: Option<Vec<u8>>,
    detail_png: Option<Vec<u8>>,
    palette: Option<ArtworkPaletteComponents>,
}

struct CachedArtworkRow {
    tile_png: Option<Vec<u8>>,
    detail_png: Option<Vec<u8>>,
    palette: Option<ArtworkPaletteComponents>,
}

impl CachedArtworkRow {
    fn decode(self) -> Option<DecodedArtwork> {
        if self.tile_png.is_none() && self.detail_png.is_none() && self.palette.is_none() {
            return Some(DecodedArtwork::default());
        }

        let tile_texture = self.tile_png.as_deref().and_then(texture_from_png)?;
        let detail_texture = self.detail_png.as_deref().and_then(texture_from_png)?;
        Some(DecodedArtwork {
            tile_texture: Some(tile_texture),
            detail_texture: Some(detail_texture),
            palette: self.palette.map(ArtworkPalette::from_components),
        })
    }
}

fn decode_artwork(bytes: Option<Vec<u8>>) -> DecodedArtworkRecord {
    let Some(bytes) = bytes else {
        return DecodedArtworkRecord::default();
    };
    let dimensions = match validate_encoded_artwork(&bytes) {
        Ok(dimensions) => dimensions,
        Err(_) => return DecodedArtworkRecord::default(),
    };
    let Some((decode_width, decode_height)) =
        scaled_dimensions(dimensions, DETAIL_TEXTURE_MAX_SIDE)
    else {
        return DecodedArtworkRecord::default();
    };
    let Some(pixbuf) = pixbuf_from_bytes_at_scale(bytes, decode_width, decode_height) else {
        return DecodedArtworkRecord::default();
    };

    let tile_pixbuf = scaled_pixbuf(&pixbuf, TILE_TEXTURE_MAX_SIDE);
    let detail_pixbuf = scaled_pixbuf(&pixbuf, DETAIL_TEXTURE_MAX_SIDE);
    let palette = ArtworkPalette::from_pixbuf(&pixbuf);
    let cache_entry = CachedArtwork {
        tile_png: tile_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        detail_png: detail_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        palette: palette.map(ArtworkPalette::components),
    };
    let artwork = DecodedArtwork {
        tile_texture: tile_pixbuf.as_ref().map(gdk::Texture::for_pixbuf),
        detail_texture: detail_pixbuf.as_ref().map(gdk::Texture::for_pixbuf),
        palette,
    };

    DecodedArtworkRecord {
        artwork,
        cache_entry,
    }
}

/// Scale the source so the shorter side equals `max_side` (or the source's
/// own shorter side, whichever is smaller), then center-crop to a square.
///
/// The resulting pixbuf is always square. That matters because `GtkPicture`
/// with `ContentFit::Contain` does HEIGHT_FOR_WIDTH measurement: a 132×131
/// texture in a 132-wide cover would request natural height ≠ 132, which
/// propagates through the cover Box's measure and shifts the labels' Y
/// position downstream. Forcing the texture square pins natural height to
/// width across every album, regardless of source aspect ratio, and matches
/// the iTunes-style square-thumbnail look.
fn scaled_pixbuf(pixbuf: &gdk_pixbuf::Pixbuf, max_side: i32) -> Option<gdk_pixbuf::Pixbuf> {
    let width = pixbuf.width();
    let height = pixbuf.height();
    if width <= 0 || height <= 0 || max_side <= 0 {
        return None;
    }

    let shorter_side = width.min(height);
    let scale = (f64::from(max_side) / f64::from(shorter_side)).min(1.0);
    let scaled_width = (f64::from(width) * scale).round().max(1.0) as i32;
    let scaled_height = (f64::from(height) * scale).round().max(1.0) as i32;

    let scaled = if scaled_width == width && scaled_height == height {
        pixbuf.clone()
    } else {
        pixbuf.scale_simple(
            scaled_width,
            scaled_height,
            gdk_pixbuf::InterpType::Bilinear,
        )?
    };

    let side = scaled.width().min(scaled.height());
    if scaled.width() == side && scaled.height() == side {
        return Some(scaled);
    }

    let x_offset = (scaled.width() - side) / 2;
    let y_offset = (scaled.height() - side) / 2;
    let cropped = gdk_pixbuf::Pixbuf::new(
        scaled.colorspace(),
        scaled.has_alpha(),
        scaled.bits_per_sample(),
        side,
        side,
    )?;
    scaled.copy_area(x_offset, y_offset, side, side, &cropped, 0, 0);
    Some(cropped)
}

fn pixbuf_png_bytes(pixbuf: &gdk_pixbuf::Pixbuf) -> Option<Vec<u8>> {
    pixbuf.save_to_bufferv("png", &[]).ok()
}

fn texture_from_png(bytes: &[u8]) -> Option<gdk::Texture> {
    let dimensions = validate_encoded_artwork(bytes).ok()?;
    let width = i32::try_from(dimensions.width).ok()?;
    let height = i32::try_from(dimensions.height).ok()?;
    let pixbuf = pixbuf_from_bytes_at_scale(bytes.to_vec(), width, height)?;
    Some(gdk::Texture::for_pixbuf(&pixbuf))
}

fn scaled_dimensions(dimensions: ArtworkDimensions, max_side: i32) -> Option<(i32, i32)> {
    let width = i32::try_from(dimensions.width).ok()?;
    let height = i32::try_from(dimensions.height).ok()?;
    let shorter_side = width.min(height);
    if shorter_side <= 0 || max_side <= 0 {
        return None;
    }
    let scale = (f64::from(max_side) / f64::from(shorter_side)).min(1.0);
    Some((
        (f64::from(width) * scale).round().max(1.0) as i32,
        (f64::from(height) * scale).round().max(1.0) as i32,
    ))
}

fn pixbuf_from_bytes_at_scale(
    bytes: Vec<u8>,
    width: i32,
    height: i32,
) -> Option<gdk_pixbuf::Pixbuf> {
    let bytes = glib::Bytes::from_owned(bytes);
    let stream = gio::MemoryInputStream::from_bytes(&bytes);
    gdk_pixbuf::Pixbuf::from_stream_at_scale(
        &stream,
        width,
        height,
        false,
        None::<&gio::Cancellable>,
    )
    .ok()
}

fn palette_components_from_cache_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<Option<ArtworkPaletteComponents>> {
    let Some(background) = rgb_from_cache_columns(row, 2)? else {
        return Ok(None);
    };
    let Some(foreground) = rgb_from_cache_columns(row, 5)? else {
        return Ok(None);
    };
    let Some(secondary) = rgb_from_cache_columns(row, 8)? else {
        return Ok(None);
    };
    Ok(Some(ArtworkPaletteComponents {
        background,
        foreground,
        secondary,
    }))
}

fn rgb_from_cache_columns(
    row: &rusqlite::Row<'_>,
    first_column: usize,
) -> rusqlite::Result<Option<RgbColorComponents>> {
    let red: Option<i64> = row.get(first_column)?;
    let green: Option<i64> = row.get(first_column + 1)?;
    let blue: Option<i64> = row.get(first_column + 2)?;
    let (Some(red), Some(green), Some(blue)) = (red, green, blue) else {
        return Ok(None);
    };
    let (Ok(red), Ok(green), Ok(blue)) =
        (u8::try_from(red), u8::try_from(green), u8::try_from(blue))
    else {
        return Ok(None);
    };
    Ok(Some(RgbColorComponents { red, green, blue }))
}

#[cfg(test)]
mod tests {
    use super::texture_from_png;

    #[test]
    fn corrupt_cached_png_degrades_to_placeholder() {
        assert!(texture_from_png(b"not a cached PNG").is_none());
    }
}
