// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use gtk::glib;
use sustain_app_runtime::MetadataService;

use super::decode::{ArtworkVariant, DecodedArtwork, decode_artwork};
use super::disk_cache::{ArtworkRepository, ArtworkSource};

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

/// Bounded least-recently-used map.
///
/// Inserting a new key while at capacity evicts the least-recently-used
/// entry; both `get` and `insert` count as a use. Single-threaded — the
/// loader holds it behind a `RefCell` and only touches it from the GTK main
/// thread, so no locking is needed.
pub(super) struct LruCache<K, V> {
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
    pub(super) fn new(capacity: usize) -> Self {
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

    pub(super) fn get(&mut self, key: &K) -> Option<&V> {
        let tick = self.next_tick();
        let entry = self.entries.get_mut(key)?;
        entry.used_at = tick;
        Some(&entry.value)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
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

    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
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

pub(super) struct LoaderInner {
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

pub(super) struct WorkerRequest {
    source: ArtworkSource,
    generation: u64,
    variant: ArtworkVariant,
}

pub(super) struct WorkerResult {
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

pub(super) fn worker_loop(
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
pub(super) fn spawn_result_drainer(
    inner: Rc<LoaderInner>,
    rx: async_channel::Receiver<WorkerResult>,
) {
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

#[cfg(test)]
mod tests {
    use super::LruCache;

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
