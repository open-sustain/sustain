// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The CD-import worker: the per-track extraction → tag → publish → row
//! state machine, run on a background thread.
//!
//! Each selected track is, in order: re-checked for a disc swap, encoded to
//! an unpublished staging file beneath the library root, tagged (metadata +
//! artwork) through the canonical [`MetadataService`](crate::MetadataService), read back to capture
//! its real technical fields, then published with the no-overwrite managed
//! move and committed as a durable [`Track`] row — journaled across the
//! filesystem/SQLite boundary. A failure, cancellation, or disc swap stops
//! the run and removes only the in-progress track's unpublished staging;
//! every already-committed track and file stays imported.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::SystemTime;

use sustain_cd_import::{EncodeProgress, EncodeRequest};
use sustain_domain::{
    FieldChange, ManagedTrackPathInput, ManagedTrackPathPlanner, MetadataChange, PlayStatistics,
    Track, TrackLocation, TrackMetadata, TrackRelativePath,
};

use super::{CdImportProgress, CdImportResult, CdImportSummary, CdImportTask};
use crate::managed_library::file_ops::{
    move_file_without_copy_or_overwrite, remove_file_and_sync_parent,
};
use crate::{ApplicationRuntimeError, ApplicationRuntimeResult, cd_import::journal, library_scan};

pub fn run_cd_import_task(
    task: CdImportTask,
    progress: impl FnMut(CdImportProgress),
) -> ApplicationRuntimeResult<CdImportResult> {
    let mut context = WorkerContext {
        task,
        progress: Box::new(progress),
    };
    context.run()
}

struct WorkerContext<'a> {
    task: CdImportTask,
    progress: Box<dyn FnMut(CdImportProgress) + 'a>,
}

impl WorkerContext<'_> {
    fn run(&mut self) -> ApplicationRuntimeResult<CdImportResult> {
        // CD imports always own files beneath the library path; confirm the
        // managed filesystem is usable before touching the disc.
        self.task
            .managed_library_filesystem_validator
            .validate(&self.task.library_path)
            .map_err(ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported)?;
        // Finish or roll back any transition an earlier crash interrupted
        // before importing anything new.
        journal::recover(&self.task.library_path, self.task.library_store.as_ref())?;

        let total = self.task.plans.len();
        let mut occupied: BTreeSet<TrackRelativePath> = self
            .task
            .existing_tracks
            .iter()
            .map(|track| track.location.relative_path.clone())
            .collect();
        let planner = ManagedTrackPathPlanner::default();
        let mut next_track_id = library_scan::next_track_id(&self.task.existing_tracks)?;
        let mut imported: Vec<Track> = Vec::new();
        let extension = self.task.profile.file_extension();
        let source_hint = PathBuf::from(format!("cdtrack.{extension}"));

        // `next_track_id` is the SQLite id counter, not a loop counter: it
        // advances only when a track is fully committed, so an early return
        // on a failed/cancelled track must leave it where it was. That makes
        // it deliberately misaligned with the enumerate index.
        #[allow(clippy::explicit_counter_loop)]
        for (index, plan) in self.task.plans.iter().enumerate() {
            if self.cancelled() {
                return Ok(self.result(imported, total, true, None));
            }

            // Re-probe and compare identity before reading each track, so a
            // disc swap stops the rip rather than tagging the wrong audio.
            if !self.disc_still_present() {
                return Ok(self.result(imported, total, false, Some(disc_changed_message())));
            }

            let destination_relative = match plan_destination(
                &planner,
                &mut occupied,
                &self.task.library_path,
                &plan.metadata,
                &source_hint,
            ) {
                Ok(relative) => relative,
                Err(error) => {
                    return Ok(self.result(imported, total, false, Some(error_text(&error))));
                }
            };
            let destination = destination_relative.resolve(&self.task.library_path);
            let staging = staging_path(&self.task.library_path, extension, index);

            // --- encode to staging ---
            // Detach the encode call from `self` (clone the cheap handles)
            // so the progress closure can hold `&mut self.progress` without
            // colliding with a borrow of `self.task`.
            let completed = imported.len();
            let encoder = self.task.encoder.clone();
            let cancellation = self.task.cancellation_requested.clone();
            let request = EncodeRequest {
                device_path: self.task.snapshot.device_path.clone(),
                track: plan.track_number,
                profile: self.task.profile,
                destination: staging.clone(),
            };
            let report = &mut self.progress;
            let encode_result = encoder.encode_track(
                &request,
                &mut |EncodeProgress { percent }| {
                    report(CdImportProgress {
                        completed_tracks: completed,
                        total_tracks: total,
                        current_track_percent: percent,
                    });
                },
                &|| cancellation.load(Ordering::SeqCst),
            );
            if let Err(error) = encode_result {
                let _ = remove_file_and_sync_parent(&staging);
                return match error {
                    sustain_cd_import::CdImportError::Cancelled => {
                        Ok(self.result(imported, total, true, None))
                    }
                    other => Ok(self.result(imported, total, false, Some(other.to_string()))),
                };
            }

            // --- tag the staged file + embed artwork through the canonical writer ---
            if let Err(error) = self.tag_staged_file(&staging, &plan.metadata) {
                let _ = remove_file_and_sync_parent(&staging);
                return Ok(self.result(imported, total, false, Some(error_text(&error))));
            }

            // --- read the finished file back for authoritative technical fields ---
            let (audio_properties, has_embedded_artwork) =
                match self.task.metadata_service.read_initial_tags(&staging) {
                    Ok(tags) => (tags.metadata.audio_properties(), tags.has_embedded_artwork),
                    Err(_) => {
                        let _ = remove_file_and_sync_parent(&staging);
                        return Ok(self.result(
                            imported,
                            total,
                            false,
                            Some(error_text(&ApplicationRuntimeError::CdImportFailed)),
                        ));
                    }
                };
            let file_size_bytes = fs::metadata(&staging).map(|metadata| metadata.len()).ok();

            // --- journaled publish + durable row ---
            let Some(track_id) = sustain_domain::TrackId::new(next_track_id) else {
                let _ = remove_file_and_sync_parent(&staging);
                return Ok(self.result(
                    imported,
                    total,
                    false,
                    Some(error_text(&ApplicationRuntimeError::LibraryStoreFailed)),
                ));
            };

            let mut metadata = plan.metadata.clone();
            metadata.replace_audio_properties(audio_properties);
            let track = Track {
                id: track_id,
                location: TrackLocation::available(destination_relative.clone()),
                metadata,
                rating: sustain_domain::Rating::unrated(),
                statistics: PlayStatistics {
                    date_added_at: Some(SystemTime::now()),
                    ..PlayStatistics::default()
                },
                file_size_bytes,
                has_embedded_artwork: Some(has_embedded_artwork),
                file_modified_at: None,
            };

            if let Err(message) = self.publish_and_commit(&staging, &destination, &track) {
                return Ok(self.result(imported, total, false, Some(message)));
            }

            next_track_id += 1;
            imported.push(track);
            (self.progress)(CdImportProgress {
                completed_tracks: imported.len(),
                total_tracks: total,
                current_track_percent: 100,
            });
        }

        Ok(self.result(imported, total, false, None))
    }

    /// Publish the staged file with the no-overwrite managed move, then
    /// commit and durably flush the row — recording the transition in the
    /// crash-recovery journal across the filesystem/SQLite boundary. Any
    /// failure rolls back this track's side effects only.
    fn publish_and_commit(
        &self,
        staging: &Path,
        destination: &Path,
        track: &Track,
    ) -> Result<(), String> {
        journal::write_pending(&self.task.library_path, destination, staging)
            .map_err(|error| error_text(&error))?;

        if move_file_without_copy_or_overwrite(staging, destination).is_err() {
            let _ = remove_file_and_sync_parent(staging);
            let _ = journal::clear_pending(&self.task.library_path);
            return Err(error_text(&ApplicationRuntimeError::CdImportFailed));
        }

        if self.task.library_store.save_track(track.clone()).is_err()
            || self.task.library_store.flush_durable().is_err()
        {
            // The file is published but the row is not durable — remove the
            // file and any half-written row so disk and database agree.
            let _ = self.task.library_store.delete_track(track.id);
            let _ = remove_file_and_sync_parent(destination);
            let _ = journal::clear_pending(&self.task.library_path);
            return Err(error_text(&ApplicationRuntimeError::LibraryStoreFailed));
        }

        // Committed and durable. A failure to clear the journal is
        // self-healing: recovery will see the row and keep the file.
        let _ = journal::clear_pending(&self.task.library_path);
        Ok(())
    }

    fn tag_staged_file(
        &self,
        staging: &Path,
        metadata: &TrackMetadata,
    ) -> ApplicationRuntimeResult<()> {
        self.task
            .metadata_service
            .write_metadata(staging, full_set_change(metadata))
            .map_err(|_| ApplicationRuntimeError::MetadataWriteFailed)?;
        if let Some(cover) = self.task.cover.clone() {
            self.task
                .metadata_service
                .write_artwork(staging, Some(cover))
                .map_err(|_| ApplicationRuntimeError::MetadataWriteFailed)?;
        }
        Ok(())
    }

    fn disc_still_present(&self) -> bool {
        self.task
            .probe
            .probe(&self.task.snapshot.device_path)
            .is_some_and(|snapshot| snapshot.is_same_disc(&self.task.expected_identity))
    }

    fn cancelled(&self) -> bool {
        self.task.cancellation_requested.load(Ordering::SeqCst)
    }

    fn result(
        &self,
        imported: Vec<Track>,
        requested: usize,
        cancelled: bool,
        failure: Option<String>,
    ) -> CdImportResult {
        CdImportResult {
            summary: CdImportSummary {
                requested_tracks: requested,
                imported_tracks: imported.len(),
                cancelled,
                failure,
            },
            tracks: imported,
        }
    }
}

/// Plan a free canonical destination for a ripped track. Unlike library
/// import there is no content-hash dedup — a repeat rip is a deliberate new
/// import — but the canonical path is never allowed to overwrite an existing
/// file: an occupied name (reserved in this run or present on disk) bumps to
/// the next numbered variant.
fn plan_destination(
    planner: &ManagedTrackPathPlanner,
    occupied: &mut BTreeSet<TrackRelativePath>,
    library_path: &Path,
    metadata: &TrackMetadata,
    source_hint: &Path,
) -> ApplicationRuntimeResult<TrackRelativePath> {
    for _attempt in 0..10_000 {
        let plan = planner
            .plan(
                ManagedTrackPathInput {
                    metadata,
                    source_path: source_hint,
                },
                occupied,
            )
            .map_err(|_| ApplicationRuntimeError::CdImportFailed)?;
        if library_path.join(plan.relative_path.as_path()).exists() {
            occupied.insert(plan.relative_path);
            continue;
        }
        occupied.insert(plan.relative_path.clone());
        return Ok(plan.relative_path);
    }
    Err(ApplicationRuntimeError::CdImportFailed)
}

fn staging_path(library_path: &Path, extension: &str, index: usize) -> PathBuf {
    library_path.join(format!(
        ".sustain-cd-rip-{}-{index}.{extension}",
        std::process::id()
    ))
}

fn full_set_change(metadata: &TrackMetadata) -> MetadataChange {
    fn set<T: Clone>(value: &Option<T>) -> FieldChange<T> {
        match value {
            Some(value) => FieldChange::Set(value.clone()),
            None => FieldChange::Unchanged,
        }
    }
    MetadataChange {
        title: set(&metadata.title),
        artist: set(&metadata.artist),
        album: set(&metadata.album),
        album_artist: set(&metadata.album_artist),
        composer: set(&metadata.composer),
        grouping: set(&metadata.grouping),
        genre: set(&metadata.genre),
        track_number: set(&metadata.track_number),
        track_total: set(&metadata.track_total),
        disc_number: set(&metadata.disc_number),
        disc_total: set(&metadata.disc_total),
        year: set(&metadata.year),
        compilation: set(&metadata.compilation),
        bpm: set(&metadata.bpm),
        key: set(&metadata.key),
        comments: set(&metadata.comments),
        lyrics: set(&metadata.lyrics),
    }
}

fn disc_changed_message() -> String {
    sustain_cd_import::CdImportError::DiscChanged.to_string()
}

fn error_text(error: &ApplicationRuntimeError) -> String {
    crate::runtime_error_text(error).to_owned()
}
