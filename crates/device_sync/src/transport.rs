// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The transport seam between the sync engine and a connected device.
//!
//! The engine, differ, manifest, and per-layout writers are
//! transport-agnostic: they describe what should be on the device as
//! [`DeviceRelativePath`]s beneath a device root and hand every read and
//! write to a [`DeviceTransport`]. Mounted block filesystems use the
//! root-confined [`crate::DeviceRoot`] implementation; Android phones use
//! the gio/GVfs MTP implementation in the `device_mtp` crate. Neither the
//! engine nor a layout ever opens a path itself.

use std::io;
use std::path::{Path, PathBuf};

use sustain_domain::{DeviceRelativePath, SourceFileStat};

use crate::model::DeviceCapacity;

/// How to reach a connected device's storage.
///
/// Plain data, deliberately free of any transport machinery so it can be
/// carried through the runtime's device identity and the UI without
/// pulling gio into those layers. The concrete transport is constructed
/// from this descriptor on the sync worker thread.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum DeviceTarget {
    /// A mounted block filesystem rooted at this mount point.
    Filesystem { mount_path: PathBuf },
    /// An Android phone addressed over MTP through gio/GVfs.
    Mtp(MtpTarget),
}

impl DeviceTarget {
    /// A short human-readable description of where the device lives, for
    /// the device panel's subtitle.
    pub fn location_label(&self) -> String {
        match self {
            Self::Filesystem { mount_path } => format!("Mounted at {}", mount_path.display()),
            Self::Mtp(_) => "Connected over MTP".to_owned(),
        }
    }
}

/// Address of an MTP volume's media storage.
///
/// `volume_uri` is the gio activation URI of the whole device (e.g.
/// `mtp://Google_Pixel_9_Pro_<serial>/`); `storage` is the display name of
/// the storage holding the standard media tree (e.g.
/// `Internal shared storage`), which is the first path segment under the
/// volume URI and the root the device-relative paths resolve against.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MtpTarget {
    pub volume_uri: String,
    pub storage: String,
}

/// Root-relative storage operations the sync engine needs from a device.
///
/// Every path is a [`DeviceRelativePath`] beneath the transport's own
/// root (a mount point, or an MTP storage root). Implementations must
/// confine all access to that root and reject anything that would escape
/// it. `cancel` is polled cooperatively so a long recursive operation can
/// stop between entries without leaving the device in an unrecoverable
/// state.
pub trait DeviceTransport {
    /// Total and available bytes of the device's filesystem.
    fn capacity(&self) -> io::Result<DeviceCapacity>;

    /// Create `path` and every missing ancestor directory.
    fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()>;

    /// Whether `path` exists and is a regular file.
    fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool>;

    /// The length of the regular file at `path`, or `None` if it does not
    /// exist. Errors if `path` exists but is not a regular file.
    fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>>;

    /// Read a small text file (the identity marker), failing if it is
    /// larger than `limit` bytes or not valid UTF-8.
    fn read_to_string(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<String>;

    /// Publish `bytes` to `path`, creating parent directories as needed.
    fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()>;

    /// Copy the host file at `source_path` to `path`, refusing if the
    /// source no longer matches `expected` (so a file rewritten mid-export
    /// is never half-copied under a stale fingerprint).
    fn copy_file(
        &self,
        source_path: &Path,
        path: &DeviceRelativePath,
        expected: &SourceFileStat,
    ) -> io::Result<()>;

    /// Remove the regular file at `path` if present. Returns whether a
    /// file was removed.
    fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool>;

    /// Recursively remove the directory tree at `path` if present. Returns
    /// whether a tree was removed.
    fn remove_tree_if_exists(
        &self,
        path: &DeviceRelativePath,
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<bool>;

    /// Remove any leftover Sustain staging files directly under `path`
    /// from an interrupted earlier run.
    fn cleanup_stale_temporary_files(
        &self,
        path: &DeviceRelativePath,
        cancel: &dyn Fn() -> bool,
    ) -> io::Result<()>;
}
