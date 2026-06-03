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
//! * A small pool of worker threads consumes requests from a shared queue
//!   and runs the **entire** decode pipeline off the main thread: source
//!   resolution, `MetadataService::read_artwork` for embedded-track artwork,
//!   scaled pixbuf decode, `ArtworkPalette::from_pixbuf`, and a bounded
//!   `gdk::Texture` for the *one* size the request asked for (tile or
//!   detail). The pixbuf itself is dropped on the worker; only the finished
//!   `DecodedArtwork` is handed back. (This relies on `gdk::Texture` being
//!   `Send + Sync` in gtk-rs — it is, because GdkTexture is documented as
//!   immutable after construction.)
//! * The two sizes live in separate bounded caches: tiles (the cheap grid
//!   cover) are retained generously so scrollback never thrashes, while the
//!   far heavier detail texture is kept to a small window so it cannot
//!   exhaust the GPU's device-local memory. See `MAX_CACHED_TILE_ARTWORKS`
//!   and `MAX_CACHED_DETAIL_ARTWORKS`.
//! * A GTK main-loop poller drains the result channel under a strict
//!   per-tick budget (small max batch + short wall-clock cap) so even a
//!   burst of completions can't monopolise the main thread, places each
//!   result in the cache for its size, and fires every callback that was
//!   waiting on that (source, size).
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
    time::{Duration, Instant},
};

use gtk::{gdk, gdk_pixbuf, gio, glib};
use rusqlite::{Connection, OptionalExtension, params};
use sustain_app_runtime::MetadataService;
use sustain_artwork::{
    ArtworkDimensions, MAX_ENCODED_ARTWORK_BYTES, validate_dimensions, validate_encoded_artwork,
};

use crate::artwork_color::{ArtworkPalette, ArtworkPaletteComponents, RgbColorComponents};

/// Lower and upper bounds on the artwork worker pool (see [`worker_count`]).
const WORKER_COUNT_MIN: usize = 4;
const WORKER_COUNT_MAX: usize = 8;

/// Number of worker threads pulling from the request queue, scaled to the
/// machine.
///
/// Each request is a quick SQLite cache read followed — for the common warm
/// load — by a CPU-bound PNG/pixbuf decode. That decode runs *outside* the
/// cache's connection lock, so it parallelises: with more threads, a burst
/// of newly-visible covers during a fast Albums scroll fills in proportionally
/// fewer rounds, which is what keeps the grid feeling fluid.
///
/// Bounded at both ends. The floor keeps low-core machines on a usable pool
/// (and matches the previous fixed size). The ceiling reflects diminishing
/// returns — a viewport row is only a handful of covers, so past a small pool
/// extra threads stop helping while still holding transient decode buffers;
/// detail loads happen one at a time and don't benefit from width at all.
/// Not user-configurable: the useful range is narrow and self-tuning.
fn worker_count() -> usize {
    // `available_parallelism` only errs in exotic sandboxes; fall back to the
    // floor there rather than guessing high.
    thread::available_parallelism()
        .map(usize::from)
        .unwrap_or(WORKER_COUNT_MIN)
        .clamp(WORKER_COUNT_MIN, WORKER_COUNT_MAX)
}

/// Wall-clock budget for one slice of result delivery on the main loop.
///
/// Decoded covers are drained the instant they arrive (see
/// [`spawn_result_drainer`]), not on a fixed poll; this only bounds how long
/// a *single* drain slice runs before yielding, so a flood of completions
/// can't stall a frame. Each delivery is a cheap texture swap, so 4 ms clears
/// dozens of covers — far more than a viewport — before the breather below.
const RESULT_DELIVERY_BUDGET: Duration = Duration::from_millis(4);

/// Breather awaited after a drain slice exhausts [`RESULT_DELIVERY_BUDGET`],
/// letting GTK paint the just-delivered covers before the next slice. Only
/// reached under a sustained flood (e.g. warming thousands of tiles); the
/// common viewport-sized burst drains in one slice and never waits.
const RESULT_DELIVERY_PACING: Duration = Duration::from_millis(8);

/// Maximum side length of the smaller cached texture. Sized to cover
/// the Albums grid tile (132px) and the now-playing tile (72px) without
/// either having to upscale. Bigger consumers (album-detail panel,
/// lyrics/artwork overlay) use the detail texture below.
const TILE_TEXTURE_MAX_SIDE: i32 = 132;

/// Maximum side length of the larger cached texture. Sized to cover
/// the album-detail panel (3× the grid tile). The cache stores PNG
/// payloads at this size; views downscale further at paint time.
const DETAIL_TEXTURE_MAX_SIDE: i32 = TILE_TEXTURE_MAX_SIDE * 3;

/// Upper bound on resident *tile* textures.
///
/// A tile is the small grid / now-playing cover — at 132px RGBA roughly
/// 70 KB of texture memory. Tiles are what the Albums grid paints, so they
/// must stay resident generously: evicting a cover the user can still
/// scroll back to is exactly the thrash that makes the grid feel broken.
/// This ceiling holds several thousand tiles — far more than any viewport
/// and enough to keep a typical library's covers resident for a whole
/// session — while staying bounded so a pathological library can't grow it
/// without limit. At ~70 KB each the worst case is on the order of a few
/// hundred MB of readily-evictable texture memory, which the GPU keeps in
/// the large GTT/host heap rather than the small device-local VRAM heap.
const MAX_CACHED_TILE_ARTWORKS: usize = 4096;

/// Upper bound on resident *detail* textures.
///
/// A detail texture is the 396px panel/overlay cover — at RGBA roughly
/// 610 KB, nearly 9× a tile, and the dominant pressure on the device-local
/// VRAM heap the Vulkan renderer allocates swapchain and active textures
/// from. On the maintainer's iGPU that heap is only 512 MB, most of it
/// already spent on dual-4K scanout, so an unbounded detail set exhausted
/// it and aborted the renderer with `VK_ERROR_OUT_OF_DEVICE_MEMORY` (#170).
/// Only one detail cover is on screen at a time (the album-detail panel,
/// Get Info, or the lyrics/artwork overlay); this small window keeps recent
/// navigation instant while bounding detail-texture memory to a low, fixed
/// fraction of the heap (~32 × 610 KB ≈ 20 MB). An evicted detail reloads
/// from the on-disk PNG cache on the worker pool — at most a brief
/// re-decode, never a re-read of the audio file.
const MAX_CACHED_DETAIL_ARTWORKS: usize = 32;

const CACHE_SCHEMA_VERSION: i64 = 2;
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
    pub(crate) dimensions: Option<ArtworkDimensions>,
    pub(crate) encoded_bytes_len: Option<usize>,
}

/// Which cached texture size a request wants.
///
/// The two sizes have very different memory profiles, so they live in
/// separate bounded caches (see [`MAX_CACHED_TILE_ARTWORKS`] and
/// [`MAX_CACHED_DETAIL_ARTWORKS`]) and a worker only uploads the texture for
/// the size that was actually asked for. The on-disk cache always holds both
/// PNG payloads, so the *other* size, when later requested, is produced from
/// disk without re-reading the audio file.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum ArtworkVariant {
    /// Small grid / now-playing cover ([`TILE_TEXTURE_MAX_SIDE`]).
    Tile,
    /// Large panel / overlay cover ([`DETAIL_TEXTURE_MAX_SIDE`]).
    Detail,
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

/// Bounded least-recently-used map.
///
/// Inserting a new key while at capacity evicts the least-recently-used
/// entry; both `get` and `insert` count as a use. Single-threaded — the
/// loader holds it behind a `RefCell` and only touches it from the GTK main
/// thread, so no locking is needed.
struct LruCache<K, V> {
    capacity: usize,
    entries: HashMap<K, LruEntry<V>>,
    clock: u64,
}

struct LruEntry<V> {
    value: V,
    used_at: u64,
}

impl<K, V> LruCache<K, V>
where
    K: Clone + Eq + std::hash::Hash,
{
    fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            entries: HashMap::new(),
            clock: 0,
        }
    }

    /// Monotonic recency stamp. `wrapping_add` only matters after 2^64
    /// accesses (unreachable in any real session); it keeps the counter
    /// total rather than risking a panic on the theoretical overflow.
    fn next_tick(&mut self) -> u64 {
        self.clock = self.clock.wrapping_add(1);
        self.clock
    }

    fn get(&mut self, key: &K) -> Option<&V> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        entry.used_at = tick;
        Some(&entry.value)
    }

    fn insert(&mut self, key: K, value: V) {
        let tick = self.next_tick();
        if let Some(entry) = self.entries.get_mut(&key) {
            entry.value = value;
            entry.used_at = tick;
            return;
        }
        if self.entries.len() >= self.capacity {
            self.evict_least_recently_used();
        }
        self.entries.insert(
            key,
            LruEntry {
                value,
                used_at: tick,
            },
        );
    }

    fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key).map(|entry| entry.value)
    }

    fn evict_least_recently_used(&mut self) {
        let oldest = self
            .entries
            .iter()
            .min_by_key(|(_, entry)| entry.used_at)
            .map(|(key, _)| key.clone());
        if let Some(key) = oldest {
            self.entries.remove(&key);
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    fn contains(&self, key: &K) -> bool {
        self.entries.contains_key(key)
    }
}

pub(crate) type ArtworkCallback = Box<dyn FnOnce(DecodedArtwork) + 'static>;

#[derive(Clone)]
pub(crate) struct ArtworkLoader {
    inner: Rc<LoaderInner>,
}

struct LoaderInner {
    repository: Arc<ArtworkRepository>,
    request_tx: mpsc::Sender<WorkerRequest>,
    tiles: RefCell<LruCache<ArtworkSource, DecodedArtwork>>,
    details: RefCell<LruCache<ArtworkSource, DecodedArtwork>>,
    pending: RefCell<HashMap<(ArtworkSource, ArtworkVariant), Vec<ArtworkCallback>>>,
}

impl LoaderInner {
    /// The resident cache backing a given size. Single source of truth for
    /// the variant→cache mapping, shared by the public API and the poller.
    fn cache_for(
        &self,
        variant: ArtworkVariant,
    ) -> &RefCell<LruCache<ArtworkSource, DecodedArtwork>> {
        match variant {
            ArtworkVariant::Tile => &self.tiles,
            ArtworkVariant::Detail => &self.details,
        }
    }
}

struct WorkerRequest {
    source: ArtworkSource,
    generation: u64,
    variant: ArtworkVariant,
}

struct WorkerResult {
    source: ArtworkSource,
    generation: u64,
    variant: ArtworkVariant,
    decoded: DecodedArtwork,
}

impl ArtworkLoader {
    pub(crate) fn new(metadata_service: Arc<dyn MetadataService>, cache_dir: PathBuf) -> Self {
        let repository = Arc::new(ArtworkRepository::new(metadata_service, cache_dir));
        let (request_tx, request_rx) = mpsc::channel::<WorkerRequest>();
        let request_rx = Arc::new(Mutex::new(request_rx));
        let (result_tx, result_rx) = async_channel::unbounded::<WorkerResult>();

        for index in 0..worker_count() {
            let request_rx = Arc::clone(&request_rx);
            let result_tx = result_tx.clone();
            let repository = Arc::clone(&repository);
            thread::Builder::new()
                .name(format!("sustain-artwork-{index}"))
                .spawn(move || worker_loop(request_rx, result_tx, repository))
                .expect("spawn artwork worker thread");
        }
        // Workers each keep their own clone of the result sender; drop
        // the original so the drainer's `recv` reports the channel closed
        // once every worker has exited.
        drop(result_tx);

        let inner = Rc::new(LoaderInner {
            repository,
            request_tx,
            tiles: RefCell::new(LruCache::new(MAX_CACHED_TILE_ARTWORKS)),
            details: RefCell::new(LruCache::new(MAX_CACHED_DETAIL_ARTWORKS)),
            pending: RefCell::new(HashMap::new()),
        });

        spawn_result_drainer(Rc::clone(&inner), result_rx);

        Self { inner }
    }

    /// Returns the decoded *tile* entry for `source`, if resident. Lets a
    /// view reuse what another view already produced rather than reading the
    /// file a second time.
    pub(crate) fn cached(&self, source: &ArtworkSource) -> Option<DecodedArtwork> {
        self.cached_variant(source, ArtworkVariant::Tile)
    }

    /// Request the decoded *tile* artwork for `source`. The callback fires on
    /// the main thread when the artwork becomes available, or synchronously
    /// when the in-memory cache already holds the entry — so a tile whose
    /// neighbour just resolved the same file never schedules redundant disk
    /// work.
    ///
    /// The loader has no notion of staleness; callbacks always fire.
    /// Each caller is responsible for checking, inside its closure,
    /// whether the result still applies to the widget it would update
    /// (e.g. via a per-view generation counter). Keeping that policy
    /// with the caller is what lets one shared loader serve multiple
    /// independent views without their rebuilds invalidating each
    /// other's in-flight requests.
    pub(crate) fn request(&self, source: ArtworkSource, callback: ArtworkCallback) {
        self.request_variant(source, ArtworkVariant::Tile, callback);
    }

    /// Returns the decoded *detail* entry for `source`, if resident.
    ///
    /// The detail texture is the larger panel/overlay cover; it lives in its
    /// own small bounded cache, separate from the tiles, so the heavy detail
    /// textures cannot crowd the grid's tiles out of memory.
    pub(crate) fn cached_detail(&self, source: &ArtworkSource) -> Option<DecodedArtwork> {
        self.cached_variant(source, ArtworkVariant::Detail)
    }

    /// Request the decoded *detail* artwork for `source`. Same delivery
    /// semantics as [`Self::request`], but resolves the larger detail texture
    /// and caches it separately.
    pub(crate) fn request_detail(&self, source: ArtworkSource, callback: ArtworkCallback) {
        self.request_variant(source, ArtworkVariant::Detail, callback);
    }

    fn cached_variant(
        &self,
        source: &ArtworkSource,
        variant: ArtworkVariant,
    ) -> Option<DecodedArtwork> {
        self.inner
            .cache_for(variant)
            .borrow_mut()
            .get(source)
            .cloned()
    }

    fn request_variant(
        &self,
        source: ArtworkSource,
        variant: ArtworkVariant,
        callback: ArtworkCallback,
    ) {
        // Clone the hit out and release the cache borrow before invoking the
        // callback: `get` mutates the LRU recency, so holding the borrow
        // across a callback that re-enters (`cached`/`request`) would panic.
        let cached = self
            .inner
            .cache_for(variant)
            .borrow_mut()
            .get(&source)
            .cloned();
        if let Some(decoded) = cached {
            callback(decoded);
            return;
        }
        let key = (source.clone(), variant);
        let mut pending = self.inner.pending.borrow_mut();
        let needs_queue = !pending.contains_key(&key);
        pending.entry(key).or_default().push(callback);
        if needs_queue {
            // Send only fails if every worker has exited, which happens
            // exclusively at shutdown. Drop the callback silently in
            // that case — there is no view left to update.
            let generation = self.inner.repository.source_generation(&source);
            let _ = self.inner.request_tx.send(WorkerRequest {
                source,
                generation,
                variant,
            });
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
        // Forget the decoded values (both sizes) and any queued callbacks
        // before advancing the source generation. Results already in flight
        // are discarded by the drainer; advancing the repository generation
        // also prevents those workers from repopulating the on-disk cache
        // after its matching row is evicted.
        self.inner.tiles.borrow_mut().remove(source);
        self.inner.details.borrow_mut().remove(source);
        {
            let mut pending = self.inner.pending.borrow_mut();
            pending.remove(&(source.clone(), ArtworkVariant::Tile));
            pending.remove(&(source.clone(), ArtworkVariant::Detail));
        }
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
        // Populate both caches so the new cover is visible whether the next
        // reader asks for the tile (now-playing tile) or the detail
        // (lyrics/artwork overlay). Decoding twice is acceptable: priming
        // happens once, on a manual cover accept, not on a hot path.
        let tile = decode_artwork(Some(bytes.clone()), ArtworkVariant::Tile);
        let detail = decode_artwork(Some(bytes), ArtworkVariant::Detail);
        self.inner
            .tiles
            .borrow_mut()
            .insert(source.clone(), tile.artwork);
        self.inner
            .details
            .borrow_mut()
            .insert(source, detail.artwork);
    }
}

fn worker_loop(
    request_rx: Arc<Mutex<mpsc::Receiver<WorkerRequest>>>,
    result_tx: async_channel::Sender<WorkerResult>,
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
        let decoded = repository.load(&request.source, request.generation, request.variant);
        // The channel is unbounded, so `send_blocking` never actually
        // blocks; it only errs once every receiver is gone — i.e. at
        // shutdown — at which point the worker stops.
        if result_tx
            .send_blocking(WorkerResult {
                source: request.source,
                generation: request.generation,
                variant: request.variant,
                decoded,
            })
            .is_err()
        {
            return;
        }
    }
}

/// Drive result delivery from the worker pool onto the GTK main loop.
///
/// Event-driven rather than polled: the task parks on `recv().await` with
/// zero idle cost until a worker finishes a decode, then drains every cover
/// that is already ready in one go, so a viewport-sized burst lands within a
/// single frame instead of trickling in at a fixed poll rate. A wall-clock
/// budget caps each drain slice and yields a breather under a sustained flood
/// so painting never starves.
fn spawn_result_drainer(inner: Rc<LoaderInner>, rx: async_channel::Receiver<WorkerResult>) {
    glib::MainContext::default().spawn_local(async move {
        loop {
            // Park (no idle wakeups) until at least one result is ready.
            match rx.recv().await {
                Ok(result) => deliver_result(&inner, result),
                // Every worker has exited: shutdown. Stop the task.
                Err(_) => return,
            }
            // Drain whatever else already completed without re-awaiting, so
            // a burst lands together; bound the slice so a flood can't hog
            // the frame, yielding a breather to let GTK paint between slices.
            let mut slice_started = Instant::now();
            loop {
                if slice_started.elapsed() >= RESULT_DELIVERY_BUDGET {
                    glib::timeout_future(RESULT_DELIVERY_PACING).await;
                    slice_started = Instant::now();
                }
                match rx.try_recv() {
                    Ok(result) => deliver_result(&inner, result),
                    Err(async_channel::TryRecvError::Empty) => break,
                    Err(async_channel::TryRecvError::Closed) => return,
                }
            }
        }
    });
}

/// Place one finished decode into its size's cache and fire the callbacks
/// waiting on it — unless a newer invalidation has superseded the source, in
/// which case the stale result is dropped.
fn deliver_result(inner: &LoaderInner, result: WorkerResult) {
    if inner.repository.source_generation(&result.source) != result.generation {
        return;
    }
    inner
        .cache_for(result.variant)
        .borrow_mut()
        .insert(result.source.clone(), result.decoded.clone());
    let callbacks = inner
        .pending
        .borrow_mut()
        .remove(&(result.source.clone(), result.variant))
        .unwrap_or_default();
    for callback in callbacks {
        callback(result.decoded.clone());
    }
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

    fn load(
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

    fn load(
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

#[derive(Default)]
struct DecodedArtworkRecord {
    artwork: DecodedArtwork,
    cache_entry: CachedArtwork,
}

#[derive(Default)]
struct CachedArtwork {
    dimensions: Option<ArtworkDimensions>,
    encoded_bytes_len: Option<usize>,
    tile_png: Option<Vec<u8>>,
    detail_png: Option<Vec<u8>>,
    palette: Option<ArtworkPaletteComponents>,
}

struct CachedArtworkRow {
    original_width: Option<i64>,
    original_height: Option<i64>,
    encoded_bytes_len: Option<i64>,
    tile_png: Option<Vec<u8>>,
    detail_png: Option<Vec<u8>>,
    palette: Option<ArtworkPaletteComponents>,
}

impl CachedArtworkRow {
    /// Reconstruct the in-memory artwork for one requested size.
    ///
    /// Only the requested variant's PNG is decoded into a `gdk::Texture`;
    /// the other size's payload stays on disk untouched. This is what keeps
    /// a tile load from materialising the much larger detail texture, so the
    /// disk path preserves the same memory discipline as a fresh decode.
    fn decode(self, variant: ArtworkVariant) -> Option<DecodedArtwork> {
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

fn decode_artwork(bytes: Option<Vec<u8>>, variant: ArtworkVariant) -> DecodedArtworkRecord {
    let Some(bytes) = bytes else {
        return DecodedArtworkRecord::default();
    };
    let encoded_bytes_len = bytes.len();
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

    // Both sizes are always scaled and PNG-encoded for the on-disk cache, so
    // the *other* size can later be served from disk without re-reading the
    // audio file. Only the requested size is uploaded to a GPU texture.
    let tile_pixbuf = scaled_pixbuf(&pixbuf, TILE_TEXTURE_MAX_SIDE);
    let detail_pixbuf = scaled_pixbuf(&pixbuf, DETAIL_TEXTURE_MAX_SIDE);
    let palette = ArtworkPalette::from_pixbuf(&pixbuf);
    let cache_entry = CachedArtwork {
        dimensions: Some(dimensions),
        encoded_bytes_len: Some(encoded_bytes_len),
        tile_png: tile_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        detail_png: detail_pixbuf.as_ref().and_then(pixbuf_png_bytes),
        palette: palette.map(ArtworkPalette::components),
    };
    let artwork = artwork_for_variant(
        variant,
        tile_pixbuf.as_ref(),
        detail_pixbuf.as_ref(),
        palette,
        Some(dimensions),
        Some(encoded_bytes_len),
    );

    DecodedArtworkRecord {
        artwork,
        cache_entry,
    }
}

/// Assemble the in-memory [`DecodedArtwork`] for a single requested size.
///
/// Only the requested variant's `gdk::Texture` is uploaded; the other stays
/// `None`. This is the crux of the tile/detail split — a grid tile request
/// never materialises the ~9×-larger detail texture, so scrolling the Albums
/// grid cannot accumulate detail textures in the small device-local VRAM
/// heap. The palette and dimensions are size-independent and travel with
/// either variant.
fn artwork_for_variant(
    variant: ArtworkVariant,
    tile_pixbuf: Option<&gdk_pixbuf::Pixbuf>,
    detail_pixbuf: Option<&gdk_pixbuf::Pixbuf>,
    palette: Option<ArtworkPalette>,
    dimensions: Option<ArtworkDimensions>,
    encoded_bytes_len: Option<usize>,
) -> DecodedArtwork {
    let (tile_texture, detail_texture) = match variant {
        ArtworkVariant::Tile => (tile_pixbuf.map(gdk::Texture::for_pixbuf), None),
        ArtworkVariant::Detail => (None, detail_pixbuf.map(gdk::Texture::for_pixbuf)),
    };
    DecodedArtwork {
        tile_texture,
        detail_texture,
        palette,
        dimensions,
        encoded_bytes_len,
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
    let Some(background) = rgb_from_cache_columns(row, 5)? else {
        return Ok(None);
    };
    let Some(foreground) = rgb_from_cache_columns(row, 8)? else {
        return Ok(None);
    };
    let Some(secondary) = rgb_from_cache_columns(row, 11)? else {
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
    use super::{LruCache, texture_from_png};

    #[test]
    fn corrupt_cached_png_degrades_to_placeholder() {
        assert!(texture_from_png(b"not a cached PNG").is_none());
    }

    #[test]
    fn lru_evicts_least_recently_used_when_full() {
        let mut cache = LruCache::new(2);
        cache.insert(1, "a");
        cache.insert(2, "b");
        // Touch 1 so 2 becomes the least-recently-used entry.
        assert_eq!(cache.get(&1), Some(&"a"));
        cache.insert(3, "c");
        assert_eq!(cache.len(), 2);
        assert!(cache.contains(&1));
        assert!(cache.contains(&3));
        assert!(!cache.contains(&2));
    }

    #[test]
    fn lru_reinsert_refreshes_without_growing_or_evicting() {
        let mut cache = LruCache::new(2);
        cache.insert(1, "a");
        cache.insert(2, "b");
        cache.insert(1, "a2");
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.get(&1), Some(&"a2"));
        // 1 was just refreshed, so the next new key evicts 2, not 1.
        cache.insert(3, "c");
        assert!(cache.contains(&1));
        assert!(!cache.contains(&2));
    }

    #[test]
    fn lru_get_miss_and_remove() {
        let mut cache: LruCache<i32, &str> = LruCache::new(2);
        assert_eq!(cache.get(&1), None);
        cache.insert(1, "a");
        assert_eq!(cache.remove(&1), Some("a"));
        assert!(!cache.contains(&1));
        assert_eq!(cache.remove(&1), None);
    }

    #[test]
    fn lru_capacity_is_clamped_to_at_least_one() {
        let mut cache = LruCache::new(0);
        cache.insert(1, "a");
        cache.insert(2, "b");
        assert_eq!(cache.len(), 1);
        assert!(cache.contains(&2));
    }
}
