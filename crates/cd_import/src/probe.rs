// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Optical-disc discovery and TOC probing.
//!
//! Device enumeration is independent of `/proc/mounts` — an audio CD need
//! not be mounted — and is cheap enough to call on the main thread. The
//! actual TOC read uses libdiscid and must run on a worker thread; it
//! returns an owned [`TocSnapshot`] because `discid::DiscId` is not
//! `Send`/`Sync`. "No media", "unreadable media", and "non-audio media" are
//! all normal outcomes reported as `None`, never errors.

use std::fs;
use std::path::{Path, PathBuf};

use crate::toc::TocSnapshot;

/// Reads optical-disc tables of contents. The production implementation is
/// [`SystemOpticalProbe`]; tests inject a fake.
pub trait OpticalProbe: Send + Sync {
    /// Candidate optical device paths, discovered cheaply and without device
    /// I/O — safe to call on the GTK main thread. Enumerated independently
    /// of `/proc/mounts`.
    fn candidate_devices(&self) -> Vec<PathBuf>;

    /// Probe one device for an audio disc. `Some` is a readable audio disc;
    /// `None` covers no media, unreadable media, and non-audio media — all
    /// normal discovery outcomes. Must run off the GTK main thread.
    fn probe(&self, device: &Path) -> Option<TocSnapshot>;
}

/// The production optical probe. Zero-sized and trivially `Send`/`Sync`;
/// every libdiscid read happens inside [`OpticalProbe::probe`] on the
/// calling worker thread and is discarded before the snapshot is returned.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemOpticalProbe;

impl SystemOpticalProbe {
    pub fn new() -> Self {
        Self
    }
}

impl OpticalProbe for SystemOpticalProbe {
    fn candidate_devices(&self) -> Vec<PathBuf> {
        enumerate_optical_devices()
    }

    fn probe(&self, device: &Path) -> Option<TocSnapshot> {
        self.probe_impl(device)
    }
}

#[cfg(feature = "optical")]
impl SystemOpticalProbe {
    fn probe_impl(&self, device: &Path) -> Option<TocSnapshot> {
        use crate::toc::RawTocTrack;

        let device_str = device.to_str()?;
        // `DiscId::read` errors for no media, unreadable media, and
        // non-audio media alike — all of which we treat as "nothing to
        // import", not failures.
        let disc = discid::DiscId::read(Some(device_str)).ok()?;
        let raw: Vec<RawTocTrack> = disc
            .tracks()
            .map(|track| RawTocTrack {
                number: track.number,
                offset: track.offset,
                sectors: track.sectors,
            })
            .collect();
        if raw.is_empty() {
            return None;
        }
        Some(TocSnapshot::from_raw(
            device.to_path_buf(),
            disc.id(),
            disc.toc_string(),
            &raw,
        ))
    }
}

#[cfg(not(feature = "optical"))]
impl SystemOpticalProbe {
    fn probe_impl(&self, _device: &Path) -> Option<TocSnapshot> {
        // Built without libdiscid: discovery still enumerates drives, but
        // nothing can be read, so no CD row is ever surfaced.
        None
    }
}

/// Enumerate Linux optical-drive device paths. Prefers the kernel's
/// canonical list at `/proc/sys/dev/cdrom/info`; falls back to scanning
/// `/dev` for `sr*` block devices when that file is absent or empty.
fn enumerate_optical_devices() -> Vec<PathBuf> {
    if let Ok(contents) = fs::read_to_string("/proc/sys/dev/cdrom/info") {
        let devices = parse_cdrom_info(&contents);
        if !devices.is_empty() {
            return devices;
        }
    }
    scan_dev_sr_devices()
}

/// Parse the `drive name:` line of `/proc/sys/dev/cdrom/info` into device
/// paths. The kernel lists the bare drive names (e.g. `sr0`) separated by
/// whitespace; each becomes `/dev/<name>`.
fn parse_cdrom_info(contents: &str) -> Vec<PathBuf> {
    contents
        .lines()
        .find_map(|line| line.strip_prefix("drive name:"))
        .map(|names| {
            names
                .split_whitespace()
                .map(|name| PathBuf::from(format!("/dev/{name}")))
                .collect()
        })
        .unwrap_or_default()
}

fn scan_dev_sr_devices() -> Vec<PathBuf> {
    let mut devices: Vec<PathBuf> = match fs::read_dir("/dev") {
        Ok(entries) => entries
            .flatten()
            .filter_map(|entry| {
                let name = entry.file_name();
                let name = name.to_str()?;
                let suffix = name.strip_prefix("sr")?;
                (!suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| PathBuf::from(format!("/dev/{name}")))
            })
            .collect(),
        Err(_) => Vec::new(),
    };
    devices.sort();
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_drive_names_from_cdrom_info() {
        let contents = "\
CD-ROM information, Id: cdrom.c 3.20 2003/12/17

drive name:\t\tsr1\tsr0
drive speed:\t\t40\t24
drive # of slots:\t1\t1
";
        assert_eq!(
            parse_cdrom_info(contents),
            vec![PathBuf::from("/dev/sr1"), PathBuf::from("/dev/sr0")]
        );
    }

    #[test]
    fn cdrom_info_without_drives_is_empty() {
        // The kernel prints an empty `drive name:` line when no optical
        // drive is present.
        assert!(parse_cdrom_info("drive name:\n").is_empty());
        assert!(parse_cdrom_info("no relevant lines here\n").is_empty());
    }
}
