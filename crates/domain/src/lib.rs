// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

mod acoustic;
mod analyzer_versions;
mod beat_grid;
mod bpm;
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
mod playlist_tree;
mod rating;
mod recording_identity;
mod settings;
mod smart_playlist;
mod smart_playlist_defaults;
mod smart_playlist_evaluation;
mod statistics;
mod synced_lyrics;
mod text;
mod track;
mod track_column_layout;
mod track_fields;
mod waveform;

pub use acoustic::AcousticFeatures;
pub use analyzer_versions::AnalyzerVersions;
pub use beat_grid::{BEATS_PER_BAR, Beat, BeatGrid, InvalidBeatGrid};
pub use bpm::{Bpm, InvalidBpm};
pub use clock::{Clock, MonotonicClock, SystemClock, SystemMonotonicClock};
pub use command::ApplicationCommand;
pub use device_sync::{
    AnlzRecord, DeviceArtistSelection, DeviceKind, DeviceLayout, DeviceRelativePath,
    DeviceSelection, FilesPerFolderCap, SyncDevice, SyncDeviceId, SyncManifestEntry,
};
pub use duplicate_consolidation::{
    DuplicateArtworkQuality, DuplicateAudioQuality, DuplicateConsolidationError,
    DuplicateConsolidationPlan, DuplicateConsolidationRequest, DuplicateGroup, DuplicateMatchMode,
    DuplicateMergeChoices, DuplicateMetadataField, DuplicateMetadataFieldSelection,
    DuplicateMetadataSelection, NoRecordingEvidence, RecordingEvidence, VerifiedDuplicateMerge,
    default_duplicate_merge_choices, default_duplicate_metadata_selection, duplicate_audio_quality,
    duplicate_groups, highest_quality_duplicate_artwork, highest_quality_duplicate_audio_track_ids,
    highest_rated_duplicate_track_id, plan_duplicate_consolidation,
};
pub use id::{PlaylistFolderId, PlaylistId, SmartPlaylistId, TrackId};
pub use library_statistics::{
    DecadeCount, GenreDistribution, GenrePlayCount, GenreRating, GenreShare, LibraryStatistics,
    OtherGenres, QualityBucket, QualityDistribution, QualityRange, YearCount,
    compute_library_statistics,
};
pub use managed_library::{
    ManagedTrackPathCandidates, ManagedTrackPathError, ManagedTrackPathInput, ManagedTrackPathPlan,
    ManagedTrackPathPlanner,
};
pub use metadata::{FieldChange, KeyOrigin, MetadataChange, TrackAudioProperties, TrackMetadata};
pub use musical_key::MusicalKey;
pub use playback::{
    LazyPickContext, PlayableId, PlaybackCommand, PlaybackLocation, PlaybackOptions, PlaybackQueue,
    PlaybackQueueEntry, PlaybackQueueEntryKind, PlaybackQueueRequest, PlaybackQueueSource,
    PlaybackSource, PlaybackState, RepeatMode, ShuffleMode, VolumePercent,
};
pub use playback_session::PlaybackSession;
pub use playlist::{Playlist, PlaylistEntry};
pub use playlist_folder::{PlaylistFolder, PlaylistItem};
pub use playlist_tree::{PlaylistTree, PlaylistTreeNode};
pub use rating::Rating;
pub use recording_identity::{RecordingIdentity, RecordingProfile, compare_recordings};
pub use settings::{
    AnalysisSettings, BackgroundJobsSettings, BackgroundResourceUsage, BpmRange, CdEncodingProfile,
    CdReadMode, DEFAULT_PLAYBACK_VOLUME_PERCENT, EncodingSettings, LibraryManagementMode,
    LibrarySettings, OnlineSettings, PlaybackSettings, SmartShuffleEntropy, UiSettings,
    UiSidebarSelection, UiWindowSize, UserSettings,
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
pub use text::{
    CollationKey, Spellings, compare_optional_text, effective_sort_key, normalize_search_text,
};
pub use track::{
    SourceFileStat, SourceFingerprint, Track, TrackAvailability, TrackContentHash, TrackLocation,
    TrackRelativePath,
};
pub use track_column_layout::{
    TrackColumnEntry, TrackColumnLayout, TrackColumnLayoutScope, TrackColumnSort,
};
pub use track_fields::TrackFields;
pub use waveform::{
    DETAIL_SEGMENTS_PER_SECOND, PREVIEW_SEGMENT_COUNT, TrackAnalysis, WaveformSegment,
    WaveformSegments,
};
