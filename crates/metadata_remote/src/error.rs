// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Error surface shared by every networked metadata client.
//!
//! Per-provider failure modes collapse into a small enum so the
//! caller doesn't have to learn three different vocabularies. The
//! variants are coarse on purpose — the application has the same
//! recourse for "network failed" and "HTTP 502" (retry or give up),
//! and the user-facing message at the UI layer is the same string.
//! Finer-grained diagnostics belong in log lines, not in the type
//! the runtime branches on.

use std::{fmt, time::Duration};

use sustain_artwork::ArtworkPolicyError;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteError {
    /// Network reachability or transport failure (DNS, TCP, TLS,
    /// timeout). The user might fix it by checking connectivity; the
    /// app's job is to back off without spinning on the failure.
    Network,
    /// Server explicitly asked us to stop sending requests for a
    /// while (HTTP 429 or 503). `cool_down` is the time the HTTP
    /// client has *already* recorded against the offending host; the
    /// caller can treat this as a strong signal to stop the current
    /// batch instead of just the current track. The rate limiter
    /// holds back the next request to that host automatically — the
    /// caller does not need to re-implement the wait.
    RateLimited { cool_down: Duration },
    /// Server responded but with a status code we cannot use. Held
    /// for diagnostics; the UI does not branch on the specific code.
    BadStatus(u16),
    /// Server responded with a payload that did not match the
    /// expected schema (truncated JSON, unexpected shape, missing
    /// fields we cannot recover from).
    InvalidResponse,
    /// A binary provider response exceeded the caller's acquisition cap
    /// before it could be retained in memory.
    PayloadTooLarge,
    /// Cover-art bytes fit the encoded cap but violate the shared artwork
    /// policy (unsupported/corrupt encoding or excessive dimensions).
    ArtworkRejected(ArtworkPolicyError),
    /// The remote provider is not configured (e.g. AcoustID requires
    /// an application key that was not built into the binary). The
    /// caller is expected to skip the feature gracefully.
    NotConfigured,
}

impl fmt::Display for RemoteError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Network => f.write_str("network unavailable"),
            Self::RateLimited { cool_down } => write!(
                f,
                "remote service rate-limited us (cool-down {} s)",
                cool_down.as_secs()
            ),
            Self::BadStatus(code) => write!(f, "remote service returned HTTP {code}"),
            Self::InvalidResponse => f.write_str("remote service returned an unexpected payload"),
            Self::PayloadTooLarge => f.write_str("remote service returned an oversized payload"),
            Self::ArtworkRejected(error) => {
                write!(f, "remote service returned rejected artwork: {error}")
            }
            Self::NotConfigured => f.write_str("remote service not configured"),
        }
    }
}

impl std::error::Error for RemoteError {}

pub type RemoteResult<T> = Result<T, RemoteError>;
