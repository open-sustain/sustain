// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! User-confirmed duplicate consolidation with crash recovery.

use std::{
    collections::BTreeSet,
    fs,
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{SystemTime, UNIX_EPOCH},
};

use sustain_domain::{
    DuplicateArtworkQuality, DuplicateConsolidationRequest, DuplicateGroup, DuplicateMatchMode,
    RecordingEvidence, RecordingIdentity, Track, TrackId, TrackRelativePath,
    VerifiedDuplicateMerge, duplicate_groups, highest_quality_duplicate_artwork,
    plan_duplicate_consolidation,
};
use sustain_i18n::{gettext, ngettext, plural_count, tr_format};
use sustain_library_store::LibraryStore;
use sustain_metadata::{MetadataService, hash_reader_content};

use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity,
    library_task::LibraryTaskKind,
    managed_library::{
        ManagedLibraryFilesystemValidator,
        file_ops::{
            FileIdentity, PinnedFilePath, RegularFileCapability, open_regular_file,
            path_refers_to_capability, publish_file_capability,
            publish_pinned_file_without_overwrite, regular_file_capability_from_file,
            regular_file_identity, remove_file_and_sync_parent,
            remove_pinned_regular_file_matching_capability,
            remove_regular_file_matching_capability,
        },
    },
    metadata_writer::VerifiedDuplicateBatchResult,
    notifications::{runtime_error_text, verified_duplicate_progress_text},
    recording_evidence::{RecordingIdentityCache, WaveformRecordingEvidence},
};

const JOURNAL_NAME: &str = ".sustain-duplicate-consolidation-journal";
const JOURNAL_HEADER: &str = "# sustain duplicate consolidation journal v1";

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DuplicateConsolidationSummary {
    pub removed_tracks: usize,
    pub cleanup_failed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationResult {
    pub survivor_id: TrackId,
    pub removed_track_ids: Vec<TrackId>,
    pub summary: DuplicateConsolidationSummary,
}

/// How a batch of verified merges went, group by group.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct VerifiedDuplicateBatchSummary {
    /// The merges that committed, in batch order.
    pub merges: Vec<DuplicateConsolidationResult>,
    /// Skipped: a track changed or left the library after the preview.
    pub changed_groups: usize,
    /// Skipped: the audio no longer establishes one recording.
    pub unverified_groups: usize,
    /// Attempted, and the merger refused or rolled back.
    pub failed_groups: usize,
    /// Never attempted: the batch was cancelled first.
    pub cancelled_groups: usize,
}

impl VerifiedDuplicateBatchSummary {
    pub fn removed_tracks(&self) -> usize {
        self.merges
            .iter()
            .map(|merge| merge.summary.removed_tracks)
            .sum()
    }

    pub fn cleanup_failed(&self) -> bool {
        self.merges.iter().any(|merge| merge.summary.cleanup_failed)
    }
}

/// Why one verified merge did not run.
#[derive(Clone, Debug, Eq, PartialEq)]
enum VerifiedMergeRefusal {
    Changed,
    Unverified,
    Failed,
}

pub struct DuplicateGroupsTask {
    store: std::sync::Arc<dyn LibraryStore>,
    mode: DuplicateMatchMode,
    recording_identities: Arc<std::sync::Mutex<RecordingIdentityCache>>,
}

impl DuplicateGroupsTask {
    /// Group the library into suggested duplicates. Off the main thread:
    /// comparing waveforms the cache has no verdict for yet takes a few
    /// milliseconds per pair.
    pub fn run(self) -> ApplicationRuntimeResult<Vec<DuplicateGroup>> {
        let tracks = self
            .store
            .tracks()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        let acoustics = self
            .store
            .load_all_acoustics()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
            .into_iter()
            .collect();
        let mut evidence =
            WaveformRecordingEvidence::new(self.store.as_ref(), self.recording_identities)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        Ok(duplicate_groups(
            &tracks,
            self.mode,
            &acoustics,
            &mut evidence,
        ))
    }
}

impl ApplicationRuntime {
    pub fn duplicate_groups_task(
        &self,
        mode: DuplicateMatchMode,
    ) -> ApplicationRuntimeResult<DuplicateGroupsTask> {
        self.ensure_library_hydrated()?;
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        Ok(DuplicateGroupsTask {
            store,
            mode,
            recording_identities: self.recording_identities.clone(),
        })
    }

    pub fn consolidate_duplicate_tracks(
        &mut self,
        request: DuplicateConsolidationRequest,
    ) -> ApplicationRuntimeResult<()> {
        self.ensure_no_conflicting_library_mutation()?;
        let library_root = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        for track_id in &request.track_ids {
            self.stop_playback_if_playing(*track_id);
        }
        // A single merge cannot stop part-way.
        self.start_library_task(
            LibraryTaskKind::DuplicateConsolidation,
            NotificationCategory::DuplicateConsolidation,
            gettext("Consolidating duplicate tracks..."),
            None,
        )?;

        if let Some(writer) = self.metadata_writer() {
            if writer.consolidate_duplicate_tracks(request) {
                return Ok(());
            }
            let error = ApplicationRuntimeError::DuplicateConsolidationFailed;
            self.apply_duplicate_consolidation_result(
                crate::metadata_writer::DuplicateConsolidationWriterResult {
                    outcome: Err(error.clone()),
                },
            );
            return Err(error);
        }

        let outcome = consolidate_duplicate_tracks(
            &library_root,
            store.as_ref(),
            metadata_service.as_ref(),
            &self.managed_library_filesystem_validator,
            &request,
        );
        let command_outcome = outcome.as_ref().map(|_| ()).map_err(Clone::clone);
        self.apply_duplicate_consolidation_result(
            crate::metadata_writer::DuplicateConsolidationWriterResult { outcome },
        );
        command_outcome
    }

    /// The previewable merges of `groups`: every group the audio verified
    /// as one recording whose tracks are all in the library and available,
    /// with the choices a merge makes when nobody picks.
    pub fn verified_duplicate_merges(
        &self,
        groups: &[DuplicateGroup],
    ) -> Vec<VerifiedDuplicateMerge> {
        groups
            .iter()
            .filter(|group| group.same_recording)
            .filter_map(|group| {
                let tracks = group
                    .track_ids
                    .iter()
                    .map(|track_id| {
                        self.library_track(*track_id)
                            .filter(|track| {
                                track.location.availability
                                    == sustain_domain::TrackAvailability::Available
                            })
                            .cloned()
                    })
                    .collect::<Option<Vec<_>>>()?;
                VerifiedDuplicateMerge::new(tracks)
            })
            .collect()
    }

    /// Merge each of `merges` in turn on the metadata writer, cancellable
    /// between merges. Every merge first checks that its tracks still match
    /// the preview and that the audio still verifies them as one recording.
    pub fn consolidate_verified_duplicates(
        &mut self,
        merges: Vec<VerifiedDuplicateMerge>,
    ) -> ApplicationRuntimeResult<()> {
        if merges.is_empty() {
            return Ok(());
        }
        self.ensure_no_conflicting_library_mutation()?;
        let library_root = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        let store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;

        for merge in &merges {
            for track in &merge.tracks {
                self.stop_playback_if_playing(track.id);
            }
        }
        // The batch stops between merges.
        let cancellation = Arc::new(AtomicBool::new(false));
        self.start_library_task(
            LibraryTaskKind::DuplicateConsolidation,
            NotificationCategory::DuplicateConsolidation,
            verified_duplicate_progress_text(0, merges.len()),
            Some(cancellation.clone()),
        )?;

        let recording_identities = self.recording_identities.clone();
        if let Some(writer) = self.metadata_writer() {
            if writer.consolidate_verified_duplicates(merges, recording_identities, cancellation) {
                return Ok(());
            }
            let error = ApplicationRuntimeError::DuplicateConsolidationFailed;
            self.apply_verified_duplicate_batch_result(VerifiedDuplicateBatchResult {
                outcome: Err(error.clone()),
            });
            return Err(error);
        }

        let summary = consolidate_verified_duplicates(
            &VerifiedDuplicateBatchContext {
                library_root: &library_root,
                store: store.as_ref(),
                metadata_service: metadata_service.as_ref(),
                filesystem_validator: &self.managed_library_filesystem_validator,
                recording_identities: &recording_identities,
                cancellation: &cancellation,
            },
            &merges,
            |_| {},
        );
        self.apply_verified_duplicate_batch_result(VerifiedDuplicateBatchResult {
            outcome: Ok(summary),
        });
        Ok(())
    }

    pub(crate) fn apply_verified_duplicate_progress(&mut self, completed: usize, total: usize) {
        self.update_library_task_progress(
            LibraryTaskKind::DuplicateConsolidation,
            verified_duplicate_progress_text(completed, total),
        );
    }

    pub(crate) fn apply_verified_duplicate_batch_result(
        &mut self,
        result: VerifiedDuplicateBatchResult,
    ) {
        self.finish_library_task(LibraryTaskKind::DuplicateConsolidation);
        match result.outcome {
            Ok(summary) => {
                if !summary.merges.is_empty() && !self.adopt_consolidated_library() {
                    return;
                }
                let severity = if summary.failed_groups > 0 || summary.cleanup_failed() {
                    NotificationSeverity::Warning
                } else {
                    NotificationSeverity::Info
                };
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    severity,
                    crate::notifications::verified_duplicate_outcome_text(&summary),
                );
            }
            Err(error) => {
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error),
                );
            }
        }
    }

    /// Reload the tracks and playlists consolidation rewrote. On failure the
    /// user is told, and `false` returned.
    fn adopt_consolidated_library(&mut self) -> bool {
        let refresh = self.library_store.as_ref().map_or(
            Err(ApplicationRuntimeError::LibraryServicesUnavailable),
            |store| {
                let tracks = crate::library_scan::load_library_tracks(store.as_ref())?;
                let playlists = store
                    .playlists()
                    .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
                Ok((tracks, playlists))
            },
        );
        let Ok((tracks, playlists)) = refresh else {
            self.push_ephemeral_notification(
                NotificationCategory::DuplicateConsolidation,
                NotificationSeverity::Error,
                runtime_error_text(&ApplicationRuntimeError::LibraryStoreFailed),
            );
            return false;
        };
        self.adopt_library_tracks(tracks);
        self.adopt_playlists(playlists);
        self.refresh_playback_queue_track_ids();
        self.request_smart_shuffle_rebuild();
        true
    }

    pub(crate) fn apply_duplicate_consolidation_result(
        &mut self,
        result: crate::metadata_writer::DuplicateConsolidationWriterResult,
    ) {
        self.finish_library_task(LibraryTaskKind::DuplicateConsolidation);
        match result.outcome {
            Ok(result) => {
                if !self.adopt_consolidated_library() {
                    return;
                }
                let (severity, body) = if result.summary.cleanup_failed {
                    (
                        NotificationSeverity::Warning,
                        tr_format!(
                            ngettext(
                                "{count} duplicate track consolidated. Sustain retained recovery files because cleanup could not finish safely.",
                                "{count} duplicate tracks consolidated. Sustain retained recovery files because cleanup could not finish safely.",
                                plural_count(result.summary.removed_tracks)
                            ),
                            count = result.summary.removed_tracks
                        ),
                    )
                } else {
                    (
                        NotificationSeverity::Info,
                        tr_format!(
                            ngettext(
                                "{count} duplicate track consolidated.",
                                "{count} duplicate tracks consolidated.",
                                plural_count(result.summary.removed_tracks)
                            ),
                            count = result.summary.removed_tracks
                        ),
                    )
                };
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    severity,
                    body,
                );
            }
            Err(error) => {
                self.push_ephemeral_notification(
                    NotificationCategory::DuplicateConsolidation,
                    NotificationSeverity::Error,
                    runtime_error_text(&error),
                );
            }
        }
    }
}

/// What a batch of verified merges works against.
pub(crate) struct VerifiedDuplicateBatchContext<'batch> {
    pub(crate) library_root: &'batch Path,
    pub(crate) store: &'batch dyn LibraryStore,
    pub(crate) metadata_service: &'batch dyn MetadataService,
    pub(crate) filesystem_validator: &'batch ManagedLibraryFilesystemValidator,
    pub(crate) recording_identities: &'batch Arc<Mutex<RecordingIdentityCache>>,
    pub(crate) cancellation: &'batch AtomicBool,
}

/// Run `merges` one at a time, stopping before the next merge once
/// cancellation is requested. `progress` hears how many groups are done
/// after each.
pub(crate) fn consolidate_verified_duplicates(
    context: &VerifiedDuplicateBatchContext<'_>,
    merges: &[VerifiedDuplicateMerge],
    mut progress: impl FnMut(usize),
) -> VerifiedDuplicateBatchSummary {
    let mut summary = VerifiedDuplicateBatchSummary::default();
    for (done, merge) in merges.iter().enumerate() {
        if context.cancellation.load(Ordering::SeqCst) {
            summary.cancelled_groups = merges.len() - done;
            break;
        }
        match consolidate_verified_duplicate(context, merge) {
            Ok(result) => summary.merges.push(result),
            Err(VerifiedMergeRefusal::Changed) => summary.changed_groups += 1,
            Err(VerifiedMergeRefusal::Unverified) => summary.unverified_groups += 1,
            Err(VerifiedMergeRefusal::Failed) => summary.failed_groups += 1,
        }
        progress(done + 1);
    }
    summary
}

fn consolidate_verified_duplicate(
    context: &VerifiedDuplicateBatchContext<'_>,
    merge: &VerifiedDuplicateMerge,
) -> Result<DuplicateConsolidationResult, VerifiedMergeRefusal> {
    let mut current = Vec::with_capacity(merge.tracks.len());
    for previewed in &merge.tracks {
        match context.store.track(previewed.id) {
            Ok(Some(track)) => current.push(track),
            Ok(None) => return Err(VerifiedMergeRefusal::Changed),
            Err(_) => return Err(VerifiedMergeRefusal::Failed),
        }
    }
    if !merge.still_matches(&current) {
        return Err(VerifiedMergeRefusal::Changed);
    }

    // Judged against the waveforms stored now: a track analysed again since
    // the preview is compared afresh.
    let mut evidence =
        WaveformRecordingEvidence::new(context.store, context.recording_identities.clone())
            .map_err(|_| VerifiedMergeRefusal::Failed)?;
    for (index, left) in current.iter().enumerate() {
        for right in &current[index + 1..] {
            if evidence.identity(left.id, right.id) != RecordingIdentity::Same {
                return Err(VerifiedMergeRefusal::Unverified);
            }
        }
    }

    let artwork_track_id =
        highest_quality_artwork_track_id(context, &current).unwrap_or(merge.choices.audio_track_id);
    consolidate_duplicate_tracks(
        context.library_root,
        context.store,
        context.metadata_service,
        context.filesystem_validator,
        &merge.request(artwork_track_id),
    )
    .map_err(|_| VerifiedMergeRefusal::Failed)
}

/// The track holding the highest-resolution artwork, as the merge dialog
/// presets it. Artwork that cannot be read or decoded counts as none, as it
/// does in the dialog.
fn highest_quality_artwork_track_id(
    context: &VerifiedDuplicateBatchContext<'_>,
    tracks: &[Track],
) -> Option<TrackId> {
    let qualities = tracks
        .iter()
        .map(|track| {
            let bytes = context
                .metadata_service
                .read_artwork(&track.location.absolute_path(context.library_root))
                .ok()
                .flatten()?;
            let dimensions = sustain_artwork::validate_encoded_artwork(&bytes).ok()?;
            Some(DuplicateArtworkQuality {
                pixels: dimensions.pixels,
                encoded_bytes: bytes.len(),
            })
        })
        .collect::<Vec<_>>();
    highest_quality_duplicate_artwork(&qualities).map(|index| tracks[index].id)
}

#[derive(Clone, Debug)]
struct JournalEntry {
    track_id: TrackId,
    source_identity: FileIdentity,
    source_capability: Option<RegularFileCapability>,
    source: TrackRelativePath,
    backup: TrackRelativePath,
}

#[derive(Clone, Debug)]
struct Journal {
    survivor_id: TrackId,
    stage: TrackRelativePath,
    entries: Vec<JournalEntry>,
}

pub(crate) fn consolidate_duplicate_tracks(
    library_root: &Path,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    filesystem_validator: &ManagedLibraryFilesystemValidator,
    request: &DuplicateConsolidationRequest,
) -> ApplicationRuntimeResult<DuplicateConsolidationResult> {
    filesystem_validator
        .validate(library_root)
        .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
    recover_duplicate_consolidation_journal(library_root, store)?;

    let tracks = store
        .tracks()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let playlists = store
        .playlists()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    // Validate every user-controlled reference and the enforced audio-quality
    // policy before publishing a journal or touching any pathname.
    plan_duplicate_consolidation(&tracks, &playlists, request, 0, false)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let selected = selected_tracks(&tracks, request)?;
    // Every selected file is hard-linked, rewritten, and removed, so a single
    // file missing on disk would leave the merge half-done. Reject up front
    // with a distinct error (#126) rather than failing opaquely mid-journal.
    ensure_selected_files_present(library_root, &selected)?;
    let journal = plan_journal(library_root, request.audio_track_id, &selected)?;
    let artwork_reference = selected
        .iter()
        .find(|track| track.id == request.artwork_track_id)
        .expect("selected references validated");
    let artwork = metadata_service
        .read_artwork(&artwork_reference.location.absolute_path(library_root))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;

    write_journal(library_root, &journal)?;
    let pre_commit = run_pre_commit(
        library_root,
        store,
        metadata_service,
        request,
        &tracks,
        &playlists,
        &journal,
        &artwork,
    );
    let plan = match pre_commit {
        Ok(plan) => plan,
        Err(error) => {
            rollback_pre_commit(library_root, &journal)?;
            return Err(error);
        }
    };

    store
        .flush_durable()
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    let cleanup_failed = cleanup_committed(library_root, &journal).is_err();
    Ok(DuplicateConsolidationResult {
        survivor_id: plan.survivor.id,
        removed_track_ids: plan.removed_track_ids,
        summary: DuplicateConsolidationSummary {
            removed_tracks: journal.entries.len().saturating_sub(1),
            cleanup_failed,
        },
    })
}

#[allow(clippy::too_many_arguments)]
fn run_pre_commit(
    library_root: &Path,
    store: &dyn LibraryStore,
    metadata_service: &dyn MetadataService,
    request: &DuplicateConsolidationRequest,
    tracks: &[Track],
    playlists: &[sustain_domain::Playlist],
    journal: &Journal,
    artwork: &Option<Vec<u8>>,
) -> ApplicationRuntimeResult<sustain_domain::DuplicateConsolidationPlan> {
    for entry in &journal.entries {
        let source = entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if !path_refers_to_capability(&entry.source.resolve(library_root), source).unwrap_or(false)
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        create_link(source, &entry.backup.resolve(library_root))?;
    }
    let survivor_entry = survivor_entry(journal);
    let survivor_source = survivor_entry.source.resolve(library_root);
    let stage_path = journal.stage.resolve(library_root);
    copy_to_stage(
        survivor_entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        &stage_path,
    )?;

    let preliminary = plan_duplicate_consolidation(
        tracks,
        playlists,
        request,
        file_size(&stage_path)?,
        artwork.is_some(),
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let staged = metadata_service
        .read_persisted_tags(&stage_path)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?
        .metadata;
    crate::staged_tags::mirror_onto_staged_file(
        metadata_service,
        &stage_path,
        &staged,
        &preliminary.survivor.metadata,
        preliminary.survivor.rating,
        artwork,
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;

    let stage = open_regular_file(&stage_path)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    replace_original_with_stage(
        &survivor_source,
        survivor_entry
            .source_capability
            .as_ref()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        &stage,
    )?;
    for entry in journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
    {
        remove_matching_file(
            &entry.source.resolve(library_root),
            entry
                .source_capability
                .as_ref()
                .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        )?;
    }

    let plan = plan_duplicate_consolidation(
        tracks,
        playlists,
        request,
        file_size(&survivor_source)?,
        artwork.is_some(),
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    store
        .commit_duplicate_consolidation(&plan)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
    Ok(plan)
}

pub fn recover_duplicate_consolidation_journal(
    library_root: &Path,
    store: &dyn LibraryStore,
) -> ApplicationRuntimeResult<()> {
    if !journal_path(library_root).exists() {
        return Ok(());
    }
    let journal = read_journal(library_root)?;
    let survivor_exists = store
        .track(journal.survivor_id)
        .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?
        .is_some();
    let removed_rows = journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
        .map(|entry| {
            store
                .track(entry.track_id)
                .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;

    if survivor_exists && removed_rows.iter().all(Option::is_none) {
        store
            .flush_durable()
            .map_err(|_| ApplicationRuntimeError::LibraryStoreFailed)?;
        cleanup_committed(library_root, &journal)
    } else if survivor_exists && removed_rows.iter().all(Option::is_some) {
        rollback_pre_commit(library_root, &journal)
    } else {
        Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
    }
}

fn selected_tracks<'a>(
    tracks: &'a [Track],
    request: &DuplicateConsolidationRequest,
) -> ApplicationRuntimeResult<Vec<&'a Track>> {
    let selected_ids = request.track_ids.iter().copied().collect::<BTreeSet<_>>();
    if selected_ids.len() < 2 || selected_ids.len() != request.track_ids.len() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let selected = request
        .track_ids
        .iter()
        .map(|track_id| {
            tracks
                .iter()
                .find(|track| track.id == *track_id && !track.location.is_missing())
                .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
        })
        .collect::<Result<Vec<_>, _>>()?;
    for reference in [
        request.audio_track_id,
        request.artwork_track_id,
        request.rating_track_id,
    ] {
        if !selected.iter().any(|track| track.id == reference) {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    Ok(selected)
}

/// Reject the merge before it touches any pathname if a selected file is not a
/// readable regular file on disk (#126). The selection already screened the
/// stale `is_missing` flag; this catches a file deleted out from under a track
/// whose flag was never updated.
fn ensure_selected_files_present(
    library_root: &Path,
    selected: &[&Track],
) -> ApplicationRuntimeResult<()> {
    for track in selected {
        if regular_file_identity(&track.location.absolute_path(library_root)).is_err() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationSourceMissing);
        }
    }
    Ok(())
}

fn plan_journal(
    library_root: &Path,
    survivor_id: TrackId,
    tracks: &[&Track],
) -> ApplicationRuntimeResult<Journal> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let recovery_directory = format!(
        ".sustain-duplicate-consolidation-{}-{unique}",
        std::process::id()
    );
    let survivor = tracks
        .iter()
        .find(|track| track.id == survivor_id)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let extension = survivor
        .location
        .path()
        .extension()
        .and_then(|extension| extension.to_str())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let stage = relative_path(format!("{recovery_directory}/stage.{extension}"))?;
    let entries = tracks
        .iter()
        .enumerate()
        .map(|(index, track)| {
            let source = track.location.relative_path.clone();
            let source_capability = open_regular_file(&source.resolve(library_root))
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
            Ok(JournalEntry {
                track_id: track.id,
                source_identity: source_capability.identity(),
                source_capability: Some(source_capability),
                source,
                backup: relative_path(format!("{recovery_directory}/backup-{index}"))?,
            })
        })
        .collect::<ApplicationRuntimeResult<Vec<_>>>()?;
    Ok(Journal {
        survivor_id,
        stage,
        entries,
    })
}

fn survivor_entry(journal: &Journal) -> &JournalEntry {
    journal
        .entries
        .iter()
        .find(|entry| entry.track_id == journal.survivor_id)
        .expect("journal survivor entry")
}

fn create_link(source: &RegularFileCapability, destination: &Path) -> ApplicationRuntimeResult<()> {
    publish_file_capability(source, destination)
        .map(drop)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn copy_to_stage(
    source: &RegularFileCapability,
    destination: &Path,
) -> ApplicationRuntimeResult<()> {
    let destination = PinnedFilePath::creating_parent(destination)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    if destination
        .exists()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut destination_file = destination
        .create_new_file()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let destination_capability = regular_file_capability_from_file(
        destination_file
            .try_clone()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let result = (|| {
        let mut source_file = source
            .try_clone_file()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        source_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let source_metadata = source_file
            .metadata()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let copied = io::copy(&mut source_file, &mut destination_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if copied != source_metadata.len() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        destination_file
            .set_permissions(source_metadata.permissions())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        destination_file
            .flush()
            .and_then(|()| destination_file.sync_all())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        source_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        destination_file
            .seek(SeekFrom::Start(0))
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let source_hash = hash_reader_content(&mut source_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let destination_hash = hash_reader_content(&mut destination_file)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        if source_hash != destination_hash
            || !destination
                .refers_to(&destination_capability)
                .unwrap_or(false)
            || destination.sync_parent().is_err()
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        Ok(())
    })();
    if result.is_err() {
        let _ =
            remove_pinned_regular_file_matching_capability(&destination, &destination_capability);
    }
    result
}

fn remove_matching_file(
    path: &Path,
    expected: &RegularFileCapability,
) -> ApplicationRuntimeResult<()> {
    remove_regular_file_matching_capability(path, expected)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn replace_original_with_stage(
    original: &Path,
    original_capability: &RegularFileCapability,
    stage: &RegularFileCapability,
) -> ApplicationRuntimeResult<()> {
    remove_matching_file(original, original_capability)?;
    create_link(stage, original)
}

fn rollback_pre_commit(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage = open_regular_file(&journal.stage.resolve(library_root)).ok();
    for entry in &journal.entries {
        let source = entry.source.resolve(library_root);
        let backup = entry.backup.resolve(library_root);
        let backup = match open_regular_file(&backup) {
            Ok(backup) => backup,
            Err(_) if path_is_missing(&backup) => {
                if regular_file_identity(&source).is_err() {
                    return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
                }
                continue;
            }
            Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        };
        match path_refers_to_capability(&source, &backup) {
            Ok(true) => {}
            Err(_) if path_is_missing(&source) => create_link(&backup, &source)?,
            Ok(false)
                if entry.track_id == journal.survivor_id
                    && stage.as_ref().is_some_and(|stage| {
                        path_refers_to_capability(&source, stage).unwrap_or(false)
                    }) =>
            {
                remove_matching_file(
                    &source,
                    stage
                        .as_ref()
                        .expect("stage capability checked in match guard"),
                )?;
                create_link(&backup, &source)?;
            }
            _ => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    cleanup_recovery_files(library_root, journal)?;
    remove_journal(library_root)
}

fn cleanup_committed(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let survivor = survivor_entry(journal).source.resolve(library_root);
    regular_file_identity(&survivor)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    for entry in journal
        .entries
        .iter()
        .filter(|entry| entry.track_id != journal.survivor_id)
    {
        let source = entry.source.resolve(library_root);
        let backup_path = entry.backup.resolve(library_root);
        let backup = match open_regular_file(&backup_path) {
            Ok(backup) => backup,
            // Every donor source is removed before any backup, so a missing
            // backup means an earlier cleanup already got past this donor.
            // A file now at its source path is not ours to remove.
            Err(_) if path_is_missing(&backup_path) => continue,
            Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        };
        match path_refers_to_capability(&source, &backup) {
            Ok(true) => remove_matching_file(&source, &backup)?,
            // Not the donor: a file the user put there since. Not ours.
            Ok(false) => {}
            Err(_) if path_is_missing(&source) => {}
            Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    cleanup_recovery_files(library_root, journal)?;
    remove_journal(library_root)
}

fn cleanup_recovery_files(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage_path = journal.stage.resolve(library_root);
    let recovery_directory = stage_path
        .parent()
        .map(Path::to_path_buf)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_directory_path = PinnedFilePath::existing_parent(&recovery_directory)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_directory_capability = match recovery_directory_path.open_directory() {
        Ok(directory) => Arc::new(directory),
        Err(_) if path_is_missing(&recovery_directory) => return Ok(()),
        Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
    };

    let stage = pinned_recovery_child(
        &recovery_directory,
        recovery_directory_capability.clone(),
        &stage_path,
    )?;
    match stage.open_regular_file() {
        Ok(stage_capability) => {
            remove_pinned_regular_file_matching_capability(&stage, &stage_capability)
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        }
        Err(_) if !stage.exists().unwrap_or(true) => {}
        Err(_) => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
    }
    for entry in &journal.entries {
        let backup = pinned_recovery_child(
            &recovery_directory,
            recovery_directory_capability.clone(),
            &entry.backup.resolve(library_root),
        )?;
        match backup.open_regular_file() {
            Ok(backup_capability) if backup_capability.identity() == entry.source_identity => {
                remove_pinned_regular_file_matching_capability(&backup, &backup_capability)
                    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
            }
            Err(_) if !backup.exists().unwrap_or(true) => {}
            _ => return Err(ApplicationRuntimeError::DuplicateConsolidationFailed),
        }
    }
    if !recovery_directory_path
        .refers_to_directory(recovery_directory_capability.as_ref())
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    recovery_directory_path
        .remove_directory()
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn pinned_recovery_child(
    recovery_directory: &Path,
    recovery_directory_capability: Arc<fs::File>,
    path: &Path,
) -> ApplicationRuntimeResult<PinnedFilePath> {
    if path.parent() != Some(recovery_directory) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    PinnedFilePath::in_open_parent(
        recovery_directory,
        recovery_directory_capability,
        path.file_name()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )
    .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

/// Whether `path` names nothing: it is absent, or one of its ancestors is
/// not a directory, so no file can exist there.
fn path_is_missing(path: &Path) -> bool {
    fs::symlink_metadata(path).is_err_and(|error| {
        matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::NotADirectory
        )
    })
}

fn remove_journal(library_root: &Path) -> ApplicationRuntimeResult<()> {
    let path = journal_path(library_root);
    let journal = open_regular_file(&path)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    remove_regular_file_matching_capability(&path, &journal)
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn write_journal(library_root: &Path, journal: &Journal) -> ApplicationRuntimeResult<()> {
    validate_journal(journal)?;
    let destination = journal_path(library_root);
    if destination.exists() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let temporary = library_root.join(format!(
        ".sustain-duplicate-consolidation-journal-{}-{unique}.tmp",
        std::process::id()
    ));
    let result = (|| {
        let temporary = PinnedFilePath::existing_parent(&temporary)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let mut file = temporary
            .create_new_file()
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let temporary_capability = regular_file_capability_from_file(
            file.try_clone()
                .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?,
        )
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        writeln!(file, "{JOURNAL_HEADER}")
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        writeln!(
            file,
            "survivor\t{}\t{}",
            journal.survivor_id.get(),
            encode_path(&journal.stage)
        )
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        for entry in &journal.entries {
            writeln!(
                file,
                "entry\t{}\t{}\t{}\t{}\t{}",
                entry.track_id.get(),
                entry.source_identity.device,
                entry.source_identity.inode,
                encode_path(&entry.source),
                encode_path(&entry.backup),
            )
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        }
        file.flush()
            .and_then(|()| file.sync_all())
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        let destination = PinnedFilePath::existing_parent(&destination)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
        publish_pinned_file_without_overwrite(&temporary, &destination, &temporary_capability)
            .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
    })();
    if result.is_err() {
        let _ = remove_file_and_sync_parent(&temporary);
    }
    result
}

/*
 * Kept below this point: journal parsing and path encoding. The journal is
 * intentionally plain text so recovery can reject malformed state without
 * guessing.
 */

fn read_journal(library_root: &Path) -> ApplicationRuntimeResult<Journal> {
    let journal = open_regular_file(&journal_path(library_root))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let mut contents = String::new();
    journal
        .try_clone_file()
        .and_then(|mut journal| journal.read_to_string(&mut contents))
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let mut lines = contents.lines();
    if lines.next() != Some(JOURNAL_HEADER) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut survivor_parts = lines
        .next()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?
        .split('\t');
    if survivor_parts.next() != Some("survivor") {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let survivor_id = parse_track_id(survivor_parts.next())?;
    let stage = decode_path(
        survivor_parts
            .next()
            .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
    )?;
    if survivor_parts.next().is_some() {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let mut entries = Vec::new();
    for line in lines {
        let mut parts = line.split('\t');
        if parts.next() != Some("entry") {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
        entries.push(JournalEntry {
            track_id: parse_track_id(parts.next())?,
            source_identity: FileIdentity {
                device: parse_u64(parts.next())?,
                inode: parse_u64(parts.next())?,
            },
            source_capability: None,
            source: decode_path(
                parts
                    .next()
                    .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
            )?,
            backup: decode_path(
                parts
                    .next()
                    .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?,
            )?,
        });
        if parts.next().is_some() {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    let journal = Journal {
        survivor_id,
        stage,
        entries,
    };
    validate_journal(&journal)?;
    Ok(journal)
}

fn validate_journal(journal: &Journal) -> ApplicationRuntimeResult<()> {
    let stage = journal.stage.as_path();
    let recovery_directory = stage
        .parent()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let directory_name = recovery_directory
        .file_name()
        .and_then(|name| name.to_str())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    let recovery_prefix = ".sustain-duplicate-consolidation-";
    if recovery_directory.components().count() != 1
        || !directory_name.starts_with(recovery_prefix)
        || directory_name.len() == recovery_prefix.len()
        || !stage
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("stage.") && name.len() > "stage.".len())
        || journal.entries.len() < 2
    {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }

    let mut track_ids = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for (index, entry) in journal.entries.iter().enumerate() {
        if entry.backup.as_path() != recovery_directory.join(format!("backup-{index}"))
            || entry.source.as_path().starts_with(recovery_directory)
            || !track_ids.insert(entry.track_id)
            || !sources.insert(entry.source.as_path())
        {
            return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
        }
    }
    if !track_ids.contains(&journal.survivor_id) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    Ok(())
}

fn journal_path(library_root: &Path) -> PathBuf {
    library_root.join(JOURNAL_NAME)
}

fn file_size(path: &Path) -> ApplicationRuntimeResult<u64> {
    fs::metadata(path)
        .map(|metadata| metadata.len())
        .map_err(|_| ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn relative_path(path: impl Into<PathBuf>) -> ApplicationRuntimeResult<TrackRelativePath> {
    TrackRelativePath::new(path.into()).ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn parse_track_id(value: Option<&str>) -> ApplicationRuntimeResult<TrackId> {
    value
        .and_then(|value| value.parse().ok())
        .and_then(TrackId::new)
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn parse_u64(value: Option<&str>) -> ApplicationRuntimeResult<u64> {
    value
        .and_then(|value| value.parse().ok())
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)
}

fn encode_path(path: &TrackRelativePath) -> String {
    use std::os::unix::ffi::OsStrExt;
    path.as_path()
        .as_os_str()
        .as_bytes()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn decode_path(value: &str) -> ApplicationRuntimeResult<TrackRelativePath> {
    use std::os::unix::ffi::OsStringExt;
    if !value.len().is_multiple_of(2) {
        return Err(ApplicationRuntimeError::DuplicateConsolidationFailed);
    }
    let bytes = value
        .as_bytes()
        .chunks_exact(2)
        .map(|chunk| Some((hex(chunk[0])? << 4) | hex(chunk[1])?))
        .collect::<Option<Vec<_>>>()
        .ok_or(ApplicationRuntimeError::DuplicateConsolidationFailed)?;
    relative_path(std::ffi::OsString::from_vec(bytes))
}

fn hex(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    #![allow(
        clippy::unwrap_in_result,
        reason = "test doubles use expect to state fixture mutation invariants"
    )]

    use std::{fs, path::Path, time::SystemTime};

    use sustain_domain::{
        Bpm, PlayStatistics, Rating, TrackLocation, TrackMetadata, TrackRelativePath,
    };
    use sustain_library_store::{LibraryStore, SqliteLibraryStore};
    use sustain_metadata::{InitialTags, MetadataError, MetadataResult};

    use super::*;
    use crate::test_waveforms::{analyse, music};

    #[test]
    fn consolidation_verifies_preserved_and_mirrored_analysis_tags() {
        use sustain_domain::{FieldChange, KeyOrigin, MetadataChange, MusicalKey};
        use sustain_metadata::LoftyMetadataService;

        for (has_file_tags, has_library_values, user_typed) in [
            (true, true, false),
            (false, true, false),
            (true, true, true),
            (true, false, false),
        ] {
            let root = test_root("analysis-tag-policy");
            fs::create_dir_all(&root).expect("create root");
            let mut frame = vec![0xFF, 0xFB, 0x90, 0x00];
            frame.resize(417, 0);
            fs::write(root.join("keep.mp3"), frame.repeat(4)).expect("write MP3 frames");
            let source_bpm = has_file_tags.then(|| Bpm::from_whole(88).expect("BPM"));
            let source_key = has_file_tags.then_some(MusicalKey::CMajor);
            let service = LoftyMetadataService;
            service
                .write_metadata(
                    &root.join("keep.mp3"),
                    MetadataChange {
                        title: FieldChange::Set("Old".to_owned()),
                        bpm: source_bpm.map_or(FieldChange::Clear, FieldChange::Set),
                        key: source_key.map_or(FieldChange::Clear, FieldChange::Set),
                        ..MetadataChange::default()
                    },
                )
                .expect("seed original tags");
            fs::copy(root.join("keep.mp3"), root.join("remove.mp3")).expect("copy donor");
            let library_bpm =
                has_library_values.then(|| Bpm::from_hundredths(13_273).expect("fractional BPM"));
            let library_key = has_library_values.then_some(MusicalKey::BbMinor);
            let mut reference = track(2, "remove.mp3", "Chosen");
            reference.metadata.bpm = library_bpm;
            reference.metadata.bpm_set_by_user = user_typed;
            reference.metadata.key = library_key;
            reference.metadata.key_origin = if user_typed {
                KeyOrigin::Typed
            } else {
                KeyOrigin::Measured
            };
            let store = SqliteLibraryStore::open_in_memory().expect("SQLite store");
            store
                .save_track(track(1, "keep.mp3", "Old"))
                .expect("save survivor");
            store.save_track(reference).expect("save donor");

            consolidate_duplicate_tracks(
                &root,
                &store,
                &service,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                    artwork_track_id: track_id(1),
                    rating_track_id: track_id(1),
                },
            )
            .expect("merge must verify the intended tag values");

            let persisted = service
                .read_persisted_tags(&root.join("keep.mp3"))
                .expect("read result");
            let expected_bpm = if has_library_values && !user_typed {
                source_bpm
            } else {
                library_bpm.and_then(|bpm| Bpm::from_whole(bpm.whole()))
            };
            let expected_key = if has_library_values && !user_typed {
                source_key
            } else {
                library_key
            };
            assert_eq!(persisted.metadata.bpm, expected_bpm);
            assert_eq!(persisted.metadata.key, expected_key);
            assert_eq!(persisted.metadata.title.as_deref(), Some("Chosen"));
            let survivor = store
                .track(track_id(1))
                .expect("load survivor")
                .expect("survivor exists");
            assert_eq!(survivor.metadata.bpm, library_bpm);
            assert_eq!(survivor.metadata.key, library_key);
            assert_eq!(store.track(track_id(2)), Ok(None));
            fs::remove_dir_all(root).expect("cleanup");
        }
    }

    #[test]
    fn preserved_analysis_tags_do_not_hide_a_failed_metadata_write() {
        use sustain_domain::{KeyOrigin, MusicalKey};

        let root = test_root("unwritten-metadata");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("donor");
        let store = SqliteLibraryStore::open_in_memory().expect("SQLite store");
        store
            .save_track(track(1, "keep.flac", "Old"))
            .expect("save survivor");
        let mut reference = track(2, "remove.flac", "Chosen");
        reference.metadata.bpm = Bpm::from_hundredths(13_273);
        reference.metadata.key = Some(MusicalKey::BbMinor);
        reference.metadata.key_origin = KeyOrigin::Measured;
        store.save_track(reference).expect("save donor");
        // This service silently ignores writes. Preserved BPM/key values must
        // not weaken verification of a title that should have changed.
        let metadata = FixedMetadata {
            persisted: InitialTags {
                metadata: TrackMetadata {
                    title: Some("Old".to_owned()),
                    bpm: Bpm::from_whole(88),
                    key: Some(MusicalKey::CMajor),
                    ..TrackMetadata::default()
                },
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            },
        };
        assert_eq!(
            consolidate_duplicate_tracks(
                &root,
                &store,
                &metadata,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                    artwork_track_id: track_id(1),
                    rating_track_id: track_id(1),
                },
            ),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert_eq!(
            fs::read(root.join("remove.flac")).expect("donor"),
            b"duplicate"
        );
        assert!(store.track(track_id(2)).expect("load donor").is_some());
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn consolidation_publishes_verified_survivor_then_removes_duplicate() {
        let root = test_root("success");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        let survivor = track(1, "keep.flac", "Old");
        let metadata_reference = track(2, "remove.flac", "Chosen");
        store.save_track(survivor).expect("save survivor");
        store
            .save_track(metadata_reference.clone())
            .expect("save reference");
        let metadata = FixedMetadata {
            persisted: InitialTags {
                metadata: metadata_reference.metadata,
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            },
        };

        let result = consolidate_duplicate_tracks(
            &root,
            &store,
            &metadata,
            &ManagedLibraryFilesystemValidator::default(),
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                artwork_track_id: track_id(2),
                rating_track_id: track_id(1),
            },
        )
        .expect("consolidate");

        assert_eq!(result.survivor_id, track_id(1));
        assert_eq!(result.removed_track_ids, vec![track_id(2)]);
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert!(!root.join("remove.flac").exists());
        assert_eq!(
            store
                .track(track_id(1))
                .expect("load survivor")
                .expect("survivor exists")
                .metadata
                .title
                .as_deref(),
            Some("Chosen")
        );
        assert_eq!(store.track(track_id(2)), Ok(None));
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn rollback_refuses_to_remove_an_unexpected_survivor_replacement() {
        let root = test_root("unexpected");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let survivor = track(1, "keep.flac", "Old");
        let removed = track(2, "remove.flac", "Old");
        let tracks = [&survivor, &removed];
        let journal = plan_journal(&root, survivor.id, &tracks).expect("plan journal");
        write_journal(&root, &journal).expect("write journal");
        for entry in &journal.entries {
            create_link(
                entry.source_capability.as_ref().expect("source capability"),
                &entry.backup.resolve(&root),
            )
            .expect("backup");
        }
        let survivor_entry = survivor_entry(&journal);
        copy_to_stage(
            survivor_entry
                .source_capability
                .as_ref()
                .expect("source capability"),
            &journal.stage.resolve(&root),
        )
        .expect("stage");
        remove_matching_file(
            &survivor_entry.source.resolve(&root),
            survivor_entry
                .source_capability
                .as_ref()
                .expect("source capability"),
        )
        .expect("remove original");
        fs::write(root.join("keep.flac"), b"external replacement").expect("replace externally");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store.save_track(survivor).expect("save survivor");
        store.save_track(removed).expect("save removed");

        assert_eq!(
            recover_duplicate_consolidation_journal(&root, &store),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("replacement survives"),
            b"external replacement"
        );
        assert!(journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// The last window of a committed merge: cleanup has removed the donor
    /// and every recovery file, but not yet the journal. Recovery finishes
    /// the cleanup, and a file that appeared at the donor path since is not
    /// the donor and is kept.
    #[test]
    fn recovery_finishes_a_committed_cleanup_interrupted_before_the_journal_removal() {
        let root = test_root("committed-cleanup");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let survivor = track(1, "keep.flac", "Old");
        let removed = track(2, "remove.flac", "Old");
        let tracks = [&survivor, &removed];
        let journal = plan_journal(&root, survivor.id, &tracks).expect("plan journal");
        write_journal(&root, &journal).expect("write journal");
        fs::remove_file(root.join("remove.flac")).expect("cleanup removed the donor");
        let recovery_directory = journal
            .stage
            .resolve(&root)
            .parent()
            .expect("recovery directory")
            .to_path_buf();
        assert!(!recovery_directory.exists(), "no recovery files were made");
        fs::write(root.join("remove.flac"), b"user's new file").expect("write newcomer");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store.save_track(survivor).expect("save survivor");

        assert_eq!(
            recover_duplicate_consolidation_journal(&root, &store),
            Ok(())
        );
        assert!(!journal_path(&root).exists());
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert_eq!(
            fs::read(root.join("remove.flac")).expect("newcomer survives"),
            b"user's new file"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// A committed merge interrupted before any backup was removed, after
    /// the user put a new file at the donor path: the newcomer is not the
    /// donor, so recovery keeps it and still finishes the cleanup.
    #[test]
    fn recovery_keeps_a_newcomer_at_a_donor_path_while_its_backup_survives() {
        let root = test_root("committed-newcomer");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let survivor = track(1, "keep.flac", "Old");
        let removed = track(2, "remove.flac", "Old");
        let tracks = [&survivor, &removed];
        let journal = plan_journal(&root, survivor.id, &tracks).expect("plan journal");
        write_journal(&root, &journal).expect("write journal");
        let donor_backup = journal
            .entries
            .iter()
            .find(|entry| entry.track_id == removed.id)
            .expect("donor entry")
            .backup
            .resolve(&root);
        fs::create_dir_all(donor_backup.parent().expect("recovery directory"))
            .expect("create recovery directory");
        fs::hard_link(root.join("remove.flac"), &donor_backup).expect("back up the donor");
        fs::remove_file(root.join("remove.flac")).expect("the merge removed the donor");
        fs::write(root.join("remove.flac"), b"user's new file").expect("write newcomer");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store.save_track(survivor).expect("save survivor");

        assert_eq!(
            recover_duplicate_consolidation_journal(&root, &store),
            Ok(())
        );
        assert!(!journal_path(&root).exists());
        assert!(!donor_backup.exists(), "the backup is cleaned up");
        assert_eq!(
            fs::read(root.join("remove.flac")).expect("newcomer survives"),
            b"user's new file"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    /// A committed merge whose donor folder has since been replaced by a
    /// file: no donor can exist under it, so recovery finishes.
    #[test]
    fn recovery_finishes_when_a_donor_folder_became_a_file() {
        let root = test_root("committed-donor-folder");
        fs::create_dir_all(root.join("Album")).expect("create album folder");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("Album/remove.flac"), b"duplicate").expect("write duplicate");
        let survivor = track(1, "keep.flac", "Old");
        let removed = track(2, "Album/remove.flac", "Old");
        let tracks = [&survivor, &removed];
        let journal = plan_journal(&root, survivor.id, &tracks).expect("plan journal");
        write_journal(&root, &journal).expect("write journal");
        let donor_backup = journal
            .entries
            .iter()
            .find(|entry| entry.track_id == removed.id)
            .expect("donor entry")
            .backup
            .resolve(&root);
        fs::create_dir_all(donor_backup.parent().expect("recovery directory"))
            .expect("create recovery directory");
        fs::hard_link(root.join("Album/remove.flac"), &donor_backup).expect("back up the donor");
        fs::remove_dir_all(root.join("Album")).expect("the merge removed the donor");
        fs::write(root.join("Album"), b"user's file").expect("a file takes the folder's name");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store.save_track(survivor).expect("save survivor");

        assert_eq!(
            recover_duplicate_consolidation_journal(&root, &store),
            Ok(())
        );
        assert!(!journal_path(&root).exists());
        assert!(!donor_backup.exists(), "the backup is cleaned up");
        assert_eq!(
            fs::read(root.join("Album")).expect("user's file"),
            b"user's file"
        );
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn staged_write_failure_leaves_original_files_unchanged() {
        let root = test_root("staged-write-failure");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        fs::write(root.join("remove.flac"), b"duplicate").expect("write duplicate");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store
            .save_track(track(1, "keep.flac", "Old"))
            .expect("save survivor");
        store
            .save_track(track(2, "remove.flac", "Chosen"))
            .expect("save reference");

        assert_eq!(
            consolidate_duplicate_tracks(
                &root,
                &store,
                &FailingMetadata,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(2)),
                    artwork_track_id: track_id(2),
                    rating_track_id: track_id(1),
                },
            ),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );

        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert_eq!(
            fs::read(root.join("remove.flac")).expect("duplicate"),
            b"duplicate"
        );
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn a_selected_file_missing_on_disk_is_rejected_before_any_change() {
        let root = test_root("missing-on-disk");
        fs::create_dir_all(&root).expect("create root");
        // The survivor exists; the duplicate's row claims it is present
        // (`is_missing` was never set) but its file is gone from disk (#126).
        fs::write(root.join("keep.flac"), b"audio").expect("write survivor");
        let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
        store
            .save_track(track(1, "keep.flac", "Old"))
            .expect("save survivor");
        store
            .save_track(track(2, "gone.flac", "Chosen"))
            .expect("save vanished duplicate");

        assert_eq!(
            consolidate_duplicate_tracks(
                &root,
                &store,
                &FailingMetadata,
                &ManagedLibraryFilesystemValidator::default(),
                &DuplicateConsolidationRequest {
                    track_ids: vec![track_id(1), track_id(2)],
                    audio_track_id: track_id(1),
                    metadata: sustain_domain::DuplicateMetadataSelection::from_track(track_id(1)),
                    artwork_track_id: track_id(1),
                    rating_track_id: track_id(1),
                },
            ),
            Err(ApplicationRuntimeError::DuplicateConsolidationSourceMissing)
        );

        // Nothing was staged, linked, or committed: the survivor is untouched,
        // both rows remain, and no journal was published.
        assert_eq!(
            fs::read(root.join("keep.flac")).expect("survivor"),
            b"audio"
        );
        assert!(store.track(track_id(1)).expect("load survivor").is_some());
        assert!(store.track(track_id(2)).expect("load duplicate").is_some());
        assert!(!journal_path(&root).exists());
        fs::remove_dir_all(root).expect("cleanup");
    }

    #[test]
    fn journal_validation_rejects_a_stage_outside_the_recovery_directory() {
        let journal = Journal {
            survivor_id: track_id(1),
            stage: relative_path("victim.flac").expect("stage"),
            entries: vec![
                JournalEntry {
                    track_id: track_id(1),
                    source_identity: FileIdentity {
                        device: 1,
                        inode: 1,
                    },
                    source_capability: None,
                    source: relative_path("keep.flac").expect("source"),
                    backup: relative_path(".sustain-duplicate-consolidation-1/backup-0")
                        .expect("backup"),
                },
                JournalEntry {
                    track_id: track_id(2),
                    source_identity: FileIdentity {
                        device: 1,
                        inode: 2,
                    },
                    source_capability: None,
                    source: relative_path("remove.flac").expect("source"),
                    backup: relative_path(".sustain-duplicate-consolidation-1/backup-1")
                        .expect("backup"),
                },
            ],
        };

        assert_eq!(
            validate_journal(&journal),
            Err(ApplicationRuntimeError::DuplicateConsolidationFailed)
        );
    }

    /// A library of `pairs` duplicate pairs: tracks `2n-1` and `2n` share
    /// title `n`, a file on disk and an analysed recording.
    struct VerifiedLibrary {
        root: PathBuf,
        store: SqliteLibraryStore,
        metadata: FixedMetadata,
        cache: Arc<Mutex<RecordingIdentityCache>>,
        cancellation: AtomicBool,
    }

    impl VerifiedLibrary {
        fn new(label: &str, pairs: i64) -> Self {
            let root = test_root(label);
            fs::create_dir_all(&root).expect("create root");
            let store = SqliteLibraryStore::open_in_memory().expect("in-memory SQLite store");
            for id in 1..=pairs * 2 {
                let path = format!("{id}.flac");
                fs::write(root.join(&path), format!("audio {id}")).expect("write track");
                store
                    .save_track(track(id, &path, "Title"))
                    .expect("save track");
                let recording = u64::try_from((id + 1) / 2).expect("positive id");
                analyse(&store, track_id(id), music(recording, 200), 100);
            }
            Self {
                root,
                store,
                metadata: FixedMetadata {
                    persisted: InitialTags {
                        metadata: track(1, "1.flac", "Title").metadata,
                        rating: Rating::unrated(),
                        has_embedded_artwork: false,
                    },
                },
                cache: Arc::default(),
                cancellation: AtomicBool::new(false),
            }
        }

        /// The merges a preview of the library now would offer.
        fn preview(&self, pairs: i64) -> Vec<VerifiedDuplicateMerge> {
            (1..=pairs)
                .map(|pair| {
                    let tracks = [pair * 2 - 1, pair * 2].map(|id| {
                        self.store
                            .track(track_id(id))
                            .expect("load")
                            .expect("track")
                    });
                    VerifiedDuplicateMerge::new(tracks.to_vec()).expect("merge")
                })
                .collect()
        }

        fn run(
            &self,
            merges: &[VerifiedDuplicateMerge],
            progress: impl FnMut(usize),
        ) -> VerifiedDuplicateBatchSummary {
            consolidate_verified_duplicates(
                &VerifiedDuplicateBatchContext {
                    library_root: &self.root,
                    store: &self.store,
                    metadata_service: &self.metadata,
                    filesystem_validator: &ManagedLibraryFilesystemValidator::default(),
                    recording_identities: &self.cache,
                    cancellation: &self.cancellation,
                },
                merges,
                progress,
            )
        }

        fn present(&self, id: i64) -> bool {
            self.root.join(format!("{id}.flac")).exists()
        }
    }

    impl Drop for VerifiedLibrary {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn a_verified_batch_merges_every_group_and_reports_progress() {
        let library = VerifiedLibrary::new("verified_batch", 2);
        let merges = library.preview(2);
        let mut progress = Vec::new();

        let summary = library.run(&merges, |done| progress.push(done));

        assert_eq!(progress, vec![1, 2]);
        assert_eq!(summary.merges.len(), 2);
        assert_eq!(summary.removed_tracks(), 2);
        assert_eq!(
            summary
                .merges
                .iter()
                .map(|merge| merge.survivor_id)
                .collect::<Vec<_>>(),
            vec![track_id(1), track_id(3)]
        );
        assert!(library.present(1) && library.present(3));
        assert!(!library.present(2) && !library.present(4));
    }

    #[test]
    fn a_group_changed_since_the_preview_is_left_alone() {
        let library = VerifiedLibrary::new("verified_changed", 1);
        let merges = library.preview(1);
        library
            .store
            .update_track_rating(track_id(2), Rating::new(5).expect("rating"))
            .expect("rate after the preview");

        let summary = library.run(&merges, |_| {});

        assert_eq!(summary.changed_groups, 1);
        assert!(summary.merges.is_empty());
        assert!(library.present(1) && library.present(2));
    }

    #[test]
    fn a_play_since_the_preview_does_not_hold_a_group_back() {
        let library = VerifiedLibrary::new("verified_played", 1);
        let merges = library.preview(1);
        let statistics = PlayStatistics {
            play_count: 3,
            ..PlayStatistics::default()
        };
        library
            .store
            .update_track_statistics(track_id(2), &statistics)
            .expect("play after the preview");

        let summary = library.run(&merges, |_| {});

        assert_eq!(summary.merges.len(), 1);
    }

    #[test]
    fn a_group_whose_audio_no_longer_verifies_is_left_alone() {
        let library = VerifiedLibrary::new("verified_reanalysed", 1);
        let merges = library.preview(1);
        analyse(&library.store, track_id(2), music(99, 200), 200);

        let summary = library.run(&merges, |_| {});

        assert_eq!(summary.unverified_groups, 1);
        assert!(summary.merges.is_empty());
        assert!(library.present(1) && library.present(2));
    }

    #[test]
    fn a_group_the_merger_refuses_counts_as_failed_and_the_batch_goes_on() {
        let library = VerifiedLibrary::new("verified_failed", 2);
        let merges = library.preview(2);
        fs::remove_file(library.root.join("2.flac")).expect("remove file");

        let summary = library.run(&merges, |_| {});

        assert_eq!(summary.failed_groups, 1);
        assert_eq!(summary.merges.len(), 1);
        assert_eq!(summary.merges[0].survivor_id, track_id(3));
        assert!(library.present(1));
    }

    #[test]
    fn cancelling_stops_before_the_next_merge() {
        let library = VerifiedLibrary::new("verified_cancelled", 3);
        let merges = library.preview(3);

        let summary = library.run(&merges, |_| {
            library.cancellation.store(true, Ordering::SeqCst);
        });

        assert_eq!(summary.merges.len(), 1);
        assert_eq!(summary.cancelled_groups, 2);
        assert!(library.present(3) && library.present(4));
    }

    struct FixedMetadata {
        persisted: InitialTags,
    }

    impl MetadataService for FixedMetadata {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(self.persisted.clone())
        }

        fn read_persisted_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(self.persisted.clone())
        }

        fn write_metadata(
            &self,
            _path: &Path,
            _change: sustain_domain::MetadataChange,
        ) -> MetadataResult<()> {
            Ok(())
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    struct FailingMetadata;

    impl MetadataService for FailingMetadata {
        fn read_initial_tags(&self, _path: &Path) -> MetadataResult<InitialTags> {
            Ok(InitialTags {
                metadata: TrackMetadata::default(),
                rating: Rating::unrated(),
                has_embedded_artwork: false,
            })
        }

        fn write_metadata(
            &self,
            path: &Path,
            _change: sustain_domain::MetadataChange,
        ) -> MetadataResult<()> {
            fs::write(path, b"partially rewritten").expect("mutate staged file");
            Err(MetadataError::WriteFailed)
        }

        fn write_rating(&self, _path: &Path, _rating: Rating) -> MetadataResult<()> {
            Ok(())
        }

        fn read_artwork(&self, _path: &Path) -> MetadataResult<Option<Vec<u8>>> {
            Ok(None)
        }

        fn write_artwork(&self, _path: &Path, _artwork: Option<Vec<u8>>) -> MetadataResult<()> {
            Ok(())
        }
    }

    fn track(id: i64, path: &str, title: &str) -> Track {
        Track {
            id: track_id(id),
            location: TrackLocation::available(
                TrackRelativePath::new(path).expect("relative path"),
            ),
            metadata: TrackMetadata {
                title: Some(title.to_owned()),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: None,
            has_embedded_artwork: None,
            file_modified_at: None,
        }
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("track id")
    }

    fn test_root(label: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "sustain_duplicate_consolidation_{label}_{}",
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ))
    }
}
