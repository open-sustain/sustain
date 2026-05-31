// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{
    fs::{self, File, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Mutex, MutexGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use directories::BaseDirs;
use serde::{Deserialize, Serialize};

pub use sustain_domain::{
    AnalysisSettings, BackgroundJobsSettings, BackgroundResourceUsage,
    DEFAULT_PLAYBACK_VOLUME_PERCENT, LibraryManagementMode, LibrarySettings, OnlineSettings,
    PlaybackSettings, PlaylistFolderId, PlaylistId, PlaylistItem, ShuffleMode, SmartPlaylistId,
    SmartShuffleEntropy, UiSettings, UiSidebarSelection, UserSettings, VolumePercent,
};

pub type SettingsResult<T> = Result<T, SettingsError>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SettingsError {
    ConfigDirectoryUnavailable,
    LoadFailed,
    SaveFailed,
    StoreUnavailable,
}

pub trait SettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings>;
    fn save_settings(&self, settings: UserSettings) -> SettingsResult<()>;
}

#[derive(Debug)]
pub struct InMemorySettingsStore {
    settings: Mutex<UserSettings>,
}

impl InMemorySettingsStore {
    pub fn new(settings: UserSettings) -> Self {
        Self {
            settings: Mutex::new(settings),
        }
    }

    fn settings_guard(&self) -> SettingsResult<MutexGuard<'_, UserSettings>> {
        self.settings
            .lock()
            .map_err(|_| SettingsError::StoreUnavailable)
    }
}

impl Default for InMemorySettingsStore {
    fn default() -> Self {
        Self::new(UserSettings::default())
    }
}

impl SettingsStore for InMemorySettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings> {
        Ok(self.settings_guard()?.clone())
    }

    fn save_settings(&self, settings: UserSettings) -> SettingsResult<()> {
        *self.settings_guard()? = settings;
        Ok(())
    }
}

#[derive(Debug)]
pub struct TomlSettingsStore {
    path: PathBuf,
}

impl TomlSettingsStore {
    pub fn open_default() -> SettingsResult<Self> {
        let base_dirs = BaseDirs::new().ok_or(SettingsError::ConfigDirectoryUnavailable)?;
        Ok(Self::new(
            base_dirs.config_dir().join("sustain").join("settings.toml"),
        ))
    }

    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl SettingsStore for TomlSettingsStore {
    fn load_settings(&self) -> SettingsResult<UserSettings> {
        if !self.path.exists() {
            return Ok(UserSettings::default());
        }

        let document = fs::read_to_string(&self.path).map_err(|_| SettingsError::LoadFailed)?;
        toml::from_str::<SettingsDocument>(&document)
            .map(SettingsDocument::into_settings)
            .map_err(|_| SettingsError::LoadFailed)
    }

    fn save_settings(&self, settings: UserSettings) -> SettingsResult<()> {
        let document = SettingsDocument::from_settings(settings);
        let serialized =
            toml::to_string_pretty(&document).map_err(|_| SettingsError::SaveFailed)?;
        atomic_replace(&self.path, |file| file.write_all(serialized.as_bytes()))
            .map_err(|_| SettingsError::SaveFailed)
    }
}

/// Atomically replace the file at `path` with the bytes produced by
/// `write_body`, so a crash, disk-full condition, or I/O error can never
/// truncate or corrupt the previous file. `settings.toml` carries the
/// library path and every preference, and a partial write would turn the
/// next launch's load into a hard failure rather than a recoverable prior
/// version.
///
/// The bytes land in an exclusive sibling temp file that is flushed and
/// `sync_all`'d, renamed over the destination (the atomic publish step),
/// and finally made durable by an fsync of the containing directory. Any
/// failure removes the temp file and leaves the prior file untouched, so a
/// successful return means the replacement is durably on disk and a failed
/// return means the previous file is still the one readers will see.
fn atomic_replace(
    path: &Path,
    write_body: impl FnOnce(&mut File) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path.parent().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "settings path has no parent directory",
        )
    })?;
    fs::create_dir_all(parent)?;

    let temp_path = temporary_sibling_path(path);
    let write_result = (|| -> io::Result<()> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)?;
        write_body(&mut file)?;
        file.flush()?;
        file.sync_all()
    })();
    if let Err(error) = write_result {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(error);
    }

    // fsync the directory so the rename itself is durable: without it a
    // crash right after `rename` could lose the directory entry update and
    // leave neither the old nor the new inode reachable.
    File::open(parent)?.sync_all()
}

/// A hidden, process-unique sibling of `path` to stage an atomic write in.
/// The pid, a monotonic-ish nanosecond stamp, and a process-local counter
/// together make a collision effectively impossible; `create_new` in
/// [`atomic_replace`] is the hard guard that would reject one anyway.
fn temporary_sibling_path(path: &Path) -> PathBuf {
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "settings".to_owned());
    let temp_name = format!(".{file_name}.{}.{nanos}.{sequence}.tmp", std::process::id());
    match path.parent() {
        Some(parent) => parent.join(temp_name),
        None => PathBuf::from(temp_name),
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct SettingsDocument {
    #[serde(default)]
    library: LibrarySettingsDocument,
    #[serde(default)]
    playback: PlaybackSettingsDocument,
    #[serde(default)]
    ui: UiSettingsDocument,
    #[serde(default)]
    analysis: AnalysisSettingsDocument,
    #[serde(default)]
    online: OnlineSettingsDocument,
    #[serde(default)]
    background_jobs: BackgroundJobsSettingsDocument,
}

#[derive(Debug, Deserialize, Serialize)]
struct LibrarySettingsDocument {
    path: Option<PathBuf>,
    #[serde(default)]
    management_mode: LibraryManagementModeDocument,
    #[serde(default = "default_honor_sort_tags")]
    honor_sort_tags: bool,
}

impl Default for LibrarySettingsDocument {
    fn default() -> Self {
        Self {
            path: None,
            management_mode: LibraryManagementModeDocument::default(),
            honor_sort_tags: default_honor_sort_tags(),
        }
    }
}

fn default_honor_sort_tags() -> bool {
    true
}

#[derive(Debug, Deserialize, Serialize)]
struct PlaybackSettingsDocument {
    /// Percent (0..=100). Defaults to [`DEFAULT_PLAYBACK_VOLUME_PERCENT`]
    /// when absent from disk, and is clamped on read so a hand-edited TOML
    /// with an out-of-range value can never crash the app at startup.
    #[serde(default = "default_volume_percent")]
    volume_percent: u8,
    #[serde(default)]
    shuffle_mode: ShuffleModeDocument,
    #[serde(default)]
    smart_shuffle_entropy: SmartShuffleEntropyDocument,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ShuffleModeDocument {
    #[default]
    Off,
    Pure,
    Smart,
}

impl ShuffleModeDocument {
    fn from_domain(mode: ShuffleMode) -> Self {
        match mode {
            ShuffleMode::Off => Self::Off,
            ShuffleMode::Pure => Self::Pure,
            ShuffleMode::Smart => Self::Smart,
        }
    }

    fn into_domain(self) -> ShuffleMode {
        match self {
            Self::Off => ShuffleMode::Off,
            Self::Pure => ShuffleMode::Pure,
            Self::Smart => ShuffleMode::Smart,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum SmartShuffleEntropyDocument {
    Focused,
    #[default]
    Balanced,
    Adventurous,
}

impl SmartShuffleEntropyDocument {
    fn from_domain(value: SmartShuffleEntropy) -> Self {
        match value {
            SmartShuffleEntropy::Focused => Self::Focused,
            SmartShuffleEntropy::Balanced => Self::Balanced,
            SmartShuffleEntropy::Adventurous => Self::Adventurous,
        }
    }

    fn into_domain(self) -> SmartShuffleEntropy {
        match self {
            Self::Focused => SmartShuffleEntropy::Focused,
            Self::Balanced => SmartShuffleEntropy::Balanced,
            Self::Adventurous => SmartShuffleEntropy::Adventurous,
        }
    }
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct UiSettingsDocument {
    #[serde(default)]
    search_text: String,
    #[serde(default)]
    sidebar_selection: UiSidebarSelectionDocument,
    #[serde(default)]
    sidebar_collapsed: bool,
    #[serde(default)]
    sidebar_width: Option<u32>,
    #[serde(default)]
    library_section_collapsed: bool,
    #[serde(default)]
    playlists_section_collapsed: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct AnalysisSettingsDocument {
    #[serde(default)]
    bpm: bool,
    #[serde(default)]
    key: bool,
    #[serde(default)]
    audio: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct OnlineSettingsDocument {
    #[serde(default)]
    artwork: bool,
    #[serde(default)]
    tags: bool,
    #[serde(default)]
    lyrics: bool,
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct BackgroundJobsSettingsDocument {
    #[serde(default)]
    resource_usage: BackgroundResourceUsageDocument,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum BackgroundResourceUsageDocument {
    Innocuous,
    #[default]
    Balanced,
    Aggressive,
}

impl Default for PlaybackSettingsDocument {
    fn default() -> Self {
        Self {
            volume_percent: DEFAULT_PLAYBACK_VOLUME_PERCENT,
            shuffle_mode: ShuffleModeDocument::default(),
            smart_shuffle_entropy: SmartShuffleEntropyDocument::default(),
        }
    }
}

fn default_volume_percent() -> u8 {
    DEFAULT_PLAYBACK_VOLUME_PERCENT
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum LibraryManagementModeDocument {
    #[default]
    ReferenceFilesInPlace,
    CopyAddedFilesIntoLibrary,
}

/// Persisted form of [`UiSidebarSelection`]. Serialised as a tagged
/// table with a `kind` discriminant; playlist-typed selections carry
/// the numeric id under `id`. Unknown or missing tables fall back to
/// the default Music selection on load.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
enum UiSidebarSelectionDocument {
    #[default]
    Music,
    Albums,
    Statistics,
    Playlist(i64),
    SmartPlaylist(i64),
    Folder(i64),
}

impl SettingsDocument {
    fn from_settings(settings: UserSettings) -> Self {
        Self {
            library: LibrarySettingsDocument {
                path: settings.library.path,
                management_mode: LibraryManagementModeDocument::from_domain(
                    settings.library.management_mode,
                ),
                honor_sort_tags: settings.library.honor_sort_tags,
            },
            playback: PlaybackSettingsDocument {
                volume_percent: settings.playback.volume.get(),
                shuffle_mode: ShuffleModeDocument::from_domain(settings.playback.shuffle_mode),
                smart_shuffle_entropy: SmartShuffleEntropyDocument::from_domain(
                    settings.playback.smart_shuffle_entropy,
                ),
            },
            ui: UiSettingsDocument {
                search_text: settings.ui.search_text,
                sidebar_selection: UiSidebarSelectionDocument::from_domain(
                    settings.ui.sidebar_selection,
                ),
                sidebar_collapsed: settings.ui.sidebar_collapsed,
                sidebar_width: settings.ui.sidebar_width,
                library_section_collapsed: settings.ui.library_section_collapsed,
                playlists_section_collapsed: settings.ui.playlists_section_collapsed,
            },
            analysis: AnalysisSettingsDocument {
                bpm: settings.analysis.bpm,
                key: settings.analysis.key,
                audio: settings.analysis.audio,
            },
            online: OnlineSettingsDocument {
                artwork: settings.online.artwork,
                tags: settings.online.tags,
                lyrics: settings.online.lyrics,
            },
            background_jobs: BackgroundJobsSettingsDocument {
                resource_usage: BackgroundResourceUsageDocument::from_domain(
                    settings.background_jobs.resource_usage,
                ),
            },
        }
    }

    fn into_settings(self) -> UserSettings {
        UserSettings {
            library: LibrarySettings {
                path: self.library.path,
                management_mode: self.library.management_mode.into_domain(),
                honor_sort_tags: self.library.honor_sort_tags,
            },
            playback: PlaybackSettings {
                volume: VolumePercent::from_clamped(self.playback.volume_percent),
                shuffle_mode: self.playback.shuffle_mode.into_domain(),
                smart_shuffle_entropy: self.playback.smart_shuffle_entropy.into_domain(),
            },
            ui: UiSettings {
                search_text: self.ui.search_text,
                sidebar_selection: self.ui.sidebar_selection.into_domain(),
                sidebar_collapsed: self.ui.sidebar_collapsed,
                sidebar_width: self.ui.sidebar_width,
                library_section_collapsed: self.ui.library_section_collapsed,
                playlists_section_collapsed: self.ui.playlists_section_collapsed,
            },
            // Normalize on load so a hand-edited config with
            // `audio = true, bpm = false` reaches the runtime as the
            // valid all-on state — `audio` always implies bpm + key.
            analysis: AnalysisSettings {
                bpm: self.analysis.bpm,
                key: self.analysis.key,
                audio: self.analysis.audio,
            }
            .normalized(),
            online: OnlineSettings {
                artwork: self.online.artwork,
                tags: self.online.tags,
                lyrics: self.online.lyrics,
            },
            background_jobs: BackgroundJobsSettings {
                resource_usage: self.background_jobs.resource_usage.into_domain(),
            },
        }
    }
}

impl BackgroundResourceUsageDocument {
    fn from_domain(usage: BackgroundResourceUsage) -> Self {
        match usage {
            BackgroundResourceUsage::Innocuous => Self::Innocuous,
            BackgroundResourceUsage::Balanced => Self::Balanced,
            BackgroundResourceUsage::Aggressive => Self::Aggressive,
        }
    }

    fn into_domain(self) -> BackgroundResourceUsage {
        match self {
            Self::Innocuous => BackgroundResourceUsage::Innocuous,
            Self::Balanced => BackgroundResourceUsage::Balanced,
            Self::Aggressive => BackgroundResourceUsage::Aggressive,
        }
    }
}

impl LibraryManagementModeDocument {
    fn from_domain(mode: LibraryManagementMode) -> Self {
        match mode {
            LibraryManagementMode::ReferenceFilesInPlace => Self::ReferenceFilesInPlace,
            LibraryManagementMode::CopyAddedFilesIntoLibrary => Self::CopyAddedFilesIntoLibrary,
        }
    }

    fn into_domain(self) -> LibraryManagementMode {
        match self {
            Self::ReferenceFilesInPlace => LibraryManagementMode::ReferenceFilesInPlace,
            Self::CopyAddedFilesIntoLibrary => LibraryManagementMode::CopyAddedFilesIntoLibrary,
        }
    }
}

impl UiSidebarSelectionDocument {
    fn from_domain(selection: UiSidebarSelection) -> Self {
        match selection {
            UiSidebarSelection::Music => Self::Music,
            UiSidebarSelection::Albums => Self::Albums,
            UiSidebarSelection::Statistics => Self::Statistics,
            UiSidebarSelection::Playlist(PlaylistItem::Playlist(id)) => Self::Playlist(id.get()),
            UiSidebarSelection::Playlist(PlaylistItem::SmartPlaylist(id)) => {
                Self::SmartPlaylist(id.get())
            }
            UiSidebarSelection::Playlist(PlaylistItem::Folder(id)) => Self::Folder(id.get()),
        }
    }

    /// Lossy in one direction: a persisted playlist/smart/folder id that
    /// no longer exists in the library (deleted between sessions) is
    /// silently demoted to the default Music selection rather than
    /// surfaced as an error. The caller has no UI affordance for "your
    /// last selection is gone" and falling back to Music is the same
    /// place a fresh install lands.
    fn into_domain(self) -> UiSidebarSelection {
        match self {
            Self::Music => UiSidebarSelection::Music,
            Self::Albums => UiSidebarSelection::Albums,
            Self::Statistics => UiSidebarSelection::Statistics,
            Self::Playlist(id) => PlaylistId::new(id)
                .map(PlaylistItem::Playlist)
                .map(UiSidebarSelection::Playlist)
                .unwrap_or_default(),
            Self::SmartPlaylist(id) => SmartPlaylistId::new(id)
                .map(PlaylistItem::SmartPlaylist)
                .map(UiSidebarSelection::Playlist)
                .unwrap_or_default(),
            Self::Folder(id) => PlaylistFolderId::new(id)
                .map(PlaylistItem::Folder)
                .map(UiSidebarSelection::Playlist)
                .unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Write},
        path::{Path, PathBuf},
    };

    use super::{
        AnalysisSettings, BackgroundJobsSettings, BackgroundResourceUsage,
        DEFAULT_PLAYBACK_VOLUME_PERCENT, InMemorySettingsStore, LibraryManagementMode,
        OnlineSettings, PlaylistId, PlaylistItem, SettingsStore, ShuffleMode, SmartShuffleEntropy,
        TomlSettingsStore, UiSettings, UiSidebarSelection, UserSettings, VolumePercent,
        atomic_replace,
    };

    #[test]
    fn in_memory_settings_store_defaults_to_no_library_path() {
        let store = InMemorySettingsStore::default();

        assert_eq!(store.load_settings(), Ok(UserSettings::default()));
    }

    #[test]
    fn in_memory_settings_store_saves_settings() {
        let store = InMemorySettingsStore::default();
        let settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));

        assert_eq!(store.save_settings(settings.clone()), Ok(()));

        assert_eq!(store.load_settings(), Ok(settings));
    }

    #[test]
    fn toml_settings_store_defaults_when_file_is_missing() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);

        assert_eq!(store.load_settings(), Ok(UserSettings::default()));
    }

    #[test]
    fn toml_settings_store_saves_and_loads_library_path() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let mut settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));
        settings.library.management_mode = LibraryManagementMode::CopyAddedFilesIntoLibrary;

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        assert_eq!(store.load_settings(), Ok(settings));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_round_trips_playback_volume() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let mut settings = UserSettings::default();
        settings.playback.volume = VolumePercent::from_clamped(37);

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        assert_eq!(store.load_settings(), Ok(settings));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_round_trips_playback_shuffle() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let mut settings = UserSettings::default();
        settings.playback.shuffle_mode = ShuffleMode::Smart;
        settings.playback.smart_shuffle_entropy = SmartShuffleEntropy::Adventurous;

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        assert_eq!(store.load_settings(), Ok(settings));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn loading_audio_without_bpm_key_normalizes_to_all_on() {
        // A hand-edited config that turns audio analysis on but leaves
        // bpm/key off (their default) is contradictory — audio yields
        // all three off one decode — so the loader normalizes it.
        let path = unique_settings_path();
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[analysis]\naudio = true\n").expect("write hand-edited config");

        let store = TomlSettingsStore::new(&path);
        let loaded = store.load_settings().expect("load settings");
        assert!(loaded.analysis.audio);
        assert!(loaded.analysis.bpm, "audio on must imply bpm on");
        assert!(loaded.analysis.key, "audio on must imply key on");

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_round_trips_ui_state() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let settings = UserSettings {
            ui: UiSettings {
                search_text: "radiohead".to_owned(),
                sidebar_selection: UiSidebarSelection::Playlist(PlaylistItem::Playlist(
                    PlaylistId::new(7).expect("positive playlist id"),
                )),
                sidebar_collapsed: true,
                sidebar_width: Some(248),
                library_section_collapsed: false,
                playlists_section_collapsed: true,
            },
            ..UserSettings::default()
        };

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        assert_eq!(store.load_settings(), Ok(settings));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_defaults_volume_when_section_is_missing() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[library]\npath = \"/music\"\n").expect("write settings");

        let settings = store.load_settings().expect("settings load");

        assert_eq!(
            settings.playback.volume,
            VolumePercent::from_clamped(DEFAULT_PLAYBACK_VOLUME_PERCENT)
        );

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_clamps_out_of_range_volume_on_load() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[playback]\nvolume_percent = 250\n").expect("write settings");

        let settings = store.load_settings().expect("settings load");

        assert_eq!(settings.playback.volume, VolumePercent::from_clamped(100));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_defaults_management_mode_when_missing() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[library]\npath = \"/music\"\n").expect("write settings");

        let settings = store.load_settings().expect("settings load");

        assert_eq!(
            settings.library.management_mode,
            LibraryManagementMode::ReferenceFilesInPlace
        );

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_round_trips_analysis_and_online_toggles() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        // A valid combination that round-trips unchanged: audio off, so
        // the load-time normalization (audio ⇒ bpm + key) leaves these
        // exactly as written. The normalization itself is covered by
        // `loading_audio_without_bpm_key_normalizes_to_all_on`.
        let settings = UserSettings {
            analysis: AnalysisSettings {
                bpm: true,
                key: false,
                audio: false,
            },
            online: OnlineSettings {
                artwork: true,
                tags: true,
                lyrics: false,
            },
            ..UserSettings::default()
        };

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        assert_eq!(store.load_settings(), Ok(settings));

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_round_trips_background_jobs_resource_usage() {
        for usage in [
            BackgroundResourceUsage::Innocuous,
            BackgroundResourceUsage::Balanced,
            BackgroundResourceUsage::Aggressive,
        ] {
            let path = unique_settings_path();
            let store = TomlSettingsStore::new(&path);
            let settings = UserSettings {
                background_jobs: BackgroundJobsSettings {
                    resource_usage: usage,
                },
                ..UserSettings::default()
            };

            assert_eq!(store.save_settings(settings.clone()), Ok(()));
            assert_eq!(store.load_settings(), Ok(settings));

            let root = path
                .parent()
                .and_then(|parent| parent.parent())
                .expect("test path has two parents");
            fs::remove_dir_all(root).expect("remove test settings directory");
        }
    }

    #[test]
    fn toml_settings_store_defaults_background_jobs_when_section_missing() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[library]\npath = \"/music\"\n").expect("write settings");

        let settings = store.load_settings().expect("settings load");

        assert_eq!(
            settings.background_jobs,
            BackgroundJobsSettings::default(),
            "missing section must fall back to Balanced default"
        );
        assert_eq!(
            settings.background_jobs.resource_usage,
            BackgroundResourceUsage::Balanced
        );

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_defaults_analysis_and_online_when_sections_missing() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        fs::create_dir_all(path.parent().expect("settings path has parent"))
            .expect("create settings dir");
        fs::write(&path, "[library]\npath = \"/music\"\n").expect("write settings");

        let settings = store.load_settings().expect("settings load");

        assert_eq!(settings.analysis, AnalysisSettings::default());
        assert_eq!(settings.online, OnlineSettings::default());

        let root = path
            .parent()
            .and_then(|parent| parent.parent())
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    #[test]
    fn toml_settings_store_save_publishes_durably_without_temp_litter() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));

        assert_eq!(store.save_settings(settings.clone()), Ok(()));
        // A successful return means the bytes are published and loadable...
        assert_eq!(store.load_settings(), Ok(settings));
        // ...and the staging temp file is gone, not orphaned beside it.
        assert_no_temp_litter(&path);

        remove_settings_tree(&path);
    }

    #[test]
    fn atomic_replace_failure_preserves_previous_file() {
        let path = unique_settings_path();
        let store = TomlSettingsStore::new(&path);
        let good = UserSettings::with_library_path(Some(PathBuf::from("/music")));
        assert_eq!(store.save_settings(good.clone()), Ok(()));
        let published = fs::read(&path).expect("read published settings");

        // A write that fails midway (disk full, I/O error) must not touch
        // the already-published file: the bytes go to the temp sibling,
        // which is discarded before the rename ever happens.
        let outcome = atomic_replace(&path, |file| {
            file.write_all(b"# corrupt partial settings\n")?;
            Err(io::Error::other("injected write failure"))
        });
        assert!(outcome.is_err(), "injected failure must propagate");

        let after_failure = fs::read(&path).expect("read settings after failed save");
        assert_eq!(
            published, after_failure,
            "a failed save must not truncate or replace the last valid file"
        );
        assert_eq!(store.load_settings(), Ok(good));
        assert_no_temp_litter(&path);

        remove_settings_tree(&path);
    }

    fn assert_no_temp_litter(settings_path: &Path) {
        let dir = settings_path.parent().expect("settings path has parent");
        for entry in fs::read_dir(dir).expect("read settings directory") {
            let name = entry.expect("settings directory entry").file_name();
            assert!(
                !name.to_string_lossy().ends_with(".tmp"),
                "atomic write left a temp file behind: {}",
                name.to_string_lossy()
            );
        }
    }

    fn remove_settings_tree(path: &Path) {
        let root = path
            .parent()
            .and_then(Path::parent)
            .expect("test path has two parents");
        fs::remove_dir_all(root).expect("remove test settings directory");
    }

    fn unique_settings_path() -> PathBuf {
        let unique_suffix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir()
            .join(format!("sustain_settings_test_{unique_suffix}"))
            .join("sustain")
            .join("settings.toml")
    }
}
