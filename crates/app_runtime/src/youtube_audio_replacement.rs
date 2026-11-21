// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Verified publication of a staged YouTube audio replacement.

use std::{fs, path::Path, time::Duration};

use sustain_domain::{
    LibraryManagementMode, ManagedTrackPathInput, ManagedTrackPathPlanner, Track, TrackLocation,
    TrackRelativePath,
};
use sustain_i18n::gettext;
use sustain_library_store::LibraryStore;
use sustain_metadata::{MetadataService, audio_format_from_path, hash_file_content};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity, YoutubeAudioDownloadResult,
    managed_library::file_ops::{RegularFileCapability, copy_file_verified, open_regular_file},
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
                gettext(
                    "Recovered an interrupted YouTube audio replacement, but retained the previous pathname because its identity could not be proven.",
                ),
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
            gettext("Downloading replacement audio from YouTube..."),
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
                        gettext("The downloaded audio could not be published safely."),
                    );
                }
            }
            Err(error) => {
                self.finish_youtube_audio_replacement(
                    result.track_id,
                    NotificationSeverity::Error,
                    youtube_audio_download_error_text(error),
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
                // The new audio discards the old file's analysis.
                self.apply_track_updated(crate::TrackUpdated {
                    track_id: result.track_id,
                    analysis: true,
                });
                self.refresh_playback_queue_track_ids();
                self.wake_background_sweeps();
                self.smart_shuffle_index = None;
                self.smart_shuffle_metadata = None;
                self.request_smart_shuffle_rebuild();
                let (severity, body) = if result.original_retained {
                    (
                        NotificationSeverity::Warning,
                        gettext(
                            "Replacement installed, but the previous audio file could not be moved to trash.",
                        ),
                    )
                } else {
                    (
                        NotificationSeverity::Info,
                        gettext("Track audio replaced from YouTube."),
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

    pub(crate) fn register_pending_youtube_audio_replacement(
        &mut self,
        track_id: sustain_domain::TrackId,
    ) {
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

fn youtube_audio_download_error_text(error: YoutubeAudioDownloadError) -> String {
    match error {
        YoutubeAudioDownloadError::InvalidUrl => gettext("Paste a valid YouTube URL."),
        YoutubeAudioDownloadError::YtDlpUnavailable => {
            gettext("Install yt-dlp and FFmpeg before replacing audio from YouTube.")
        }
        YoutubeAudioDownloadError::DownloadFailed => {
            gettext("yt-dlp could not download usable audio from that YouTube URL.")
        }
        YoutubeAudioDownloadError::OutputRejected => {
            gettext("yt-dlp returned an unsupported or unsafe audio file.")
        }
        YoutubeAudioDownloadError::PayloadTooLarge => {
            gettext("The downloaded YouTube replacement exceeded Sustain's size limit.")
        }
        YoutubeAudioDownloadError::TimedOut => {
            gettext("The YouTube audio download exceeded Sustain's time limit.")
        }
        YoutubeAudioDownloadError::Cancelled => {
            gettext("The YouTube audio download was cancelled.")
        }
    }
}

/// Replace `track_id`'s audio with the `staged` download. Once the
/// library points at the replacement, `trash_original` moves the original
/// file to the trash (the desktop trash in production); a failure leaves
/// the original in place and is reported as retained.
pub(crate) fn replace_track_audio_from_youtube(
    library_root: &Path,
    management_mode: LibraryManagementMode,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    track_id: sustain_domain::TrackId,
    staged: StagedYoutubeAudio,
    trash_original: impl FnOnce(&Path, &RegularFileCapability) -> std::io::Result<()>,
) -> ApplicationRuntimeResult<YoutubeAudioReplacementOutcome> {
    let canonical_root = fs::canonicalize(library_root)
        .map_err(|_| ApplicationRuntimeError::LibraryPathUnavailable)?;
    let track = store
        .track(track_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .filter(|track| !track.location.is_missing())
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
    crate::staged_tags::mirror_onto_staged_file(
        metadata_service,
        &staged.path,
        &downloaded.metadata,
        &track.metadata,
        track.rating,
        &artwork,
    )
    .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;

    let destination_relative_path = replacement_destination(
        &canonical_root,
        management_mode,
        store,
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

    let original_retained = trash_original(&original_path, &original).is_err();
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

/// Where the replacement of `track` goes: the first candidate path that no
/// library track, the original included, holds and no file occupies.
fn replacement_destination(
    library_root: &Path,
    management_mode: LibraryManagementMode,
    store: &dyn LibraryStore,
    track: &Track,
    staged_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    match management_mode {
        LibraryManagementMode::ReferenceFilesInPlace => {
            side_by_side_destination(library_root, store, track, staged_path)
        }
        LibraryManagementMode::CopyAddedFilesIntoLibrary => {
            managed_destination(library_root, store, track, staged_path)
        }
    }
}

fn is_free(
    library_root: &Path,
    store: &dyn LibraryStore,
    relative: &TrackRelativePath,
) -> ApplicationRuntimeResult<bool> {
    let holder = store
        .track_id_at(relative)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(holder.is_none() && !relative.resolve(library_root).exists())
}

fn side_by_side_destination(
    library_root: &Path,
    store: &dyn LibraryStore,
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
        if is_free(library_root, store, &relative)? {
            return Ok(relative);
        }
    }
    Err(ApplicationRuntimeError::YoutubeAudioReplacementFailed)
}

fn managed_destination(
    library_root: &Path,
    store: &dyn LibraryStore,
    track: &Track,
    staged_path: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    let candidates = ManagedTrackPathPlanner::default()
        .candidates(ManagedTrackPathInput {
            metadata: &track.metadata,
            source_path: staged_path,
        })
        .map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
    for candidate in candidates {
        let plan = candidate.map_err(|_| ApplicationRuntimeError::YoutubeAudioReplacementFailed)?;
        if is_free(library_root, store, &plan.relative_path)? {
            return Ok(plan.relative_path);
        }
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
        let store = sustain_library_store::SqliteLibraryStore::open_in_memory().expect("store");
        store.save_track(track.clone()).expect("save track");
        fs::write(root.path().join("old (YouTube).opus"), b"occupied").expect("write collision");

        let relative =
            side_by_side_destination(root.path(), &store, &track, Path::new("/staged/audio.opus"))
                .expect("destination");

        assert_eq!(relative.as_path(), Path::new("old (YouTube) 2.opus"));
    }

    #[test]
    fn an_analysed_tempo_the_download_does_not_carry_still_verifies() {
        use sustain_domain::{Bpm, KeyOrigin, MusicalKey};
        use sustain_metadata::LoftyMetadataService;

        // Four 128 kbps MPEG-1 Layer III frames: a real, if short, MP3.
        let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
        frame.resize(417, 0);
        let root = tempfile::tempdir().expect("library root");
        fs::write(root.path().join("old.mp3"), frame.repeat(4)).expect("write original");
        let staging = tempfile::tempdir().expect("staging directory");
        let staged_path = staging.path().join("audio.mp3");
        fs::write(&staged_path, frame.repeat(4)).expect("write download");

        // The library holds a measured tempo and key; the download has no
        // analysis tags, and the mirror leaves measured values off it.
        let mut track = sample_track();
        track.metadata.duration = Some(Duration::ZERO);
        track.metadata.title = Some("Kept".to_owned());
        track.metadata.bpm = Bpm::from_whole(120);
        track.metadata.bpm_set_by_user = false;
        track.metadata.key = Some(MusicalKey::CMajor);
        track.metadata.key_origin = KeyOrigin::Measured;
        let store = sustain_library_store::SqliteLibraryStore::open_in_memory().expect("store");
        store.save_track(track.clone()).expect("save track");

        replace_track_audio_from_youtube(
            root.path(),
            LibraryManagementMode::ReferenceFilesInPlace,
            &store,
            &LoftyMetadataService,
            track.id,
            StagedYoutubeAudio {
                _directory: staging,
                path: staged_path,
            },
            // Keep the test's files out of every real trash.
            |_original, _capability| Ok(()),
        )
        .expect("a correctly mirrored download verifies");

        let replaced = store
            .track(track.id)
            .expect("load track")
            .expect("track exists");
        assert_eq!(replaced.metadata.bpm, Bpm::from_whole(120));
        assert_eq!(replaced.metadata.title.as_deref(), Some("Kept"));
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
