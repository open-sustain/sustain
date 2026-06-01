// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::path::{Path, PathBuf};

use crate::{PlaylistItem, ShuffleMode, VolumePercent};

/// Volume picked the first time the app runs, before any persisted value
/// exists. 80% matches the previous UI-side constant and is loud enough to
/// be obviously audible without startling anyone with sensitive headphones.
pub const DEFAULT_PLAYBACK_VOLUME_PERCENT: u8 = 80;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LibrarySettings {
    pub path: Option<PathBuf>,
    pub management_mode: LibraryManagementMode,
    /// Honor the tag-derived "sort as" names (issue #13) when ordering
    /// the library. On by default for well-tagged libraries; users who
    /// tag sort fields inconsistently can turn it off for strict
    /// alphabetic-as-shown ordering.
    pub honor_sort_tags: bool,
}

impl Default for LibrarySettings {
    fn default() -> Self {
        Self {
            path: None,
            management_mode: LibraryManagementMode::default(),
            honor_sort_tags: true,
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum LibraryManagementMode {
    #[default]
    ReferenceFilesInPlace,
    CopyAddedFilesIntoLibrary,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PlaybackSettings {
    pub volume: VolumePercent,
    /// Persisted shuffle preference. Restored at startup into the
    /// runtime's initial `PlaybackQueue::options()` so a user who
    /// closed the app with Smart shuffle on reopens with it on. The
    /// tri-state enum replaces the old `shuffle_enabled: bool` —
    /// `ShuffleMode::Off` is the silent default, `ShuffleMode::Pure`
    /// is the Fisher-Yates random walk, `ShuffleMode::Smart` defers
    /// next-track choice to the perceptual transition picker.
    pub shuffle_mode: ShuffleMode,
    /// Smart-shuffle exploration slider (focused / balanced /
    /// adventurous), exposed in the Shuffle preferences tab.
    /// Controls the candidate pool width and the softmax temperature
    /// applied to candidate scores; has no effect when Pure shuffle
    /// is active.
    pub smart_shuffle_entropy: SmartShuffleEntropy,
}

impl Default for PlaybackSettings {
    fn default() -> Self {
        Self {
            volume: VolumePercent::from_clamped(DEFAULT_PLAYBACK_VOLUME_PERCENT),
            shuffle_mode: ShuffleMode::Off,
            smart_shuffle_entropy: SmartShuffleEntropy::default(),
        }
    }
}

/// User-facing entropy preset for Smart Shuffle. The three stops on
/// the preferences slider map onto softmax temperatures; higher
/// entropy widens the distribution, giving lower-scoring candidates
/// more chance of being chosen.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SmartShuffleEntropy {
    Focused,
    #[default]
    Balanced,
    Adventurous,
}

impl SmartShuffleEntropy {
    /// Softmax temperature applied to the candidate score
    /// distribution. Higher = flatter (more exploration); lower =
    /// peakier (more exploitation of the top-scoring candidate).
    /// Calibrated empirically — the absolute values are not load-
    /// bearing, only their order matters.
    pub const fn temperature(self) -> f32 {
        match self {
            Self::Focused => 0.35,
            Self::Balanced => 0.7,
            Self::Adventurous => 1.4,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UiSettings {
    pub search_text: String,
    /// What the sidebar currently has selected — i.e. which view the
    /// user is looking at. The sidebar is the sole navigation surface,
    /// so a single enum captures both *which page* (Music / Albums /
    /// Playlists) and *which playlist* in one value.
    pub sidebar_selection: UiSidebarSelection,
    /// Whether the sidebar is slid out of view. The content beneath
    /// still occupies the full window width; the sidebar comes back
    /// from the floating bottom-left collapse toggle.
    pub sidebar_collapsed: bool,
    /// The user's manually-set sidebar width, in pixels. `None` means
    /// "no override has been saved" — the UI falls back to its
    /// default width. Always the last *expanded* width; collapsing
    /// the sidebar does not zero this out, so re-expanding restores
    /// the same width.
    pub sidebar_width: Option<u32>,
    /// Whether the LIBRARY disclosure section (Music / Albums rows) is
    /// folded shut. Independent of [`Self::playlists_section_collapsed`];
    /// folding only hides the section's rows, it does not change which
    /// view is selected.
    pub library_section_collapsed: bool,
    /// Whether the PLAYLISTS disclosure section (the playlist tree) is
    /// folded shut. Independent of [`Self::library_section_collapsed`].
    pub playlists_section_collapsed: bool,
    /// Whether the sidebar's LIBRARY → Duplicates row is shown. On by
    /// default.
    pub sidebar_show_duplicates: bool,
    /// Whether the sidebar's LIBRARY → Statistics row is shown. On by
    /// default.
    pub sidebar_show_statistics: bool,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            search_text: String::new(),
            sidebar_selection: UiSidebarSelection::default(),
            sidebar_collapsed: false,
            sidebar_width: None,
            library_section_collapsed: false,
            playlists_section_collapsed: false,
            sidebar_show_duplicates: true,
            sidebar_show_statistics: true,
        }
    }
}

/// The persisted sidebar entry the user had selected when the session
/// ended. Drives both which content-stack page is shown on next launch
/// and which row the sidebar paints as active.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum UiSidebarSelection {
    /// LIBRARY → Music. Default for a fresh install and the natural
    /// landing surface — a full track table of the whole library.
    #[default]
    Music,
    /// LIBRARY → Albums. Full-width album-cover grid.
    Albums,
    /// LIBRARY → Statistics. Whole-library diagnostic charts.
    Statistics,
    /// PLAYLISTS → a specific playlist, smart playlist, or folder row.
    Playlist(PlaylistItem),
}

/// Background-capability toggles for local audio analysis. Each flag enables
/// a paced background worker that fills the matching value on tracks that
/// are missing it. Flags never gate manual right-click runs — those are
/// always available and intentionally overwrite existing values.
///
/// The `audio` flag covers the single heavy decode pass: the perceptual
/// acoustic features (loudness, onset density, timbre) Smart Shuffle
/// consumes, the color waveforms (skipped on very long tracks), **and**
/// BPM + key — the analyzer derives all three from the one in-memory
/// decode, so enabling `audio` implies enabling `bpm` and `key`. See
/// [`AnalysisSettings::normalized`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct AnalysisSettings {
    pub bpm: bool,
    pub key: bool,
    pub audio: bool,
}

impl AnalysisSettings {
    /// Enforce the invariant that the heavy `audio` pass also yields BPM
    /// and key. The analyzer decodes one centered window per track and
    /// reads BPM, key, and the acoustic features off it, so asking for
    /// `audio` without `bpm`/`key` is a contradiction — there is no
    /// cheaper path that produces acoustics but skips the tempo/key it
    /// already has the samples for. Normalising at every ingestion point
    /// (settings load, the `UpdateSettings` command) means a hand-edited
    /// config or a stale UI state can never reach the scheduler with
    /// `audio: true, bpm: false`.
    pub fn normalized(self) -> Self {
        if self.audio {
            Self {
                bpm: true,
                key: true,
                audio: true,
            }
        } else {
            self
        }
    }
}

/// Background-capability toggles for network-bound retrieval. Same
/// missing-only, paced-background semantics as [`AnalysisSettings`].
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OnlineSettings {
    pub artwork: bool,
    pub tags: bool,
    pub lyrics: bool,
}

/// How much of the machine's resources background jobs (audio analysis
/// today; potentially other long-running workers later) are allowed to
/// take. The setting controls both the number of worker threads spawned
/// for these jobs and their CPU/IO scheduling priority — the
/// `Innocuous` end is intentionally polite (one thread, deeply niced)
/// so that day-job playback and UI work always win, while `Aggressive`
/// is closer to "drain the queue as fast as possible". The middle
/// `Balanced` stop is the maintainer's default: enough parallelism to
/// chew through a large library overnight while still leaving the
/// machine usable.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum BackgroundResourceUsage {
    /// One worker, lowest priority. Suitable when the machine is also
    /// the daily driver and the user prefers absolute zero impact on
    /// foreground work.
    Innocuous,
    /// Default. Roughly half the available cores, mid-low priority.
    #[default]
    Balanced,
    /// All available cores, near-default priority. Suitable for
    /// dedicated transcoding/analysis sessions.
    Aggressive,
}

impl BackgroundResourceUsage {
    /// Number of worker threads to spawn given the machine's available
    /// parallelism. Always at least one (a zero-worker scheduler would
    /// be silently broken). Every preset other than `Innocuous`
    /// **reserves two cores for the foreground** — playback, UI, and
    /// the rest of the desktop session — so even `Aggressive` does
    /// not saturate the machine. The reserved headroom matters more
    /// than the absolute throughput; a 32-core box running 32 workers
    /// at +0 nice would still stutter the audio pipeline.
    pub fn worker_count(self, cores: usize) -> usize {
        let cores = cores.max(1);
        // Headroom (in cores) reserved for foreground work. Anything
        // less than this clamps the preset to a single worker — the
        // user's day-job CPU time wins over background analysis on
        // small machines.
        const HEADROOM: usize = 2;
        match self {
            Self::Innocuous => 1,
            // Half the box, minus headroom; clamped to ≥ 1 so a
            // 4- or 6-core machine still gets one worker.
            Self::Balanced => (cores / 2).saturating_sub(HEADROOM).max(1),
            // Whole box minus headroom; clamped to ≥ 1.
            Self::Aggressive => cores.saturating_sub(HEADROOM).max(1),
        }
    }
}

/// Settings that govern how background jobs (audio analysis,
/// long-running scans) share the machine with the foreground app.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BackgroundJobsSettings {
    pub resource_usage: BackgroundResourceUsage,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct UserSettings {
    pub library: LibrarySettings,
    pub playback: PlaybackSettings,
    pub ui: UiSettings,
    pub analysis: AnalysisSettings,
    pub online: OnlineSettings,
    pub background_jobs: BackgroundJobsSettings,
}

impl UserSettings {
    pub fn with_library_path(library_path: Option<PathBuf>) -> Self {
        Self {
            library: LibrarySettings {
                path: library_path,
                management_mode: LibraryManagementMode::ReferenceFilesInPlace,
                honor_sort_tags: true,
            },
            playback: PlaybackSettings::default(),
            ui: UiSettings::default(),
            analysis: AnalysisSettings::default(),
            online: OnlineSettings::default(),
            background_jobs: BackgroundJobsSettings::default(),
        }
    }

    pub fn library_path(&self) -> Option<&Path> {
        self.library.path.as_deref()
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::{
        AnalysisSettings, BackgroundJobsSettings, BackgroundResourceUsage, LibraryManagementMode,
        OnlineSettings, UserSettings,
    };

    #[test]
    fn library_path_is_unset_by_default() {
        assert_eq!(UserSettings::default().library.path, None);
        assert_eq!(
            UserSettings::default().library.management_mode,
            LibraryManagementMode::ReferenceFilesInPlace
        );
    }

    #[test]
    fn settings_can_hold_a_library_path() {
        let settings = UserSettings::with_library_path(Some(PathBuf::from("/music")));

        assert_eq!(settings.library.path, Some(PathBuf::from("/music")));
        assert_eq!(
            settings.library.management_mode,
            LibraryManagementMode::ReferenceFilesInPlace
        );
    }

    #[test]
    fn background_capability_toggles_are_off_by_default() {
        let settings = UserSettings::default();

        assert_eq!(settings.analysis, AnalysisSettings::default());
        assert_eq!(settings.online, OnlineSettings::default());
        assert!(!settings.analysis.bpm);
        assert!(!settings.analysis.key);
        assert!(!settings.analysis.audio);
        assert!(!settings.online.artwork);
        assert!(!settings.online.tags);
        assert!(!settings.online.lyrics);
    }

    #[test]
    fn audio_normalization_forces_bpm_and_key_on() {
        // `audio` on implies `bpm` and `key` on — they come off the same
        // decode, so the scheduler must never see audio-without-the-rest.
        assert_eq!(
            AnalysisSettings {
                bpm: false,
                key: false,
                audio: true,
            }
            .normalized(),
            AnalysisSettings {
                bpm: true,
                key: true,
                audio: true,
            }
        );
        // With `audio` off the other flags are left exactly as they are.
        let bpm_only = AnalysisSettings {
            bpm: true,
            key: false,
            audio: false,
        };
        assert_eq!(bpm_only.normalized(), bpm_only);
        assert_eq!(
            AnalysisSettings::default().normalized(),
            AnalysisSettings::default()
        );
    }

    #[test]
    fn background_jobs_default_to_balanced() {
        let settings = UserSettings::default();

        assert_eq!(settings.background_jobs, BackgroundJobsSettings::default());
        assert_eq!(
            settings.background_jobs.resource_usage,
            BackgroundResourceUsage::Balanced
        );
    }

    #[test]
    fn background_resource_usage_worker_count_matches_preset() {
        // 32 cores (Ryzen AI Max+ 395 with SMT): Balanced is half the
        // box minus 2 headroom cores = 14; Aggressive is the box
        // minus 2 headroom cores = 30. Even Aggressive leaves room
        // for playback + UI.
        assert_eq!(BackgroundResourceUsage::Innocuous.worker_count(32), 1);
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(32), 14);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(32), 30);

        // 24 cores (Ryzen 7900 with SMT): same formula.
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(24), 10);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(24), 22);

        // 16 cores: half = 8, minus 2 = 6.
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(16), 6);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(16), 14);

        // 12 cores: half = 6, minus 2 = 4. Aggressive = 10.
        assert_eq!(BackgroundResourceUsage::Innocuous.worker_count(12), 1);
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(12), 4);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(12), 10);

        // 8 cores: half = 4, minus 2 = 2. Aggressive = 6.
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(8), 2);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(8), 6);

        // 4 cores: half = 2, minus 2 = 0, clamped to 1. Aggressive
        // = 4 - 2 = 2 (still leaves the headroom).
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(4), 1);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(4), 2);

        // 3 cores: every non-Innocuous preset clamps to 1 — Balanced's
        // (3/2)=1 minus 2 saturates to 0, Aggressive's 3-2=1.
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(3), 1);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(3), 1);

        // 1 core: every preset collapses to 1.
        assert_eq!(BackgroundResourceUsage::Innocuous.worker_count(1), 1);
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(1), 1);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(1), 1);

        // 0 cores (defensive — `available_parallelism` is documented to
        // never return zero, but the helper still has to round up to a
        // usable worker).
        assert_eq!(BackgroundResourceUsage::Innocuous.worker_count(0), 1);
        assert_eq!(BackgroundResourceUsage::Balanced.worker_count(0), 1);
        assert_eq!(BackgroundResourceUsage::Aggressive.worker_count(0), 1);
    }
}
