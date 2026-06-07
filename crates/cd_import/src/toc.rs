// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Owned, thread-safe snapshots of an audio CD's table of contents.
//!
//! `discid::DiscId` is intentionally not `Send`/`Sync`, so the probing
//! worker builds one, reads everything off it, and discards it — returning
//! the [`TocSnapshot`] here for the rest of the application to carry across
//! threads.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Red Book sector rate: CD sectors ("frames") per second. Track lengths
/// from libdiscid are expressed in these sectors.
pub const CD_FRAMES_PER_SECOND: u32 = 75;

/// One audio track of a probed disc, normalized from libdiscid's signed
/// sector counts into Sustain's unsigned domain values.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TocTrack {
    /// 1-based physical track number reported by the disc TOC.
    pub number: u32,
    /// Track start offset, in CD sectors from the start of the disc.
    pub offset_frames: u32,
    /// Track length, in CD sectors.
    pub length_frames: u32,
}

impl TocTrack {
    /// Track duration derived from its sector count. Microsecond precision
    /// keeps the 1/75-second sector grid representable without rounding to
    /// whole seconds.
    pub fn duration(self) -> Duration {
        Duration::from_micros(
            u64::from(self.length_frames) * 1_000_000 / u64::from(CD_FRAMES_PER_SECOND),
        )
    }

    /// Track duration in whole milliseconds, the unit MusicBrainz uses.
    pub fn duration_ms(self) -> u64 {
        u64::from(self.length_frames) * 1_000 / u64::from(CD_FRAMES_PER_SECOND)
    }
}

/// A raw per-track TOC entry as read from libdiscid, before normalization.
/// Kept as a distinct type so the discid → domain mapping is a pure
/// function testable without a physical drive or the `optical` feature.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RawTocTrack {
    pub number: i32,
    pub offset: i32,
    pub sectors: i32,
}

/// An owned, thread-safe snapshot of an audio CD's table of contents.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TocSnapshot {
    /// The optical device this disc was read from (e.g. `/dev/sr0`).
    pub device_path: PathBuf,
    /// MusicBrainz Disc ID computed from the TOC.
    pub disc_id: String,
    /// libdiscid's TOC string — the complete TOC fingerprint. The Disc ID
    /// is itself derived from this, so the two together fully identify the
    /// disc.
    pub toc_fingerprint: String,
    /// Audio tracks, in physical order.
    pub tracks: Vec<TocTrack>,
}

impl TocSnapshot {
    /// Build a snapshot from libdiscid's signed sector values. Negative
    /// counts (which libdiscid never emits for a valid disc) clamp to zero
    /// rather than wrap, and tracks are sorted by physical number so the
    /// list is always in play order.
    pub fn from_raw(
        device_path: PathBuf,
        disc_id: String,
        toc_fingerprint: String,
        raw_tracks: &[RawTocTrack],
    ) -> Self {
        let mut tracks: Vec<TocTrack> = raw_tracks
            .iter()
            .map(|raw| TocTrack {
                number: raw.number.max(0) as u32,
                offset_frames: raw.offset.max(0) as u32,
                length_frames: raw.sectors.max(0) as u32,
            })
            .collect();
        tracks.sort_by_key(|track| track.number);
        Self {
            device_path,
            disc_id,
            toc_fingerprint,
            tracks,
        }
    }

    /// Number of audio tracks on the disc — the value passed to the
    /// MusicBrainz disc-id compatibility check.
    pub fn audio_track_count(&self) -> u32 {
        self.tracks.len() as u32
    }

    /// The stable session identity: the device plus the Disc ID (which is
    /// derived from the complete TOC). Two probes that yield equal
    /// identities are the same disc in the same drive; an inequality means
    /// the disc was swapped and any in-flight metadata is stale.
    pub fn identity(&self) -> DiscIdentity {
        DiscIdentity {
            device_path: self.device_path.clone(),
            disc_id: self.disc_id.clone(),
        }
    }

    /// Whether `other` was read from the same drive holding the same disc.
    pub fn is_same_disc(&self, other: &DiscIdentity) -> bool {
        &self.identity() == other
    }

    pub fn track(&self, number: u32) -> Option<&TocTrack> {
        self.tracks.iter().find(|track| track.number == number)
    }
}

/// The device-plus-Disc-ID identity used to detect a disc swap between the
/// probe that produced a snapshot and a later read at import time.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct DiscIdentity {
    pub device_path: PathBuf,
    pub disc_id: String,
}

impl DiscIdentity {
    pub fn new(device_path: impl Into<PathBuf>, disc_id: impl Into<String>) -> Self {
        Self {
            device_path: device_path.into(),
            disc_id: disc_id.into(),
        }
    }

    pub fn device_path(&self) -> &Path {
        &self.device_path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(number: i32, offset: i32, sectors: i32) -> RawTocTrack {
        RawTocTrack {
            number,
            offset,
            sectors,
        }
    }

    #[test]
    fn from_raw_maps_physical_numbering_and_durations() {
        let snapshot = TocSnapshot::from_raw(
            PathBuf::from("/dev/sr0"),
            "disc-id".to_owned(),
            "1 3 ...".to_owned(),
            // Deliberately out of order to prove the snapshot sorts by number.
            &[
                raw(3, 90150, 13500),
                raw(1, 150, 13350),
                raw(2, 13500, 76650),
            ],
        );

        assert_eq!(snapshot.audio_track_count(), 3);
        let numbers: Vec<u32> = snapshot.tracks.iter().map(|track| track.number).collect();
        assert_eq!(numbers, vec![1, 2, 3]);
        assert_eq!(snapshot.track(1).expect("track 1").offset_frames, 150);
        // 13350 sectors / 75 = 178 s exactly.
        assert_eq!(snapshot.track(1).expect("track 1").duration_ms(), 178_000);
        assert_eq!(
            snapshot.track(1).expect("track 1").duration(),
            Duration::from_secs(178)
        );
    }

    #[test]
    fn from_raw_clamps_negative_sector_values() {
        let snapshot = TocSnapshot::from_raw(
            PathBuf::from("/dev/sr0"),
            "disc-id".to_owned(),
            String::new(),
            &[raw(1, -1, -1)],
        );

        let track = snapshot.track(1).expect("track 1");
        assert_eq!(track.offset_frames, 0);
        assert_eq!(track.length_frames, 0);
        assert_eq!(track.duration_ms(), 0);
    }

    #[test]
    fn identity_detects_disc_swap() {
        let snapshot = TocSnapshot::from_raw(
            PathBuf::from("/dev/sr0"),
            "first-disc".to_owned(),
            String::new(),
            &[raw(1, 150, 13350)],
        );

        assert!(snapshot.is_same_disc(&DiscIdentity::new("/dev/sr0", "first-disc")));
        // Same drive, different disc.
        assert!(!snapshot.is_same_disc(&DiscIdentity::new("/dev/sr0", "second-disc")));
        // Same disc id is not enough if the drive differs.
        assert!(!snapshot.is_same_disc(&DiscIdentity::new("/dev/sr1", "first-disc")));
    }
}
