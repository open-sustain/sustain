// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{sync::Arc, thread};

use sustain_library_store::LibraryStore;

use crate::{
    ApplicationRuntimeError, ApplicationRuntimeResult, Playlist, PlaylistFolder,
    SMART_SHUFFLE_INDEX_SCHEMA_VERSION, SearchIndex, SmartPlaylist, SmartShuffleIndex, Track,
};

/// Track-sized runtime state loaded after the GTK shell reaches first idle.
///
/// Keeping the index beside the tracks makes snapshot adoption O(1) on the
/// main thread. The worker also restores Smart Shuffle's persisted blob:
/// decoding that blob scales with the indexed library and therefore belongs
/// outside the cold-start path too.
pub struct LibraryHydrationSnapshot {
    pub(crate) tracks: Vec<Track>,
    pub(crate) search_index: SearchIndex,
    pub(crate) playlists: Vec<Playlist>,
    pub(crate) playlist_folders: Vec<PlaylistFolder>,
    pub(crate) smart_playlists: Vec<SmartPlaylist>,
    pub(crate) smart_shuffle_index: Option<SmartShuffleIndex>,
}

/// One-shot worker owned by [`crate::ApplicationRuntime`].
///
/// The result channel exists before the thread starts so the GTK shell can
/// install its main-loop consumer while building the window, then start the
/// actual SQLite load from the post-first-idle hook.
pub(crate) struct LibraryHydrationWorker {
    result_tx: async_channel::Sender<ApplicationRuntimeResult<LibraryHydrationSnapshot>>,
    result_rx: async_channel::Receiver<ApplicationRuntimeResult<LibraryHydrationSnapshot>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl LibraryHydrationWorker {
    pub(crate) fn new() -> Self {
        let (result_tx, result_rx) = async_channel::bounded(1);
        Self {
            result_tx,
            result_rx,
            handle: None,
        }
    }

    pub(crate) fn result_receiver(
        &self,
    ) -> async_channel::Receiver<ApplicationRuntimeResult<LibraryHydrationSnapshot>> {
        self.result_rx.clone()
    }

    pub(crate) fn start(&mut self, store: Arc<dyn LibraryStore>) {
        debug_assert!(self.handle.is_none(), "library hydration is one-shot");
        let result_tx = self.result_tx.clone();
        self.handle = Some(thread::spawn(move || {
            let _ = result_tx.send_blocking(load_snapshot(store.as_ref()));
        }));
    }

    pub(crate) fn join_finished(&mut self) {
        if let Some(handle) = self.handle.take()
            && let Err(error) = handle.join()
        {
            eprintln!("sustain: library hydration worker panicked: {error:?}");
        }
    }

    pub(crate) fn shutdown(&mut self) {
        self.join_finished();
    }
}

pub(crate) fn load_snapshot(
    store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<LibraryHydrationSnapshot> {
    let tracks = store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let mut search_index = SearchIndex::new();
    search_index.rebuild(&tracks);
    let playlists = store
        .playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let playlist_folders = store
        .playlist_folders()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let smart_playlists = store
        .smart_playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let smart_shuffle_index = load_smart_shuffle_index(store)?;
    Ok(LibraryHydrationSnapshot {
        tracks,
        search_index,
        playlists,
        playlist_folders,
        smart_playlists,
        smart_shuffle_index,
    })
}

pub(crate) fn load_smart_shuffle_index(
    store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<Option<SmartShuffleIndex>> {
    let Some(stored) = store
        .load_smart_shuffle_index()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
    else {
        return Ok(None);
    };
    if stored.schema_version != SMART_SHUFFLE_INDEX_SCHEMA_VERSION {
        // This is derived data. Failure to clear it is harmless: every load
        // keeps rejecting the stale schema until a successful rebuild replaces
        // the blob.
        let _ = store.clear_smart_shuffle_index();
        return Ok(None);
    }
    match SmartShuffleIndex::from_blob(&stored.index_blob) {
        Ok(index) => Ok(Some(index)),
        Err(_) => {
            // Same derived-data policy as the stale-schema path above.
            let _ = store.clear_smart_shuffle_index();
            Ok(None)
        }
    }
}
