// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Device identity and discovery.
//!
//! A device is recognised, in order: by the `.sustain-device-id` marker
//! Sustain writes on first sync (transport-agnostic, survives remounts
//! and moving between machines); failing that, by the filesystem volume
//! id, which re-recognises a device whose marker was deleted. The mount
//! path is never used as identity — it is unstable across sessions.
//!
//! Discovery enumerates mounted removable block devices (USB sticks, SD
//! cards) from `/proc/mounts`. Android/MTP devices are not block mounts
//! and are out of scope until their transport lands; nothing here blocks
//! on slow hardware, and callers run it lazily (never at startup).

use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sustain_domain::{DeviceKind, DeviceRelativePath, SyncDevice, SyncDeviceId};

use crate::device_root::DeviceRoot;

/// Name of the identity marker written at a device's root.
pub const MARKER_FILE: &str = ".sustain-device-id";

/// A currently-connected device, resolved against Sustain's known set.
#[derive(Clone, Debug)]
pub struct ConnectedDevice {
    pub id: SyncDeviceId,
    pub kind: DeviceKind,
    /// Filesystem mount point.
    pub mount_path: PathBuf,
    pub volume_id: Option<String>,
    /// Suggested label (the volume's mount-dir name).
    pub label: String,
    /// True when this id matches a device Sustain already has config for.
    pub is_known: bool,
    /// True when the `.sustain-device-id` marker is present on the device
    /// (false means it must be (re)written on the next sync).
    pub has_marker: bool,
}

/// Generate a fresh random device id (UUID-v4 shaped). Returns `None`
/// only if the system entropy source cannot be read.
pub fn generate_device_id() -> Option<SyncDeviceId> {
    let mut bytes = [0u8; 16];
    let mut file = fs::File::open("/dev/urandom").ok()?;
    file.read_exact(&mut bytes).ok()?;
    // Set the version (4) and variant bits, matching UUID-v4 shape.
    bytes[6] = (bytes[6] & 0x0F) | 0x40;
    bytes[8] = (bytes[8] & 0x3F) | 0x80;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let formatted = format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32],
    );
    SyncDeviceId::new(formatted)
}

/// Read the identity marker at a device root, if present and valid.
pub fn read_marker(mount: &Path) -> Option<SyncDeviceId> {
    let root = DeviceRoot::open(mount).ok()?;
    let marker = DeviceRelativePath::new(MARKER_FILE).expect("static marker path is safe");
    let contents = root.read_to_string(&marker, 4096).ok()?;
    SyncDeviceId::new(contents.trim())
}

/// Write the identity marker at a device root.
pub fn write_marker(mount: &Path, id: &SyncDeviceId) -> std::io::Result<()> {
    let root = DeviceRoot::open(mount)?;
    write_marker_to_root(&root, id)
}

pub(crate) fn write_marker_to_root(root: &DeviceRoot, id: &SyncDeviceId) -> std::io::Result<()> {
    let marker = DeviceRelativePath::new(MARKER_FILE).expect("static marker path is safe");
    root.write_file(&marker, id.as_str().as_bytes())
}

/// A mounted filesystem entry from `/proc/mounts`.
struct MountInfo {
    device: String,
    mount_point: PathBuf,
}

/// Enumerate mounted removable block devices that look like
/// user-plugged storage (USB sticks, SD cards): a real `/dev` block
/// device mounted under `/media`, `/run/media`, or `/mnt`.
fn removable_mounts() -> Vec<MountInfo> {
    let Ok(contents) = fs::read_to_string("/proc/mounts") else {
        return Vec::new();
    };
    let mut mounts = Vec::new();
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(device) = fields.next() else {
            continue;
        };
        let Some(raw_mount) = fields.next() else {
            continue;
        };
        if !device.starts_with("/dev/") {
            continue;
        }
        let mount_point = PathBuf::from(decode_mount_escapes(raw_mount));
        if is_removable_mount(&mount_point) {
            mounts.push(MountInfo {
                device: device.to_owned(),
                mount_point,
            });
        }
    }
    mounts
}

fn is_removable_mount(mount: &Path) -> bool {
    let prefixes = ["/media", "/run/media", "/mnt"];
    prefixes.iter().any(|prefix| {
        // Require a directory *under* the prefix, not the prefix itself.
        mount.starts_with(prefix) && mount != Path::new(prefix)
    })
}

/// Decode `/proc/mounts` octal escapes (`\040` space, `\011` tab, …).
fn decode_mount_escapes(raw: &str) -> String {
    let bytes = raw.as_bytes();
    let mut out = String::with_capacity(raw.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\'
            && i + 3 < bytes.len()
            && bytes[i + 1..i + 4].iter().all(u8::is_ascii_digit)
        {
            let octal = &raw[i + 1..i + 4];
            if let Ok(code) = u8::from_str_radix(octal, 8) {
                out.push(code as char);
                i += 4;
                continue;
            }
        }
        out.push(bytes[i] as char);
        i += 1;
    }
    out
}

/// Look up a block device's filesystem UUID via `/dev/disk/by-uuid`.
fn volume_id_for_device(device: &str) -> Option<String> {
    let target = fs::canonicalize(device).ok()?;
    let entries = fs::read_dir("/dev/disk/by-uuid").ok()?;
    for entry in entries.flatten() {
        if let Ok(link) = fs::canonicalize(entry.path())
            && link == target
        {
            return entry.file_name().to_str().map(str::to_owned);
        }
    }
    None
}

/// Discover connected devices, resolving each against `known` config.
///
/// Recognition order per mount: marker file, then volume-id fallback,
/// then a freshly generated id for an unseen device.
pub fn discover(known: &[SyncDevice]) -> Vec<ConnectedDevice> {
    let mut devices = Vec::new();
    for mount in removable_mounts() {
        let volume_id = volume_id_for_device(&mount.device);
        let label = mount
            .mount_point
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("USB Device")
            .to_owned();

        let marker = read_marker(&mount.mount_point);
        let (id, is_known, has_marker) = if let Some(id) = marker {
            let known_match = known.iter().any(|d| d.id == id);
            (id, known_match, true)
        } else if let Some(matched) = volume_id.as_ref().and_then(|vid| {
            known
                .iter()
                .find(|d| d.volume_id.as_deref() == Some(vid.as_str()))
        }) {
            // Marker was deleted; re-recognise by volume id.
            (matched.id.clone(), true, false)
        } else {
            match generate_device_id() {
                Some(id) => (id, false, false),
                None => continue,
            }
        };

        let stored_label = known
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.label.clone())
            .filter(|l| !l.is_empty());

        devices.push(ConnectedDevice {
            id,
            kind: DeviceKind::UsbDrive,
            mount_path: mount.mount_point,
            volume_id,
            label: stored_label.unwrap_or(label),
            is_known,
            has_marker,
        });
    }
    devices
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_ids_are_uuid_shaped_and_unique() {
        let a = generate_device_id().expect("entropy available");
        let b = generate_device_id().expect("entropy available");
        assert_ne!(a, b);
        assert_eq!(a.as_str().len(), 36);
        assert_eq!(a.as_str().chars().filter(|c| *c == '-').count(), 4);
    }

    #[test]
    fn marker_round_trips() {
        let dir = tempfile::tempdir().expect("tempdir");
        let id = generate_device_id().expect("entropy available");
        write_marker(dir.path(), &id).expect("write marker");
        assert_eq!(read_marker(dir.path()), Some(id));
    }

    #[test]
    fn marker_directory_is_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir(dir.path().join(MARKER_FILE)).expect("create marker directory");
        assert_eq!(read_marker(dir.path()), None);
    }

    #[test]
    fn decodes_mount_escapes() {
        assert_eq!(decode_mount_escapes("/media/My\\040Disk"), "/media/My Disk");
        assert_eq!(decode_mount_escapes("/mnt/plain"), "/mnt/plain");
    }

    #[test]
    fn removable_predicate() {
        assert!(is_removable_mount(Path::new("/media/user/USB")));
        assert!(is_removable_mount(Path::new("/run/media/user/SD")));
        assert!(!is_removable_mount(Path::new("/")));
        assert!(!is_removable_mount(Path::new("/home/user")));
        assert!(!is_removable_mount(Path::new("/media")));
    }
}
