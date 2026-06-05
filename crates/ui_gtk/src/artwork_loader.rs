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
//!   far heavier detail texture is kept to a small window. This prevents
//!   unbounded texture growth without reducing the tile cache to the realized
//!   viewport and making long Albums-grid scrolls churn. See
//!   `MAX_CACHED_TILE_ARTWORKS` and `MAX_CACHED_DETAIL_ARTWORKS`.
//! * A bounded result channel applies backpressure to workers if GTK cannot
//!   keep up, so completed textures cannot accumulate outside those caches.
//!   A GTK main-loop drainer handles results under a strict wall-clock budget
//!   so a burst cannot monopolise the main thread, places each result in the
//!   cache for its size, and fires every callback waiting on that
//!   (source, size).
//! * Staleness — discarding callbacks whose target widget is no longer
//!   relevant (Albums grid rebuilt, now-playing track changed) — is the
//!   caller's concern. Each view tracks its own per-view generation
//!   counter and checks it inside the callback closure before touching
//!   widgets. Keeping that policy with the caller lets independent
//!   views share one loader without one view's rebuild invalidating
//!   another view's in-flight requests.
//! * The repository checks a small SQLite cache before touching the audio file.
//!   Cache rows are keyed by source plus the representative file fingerprint,
//!   and merge whichever already-scaled tile/detail PNG payloads have been
//!   requested plus the derived palette. Today the only source is embedded
//!   artwork from an audio file; the explicit source boundary is where the
//!   missing-artwork downloader should plug in.

mod decode;
mod disk_cache;
mod loader;

pub(crate) use decode::DecodedArtwork;
pub(crate) use disk_cache::ArtworkSource;
pub(crate) use loader::ArtworkLoader;
