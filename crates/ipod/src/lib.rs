// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

//! Classic iPod on-device format writer (`sustain-ipod`).
//!
//! The music catalogue a classic iPod's stock firmware reads — the
//! `iPod_Control/iTunes/iTunesDB` file — and the hash58 signature the
//! Classic and the nano 3G/4G require on it, plus the model table that
//! tells which iPod needs what. Pure format code like `sustain-pioneer`:
//! flat inputs in, bytes out, no I/O, no library model, no transport.
//!
//! The byte layout follows libgpod's writer, the most widely deployed
//! third-party producer of these files; `PROVENANCE.md` records the
//! sources and what each module owes them. See `docs/ipod-sync-design.md`
//! for how the sync engine drives this crate.

pub mod hash58;
pub mod itunesdb;
pub mod model;
pub mod profile;
mod sort;

pub use hash58::FirewireGuid;
pub use itunesdb::{WriteError, track_count, write};
pub use model::{FileType, IpodLibrary, IpodPlaylist, IpodTrack};
pub use profile::{Checksum, Family, Profile, SysInfo};

/// Where the catalogue lives, relative to the iPod's root.
pub const ITUNESDB_RELATIVE_PATH: &str = "iPod_Control/iTunes/iTunesDB";

/// Files the firmware writes beside the catalogue, recording what happened
/// on the device since the last sync — play counts, ratings, the
/// On-The-Go playlist. Their rows refer to tracks by position in the
/// catalogue they were recorded against, so they must be removed before a
/// new catalogue is published. Numbered On-The-Go siblings
/// (`OTGPlaylistInfo_1`, …) need no removal: the firmware drops them
/// itself when it finds the catalogue changed, which is what libgpod
/// relies on too.
pub const DEVICE_EVENT_RELATIVE_PATHS: [&str; 2] = [
    "iPod_Control/iTunes/Play Counts",
    "iPod_Control/iTunes/OTGPlaylistInfo",
];
