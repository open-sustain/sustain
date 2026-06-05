// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

mod acoustic;
mod clock;
mod command;
mod device_sync;
mod duplicate_consolidation;
mod id;
mod library_statistics;
mod managed_library;
mod metadata;
mod musical_key;
mod playback;
mod playback_session;
mod playlist;
mod playlist_folder;
mod query;
mod rating;
mod settings;
mod smart_playlist;
mod smart_playlist_defaults;
mod smart_playlist_evaluation;
mod statistics;
mod synced_lyrics;
mod track;
mod track_column_layout;
mod waveform;

pub use acoustic::AcousticFeatures;
pub use clock::{Clock, MonotonicClock, SystemClock, SystemMonotonicClock};
pub use command::{ApplicationCommand, ApplicationQuery};
pub use device_sync::{
    DeviceKind, DeviceLayout, DeviceRelativePath, DeviceSelection, FilesPerFolderCap, SyncDevice,
    SyncDeviceId, SyncManifestEntry,
};
pub use duplicate_consolidation::{
    DuplicateAudioQuality, DuplicateConsolidationError, DuplicateConsolidationPlan,
    DuplicateConsolidationRequest, DuplicateMatchMode, DuplicateMetadataField,
    DuplicateMetadataFieldSelection, DuplicateMetadataSelection,
    default_duplicate_metadata_selection, duplicate_audio_quality, duplicate_groups,
    duplicate_groups_with_acoustics, highest_quality_duplicate_audio_track_ids,
    plan_duplicate_consolidation,
};
pub use id::{PlaylistFolderId, PlaylistId, SmartPlaylistId, TrackId};
pub use library_statistics::{
    DecadeCount, GenreDistribution, GenrePlayCount, GenreRating, GenreShare, LibraryStatistics,
    OtherGenres, QualityBucket, QualityDistribution, QualityRange, YearCount,
    compute_library_statistics,
};
pub use managed_library::{
    ManagedTrackPathError, ManagedTrackPathInput, ManagedTrackPathPlan, ManagedTrackPathPlanner,
};
pub use metadata::{FieldChange, MetadataChange, TrackAudioProperties, TrackMetadata};
pub use musical_key::MusicalKey;
pub use playback::{
    LazyPickContext, PlaybackCommand, PlaybackOptions, PlaybackQueue, PlaybackQueueEntry,
    PlaybackQueueEntryKind, PlaybackQueueRequest, PlaybackQueueSource, PlaybackState, RepeatMode,
    ShuffleMode, TrackPlaybackSource, VolumePercent,
};
pub use playback_session::PlaybackSession;
pub use playlist::{Playlist, PlaylistEntry};
pub use playlist_folder::{PlaylistFolder, PlaylistItem};
pub use query::{
    LibraryQuery, SortDirection, TrackSort, TrackSortColumn, compare_optional_text,
    effective_sort_key,
};
pub use rating::Rating;
pub use settings::{
    AnalysisSettings, BackgroundJobsSettings, BackgroundResourceUsage,
    DEFAULT_PLAYBACK_VOLUME_PERCENT, LibraryManagementMode, LibrarySettings, OnlineSettings,
    PlaybackSettings, SmartShuffleEntropy, UiSettings, UiSidebarSelection, UserSettings,
};
pub use smart_playlist::{
    SmartPlaylist, SmartPlaylistBoolField, SmartPlaylistBoolRule, SmartPlaylistDateField,
    SmartPlaylistLimit, SmartPlaylistLimitSelection, SmartPlaylistMatchKind,
    SmartPlaylistNumberField, SmartPlaylistNumberOperator, SmartPlaylistRule, SmartPlaylistRuleSet,
    SmartPlaylistTextField, SmartPlaylistTextOperator,
};
pub use smart_playlist_defaults::default_smart_playlists;
pub use smart_playlist_evaluation::{matching_tracks, track_matches_rule, track_matches_rule_set};
pub use statistics::PlayStatistics;
pub use synced_lyrics::{SyncedLyrics, SyncedLyricsLine};
pub use track::{
    SourceFileStat, SourceFingerprint, Track, TrackAvailability, TrackContentHash, TrackLocation,
    TrackRelativePath,
};
pub use track_column_layout::{TrackColumnEntry, TrackColumnLayout, TrackColumnLayoutScope};
pub use waveform::{
    BeatGrid, DETAIL_SEGMENTS_PER_SECOND, PREVIEW_SEGMENT_COUNT, TrackAnalysis, WaveformSegment,
    WaveformSegments,
};
