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
use hashlink::LinkedHashMap;
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
/// 70 KB of pixel payload before renderer-specific allocation overhead. The
/// Albums grid virtualizes its widgets, not its artwork working set: after a
/// tile leaves the LRU, scrolling back must read its PNG from SQLite and
/// decode/upload it again. A viewport-sized cache therefore makes long,
/// fast scrolls and scrollback visibly churn.
///
/// Keep several thousand tiles resident so a typical library's album covers
/// warm once per session. The 4,096-entry ceiling still bounds the pixel
/// payload near 285 MB while avoiding that steady-state decode churn. Do not
/// lower it merely to fit the realized viewport; change it only with album-
/// grid scroll profiling and renderer-memory measurements on the affected
/// hardware. The separately bounded worker-result channel prevents completed
/// textures from accumulating outside this LRU (#176).
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
/// entry; both `get` and `insert` count as a use. Lookup, refresh, insertion,
/// and eviction are all constant-time: eviction happens on the GTK main
/// thread as artwork results arrive, so scanning the full 4,096-tile cache
/// for every newly visible cover would produce a main-loop stall at each
/// album-row boundary. Single-threaded — the loader holds it behind a
/// `RefCell` and only touches it from the GTK main thread, so no locking is
/// needed.
pub(super) struct LruCache<K, V> {
    capacity: usize,
    /// Oldest entry at the front, most-recently-used entry at the back.
    entries: LinkedHashMap<K, V>,
}

impl<K, V> LruCache<K, V>
where
    K: Eq + std::hash::Hash,
{
    pub(super) fn new(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            capacity,
            entries: LinkedHashMap::with_capacity(capacity),
        }
    }

    pub(super) fn get(&mut self, key: &K) -> Option<&V> {
        self.entries.to_back(key).map(|value| &*value)
    }

    pub(super) fn insert(&mut self, key: K, value: V) {
        // Evict before inserting so the backing hash table never needs to
        // grow beyond the configured cache capacity. Existing entries are
        // replaced in place and moved to the MRU end by `insert`.
        if self.entries.len() >= self.capacity && !self.entries.contains_key(&key) {
            self.entries.pop_front();
        }
        self.entries.insert(key, value);
    }

    pub(super) fn remove(&mut self, key: &K) -> Option<V> {
        self.entries.remove(key)
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
    /// Worker results that reached [`deliver_result`], stale ones
    /// included, so a test can wait for a result that answers nobody.
    #[cfg(test)]
    delivered_results: std::cell::Cell<usize>,
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
        let worker_count = worker_count();
        // Each queued result owns a GdkTexture outside the resident LRUs.
        // Limit the queue to one completed result per worker so a busy or
        // renderer-blocked main loop applies backpressure instead of allowing
        // transient texture memory to grow without bound.
        let (result_tx, result_rx) = async_channel::bounded::<WorkerResult>(worker_count);

        for index in 0..worker_count {
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
            #[cfg(test)]
            delivered_results: std::cell::Cell::new(0),
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
    /// The callback fires exactly once, with artwork for the source's
    /// current generation: a request still waiting when the source is
    /// invalidated or primed is answered with the new artwork, never
    /// dropped. Only shutdown, when no view is left, leaves it unfired.
    ///
    /// The loader has no notion of view staleness. Each caller is
    /// responsible for checking, inside its closure,
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
    /// Requests still waiting for `source` are kept and loaded again at
    /// the new generation, so they are answered with the new artwork.
    /// Nothing else is repainted from here: views that already show the
    /// old artwork are expected to re-issue their request (typically via
    /// their existing track-row-changed callback). That keeps the
    /// invalidation hook narrowly responsible and avoids reaching across
    /// the UI tree from a model-layer cache.
    pub(crate) fn invalidate(&self, source: &ArtworkSource) {
        // Forget the decoded values (both sizes) before advancing the source
        // generation. Results already in flight are discarded by the
        // drainer; advancing the repository generation also prevents those
        // workers from repopulating the on-disk cache after its matching
        // row is evicted.
        self.inner.tiles.borrow_mut().remove(source);
        self.inner.details.borrow_mut().remove(source);
        self.inner.repository.invalidate(source);
        let generation = self.inner.repository.source_generation(source);
        for variant in [ArtworkVariant::Tile, ArtworkVariant::Detail] {
            let waiting = self
                .inner
                .pending
                .borrow()
                .contains_key(&(source.clone(), variant));
            if waiting {
                // As in `request_variant`, a send fails only at shutdown.
                let _ = self.inner.request_tx.send(WorkerRequest {
                    source: source.clone(),
                    generation,
                    variant,
                });
            }
        }
    }

    /// Replace `source`'s artwork with decoded artwork built from
    /// already-in-memory bytes.
    ///
    /// Used after a remote fetch lands: the tag-write that persists
    /// the bytes is asynchronous, so a naive "invalidate then
    /// re-request" path would race the writer and briefly display
    /// the missing-artwork state. Priming the in-memory cache
    /// directly with the freshly-decoded artwork makes the new cover
    /// visible on the very next [`Self::cached`] / [`Self::request`]
    /// call without depending on disk write ordering.
    ///
    /// Priming advances the source generation, so loads already in
    /// flight cannot overwrite the primed artwork with what the file
    /// held before the write. Requests still waiting are answered with
    /// the primed artwork. The disk cache row is dropped and will be
    /// repopulated by the next miss-driven worker load once the tag
    /// write has landed and the file fingerprint has updated.
    pub(crate) fn prime(&self, source: ArtworkSource, bytes: Vec<u8>) {
        self.prime_from_bytes(source, Some(bytes));
    }

    /// Insert an explicit "no artwork" entry for `source`.
    ///
    /// Manual artwork removal is committed through the asynchronous tag writer.
    /// Priming the in-memory caches as absent lets the UI reflect the accepted
    /// user intent immediately without racing a worker reload against the
    /// still-pending file rewrite.
    pub(crate) fn prime_missing(&self, source: ArtworkSource) {
        self.prime_from_bytes(source, None);
    }

    fn prime_from_bytes(&self, source: ArtworkSource, bytes: Option<Vec<u8>>) {
        self.inner.repository.invalidate(&source);
        // Populate both caches so the new cover is visible whether the next
        // reader asks for the tile (now-playing tile) or the detail
        // (lyrics/artwork overlay). Decoding twice is acceptable: priming
        // happens once, on a manual cover accept, not on a hot path.
        //
        // Both caches are filled before any callback runs, so a callback
        // that asks for the other size gets the primed artwork too.
        let tile = decode_artwork(bytes.clone(), ArtworkVariant::Tile).artwork;
        let detail = decode_artwork(bytes, ArtworkVariant::Detail).artwork;
        let tile_callbacks = cache(&self.inner, &source, ArtworkVariant::Tile, tile.clone());
        let detail_callbacks = cache(&self.inner, &source, ArtworkVariant::Detail, detail.clone());
        answer(tile_callbacks, &tile);
        answer(detail_callbacks, &detail);
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
        // Intentionally block when the main loop already has one completed
        // result per worker waiting. This bounds transient GdkTextures outside
        // the LRUs; the send only errors once the receiver is gone at shutdown.
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
    #[cfg(test)]
    inner
        .delivered_results
        .set(inner.delivered_results.get() + 1);
    if inner.repository.source_generation(&result.source) != result.generation {
        return;
    }
    let callbacks = cache(
        inner,
        &result.source,
        result.variant,
        result.decoded.clone(),
    );
    answer(callbacks, &result.decoded);
}

/// Cache `decoded` as the current `variant` of `source`, and take the
/// requests waiting on it for [`answer`]. Run every `cache` of one update
/// before any `answer`, so no callback sees the update half applied.
fn cache(
    inner: &LoaderInner,
    source: &ArtworkSource,
    variant: ArtworkVariant,
    decoded: DecodedArtwork,
) -> Vec<ArtworkCallback> {
    inner
        .cache_for(variant)
        .borrow_mut()
        .insert(source.clone(), decoded);
    inner
        .pending
        .borrow_mut()
        .remove(&(source.clone(), variant))
        .unwrap_or_default()
}

/// Call each waiting request with `decoded`. No loader borrow is held,
/// so a callback may request artwork again.
fn answer(callbacks: Vec<ArtworkCallback>, decoded: &DecodedArtwork) {
    for callback in callbacks {
        callback(decoded.clone());
    }
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::collections::HashSet;
    use std::path::{Path, PathBuf};
    use std::rc::Rc;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    use gtk::glib;
    use sustain_app_runtime::{
        InitialTags, MetadataChange, MetadataError, MetadataResult, MetadataService, Rating,
    };

    use super::{ArtworkLoader, ArtworkSource, LruCache};

    const VALID_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x04, 0x00, 0x00, 0x00, 0xb5,
        0x1c, 0x0c, 0x02, 0x00, 0x00, 0x00, 0x0b, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x64,
        0xf8, 0x0f, 0x00, 0x01, 0x05, 0x01, 0x01, 0x27, 0x18, 0xe3, 0x66, 0x00, 0x00, 0x00, 0x00,
        0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    /// Artwork reads that wait until the test releases them, so a request
    /// can be held in flight while the source changes and reads can finish
    /// in an order the test picks. Each read returns the cover the file
    /// held when the read started.
    #[derive(Default)]
    struct GatedArtwork {
        state: Mutex<GateState>,
        changed: Condvar,
    }

    #[derive(Default)]
    struct GateState {
        cover: Option<Vec<u8>>,
        /// The reads, numbered from 0 in the order they started, that may
        /// finish.
        released: HashSet<usize>,
        started: usize,
        finished: usize,
    }

    impl GatedArtwork {
        fn with_state<T>(&self, f: impl FnOnce(&mut GateState) -> T) -> T {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poison| poison.into_inner());
            let value = f(&mut state);
            self.changed.notify_all();
            value
        }

        fn set_cover(&self, cover: Option<Vec<u8>>) {
            self.with_state(|state| state.cover = cover);
        }

        /// Let the `read`th read (counting from 0) finish.
        fn release(&self, read: usize) {
            self.with_state(|state| state.released.insert(read));
        }

        fn started(&self) -> usize {
            self.with_state(|state| state.started)
        }

        fn finished(&self) -> usize {
            self.with_state(|state| state.finished)
        }
    }

    impl MetadataService for GatedArtwork {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Err(MetadataError::ReadFailed)
        }

        fn write_metadata(&self, _path: &Path, _change: MetadataChange) -> MetadataResult<()> {
            Err(MetadataError::WriteFailed)
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Err(MetadataError::WriteFailed)
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            let Ok(mut state) = self.state.lock() else {
                return Err(MetadataError::ReadFailed);
            };
            let cover = state.cover.clone();
            let read = state.started;
            state.started += 1;
            self.changed.notify_all();
            let Ok(mut state) = self
                .changed
                .wait_while(state, |state| !state.released.contains(&read))
            else {
                return Err(MetadataError::ReadFailed);
            };
            state.finished += 1;
            Ok(cover)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Err(MetadataError::WriteFailed)
        }
    }

    /// A loader over [`GatedArtwork`] and a source with no file behind it,
    /// so no disk-cache row can answer for the gated read.
    fn gated_loader() -> (Rc<ArtworkLoader>, Arc<GatedArtwork>, ArtworkSource) {
        let service = Arc::new(GatedArtwork::default());
        let cache_dir = std::env::temp_dir().join(format!(
            "sustain-artwork-loader-test-{}",
            std::process::id()
        ));
        let loader = Rc::new(ArtworkLoader::new(service.clone(), cache_dir));
        let missing = PathBuf::from("/nonexistent/sustain-artwork-loader-test.flac");
        let source = ArtworkSource::embedded_track(missing.clone(), missing);
        (loader, service, source)
    }

    /// Run the main loop until `done` holds or a generous deadline passes.
    fn run_main_loop_until(done: impl Fn() -> bool) {
        let context = glib::MainContext::default();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !done() && Instant::now() < deadline {
            context.iteration(false);
            std::thread::sleep(Duration::from_millis(1));
        }
    }

    #[test]
    fn a_request_waiting_when_its_source_is_invalidated_gets_the_new_cover() {
        crate::test_support::with_gtk(|| {
            let (loader, service, source) = gated_loader();
            let answers = Rc::new(RefCell::new(Vec::new()));
            let answers_for_request = answers.clone();
            loader.request(
                source.clone(),
                Box::new(move |artwork| {
                    answers_for_request
                        .borrow_mut()
                        .push(artwork.tile_texture.is_some());
                }),
            );
            // The first read starts while the file has no cover.
            run_main_loop_until(|| service.started() >= 1);

            service.set_cover(Some(VALID_PNG.to_vec()));
            loader.invalidate(&source);
            run_main_loop_until(|| service.started() >= 2);
            // The fresh read finishes first; the stale one lands after it.
            service.release(1);
            run_main_loop_until(|| service.finished() >= 1 && !answers.borrow().is_empty());
            service.release(0);
            run_main_loop_until(|| loader.inner.delivered_results.get() >= 2);
            assert_eq!(
                loader.inner.delivered_results.get(),
                2,
                "both reads delivered"
            );

            assert_eq!(
                *answers.borrow(),
                [true],
                "answered once, with the new cover"
            );
            let cached = loader.cached(&source).expect("the new cover stays cached");
            assert!(
                cached.tile_texture.is_some(),
                "the stale read must not replace the new cover"
            );
        });
    }

    #[test]
    fn priming_answers_waiting_requests_and_outlives_loads_already_in_flight() {
        crate::test_support::with_gtk(|| {
            let (loader, service, source) = gated_loader();
            let answered = Rc::new(Cell::new(None));
            let answered_for_request = answered.clone();
            let loader_for_request = loader.clone();
            let source_for_request = source.clone();
            loader.request(
                source.clone(),
                Box::new(move |artwork| {
                    // A tile answer must already see the primed detail.
                    let detail = loader_for_request
                        .cached_detail(&source_for_request)
                        .is_some_and(|detail| detail.detail_texture.is_some());
                    answered_for_request.set(Some((artwork.tile_texture.is_some(), detail)));
                }),
            );
            run_main_loop_until(|| service.started() >= 1);

            loader.prime(source.clone(), VALID_PNG.to_vec());
            assert_eq!(
                answered.get(),
                Some((true, true)),
                "answered with the primed cover, in both sizes"
            );

            // The load that was in flight finds the file without artwork.
            // It must not replace the primed cover.
            service.release(0);
            run_main_loop_until(|| loader.inner.delivered_results.get() >= 1);
            assert_eq!(
                loader.inner.delivered_results.get(),
                1,
                "the in-flight read delivered"
            );
            let cached = loader
                .cached(&source)
                .expect("the primed cover stays cached");
            assert!(cached.tile_texture.is_some());
        });
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

    #[test]
    fn lru_sustained_eviction_stays_bounded_and_preserves_recency() {
        const CAPACITY: usize = 4_096;
        let mut cache = LruCache::new(CAPACITY);
        for key in 0..CAPACITY {
            cache.insert(key, key);
        }

        // Exercise the full tile-cache steady state. This used to scan all
        // 4,096 resident entries on every insertion, which put the repeated
        // eviction work on the GTK main thread while scrolling Albums.
        for key in CAPACITY..CAPACITY * 9 {
            cache.insert(key, key);
        }

        assert_eq!(cache.len(), CAPACITY);
        assert!(!cache.contains(&(CAPACITY * 8 - 1)));
        assert!(cache.contains(&(CAPACITY * 8)));
        assert!(cache.contains(&(CAPACITY * 9 - 1)));

        // A hit refreshes recency, so the touched oldest entry survives the
        // next insertion and the next-oldest entry is evicted instead.
        assert_eq!(cache.get(&(CAPACITY * 8)), Some(&(CAPACITY * 8)));
        cache.insert(CAPACITY * 9, CAPACITY * 9);
        assert!(cache.contains(&(CAPACITY * 8)));
        assert!(!cache.contains(&(CAPACITY * 8 + 1)));
    }
}
