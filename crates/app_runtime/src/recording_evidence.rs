// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Recording evidence for duplicate grouping, read from the waveform cache
//! (#302).
//!
//! Comparing two waveforms costs a few milliseconds, and the Duplicates
//! view rescans after every change, so verdicts are kept across scans. A
//! verdict is keyed by both tracks' waveform versions: re-analysing either
//! track's audio gives it a new version, and the old verdict simply stops
//! matching. Tracks without a stored waveform (audio analysis off, a track
//! too long to keep one, an unavailable cache) yield
//! [`RecordingIdentity::NotEstablished`], which leaves their suggestions
//! exactly as the tags alone make them.

use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use sustain_domain::{
    RecordingEvidence, RecordingIdentity, RecordingProfile, TrackId, compare_recordings,
};
use sustain_library_store::{LibraryStore, WaveformVersion};

/// Prepared waveforms kept while one scan runs. Grouping asks about the
/// pairs of one title together, so a handful covers every reuse; the bound
/// keeps a scan over thousands of analysed tracks from holding them all.
const PROFILES_KEPT: usize = 8;

/// Verdicts computed by earlier scans, valid while both tracks' waveforms
/// keep the versions they were computed from.
#[derive(Debug, Default)]
pub(crate) struct RecordingIdentityCache {
    verdicts: HashMap<(TrackId, TrackId), CachedIdentity>,
}

#[derive(Clone, Copy, Debug)]
struct CachedIdentity {
    versions: (WaveformVersion, WaveformVersion),
    identity: RecordingIdentity,
}

impl RecordingIdentityCache {
    fn get(
        &self,
        pair: (TrackId, TrackId),
        versions: (WaveformVersion, WaveformVersion),
    ) -> Option<RecordingIdentity> {
        self.verdicts
            .get(&pair)
            .filter(|cached| cached.versions == versions)
            .map(|cached| cached.identity)
    }

    fn insert(
        &mut self,
        pair: (TrackId, TrackId),
        versions: (WaveformVersion, WaveformVersion),
        identity: RecordingIdentity,
    ) {
        self.verdicts
            .insert(pair, CachedIdentity { versions, identity });
    }

    /// Forget verdicts that can no longer match: a track that left the
    /// library, lost its waveform or was analysed again.
    fn retain_current(&mut self, versions: &HashMap<TrackId, WaveformVersion>) {
        self.verdicts.retain(|(left, right), cached| {
            versions.get(left) == Some(&cached.versions.0)
                && versions.get(right) == Some(&cached.versions.1)
        });
    }
}

/// [`RecordingEvidence`] backed by the library store's waveform cache.
pub(crate) struct WaveformRecordingEvidence<'scan> {
    store: &'scan dyn LibraryStore,
    versions: HashMap<TrackId, WaveformVersion>,
    cache: Arc<Mutex<RecordingIdentityCache>>,
    profiles: VecDeque<(TrackId, Option<Arc<RecordingProfile>>)>,
}

impl<'scan> WaveformRecordingEvidence<'scan> {
    /// Evidence over the waveforms `store` holds now. Verdicts the cache
    /// keeps for waveforms that have since changed are dropped.
    pub(crate) fn new(
        store: &'scan dyn LibraryStore,
        cache: Arc<Mutex<RecordingIdentityCache>>,
    ) -> sustain_library_store::StoreResult<Self> {
        let versions = store.waveform_versions()?;
        lock(&cache).retain_current(&versions);
        Ok(Self {
            store,
            versions,
            cache,
            profiles: VecDeque::with_capacity(PROFILES_KEPT),
        })
    }

    /// The comparison-ready waveform of `track_id`; `None` when it has none
    /// that can be judged. A store error counts as no waveform: the pair is
    /// then simply not established, and nothing is cached for it.
    fn profile(&mut self, track_id: TrackId) -> Result<Option<Arc<RecordingProfile>>, ()> {
        if let Some((_, profile)) = self.profiles.iter().find(|(id, _)| *id == track_id) {
            return Ok(profile.clone());
        }
        let waveform = self.store.load_waveform(track_id).map_err(|_| ())?;
        let profile = waveform
            .and_then(|waveform| RecordingProfile::from_detail(&waveform.detail))
            .map(Arc::new);
        if self.profiles.len() == PROFILES_KEPT {
            self.profiles.pop_front();
        }
        self.profiles.push_back((track_id, profile.clone()));
        Ok(profile)
    }
}

impl RecordingEvidence for WaveformRecordingEvidence<'_> {
    fn identity(&mut self, left: TrackId, right: TrackId) -> RecordingIdentity {
        let pair = (left.min(right), left.max(right));
        let (Some(first), Some(second)) = (self.versions.get(&pair.0), self.versions.get(&pair.1))
        else {
            return RecordingIdentity::NotEstablished;
        };
        let versions = (*first, *second);
        let cached = lock(&self.cache).get(pair, versions);
        if let Some(identity) = cached {
            return identity;
        }
        let (Ok(first), Ok(second)) = (self.profile(pair.0), self.profile(pair.1)) else {
            return RecordingIdentity::NotEstablished;
        };
        let identity = match (first, second) {
            (Some(first), Some(second)) => compare_recordings(&first, &second),
            _ => RecordingIdentity::NotEstablished,
        };
        // A waveform re-analysed between reading the versions and loading
        // it is filed under its old version, which never matches again.
        lock(&self.cache).insert(pair, versions, identity);
        identity
    }
}

/// The cache holds plain verdicts, so one left by a panicking scan is still
/// sound to read.
fn lock(
    cache: &Mutex<RecordingIdentityCache>,
) -> std::sync::MutexGuard<'_, RecordingIdentityCache> {
    cache
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
#[path = "recording_evidence_tests.rs"]
mod tests;
