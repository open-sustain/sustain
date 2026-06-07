// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Durable value types for syncing playlists to external devices
//! (USB sticks, SD cards, and Android phones over MTP).
//!
//! These are the facts Sustain owns and persists about a device: its
//! stable identity, the on-drive layout to write, and which playlists
//! were ticked for it. The device only carries half the story (which
//! files are present); the selection lives here, keyed by a Sustain-
//! generated id stored in a `.sustain-device-id` marker on the device.
//! The sync engine, identity probing, and on-drive writers live in the
//! `sustain-device-sync` crate; this module is pure data so the storage
//! layer can persist it without pulling in that machinery.

use std::path::{Component, Path};

use crate::PlaylistItem;

/// Stable, transport-agnostic identifier Sustain assigns to a device on
/// first sync. Written into a `.sustain-device-id` marker on the device
/// and used as the SQLite key for the device's saved selection, options,
/// and manifest. Survives remounts and moving between machines.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SyncDeviceId(String);

impl SyncDeviceId {
    /// Wrap a non-empty id string (e.g. a generated UUID). Returns
    /// `None` for an empty or whitespace-only value.
    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            None
        } else {
            Some(Self(value))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

/// A normalized UTF-8 path beneath a connected device's mount root.
///
/// Removable media and persisted device state are untrusted input. Keeping
/// device-relative paths in this type prevents absolute paths, `..`, `.`,
/// repeated separators, and NUL bytes from reaching filesystem mutation
/// code. The empty value deliberately represents the mount root itself.
#[derive(Clone, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct DeviceRelativePath(String);

impl DeviceRelativePath {
    pub fn root() -> Self {
        Self::default()
    }

    pub fn new(value: impl Into<String>) -> Option<Self> {
        let value = value.into();
        if value.is_empty() {
            return Some(Self::root());
        }
        if value.contains('\0') {
            return None;
        }

        let mut normalized = Vec::new();
        for component in Path::new(&value).components() {
            match component {
                Component::Normal(component) => normalized.push(component.to_str()?),
                Component::Prefix(_)
                | Component::RootDir
                | Component::CurDir
                | Component::ParentDir => return None,
            }
        }
        (normalized.join("/") == value).then_some(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    pub fn components(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').filter(|component| !component.is_empty())
    }

    /// Join two already-validated relative paths. Since neither side can
    /// carry separators at its boundaries or traversal components, the
    /// result remains normalized and confined beneath the same root.
    pub fn join(&self, child: &Self) -> Self {
        match (self.is_root(), child.is_root()) {
            (true, _) => child.clone(),
            (_, true) => self.clone(),
            (false, false) => Self(format!("{}/{}", self.0, child.0)),
        }
    }

    pub fn join_component(&self, child: &str) -> Option<Self> {
        Self::new(child)
            .filter(|child| child.components().count() == 1)
            .map(|child| self.join(&child))
    }
}

impl std::fmt::Display for DeviceRelativePath {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// The on-drive layout written for a device. A per-device choice.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceLayout {
    /// One canonical `Music/Artist/Album/NN Title.ext` tree (each track
    /// stored once) plus one UTF-8 `.m3u8` per playlist with relative
    /// paths. For phones and players that read playlists.
    M3u,
    /// One folder per playlist holding real audio copies (a track in
    /// three playlists is copied three times). For folder-navigating car
    /// stereos and dumb players.
    FolderPerPlaylist,
    /// Pioneer's on-device database format (`export.pdb` + ANLZ
    /// waveforms), consumable by Pioneer XDJ/CDJ hardware and Rekordbox.
    Pioneer,
}

impl DeviceLayout {
    pub const ALL: [Self; 3] = [Self::M3u, Self::FolderPerPlaylist, Self::Pioneer];

    pub const fn as_db(self) -> i64 {
        match self {
            Self::M3u => 0,
            Self::FolderPerPlaylist => 1,
            Self::Pioneer => 2,
        }
    }

    pub const fn from_db(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::M3u),
            1 => Some(Self::FolderPerPlaylist),
            2 => Some(Self::Pioneer),
            _ => None,
        }
    }

    /// Short label for the UI layout chooser.
    pub const fn label(self) -> &'static str {
        match self {
            Self::M3u => "Playlists as .m3u8",
            Self::FolderPerPlaylist => "One folder per playlist",
            Self::Pioneer => "Pioneer (Rekordbox / XDJ)",
        }
    }
}

/// Optional per-folder file count cap for [`DeviceLayout::FolderPerPlaylist`].
/// Off by default; opt in for memory-limited players that choke on large
/// directories. When a playlist exceeds the cap it is split into numbered
/// subfolders (`01/`, `02/`).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesPerFolderCap {
    Unlimited,
    N64,
    N128,
    N256,
    N512,
}

impl FilesPerFolderCap {
    pub const ALL: [Self; 5] = [
        Self::Unlimited,
        Self::N64,
        Self::N128,
        Self::N256,
        Self::N512,
    ];

    /// The numeric cap, or `None` for unlimited.
    pub const fn limit(self) -> Option<u32> {
        match self {
            Self::Unlimited => None,
            Self::N64 => Some(64),
            Self::N128 => Some(128),
            Self::N256 => Some(256),
            Self::N512 => Some(512),
        }
    }

    /// Persist as the numeric value (0 = unlimited).
    pub const fn as_db(self) -> i64 {
        match self.limit() {
            None => 0,
            Some(n) => n as i64,
        }
    }

    pub const fn from_db(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::Unlimited),
            64 => Some(Self::N64),
            128 => Some(Self::N128),
            256 => Some(Self::N256),
            512 => Some(Self::N512),
            _ => None,
        }
    }

    pub const fn label(self) -> &'static str {
        match self {
            Self::Unlimited => "Unlimited",
            Self::N64 => "64",
            Self::N128 => "128",
            Self::N256 => "256",
            Self::N512 => "512",
        }
    }
}

/// What kind of device this is — drives the sidebar icon and the default
/// sub-path. Android phones are reached over MTP (see the `device_mtp`
/// crate); plain drives are mounted block filesystems.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeviceKind {
    /// A mounted block device: USB stick, SD card, external SSD.
    UsbDrive,
    /// An Android phone or tablet reached over MTP.
    Android,
}

impl DeviceKind {
    pub const fn as_db(self) -> i64 {
        match self {
            Self::UsbDrive => 0,
            Self::Android => 1,
        }
    }

    pub const fn from_db(value: i64) -> Option<Self> {
        match value {
            0 => Some(Self::UsbDrive),
            1 => Some(Self::Android),
            _ => None,
        }
    }

    /// Default sub-path under the device root to sync into. Both kinds
    /// default to the root: the MTP transport anchors at the phone's
    /// primary storage root (`/sdcard`), and the canonical `M3u` layout
    /// already nests audio under `Music/`, so audio lands at
    /// `/sdcard/Music/...` exactly as Android expects.
    pub const fn default_sub_path(self) -> &'static str {
        match self {
            Self::UsbDrive | Self::Android => "",
        }
    }
}

/// A device Sustain knows about: its identity plus the saved per-device
/// configuration. The ticked playlists are stored separately (see
/// [`crate::PlaylistItem`]); this is everything else.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncDevice {
    pub id: SyncDeviceId,
    /// Human-readable name shown in the sidebar.
    pub label: String,
    pub kind: DeviceKind,
    pub layout: DeviceLayout,
    /// Sub-path under the device root to sync into. Empty = device root.
    pub sub_path: DeviceRelativePath,
    pub files_per_folder_cap: FilesPerFolderCap,
    /// Filesystem volume id, used only to re-recognise a device whose
    /// marker file was deleted. `None` until first observed.
    pub volume_id: Option<String>,
}

/// One row of a device's sync manifest: a track Sustain last wrote to
/// the device, where it put it, and a fingerprint of the source content
/// at the time. On re-sync the engine diffs the resolved track set
/// against these rows and copies only what changed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncManifestEntry {
    pub track_id: crate::TrackId,
    /// Path on the device, relative to the device root.
    pub on_device_path: DeviceRelativePath,
    /// Fingerprint of the source file when it was last written (content
    /// hash when known, else a size-based token). A change means the
    /// on-device copy is stale and must be rewritten.
    pub fingerprint: String,
}

/// A device's saved playlist selection: the ticked playlists and smart
/// playlists, in display order. Folders are not selectable for sync.
pub type DeviceSelection = Vec<PlaylistItem>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_id_rejects_blank() {
        assert!(SyncDeviceId::new("  ").is_none());
        assert_eq!(
            SyncDeviceId::new("abc").map(SyncDeviceId::into_string),
            Some("abc".to_owned())
        );
    }

    #[test]
    fn layout_round_trips_through_db() {
        for layout in DeviceLayout::ALL {
            assert_eq!(DeviceLayout::from_db(layout.as_db()), Some(layout));
        }
        assert_eq!(DeviceLayout::from_db(99), None);
    }

    #[test]
    fn cap_round_trips_and_reports_limit() {
        for cap in FilesPerFolderCap::ALL {
            assert_eq!(FilesPerFolderCap::from_db(cap.as_db()), Some(cap));
        }
        assert_eq!(FilesPerFolderCap::Unlimited.limit(), None);
        assert_eq!(FilesPerFolderCap::N128.limit(), Some(128));
    }

    #[test]
    fn kind_round_trips_and_has_default_sub_path() {
        for kind in [DeviceKind::UsbDrive, DeviceKind::Android] {
            assert_eq!(DeviceKind::from_db(kind.as_db()), Some(kind));
        }
        assert_eq!(DeviceKind::Android.default_sub_path(), "");
        assert_eq!(DeviceKind::UsbDrive.default_sub_path(), "");
    }

    #[test]
    fn device_relative_path_accepts_only_normalized_beneath_root_paths() {
        assert_eq!(
            DeviceRelativePath::new(""),
            Some(DeviceRelativePath::root())
        );
        assert_eq!(
            DeviceRelativePath::new("Music/Artist/song.mp3").map(|path| path.as_str().to_owned()),
            Some("Music/Artist/song.mp3".to_owned())
        );
        for invalid in [
            "/Music/song.mp3",
            "../host-file",
            "Music/../host-file",
            "./Music",
            "Music//song.mp3",
            "Music/",
            "Music/\0song.mp3",
        ] {
            assert!(
                DeviceRelativePath::new(invalid).is_none(),
                "{invalid:?} must be rejected"
            );
        }
        let music = DeviceRelativePath::new("Music").expect("safe path");
        assert_eq!(
            music.join_component("song.mp3"),
            DeviceRelativePath::new("Music/song.mp3")
        );
        assert_eq!(music.join_component("Album/song.mp3"), None);
    }
}
