// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Off-thread driver for `RemoteMetadataService::fetch_artwork`.
//!
//! Reaching out to MusicBrainz and Cover Art Archive is a multi-step
//! network operation: a Lucene search, optionally an AcoustID
//! roundtrip, then one or more redirect-following downloads from
//! Cover Art Archive. End-to-end latency on a healthy network is
//! ~1-2 seconds; on a sluggish one it can easily climb past 10. None
//! of that work belongs on the GTK main thread.
//!
//! This module mirrors the shape of [`crate::metadata_writer`]:
//!
//! * Owns a single worker thread that drains a request channel.
//! * Per-job completion fires a caller-supplied callback on the worker
//!   thread; the runtime forwards outcomes through an `async_channel`
//!   sink consumed by the UI's main loop.
//! * Shutdown drops the sender, drains the queue, and joins the
//!   thread.
//!
//! Network errors collapse into [`ArtworkFetchOutcome::Failed`] and
//! "no cover found" into [`ArtworkFetchOutcome::NoMatch`]. The UI
//! cares about both states (one is retryable, the other is "no
//! source has this cover") but the bytes themselves are the only
//! happy-path payload.

use std::{
    sync::{
        Arc,
        mpsc::{self, Sender},
    },
    thread::{self, JoinHandle},
};

use sustain_artwork::validate_encoded_artwork;
use sustain_domain::TrackId;
use sustain_metadata_remote::{RemoteError, RemoteMetadataService, TrackQuery};

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ArtworkFetchOutcome {
    /// A cover was found and downloaded. The bytes are the raw payload
    /// returned by Cover Art Archive — the caller is responsible for
    /// writing them through the standard tag-writing path.
    Fetched(Vec<u8>),
    /// Identification succeeded but no front cover was on file, or
    /// identification itself returned no confident match. From the
    /// UI's perspective these collapse into the same state ("we
    /// looked, nothing came back") — distinguishing them would only
    /// matter for a richer diagnostic.
    NoMatch,
    /// The provider returned bytes that violate Sustain's artwork resource
    /// policy. Retrying the same response would not help.
    Rejected,
    /// A network or remote-service failure. Retryable: the user may
    /// have lost connectivity, or the service may have been briefly
    /// unavailable.
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ArtworkFetchResult {
    pub track_id: TrackId,
    pub outcome: ArtworkFetchOutcome,
}

pub(crate) struct ArtworkFetchRequest {
    pub(crate) query: TrackQuery,
    pub(crate) completion: ArtworkFetchCompletionCallback,
}

pub(crate) type ArtworkFetchCompletionCallback =
    Box<dyn FnOnce(ArtworkFetchOutcome) + Send + 'static>;

/// Owns a single worker thread that serialises remote artwork
/// requests against the underlying [`RemoteMetadataService`].
pub(crate) struct ArtworkFetcher {
    sender: Sender<ArtworkFetchRequest>,
    handle: Option<JoinHandle<()>>,
}

impl ArtworkFetcher {
    pub(crate) fn start(service: Arc<dyn RemoteMetadataService>) -> Self {
        let (sender, receiver) = mpsc::channel::<ArtworkFetchRequest>();
        let handle = thread::Builder::new()
            .name("sustain-artwork-fetcher".to_owned())
            .spawn(move || worker_loop(receiver, service))
            .expect("spawn artwork fetcher thread");
        Self {
            sender,
            handle: Some(handle),
        }
    }

    pub(crate) fn submit(&self, request: ArtworkFetchRequest) {
        // Send only fails on a closed channel, which means the worker
        // has exited and a shutdown is in progress. Dropping the
        // request silently is correct in that case — there is no UI
        // observer left to inform.
        let _ = self.sender.send(request);
    }

    /// Drop the sender so the worker stops receiving new requests,
    /// then wait for the queue to drain. Safe to call at app
    /// shutdown.
    pub(crate) fn shutdown(mut self) {
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

impl Drop for ArtworkFetcher {
    fn drop(&mut self) {
        let (placeholder, _) = mpsc::channel();
        let _ = std::mem::replace(&mut self.sender, placeholder);
    }
}

fn worker_loop(
    receiver: mpsc::Receiver<ArtworkFetchRequest>,
    service: Arc<dyn RemoteMetadataService>,
) {
    while let Ok(request) = receiver.recv() {
        let outcome = match service.fetch_artwork(&request.query) {
            Ok(Some(artwork)) => match validate_encoded_artwork(&artwork.bytes) {
                Ok(_) => ArtworkFetchOutcome::Fetched(artwork.bytes),
                Err(error) => {
                    eprintln!("Sustain: artwork fetch returned rejected bytes: {error}");
                    ArtworkFetchOutcome::Rejected
                }
            },
            Ok(None) => ArtworkFetchOutcome::NoMatch,
            Err(RemoteError::ArtworkRejected(error)) => {
                eprintln!("Sustain: artwork fetch returned rejected bytes: {error}");
                ArtworkFetchOutcome::Rejected
            }
            Err(RemoteError::PayloadTooLarge) => {
                eprintln!("Sustain: artwork fetch returned an oversized payload");
                ArtworkFetchOutcome::Rejected
            }
            Err(error) => {
                // Log so a failed click is diagnosable from a terminal
                // run. The user-facing message stays generic; this
                // line is the only place the underlying provider error
                // surfaces.
                eprintln!("Sustain: artwork fetch failed: {error}");
                ArtworkFetchOutcome::Failed
            }
        };
        (request.completion)(outcome);
    }
}

/// Build a [`TrackQuery`] from the metadata Sustain already knows
/// about a track. The fingerprint slot is always left empty here —
/// Chromaprint computation belongs to a later pass over the audio
/// file and is gated by a separate user opt-in.
pub(crate) fn query_from_metadata(metadata: &sustain_domain::TrackMetadata) -> TrackQuery {
    TrackQuery {
        artist: metadata
            .artist
            .clone()
            .filter(|value| !value.trim().is_empty()),
        title: metadata
            .title
            .clone()
            .filter(|value| !value.trim().is_empty()),
        album: metadata
            .album
            .clone()
            .filter(|value| !value.trim().is_empty()),
        duration_ms: metadata
            .duration
            .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64),
        fingerprint: None,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use sustain_domain::TrackMetadata;

    use super::query_from_metadata;

    #[test]
    fn blank_fields_are_filtered_out() {
        let metadata = TrackMetadata {
            artist: Some("  ".to_owned()),
            title: Some("".to_owned()),
            album: Some("Real Album".to_owned()),
            duration: Some(Duration::from_secs(200)),
            ..TrackMetadata::default()
        };
        let query = query_from_metadata(&metadata);
        assert_eq!(query.artist, None);
        assert_eq!(query.title, None);
        assert_eq!(query.album.as_deref(), Some("Real Album"));
        assert_eq!(query.duration_ms, Some(200_000));
    }

    #[test]
    fn duration_in_milliseconds_is_propagated() {
        let metadata = TrackMetadata {
            duration: Some(Duration::from_millis(187_500)),
            ..TrackMetadata::default()
        };
        let query = query_from_metadata(&metadata);
        assert_eq!(query.duration_ms, Some(187_500));
    }
}
