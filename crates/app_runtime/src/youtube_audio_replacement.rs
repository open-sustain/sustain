// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Verified publication of a staged YouTube audio replacement.

use std::{collections::BTreeSet, fs, path::Path, time::Duration};

use sustain_domain::{
    LibraryManagementMode, ManagedTrackPathInput, ManagedTrackPathPlanner, Track, TrackLocation,
    TrackRelativePath,
};
use sustain_library_store::LibraryStore;
use sustain_metadata::{MetadataService, audio_format_from_path, hash_file_content};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity, YoutubeAudioDownloadResult, freedesktop_trash,
    managed_library::file_ops::{copy_file_verified, open_regular_file},
    metadata_writer::full_metadata_mirror,
    youtube_audio_downloader::{
        MAX_YOUTUBE_REPLACEMENT_DURATION, StagedYoutubeAudio, YoutubeAudioDownloadError,
    },
    youtube_audio_journal::{
        YoutubeReplacementJournalEntry, remove_youtube_replacement_journal_if_present,
        write_youtube_replacement_journal,
    },
};

pub const MAX_YOUTUBE_REPLACEMENT_SOURCE_BITRATE_KBPS: u32 = 192;
const MIN_YOUTUBE_REPLACEMENT_BITRATE_KBPS: u32 = 128;
const MAX_DURATION_DIFFERENCE: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct YoutubeAudioReplacementOutcome {
    pub(crate) original_retained: bool,
}

impl ApplicationRuntime {
    pub(crate) fn report_youtube_replacement_recovery(
        &mut self,
        outcome: crate::youtube_audio_journal::YoutubeReplacementRecoveryOutcome,
    ) {
        if outcome.original_retained {
            self.push_ephemeral_notification(
                NotificationCategory::YoutubeAudioReplacement,
                NotificationSeverity::Warning,
                "Recovered an interrupted YouTube audio replacement, but retained the previous pathname because its identity could not be proven."
                    .to_owned(),
            );
        }
    }

    pub fn youtube_audio_replacement_is_eligible(&self, track_id: sustain_domain::TrackId) -> bool {
        self.library_track(track_id).is_some_and(track_is_eligible)
    }

    pub(super) fn request_youtube_audio_replacement(
        &mut self,
        track_id: sustain_domain::TrackId,
        url: String,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        let track = self
            .library_track(track_id)
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        ensure_track_is_eligible(track)?;
        if !self
            .youtube_audio_downloader
            .as_ref()
            .ok_or(ApplicationRuntimeError::YoutubeAudioDownloadUnavailable)?
            .submit(track_id, url)
        {
            return Err(ApplicationRuntimeError::YoutubeAudioDownloadUnavailable);
        }
        self.register_pending_youtube_audio_replacement(track_id);
        let notification_id = self.push_persistent_notification(
            NotificationCategory::YoutubeAudioReplacement,
            NotificationSeverity::Info,
            "Downloading replacement audio from YouTube...".to_owned(),
            false,
        );
        self.youtube_audio_replacement_notification_ids
            .insert(track_id, notification_id);
        Ok(())
    }

    pub fn set_youtube_audio_download_result_sink(
        &mut self,
        sink: async_channel::Sender<YoutubeAudioDownloadResult>,
    ) {
        self.youtube_audio_download_result_sink = Some(sink);
    }

    pub fn start_youtube_audio_downloader(&mut self) -> ApplicationRuntimeResult<()> {
        if self.youtube_audio_downloader.is_some() {
            return Ok(());
        }
        let sink = self
            .youtube_audio_download_result_sink
            .clone()
            .ok_or(ApplicationRuntimeError::YoutubeAudioDownloadUnavailable)?;
        self.youtube_audio_downloader = Some(
            crate::youtube_audio_downloader::YoutubeAudioDownloader::start(sink)
                .map_err(|_| ApplicationRuntimeError::YoutubeAudioDownloadUnavailable)?,
        );
        Ok(())
    }

    pub fn shutdown_youtube_audio_downloader(&mut self) {
        if let Some(downloader) = self.youtube_audio_downloader.take() {
            downloader.shutdown();
        }
    }

    pub fn apply_youtube_audio_download_result(&mut self, result: YoutubeAudioDownloadResult) {
        match result.outcome {
            Ok(staged) => {
                let submitted = self.metadata_writer().is_some_and(|writer| {
                    writer.replace_track_audio_from_youtube(
                        result.track_id,
                        staged,
                        self.settings.library.management_mode,
                    )
                });
                if !submitted {
                    self.finish_youtube_audio_replacement(
                        result.track_id,
                        NotificationSeverity::Error,
                        "The downloaded audio could not be published safely.".to_owned(),
                    );
                }
            }
            Err(error) => {
                self.finish_youtube_audio_replacement(
                    result.track_id,
                    NotificationSeverity::Error,
                    youtube_audio_download_error_text(error).to_owned(),
                );
            }
        }
    }

    pub(crate) fn apply_youtube_audio_replacement_result(
        &mut self,
        result: crate::metadata_writer::YoutubeAudioReplacementResult,
    ) {
        match result.outcome {
            Ok(()) => {
                self.apply_track_updated(result.track_id);
                self.refresh_playback_queue_track_ids();
                self.smart_shuffle_index = None;
                self.smart_shuffle_metadata = None;
                self.request_smart_shuffle_rebuild();
                let (severity, body) = if result.original_retained {
                    (
                        NotificationSeverity::Warning,
                        "Replacement installed, but the previous audio file could not be moved to trash."
                            .to_owned(),
                    )
                } else {
                    (
                        NotificationSeverity::Info,
                        "Track audio replaced from YouTube.".to_owned(),
                    )
                };
                self.finish_youtube_audio_replacement(result.track_id, severity, body);
            }
            Err(error) => {
                self.finish_youtube_audio_replacement(
                    result.track_id,
                    NotificationSeverity::Error,
                    crate::runtime_error_text(&error),
                );
            }
        }
    }

    pub(crate) fn has_pending_youtube_audio_replacement(&self) -> bool {
        !self.pending_youtube_audio_replacements.is_empty()
    }

    fn register_pending_youtube_audio_replacement(&mut self, track_id: sustain_domain::TrackId) {
        *self
            .pending_youtube_audio_replacements
            .entry(track_id)
            .or_default() += 1;
    }

    fn finish_youtube_audio_replacement(
        &mut self,
        track_id: sustain_domain::TrackId,
        severity: NotificationSeverity,
        body: String,
    ) {
        if let Some(notification_id) = self
            .youtube_audio_replacement_notification_ids
            .remove(&track_id)
        {
            self.dismiss_notification(notification_id);
        }
        let Some(count) = self.pending_youtube_audio_replacements.get_mut(&track_id) else {
            return;
        };
        *count -= 1;
        if *count == 0 {
            self.pending_youtube_audio_replacements.remove(&track_id);
        }
        self.push_ephemeral_notification(
            NotificationCategory::YoutubeAudioReplacement,
            severity,
            body,
        );
    }
}

fn youtube_audio_download_error_text(error: YoutubeAudioDownloadError) -> &'static str {
    match error {
        YoutubeAudioDownloadError::InvalidUrl => "Paste a valid YouTube URL.",
        YoutubeAudioDownloadError::YtDlpUnavailable => {
            "Install yt-dlp and FFmpeg before replacing audio from YouTube."
        }
        YoutubeAudioDownloadError::DownloadFailed => {
            "yt-dlp could not download usable audio from that YouTube URL."
        }
        YoutubeAudioDownloadError::OutputRejected => {
            "yt-dlp returned an unsupported or unsafe audio file."
        }
        YoutubeAudioDownloadError::PayloadTooLarge => {
            "The downloaded YouTube replacement exceeded Sustain's size limit."
        }
        YoutubeAudioDownloadError::TimedOut => {
            "The YouTube audio download exceeded Sustain's time limit."
        }
        YoutubeAudioDownloadError::Cancelled => "The YouTube audio download was cancelled.",
    }
}

pub(crate) fn replace_track_audio_from_youtube(
    library_root: &Path,
    management_mode: LibraryManagementMode,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    track_id: sustain_domain::TrackId,
    staged: StagedYoutubeAudio,
) -> ApplicationRuntimeResult<YoutubeAudioReplacementOutcome> {
    let canonical_root = fs::canonicalize(library_root)
        .map_err(|_| ApplicationRuntimeError::LibraryPathUnavailable)?;
    let tracks = store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let track = tracks
        .iter()
        .find(|track| track.id == track_id && !track.location.is_missing())
        .cloned()
        .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
    ensure_track_is_eligible(&track)?;
    let original_path = track.location.absolute_path(&canonical_root);
    let original =
        open_regular_file(&original_path).map_err(|_| ApplicationRuntimeError::TrackUnavailable)?;

    audio_format_from_path(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    let downloaded = metadata_service
        .read_persisted_tags(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    validate_downloaded_audio(&track, &downloaded.metadata)?;
    let artwork = metadata_service
        .read_artwork(&original_path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    metadata_service
        .write_metadata(&staged.path, full_metadata_mirror(&track.metadata))
        .and_then(|()| metadata_service.write_rating(&staged.path, track.rating))
        .and_then(|()| metadata_service.write_artwork(&staged.path, artwork.clone()))
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;

    let written = metadata_service
        .read_persisted_tags(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    let written_artwork = metadata_service
        .read_artwork(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if !crate::duplicate_consolidation::editable_metadata_matches(
        &written.metadata,
        &track.metadata,
    ) || written.rating != track.rating
        || written_artwork != artwork
    {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }

    let destination_relative_path = replacement_destination(
        &canonical_root,
        management_mode,
        &tracks,
        &track,
        &staged.path,
    )?;
    let destination_path = destination_relative_path.resolve(&canonical_root);
    let content_hash = hash_file_content(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    let replacement_size_bytes = fs::metadata(&staged.path)
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?
        .len();
    write_youtube_replacement_journal(
        &canonical_root,
        &YoutubeReplacementJournalEntry {
            track_id: track.id,
            original_identity: original.identity(),
            original_relative_path: track.location.relative_path.clone(),
            replacement_content_hash: content_hash.clone(),
            replacement_size_bytes,
            replacement_relative_path: destination_relative_path.clone(),
        },
    )?;
    let copy = match copy_file_verified(&staged.path, &destination_path, &content_hash) {
        Ok(copy) => copy,
        Err(_) => {
            if fs::symlink_metadata(&destination_path)
                .is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
            {
                let _ = remove_youtube_replacement_journal_if_present(&canonical_root);
            }
            return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
        }
    };
    let location = TrackLocation::available(destination_relative_path);
    if store
        .replace_track_audio(
            track.id,
            &location,
            downloaded.metadata.audio_properties(),
            copy.bytes_copied,
            artwork.is_some(),
        )
        .is_err()
    {
        return Err(ApplicationRuntimeError::LibraryStoreFailed);
    }
    store
        .flush_durable()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;

    let original_retained =
        freedesktop_trash::trash_regular_file(&original_path, &original).is_err();
    remove_youtube_replacement_journal_if_present(&canonical_root)?;
    Ok(YoutubeAudioReplacementOutcome { original_retained })
}

fn track_is_eligible(track: &Track) -> bool {
    !track.location.is_missing()
        && track
            .metadata
            .bitrate_kbps
            .is_some_and(|bitrate| bitrate <= MAX_YOUTUBE_REPLACEMENT_SOURCE_BITRATE_KBPS)
        && track
            .metadata
            .duration
            .is_some_and(|duration| duration <= MAX_YOUTUBE_REPLACEMENT_DURATION)
}

fn ensure_track_is_eligible(track: &Track) -> ApplicationRuntimeResult<()> {
    track_is_eligible(track)
        .then_some(())
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementNotEligible)
}

fn validate_downloaded_audio(
    track: &Track,
    downloaded: &sustain_domain::TrackMetadata,
) -> ApplicationRuntimeResult<()> {
    let original_duration = track
        .metadata
        .duration
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementNotEligible)?;
    let downloaded_duration = downloaded
        .duration
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if downloaded_duration > MAX_YOUTUBE_REPLACEMENT_DURATION
        || original_duration.abs_diff(downloaded_duration) > MAX_DURATION_DIFFERENCE
    {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    let original_bitrate = track
        .metadata
        .bitrate_kbps
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementNotEligible)?;
    let downloaded_bitrate = downloaded
        .bitrate_kbps
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    if downloaded_bitrate < MIN_YOUTUBE_REPLACEMENT_BITRATE_KBPS
        || downloaded_bitrate < original_bitrate
    {
        return Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed);
    }
    Ok(())
}

fn replacement_destination(
    library_root: &Path,
    management_mode: LibraryManagementMode,
    tracks: &[Track],
    track: &Track,
    staged_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    match management_mode {
        LibraryManagementMode::ReferenceFilesInPlace => {
            side_by_side_destination(library_root, tracks, track, staged_path)
        }
        LibraryManagementMode::CopyAddedFilesIntoLibrary => {
            managed_destination(library_root, tracks, track, staged_path)
        }
    }
}

fn side_by_side_destination(
    library_root: &Path,
    tracks: &[Track],
    track: &Track,
    staged_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    let parent = track
        .location
        .relative_path
        .as_path()
        .parent()
        .unwrap_or_else(|| Path::new(""));
    let stem = track
        .location
        .relative_path
        .as_path()
        .file_stem()
        .and_then(|stem| stem.to_str())
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    let extension = staged_path
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    for suffix in std::iter::once(String::new()).chain((2..10_000).map(|n| format!(" {n}"))) {
        let relative =
            TrackRelativePath::new(parent.join(format!("{stem} (YouTube){suffix}.{extension}")))
                .ok_or(ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        if !tracks
            .iter()
            .any(|existing| existing.location.relative_path == relative)
            && !relative.resolve(library_root).exists()
        {
            return Ok(relative);
        }
    }
    Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
}

fn managed_destination(
    library_root: &Path,
    tracks: &[Track],
    track: &Track,
    staged_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    let planner = ManagedTrackPathPlanner::default();
    let mut occupied = tracks
        .iter()
        .map(|track| track.location.relative_path.clone())
        .collect::<BTreeSet<_>>();
    for _ in 0..10_000 {
        let plan = planner
            .plan(
                ManagedTrackPathInput {
                    metadata: &track.metadata,
                    source_path: staged_path,
                },
                &occupied,
            )
            .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        if !plan.relative_path.resolve(library_root).exists() {
            return Ok(plan.relative_path);
        }
        occupied.insert(plan.relative_path);
    }
    Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
}

#[cfg(test)]
mod tests {
    use std::time::SystemTime;

    use sustain_domain::{PlayStatistics, Rating, TrackMetadata};

    use super::*;

    #[test]
    fn eligibility_requires_present_track_at_or_below_threshold() {
        let mut track = sample_track();
        assert!(track_is_eligible(&track));

        track.metadata.bitrate_kbps = Some(MAX_YOUTUBE_REPLACEMENT_SOURCE_BITRATE_KBPS + 1);
        assert!(!track_is_eligible(&track));

        track.metadata.bitrate_kbps = None;
        assert!(!track_is_eligible(&track));

        track.metadata.bitrate_kbps = Some(96);
        track.metadata.duration = Some(MAX_YOUTUBE_REPLACEMENT_DURATION + Duration::from_secs(1));
        assert!(!track_is_eligible(&track));

        track.metadata.duration = Some(Duration::from_secs(180));
        track.location = TrackLocation::missing(relative_path("old.mp3"));
        assert!(!track_is_eligible(&track));
    }

    #[test]
    fn downloaded_audio_must_match_duration_and_not_reduce_quality() {
        let track = sample_track();
        let mut downloaded = TrackMetadata {
            duration: Some(Duration::from_secs(182)),
            bitrate_kbps: Some(128),
            ..TrackMetadata::default()
        };
        assert_eq!(validate_downloaded_audio(&track, &downloaded), Ok(()));

        downloaded.duration = Some(Duration::from_secs(183));
        assert_eq!(
            validate_downloaded_audio(&track, &downloaded),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );

        downloaded.duration = Some(Duration::from_secs(180));
        downloaded.bitrate_kbps = Some(95);
        assert_eq!(
            validate_downloaded_audio(&track, &downloaded),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );

        downloaded.duration = Some(MAX_YOUTUBE_REPLACEMENT_DURATION + Duration::from_secs(1));
        downloaded.bitrate_kbps = Some(128);
        assert_eq!(
            validate_downloaded_audio(&track, &downloaded),
            Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
        );
    }

    #[test]
    fn reference_mode_publishes_side_by_side_using_downloaded_extension() {
        let root = tempfile::tempdir().expect("tempdir");
        let track = sample_track();
        fs::write(root.path().join("old (YouTube).opus"), b"occupied").expect("write collision");

        let relative = side_by_side_destination(
            root.path(),
            std::slice::from_ref(&track),
            &track,
            Path::new("/staged/audio.opus"),
        )
        .expect("destination");

        assert_eq!(relative.as_path(), Path::new("old (YouTube) 2.opus"));
    }

    fn sample_track() -> Track {
        Track {
            id: sustain_domain::TrackId::new(1).expect("track id"),
            location: TrackLocation::available(relative_path("old.mp3")),
            metadata: TrackMetadata {
                duration: Some(Duration::from_secs(180)),
                bitrate_kbps: Some(96),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: Some(1),
            has_embedded_artwork: Some(false),
            file_modified_at: Some(SystemTime::UNIX_EPOCH),
        }
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(path).expect("relative path")
    }
}
