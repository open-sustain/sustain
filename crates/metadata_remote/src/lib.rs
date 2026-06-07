// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Networked metadata clients for Sustain.
//!
//! This crate is the home for everything that reaches the public
//! internet on Sustain's behalf:
//!
//! * **MusicBrainz** — the canonical music metadata database
//!   ([`musicbrainz::MusicBrainzClient`]). Text-based recording
//!   search and lookup-by-MBID; the source of identity, titles, and
//!   release-level structure.
//! * **Cover Art Archive** — the artwork sibling of MusicBrainz
//!   ([`cover_art_archive::CoverArtArchiveClient`]). Fetches front
//!   cover bytes for a release or release-group MBID.
//! * **AcoustID** — fingerprint-based identification fallback
//!   ([`acoustid::AcoustIdClient`]). Used when local tags are too
//!   sparse for MusicBrainz to match by text. The actual Chromaprint
//!   fingerprint computation lives outside this crate; the client
//!   here accepts a precomputed [`acoustid::AudioFingerprint`].
//! * **LRClib** — open lyrics database ([`lrclib::LrcLibClient`]).
//!   Looks up plain and time-coded lyrics by exact artist/title/album
//!   match; the only provider in this crate that does not depend on
//!   MusicBrainz at all.
//!
//! Direct callers should depend on [`RemoteMetadataService`] — the
//! single public surface that composes the three clients with the
//! selection and fallback rules documented on
//! [`ComposedRemoteMetadataService`]. The individual modules are
//! still public so consumers with unusual needs (testing, future
//! diagnostic UIs) can reach for them, but the trait is the load-
//! bearing contract.
//!
//! The crate is deliberately offline-aware: every networked operation
//! is gated by a configurable HTTP client ([`client::HttpClient`]) and
//! the [`error::RemoteError::NotConfigured`] variant lets builds
//! without an AcoustID API key fail closed cleanly rather than panic
//! at runtime.

#![forbid(unsafe_code)]

pub mod acoustid;
pub mod client;
pub mod cover_art_archive;
pub mod error;
mod http;
pub mod lrclib;
mod mbid;
pub mod musicbrainz;
pub mod service;

pub use acoustid::{AcoustIdClient, AcoustIdMatch, AudioFingerprint};
pub use client::{HttpClient, HttpClientConfig};
pub use cover_art_archive::CoverArtArchiveClient;
pub use error::{RemoteError, RemoteResult};
pub use lrclib::LrcLibClient;
pub use musicbrainz::{
    DiscRelease, DiscTrack, GenreVote, MusicBrainzClient, RecordingMatch, RecordingRelease,
    RecordingSearchTerms,
};
pub use service::{
    ComposedRemoteMetadataService, FetchedArtwork, FetchedLyrics, GenreCandidate,
    RemoteMetadataService, TrackMatch, TrackMatchRelease, TrackMatchSource, TrackQuery,
};

/// Read the compile-time AcoustID API key from the build environment.
///
/// AcoustID requires an application-level key. Sustain follows the
/// MusicBrainz Picard precedent: the key is an app-level secret
/// embedded at build time, not a value the user types into a settings
/// dialog. Production builds set `SUSTAIN_ACOUSTID_API_KEY` at compile
/// time; developer builds without the variable simply get `None` and
/// the AcoustID path becomes a graceful no-op.
///
/// Exposed here (rather than read inside the composed service) so the
/// app entry point can log a clear "AcoustID disabled in this build"
/// message at startup without poking around inside this crate's
/// internals.
pub const fn acoustid_api_key() -> Option<&'static str> {
    option_env!("SUSTAIN_ACOUSTID_API_KEY")
}
