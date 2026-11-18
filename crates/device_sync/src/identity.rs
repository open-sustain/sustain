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
//! Discovery here enumerates mounted removable block devices (USB sticks,
//! SD cards) from `/proc/mounts`. Android phones are not block mounts;
//! their MTP discovery lives in the `device_mtp` crate and is merged in by
//! the runtime. Nothing here blocks on slow hardware, and callers run it
//! lazily (never at startup).

use std::ffi::OsString;
use std::fs;
use std::io::Read;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use sustain_domain::{DeviceKind, DeviceRelativePath, SyncDevice, SyncDeviceId};

use crate::device_root::DeviceRoot;
use crate::transport::{DeviceTarget, DeviceTransport};

/// Name of the identity marker written at a device's root.
pub const MARKER_FILE: &str = ".sustain-device-id";

/// A currently-connected device, resolved against Sustain's known set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ConnectedDevice {
    pub id: SyncDeviceId,
    pub kind: DeviceKind,
    /// How to reach the device's storage (a mount point, or an MTP volume).
    pub target: DeviceTarget,
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

/// Read the identity marker through any transport, if present and valid.
pub fn read_marker_via_transport(transport: &dyn DeviceTransport) -> Option<SyncDeviceId> {
    let marker = DeviceRelativePath::new(MARKER_FILE).expect("static marker path is safe");
    let contents = transport.read_to_string(&marker, 4096).ok()?;
    SyncDeviceId::new(contents.trim())
}

/// Write the identity marker through any transport. The marker always
/// lives at the transport's own root, not under a configured sub-path.
pub fn write_marker_via_transport(
    transport: &dyn DeviceTransport,
    id: &SyncDeviceId,
) -> std::io::Result<()> {
    let marker = DeviceRelativePath::new(MARKER_FILE).expect("static marker path is safe");
    transport.write_file(&marker, id.as_str().as_bytes())
}

/// Read the identity marker at a mounted device root, if present and valid.
pub fn read_marker(mount: &Path) -> Option<SyncDeviceId> {
    let root = DeviceRoot::open(mount).ok()?;
    read_marker_via_transport(&root)
}

/// Write the identity marker at a mounted device root.
pub fn write_marker(mount: &Path, id: &SyncDeviceId) -> std::io::Result<()> {
    let root = DeviceRoot::open(mount)?;
    write_marker_via_transport(&root, id)
}

/// A mounted filesystem entry from `/proc/mounts`.
struct MountInfo {
    device: PathBuf,
    mount_point: PathBuf,
}

/// Enumerate mounted removable block devices that look like
/// user-plugged storage (USB sticks, SD cards): a real `/dev` block
/// device mounted under `/media`, `/run/media`, or `/mnt`.
fn removable_mounts() -> Vec<MountInfo> {
    // The kernel's mount table holds raw path bytes, which need not be
    // UTF-8; it is parsed as bytes so one such path hides no other mount.
    let Ok(contents) = fs::read("/proc/mounts") else {
        return Vec::new();
    };
    parse_removable_mounts(&contents)
}

fn parse_removable_mounts(contents: &[u8]) -> Vec<MountInfo> {
    contents
        .split(|&byte| byte == b'\n')
        .filter_map(|line| {
            // Fields are separated by single spaces; the kernel escapes
            // space, tab, newline and backslash inside a field, but leaves
            // other bytes, carriage return included, as they are.
            let mut fields = line.split(|&byte| byte == b' ');
            let device = fields.next()?;
            let raw_mount = fields.next()?;
            if !device.starts_with(b"/dev/") {
                return None;
            }
            let mount_point = mount_table_path(raw_mount);
            is_removable_mount(&mount_point).then(|| MountInfo {
                device: mount_table_path(device),
                mount_point,
            })
        })
        .collect()
}

/// A path field of the mount table, its octal escapes decoded.
fn mount_table_path(field: &[u8]) -> PathBuf {
    PathBuf::from(OsString::from_vec(decode_mount_escapes(field)))
}

fn is_removable_mount(mount: &Path) -> bool {
    let prefixes = ["/media", "/run/media", "/mnt"];
    prefixes.iter().any(|prefix| {
        // Require a directory *under* the prefix, not the prefix itself.
        mount.starts_with(prefix) && mount != Path::new(prefix)
    })
}

/// Decode `/proc/mounts` octal escapes (`\040` space, `\011` tab, …).
/// Each escape stands for one byte of the path, not one character.
fn decode_mount_escapes(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'\\'
            && let Some(byte) = raw.get(i + 1..i + 4).and_then(octal_byte)
        {
            out.push(byte);
            i += 4;
            continue;
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

/// The byte three octal digits spell, if they spell one.
fn octal_byte(digits: &[u8]) -> Option<u8> {
    let value = digits.iter().try_fold(0_u16, |value, digit| {
        matches!(digit, b'0'..=b'7').then(|| value * 8 + u16::from(digit - b'0'))
    })?;
    u8::try_from(value).ok()
}

/// Look up a block device's filesystem UUID via `/dev/disk/by-uuid`.
fn volume_id_for_device(device: &Path) -> Option<String> {
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
        let label = mount.mount_point.file_name().map_or_else(
            || "USB Device".to_owned(),
            |name| name.to_string_lossy().into_owned(),
        );

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
            target: DeviceTarget::Filesystem {
                mount_path: mount.mount_point,
            },
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
    use std::os::unix::ffi::OsStrExt;

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
        assert_eq!(
            decode_mount_escapes(b"/media/My\\040Disk"),
            b"/media/My Disk"
        );
        assert_eq!(decode_mount_escapes(b"/mnt/plain"), b"/mnt/plain");
        assert_eq!(decode_mount_escapes(b"/mnt/not\\9ab"), b"/mnt/not\\9ab");
    }

    #[test]
    fn a_non_ascii_mount_point_keeps_its_bytes() {
        let table = "/dev/sdb1 /media/user/Café vfat rw 0 0\n".as_bytes();
        let mounts = parse_removable_mounts(table);
        assert_eq!(mounts.len(), 1);
        assert_eq!(mounts[0].mount_point, Path::new("/media/user/Café"));
        assert_eq!(mounts[0].device, Path::new("/dev/sdb1"));
    }

    #[test]
    fn an_escaped_byte_above_ascii_is_one_byte_of_the_path() {
        // The kernel may escape the bytes of "é" (0xC3 0xA9) as octal.
        let table = b"/dev/sdb1 /media/user/Caf\\303\\251 vfat rw 0 0\n";
        let mounts = parse_removable_mounts(table);
        assert_eq!(mounts[0].mount_point, Path::new("/media/user/Café"));
    }

    #[test]
    fn a_mount_point_keeps_bytes_the_kernel_leaves_unescaped() {
        let table = b"/dev/sdb1 /media/Stick\r\xff vfat rw 0 0\n";
        let mounts = parse_removable_mounts(table);
        assert_eq!(
            mounts[0].mount_point.as_os_str().as_bytes(),
            b"/media/Stick\r\xff"
        );
    }

    #[test]
    fn a_non_utf8_mount_hides_no_other_mount() {
        let mut table = b"/dev/sdb1 /media/user/\xff vfat rw 0 0\n".to_vec();
        table.extend_from_slice(b"/dev/sdc1 /media/user/Stick vfat rw 0 0\n");
        let mounts = parse_removable_mounts(&table);
        assert_eq!(mounts.len(), 2);
        assert_eq!(
            mounts[0].mount_point.as_os_str().as_bytes(),
            b"/media/user/\xff"
        );
        assert_eq!(mounts[1].mount_point, Path::new("/media/user/Stick"));
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
