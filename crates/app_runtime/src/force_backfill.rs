// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Force-backfill: rewrite every library track's file tags from the
//! authoritative SQLite values.
//!
//! SQLite is the source of truth for every editable field; the per-edit
//! path mirrors a *changed* field back to the file as a courtesy. A
//! library consolidated from external sources (e.g. Rhythmbox / iTunes XML
//! history) can hold rich SQLite metadata whose files were never written.
//! This one-shot pass rewrites every track's tags from SQLite, reusing the
//! exact mirror primitives the per-edit path uses — so a POPM rating
//! preserves any foreign `play_counter`, the same standard tag frames are
//! touched, and listening statistics are never written to files. Driven by
//! the hidden `sustain --force-backfill` CLI command (#143).

use crate::metadata_writer::full_metadata_mirror;
use crate::{ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, Track};

/// What happened to one track during a force-backfill pass.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ForceBackfillOutcome {
    /// Metadata and rating were rewritten to the file.
    Written,
    /// The track's file is recorded as missing on disk, so it was skipped.
    SkippedMissing,
    /// The write failed; the string carries the reason.
    Failed(String),
}

/// Per-track progress handed to the caller's reporter after each track is
/// processed, in library order.
pub struct ForceBackfillProgress<'a> {
    /// 1-based position of this track in the pass.
    pub done: usize,
    /// Total number of tracks in the pass.
    pub total: usize,
    /// The track just processed.
    pub track: &'a Track,
    /// The outcome of this track's write.
    pub outcome: ForceBackfillOutcome,
}

/// Tally returned when the pass completes.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ForceBackfillSummary {
    pub total: usize,
    pub written: usize,
    pub skipped_missing: usize,
    pub failed: usize,
}

impl ApplicationRuntime {
    /// Rewrite every library track's file tags from the authoritative
    /// SQLite metadata and rating.
    ///
    /// Synchronous: every file is written before returning, so the caller
    /// can report accurate progress. `report` is invoked once per track, in
    /// library order, after the write is attempted. A per-track failure is
    /// recorded and the pass continues; the method only returns `Err` when
    /// the library services or library path are unavailable (i.e. nothing
    /// could be written at all). Requires the library to be hydrated
    /// (`set_library_services`, not the deferred variant).
    pub fn force_backfill_tags(
        &self,
        mut report: impl FnMut(ForceBackfillProgress<'_>),
    ) -> ApplicationRuntimeResult<ForceBackfillSummary> {
        let metadata_service = self
            .metadata_service
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let library_store = self
            .library_store
            .as_ref()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let root = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();

        let total = self.library_tracks.len();
        let mut summary = ForceBackfillSummary {
            total,
            ..ForceBackfillSummary::default()
        };
        for (index, track) in self.library_tracks.iter().enumerate() {
            let outcome = if track.location.is_missing() {
                summary.skipped_missing += 1;
                ForceBackfillOutcome::SkippedMissing
            } else {
                let path = root.join(track.location.relative_path.as_path());
                match metadata_service
                    .write_metadata(&path, full_metadata_mirror(&track.metadata))
                    .and_then(|()| metadata_service.write_rating(&path, track.rating))
                {
                    Ok(()) => {
                        // The file bytes changed, so the cached source
                        // fingerprint no longer describes it — invalidate it
                        // exactly as the per-edit mirror does. A failure here
                        // does not fail the backfill; the next scan recomputes.
                        let _ = library_store.invalidate_source_fingerprint(track.id);
                        summary.written += 1;
                        ForceBackfillOutcome::Written
                    }
                    Err(error) => {
                        summary.failed += 1;
                        ForceBackfillOutcome::Failed(format!("{error:?}"))
                    }
                }
            };
            report(ForceBackfillProgress {
                done: index + 1,
                total,
                track,
                outcome,
            });
        }
        Ok(summary)
    }
}
