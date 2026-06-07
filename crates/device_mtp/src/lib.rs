// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

//! MTP device sync for Android phones (`sustain-device-mtp`).
//!
//! This crate is the Android half of the device-sync transport seam
//! (issue #181). The engine, differ, manifest, and per-layout writers in
//! `sustain-device-sync` are transport-agnostic; this crate plugs an
//! [`MtpTransport`] in behind them, talking to the desktop's `gvfsd-mtp`
//! GVfs backend through gio's `GFile` API. A connected phone is therefore
//! reached exactly the way the file manager reaches it — no unsafe FFI and
//! no contest with the automounter for exclusive USB access.
//!
//! Discovery is split across two threads. [`candidates`] enumerates
//! connected MTP volumes through gio's `GVolumeMonitor`; it must run on the
//! GTK main thread (where the GLib main loop drives the volume-monitor
//! proxies) but touches only the monitor, never the device, so it is cheap.
//! [`resolve`] then probes a candidate's storage and identity marker over
//! MTP — that is the slow, device-touching half, and it runs on the sync
//! worker thread, exactly like the transport's own reads and writes.
//! [`discover`] composes both on the calling thread for the hardware tests.

mod transport;

pub use transport::MtpTransport;

use std::collections::HashSet;

use gio::FileType;
use gio::prelude::*;
use sustain_device_sync::{
    ConnectedDevice, DeviceTarget, MtpTarget, generate_device_id, read_marker_via_transport,
};
use sustain_domain::{DeviceKind, SyncDevice};

/// The storage that holds the standard Android media tree on modern
/// devices; preferred over any other storage during discovery.
const PRIMARY_STORAGE: &str = "Internal shared storage";

/// A connected MTP volume seen by the volume monitor, before any
/// device-touching probe. Carries only `Send` data — its volume URI and the
/// monitor's human-friendly name — so the slow storage and marker probe in
/// [`resolve`] can run on a worker thread off this cheap main-thread scan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MtpCandidate {
    /// The `mtp://…` activation-root URI identifying the volume.
    pub volume_uri: String,
    /// The volume monitor's display name, e.g. `Pixel 9 Pro`.
    pub label: String,
}

/// Enumerate connected MTP volumes from gio's volume monitor.
///
/// Cheap and main-thread-only: it reads the already-resolved volume list
/// (the GLib main loop drives the gvfs proxies) and never touches a device.
/// Pair each result with [`resolve`] on a worker thread to obtain a
/// [`ConnectedDevice`].
pub fn candidates() -> Vec<MtpCandidate> {
    let monitor = gio::VolumeMonitor::get();
    let mut seen = HashSet::new();
    let mut candidates = Vec::new();
    // Iterate volumes (one per physical device) rather than mounts: a phone
    // surfaces as a shadow mount plus a daemon mount, but as a single
    // volume that also carries the human-friendly name.
    for volume in monitor.volumes() {
        let Some(root) = volume.activation_root() else {
            continue;
        };
        let volume_uri = root.uri().to_string();
        if !volume_uri.starts_with("mtp://") {
            continue;
        }
        if !seen.insert(volume_uri.clone()) {
            continue;
        }
        candidates.push(MtpCandidate {
            volume_uri,
            label: volume.name().to_string(),
        });
    }
    candidates
}

/// Resolve a [`MtpCandidate`] to a [`ConnectedDevice`], matching it against
/// `known` config.
///
/// Probes the candidate's storage and identity marker over MTP, so it does
/// device I/O and must run on a worker thread, never the GTK main loop.
/// Returns `None` when the phone is unreachable (e.g. unmounted) — it is
/// then skipped until it mounts.
///
/// Mirrors the recognition order of block-device discovery: identity
/// marker, then a volume-id fallback (here the device's MTP serial), then
/// a freshly generated id for an unseen phone.
pub fn resolve(candidate: &MtpCandidate, known: &[SyncDevice]) -> Option<ConnectedDevice> {
    let root = gio::File::for_uri(&candidate.volume_uri);
    // Reading the storage list needs the volume mounted; an unmounted
    // (or unreachable) phone fails here and is skipped until it mounts.
    let storage = pick_storage(&root)?;
    let target = MtpTarget {
        volume_uri: candidate.volume_uri.clone(),
        storage,
    };
    let volume_id = mtp_serial(&candidate.volume_uri);
    let device_transport = MtpTransport::open(&target);
    let marker = read_marker_via_transport(&device_transport);

    let (id, is_known, has_marker) = if let Some(id) = marker {
        let known_match = known.iter().any(|d| d.id == id);
        (id, known_match, true)
    } else if let Some(matched) = volume_id.as_ref().and_then(|vid| {
        known
            .iter()
            .find(|d| d.volume_id.as_deref() == Some(vid.as_str()))
    }) {
        // Marker was deleted; re-recognise by the phone's serial.
        (matched.id.clone(), true, false)
    } else {
        generate_device_id().map(|id| (id, false, false))?
    };

    let stored_label = known
        .iter()
        .find(|d| d.id == id)
        .map(|d| d.label.clone())
        .filter(|l| !l.is_empty());

    Some(ConnectedDevice {
        id,
        kind: DeviceKind::Android,
        target: DeviceTarget::Mtp(target),
        volume_id,
        label: stored_label.unwrap_or_else(|| candidate.label.clone()),
        is_known,
        has_marker,
    })
}

/// Discover connected MTP devices, resolving each against `known` config.
///
/// Convenience composing [`candidates`] and [`resolve`] on the calling
/// thread. The runtime splits the two phases across threads; the live
/// hardware tests call this directly.
pub fn discover(known: &[SyncDevice]) -> Vec<ConnectedDevice> {
    candidates()
        .iter()
        .filter_map(|candidate| resolve(candidate, known))
        .collect()
}

/// Choose the storage that holds the media tree: the well-known
/// `Internal shared storage` if present, else the first directory under the
/// volume root.
fn pick_storage(root: &gio::File) -> Option<String> {
    let enumerator = root
        .enumerate_children(
            "standard::name,standard::type",
            gio::FileQueryInfoFlags::NOFOLLOW_SYMLINKS,
            gio::Cancellable::NONE,
        )
        .ok()?;
    let mut first_directory = None;
    for info in enumerator {
        let info = info.ok()?;
        if info.file_type() != FileType::Directory {
            continue;
        }
        let name = info.name().to_string_lossy().into_owned();
        if name == PRIMARY_STORAGE {
            return Some(name);
        }
        first_directory.get_or_insert(name);
    }
    first_directory
}

/// Extract the device's stable MTP serial from its gio volume URI, e.g.
/// `mtp://Google_Pixel_9_Pro_48151FDAP009YH/` → the host segment. Used as
/// the marker-loss recognition fallback.
fn mtp_serial(volume_uri: &str) -> Option<String> {
    let host = volume_uri.strip_prefix("mtp://")?;
    let host = host.split('/').next()?;
    if host.is_empty() {
        None
    } else {
        Some(host.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::mtp_serial;

    #[test]
    fn extracts_serial_from_volume_uri() {
        assert_eq!(
            mtp_serial("mtp://Google_Pixel_9_Pro_48151FDAP009YH/").as_deref(),
            Some("Google_Pixel_9_Pro_48151FDAP009YH")
        );
        assert_eq!(mtp_serial("mtp:///").as_deref(), None);
        assert_eq!(mtp_serial("smb://server/share/").as_deref(), None);
    }
}
