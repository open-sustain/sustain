// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! CD import: composing the optical backend with the library to rip an
//! inserted audio disc into owned, tagged library files.
//!
//! The runtime owns three read-only async surfaces — optical discovery,
//! MusicBrainz disc-id release lookup, and front-cover fetch — and one
//! mutating task that follows the same prepare / background worker /
//! apply-or-fail shape as library import. Discovery and lookup never claim
//! the library-mutation slot; the rip itself does, via
//! [`crate::BackgroundTaskStatus::CdImportRunning`]. Generation counters let
//! a stale probe or lookup result be discarded after a disc is ejected or
//! replaced.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::thread;

use sustain_cd_import::{CdEncoder, DiscIdentity, OpticalProbe, TocSnapshot};
use sustain_domain::{CdEncodingProfile, Track, TrackMetadata};
use sustain_library_store::LibraryStore;
use sustain_metadata::MetadataService;
use sustain_metadata_remote::DiscRelease;

use crate::managed_library::ManagedLibraryFilesystemValidator;
use crate::{
    ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult, NotificationCategory,
    NotificationSeverity, notifications,
};

mod journal;
mod plan;
mod task;
#[cfg(test)]
mod tests;

pub use task::run_cd_import_task;

/// A user request to rip selected tracks of an inserted disc.
pub struct CdImportRequest {
    /// The disc snapshot the panel was built from.
    pub snapshot: TocSnapshot,
    /// Physical track numbers the user ticked.
    pub selected_tracks: Vec<u32>,
    /// The chosen MusicBrainz release, or `None` for the fallback identity.
    pub release: Option<DiscRelease>,
    /// Validated front-cover bytes to embed in every track, if any.
    pub cover: Option<Vec<u8>>,
    /// Per-track title/artist the user typed in the CD page, keyed by
    /// physical track number. For discs MusicBrainz could not identify (or
    /// got wrong), these win over the looked-up / generated values; blank
    /// fields fall back to those.
    pub overrides: BTreeMap<u32, CdTrackOverride>,
}

/// User-typed overrides for one track's editable fields. An empty (or
/// whitespace-only) field is treated as "no override".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CdTrackOverride {
    pub title: Option<String>,
    pub artist: Option<String>,
}

/// A prepared, slot-claiming CD import, handed to [`run_cd_import_task`] on a
/// worker thread. Opaque to callers — built only by
/// [`ApplicationRuntime::prepare_cd_import`].
pub struct CdImportTask {
    pub(super) library_path: PathBuf,
    pub(super) profile: CdEncodingProfile,
    pub(super) snapshot: TocSnapshot,
    pub(super) expected_identity: DiscIdentity,
    pub(super) plans: Vec<CdTrackPlan>,
    pub(super) cover: Option<Vec<u8>>,
    pub(super) existing_tracks: Vec<Track>,
    pub(super) library_store: Arc<dyn LibraryStore>,
    pub(super) metadata_service: Arc<dyn MetadataService>,
    pub(super) probe: Arc<dyn OpticalProbe>,
    pub(super) encoder: Arc<dyn CdEncoder>,
    pub(super) managed_library_filesystem_validator: ManagedLibraryFilesystemValidator,
    pub(super) cancellation_requested: Arc<AtomicBool>,
}

pub(super) struct CdTrackPlan {
    pub(super) track_number: u32,
    pub(super) metadata: TrackMetadata,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CdImportProgress {
    pub completed_tracks: usize,
    pub total_tracks: usize,
    pub current_track_percent: u8,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CdImportResult {
    pub tracks: Vec<Track>,
    pub summary: CdImportSummary,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct CdImportSummary {
    pub requested_tracks: usize,
    pub imported_tracks: usize,
    pub cancelled: bool,
    /// `Some` when a track failed or the disc changed mid-rip; earlier
    /// completed tracks are still imported.
    pub failure: Option<String>,
}

/// A completed optical-discovery pass, tagged with the generation it was
/// kicked for so a result superseded by an eject/insert is discarded.
#[derive(Clone, Debug)]
pub struct OpticalDiscoveryResult {
    pub generation: u64,
    pub discs: Vec<TocSnapshot>,
}

/// An async disc-metadata result. Both variants carry the generation of the
/// lookup that produced them; the UI drops any whose generation is stale.
#[derive(Clone, Debug)]
pub enum CdLookupEvent {
    Releases {
        generation: u64,
        disc_id: String,
        releases: Vec<DiscRelease>,
        /// True when the lookup itself failed (network/rate-limit), as
        /// opposed to simply finding no match.
        failed: bool,
    },
    Cover {
        generation: u64,
        cover: Option<Vec<u8>>,
    },
}

impl ApplicationRuntime {
    /// Install the optical probe and encoder. The `app` composition root
    /// builds the concrete [`sustain_cd_import::SystemOpticalProbe`] /
    /// [`sustain_cd_import::GStreamerCdEncoder`]; tests inject fakes.
    pub fn set_cd_backend(&mut self, probe: Arc<dyn OpticalProbe>, encoder: Arc<dyn CdEncoder>) {
        self.cd_probe = Some(probe);
        self.cd_encoder = Some(encoder);
    }

    /// Whether a CD backend is installed at all (build-time condition).
    pub fn cd_backend_ready(&self) -> bool {
        self.cd_probe.is_some() && self.cd_encoder.is_some()
    }

    /// The precise reason CD import cannot start right now, or `None` when
    /// it can. Used by the UI to gate the Import button and report a precise
    /// missing-plugin message. Probes the encoder's GStreamer elements for
    /// the currently configured profile.
    pub fn cd_import_unavailable_reason(&self) -> Option<String> {
        let Some(encoder) = self.cd_encoder.as_ref() else {
            return Some(
                notifications::runtime_error_text(&ApplicationRuntimeError::CdBackendUnavailable)
                    .to_owned(),
            );
        };
        encoder
            .ensure_available(self.settings.encoding.cd_profile)
            .err()
            .map(|error| error.to_string())
    }

    // --- Discovery (read-only, never claims the library-mutation slot) ---

    /// Kick an off-main-thread probe of every candidate optical drive. The
    /// cheap candidate enumeration runs here; the slow per-disc TOC read is
    /// handed to a one-shot worker whose result lands on
    /// [`Self::optical_discovery_receiver`].
    pub fn refresh_optical_discs(&mut self) {
        let Some(probe) = self.cd_probe.clone() else {
            self.optical_discs.clear();
            return;
        };
        self.optical_discovery_generation = self.optical_discovery_generation.wrapping_add(1);
        let generation = self.optical_discovery_generation;
        let candidates = probe.candidate_devices();
        if candidates.is_empty() {
            // No drives: clear synchronously so an ejected/removed drive
            // disappears without waiting on a worker.
            self.optical_discs.clear();
            return;
        }
        let sink = self.optical_discovery_sink.clone();
        let _ = thread::Builder::new()
            .name("sustain-cd-discovery".to_owned())
            .spawn(move || {
                let discs = candidates
                    .iter()
                    .filter_map(|device| probe.probe(device))
                    .collect();
                let _ = sink.send_blocking(OpticalDiscoveryResult { generation, discs });
            });
    }

    /// Adopt a discovery result if it is still current and actually changed
    /// the set. Returns `true` when the cached discs changed.
    pub fn apply_optical_discovery(&mut self, result: OpticalDiscoveryResult) -> bool {
        if result.generation != self.optical_discovery_generation
            || self.optical_discs == result.discs
        {
            return false;
        }
        self.optical_discs = result.discs;
        true
    }

    pub fn optical_discs(&self) -> &[TocSnapshot] {
        &self.optical_discs
    }

    pub fn optical_discovery_receiver(&self) -> async_channel::Receiver<OpticalDiscoveryResult> {
        self.optical_discovery_source.clone()
    }

    // --- MusicBrainz disc lookup + cover (read-only) ---

    /// Look up the disc's release candidates by MusicBrainz Disc ID on a
    /// worker thread. Bumps the metadata generation so any earlier lookup or
    /// cover fetch for a now-replaced disc is discarded on arrival.
    ///
    /// Returns `true` when a lookup was actually started — i.e. a remote
    /// service is installed and a `Releases` event is guaranteed to arrive on
    /// [`Self::cd_lookup_receiver`]. Returns `false` when no remote service is
    /// installed (the UI then renders the fallback identity and shows no
    /// "searching" affordance, since nothing would ever resolve it).
    pub fn lookup_disc_releases(&mut self, snapshot: &TocSnapshot) -> bool {
        let Some(service) = self.remote_metadata_service.clone() else {
            return false;
        };
        self.cd_metadata_generation = self.cd_metadata_generation.wrapping_add(1);
        let generation = self.cd_metadata_generation;
        let disc_id = snapshot.disc_id.clone();
        let track_count = snapshot.audio_track_count();
        let sink = self.cd_lookup_sink.clone();
        let _ = thread::Builder::new()
            .name("sustain-cd-lookup".to_owned())
            .spawn(move || {
                let (releases, failed) = match service.lookup_disc(&disc_id, track_count) {
                    Ok(releases) => (releases, false),
                    Err(_) => (Vec::new(), true),
                };
                let _ = sink.send_blocking(CdLookupEvent::Releases {
                    generation,
                    disc_id,
                    releases,
                    failed,
                });
            });
        true
    }

    /// Fetch the front cover for a chosen release on a worker thread, tagged
    /// with the current metadata generation. A no-op without a remote
    /// service. Missing artwork is a normal outcome (`cover: None`).
    pub fn fetch_disc_cover(&self, release: DiscRelease) {
        let Some(service) = self.remote_metadata_service.clone() else {
            return;
        };
        let generation = self.cd_metadata_generation;
        let sink = self.cd_lookup_sink.clone();
        let _ = thread::Builder::new()
            .name("sustain-cd-cover".to_owned())
            .spawn(move || {
                let cover = service.fetch_disc_cover(&release).ok().flatten();
                let _ = sink.send_blocking(CdLookupEvent::Cover { generation, cover });
            });
    }

    pub fn cd_lookup_receiver(&self) -> async_channel::Receiver<CdLookupEvent> {
        self.cd_lookup_source.clone()
    }

    /// Whether `generation` matches the current disc-metadata generation —
    /// the UI's guard against applying a lookup/cover result for a disc that
    /// has since been ejected or replaced.
    pub fn is_current_cd_metadata_generation(&self, generation: u64) -> bool {
        generation == self.cd_metadata_generation
    }

    /// Invalidate any in-flight disc lookup/cover (e.g. the disc was ejected
    /// or the CD page closed) so their late results are dropped.
    pub fn invalidate_cd_metadata(&mut self) {
        self.cd_metadata_generation = self.cd_metadata_generation.wrapping_add(1);
    }

    // --- The rip task (prepare / worker / apply-or-fail) ---

    pub fn prepare_cd_import(
        &mut self,
        request: CdImportRequest,
    ) -> ApplicationRuntimeResult<CdImportTask> {
        self.ensure_no_conflicting_library_mutation()?;

        let probe = self
            .cd_probe
            .clone()
            .ok_or(ApplicationRuntimeError::CdBackendUnavailable)?;
        let encoder = self
            .cd_encoder
            .clone()
            .ok_or(ApplicationRuntimeError::CdBackendUnavailable)?;
        let metadata_service = self
            .metadata_service
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        let library_store = self
            .library_store
            .clone()
            .ok_or(ApplicationRuntimeError::LibraryServicesUnavailable)?;
        if request.selected_tracks.is_empty() {
            return Err(ApplicationRuntimeError::CdImportNoTracksSelected);
        }

        // CD imports always create owned files beneath the library path,
        // even in reference mode — an optical track has no durable path to
        // reference — so the managed-library filesystem must be usable.
        let library_path = self
            .settings
            .library_path()
            .ok_or(ApplicationRuntimeError::LibraryPathUnavailable)?
            .to_path_buf();
        self.ensure_managed_library_filesystem_supported_at(&library_path)?;

        // Capture the encoding profile at preparation; later Preferences
        // changes affect only subsequent imports.
        let profile = self.settings.encoding.cd_profile;
        if let Err(error) = encoder.ensure_available(profile) {
            // Precise, one-shot notification naming the missing element.
            self.push_ephemeral_notification(
                NotificationCategory::CdImport,
                NotificationSeverity::Error,
                error.to_string(),
            );
            return Err(ApplicationRuntimeError::CdImportFailed);
        }

        // Re-probe and compare identity at import start: refuse to read a
        // disc that was swapped after the panel was built.
        let snapshot = request.snapshot;
        let expected_identity = snapshot.identity();
        let still_present = probe
            .probe(&snapshot.device_path)
            .is_some_and(|current| current.is_same_disc(&expected_identity));
        if !still_present {
            return Err(ApplicationRuntimeError::CdImportDiscChanged);
        }

        let mut selected = request.selected_tracks;
        selected.sort_unstable();
        selected.dedup();
        let plans: Vec<CdTrackPlan> = selected
            .into_iter()
            .filter(|number| snapshot.track(*number).is_some())
            .map(|number| {
                let mut metadata =
                    plan::build_track_metadata(&snapshot, request.release.as_ref(), number);
                apply_track_override(&mut metadata, request.overrides.get(&number));
                CdTrackPlan {
                    track_number: number,
                    metadata,
                }
            })
            .collect();
        if plans.is_empty() {
            return Err(ApplicationRuntimeError::CdImportNoTracksSelected);
        }

        let cancellation_requested = Arc::new(AtomicBool::new(false));
        self.cd_import_cancellation = Some(cancellation_requested.clone());
        self.background_task_status = crate::BackgroundTaskStatus::CdImportRunning;
        let notification_id = self.push_persistent_notification(
            NotificationCategory::CdImport,
            NotificationSeverity::Info,
            notifications::cd_import_running_text(),
            true,
        );
        self.cd_import_notification_id = Some(notification_id);

        Ok(CdImportTask {
            library_path,
            profile,
            expected_identity,
            snapshot,
            plans,
            cover: request.cover,
            existing_tracks: self.library_tracks.clone(),
            library_store,
            metadata_service,
            probe,
            encoder,
            managed_library_filesystem_validator: self.managed_library_filesystem_validator.clone(),
            cancellation_requested,
        })
    }

    pub fn update_cd_import_progress(&mut self, completed_tracks: usize, total_tracks: usize) {
        if let Some(id) = self.cd_import_notification_id {
            self.update_notification_body(
                id,
                notifications::cd_import_progress_text(completed_tracks, total_tracks),
            );
        }
    }

    pub fn apply_cd_import_result(&mut self, result: CdImportResult) {
        let summary = result.summary;
        self.library_tracks.extend(result.tracks);
        self.library_tracks.sort_by_key(|track| track.id);
        self.rebuild_search_index();
        self.refresh_playback_queue_track_ids();
        self.cd_import_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.cd_import_notification_id.take() {
            self.dismiss_notification(id);
        }
        let severity = if summary.failure.is_some() {
            NotificationSeverity::Warning
        } else {
            NotificationSeverity::Info
        };
        self.push_ephemeral_notification(
            NotificationCategory::CdImport,
            severity,
            notifications::cd_import_outcome_text(
                summary.imported_tracks,
                summary.requested_tracks,
                summary.cancelled,
                summary.failure.as_deref(),
            ),
        );
    }

    pub fn fail_cd_import(&mut self, error: ApplicationRuntimeError) {
        self.cd_import_cancellation = None;
        self.background_task_status = crate::BackgroundTaskStatus::Idle;
        if let Some(id) = self.cd_import_notification_id.take() {
            self.dismiss_notification(id);
        }
        if !self.report_managed_library_filesystem_error(&error) {
            self.push_ephemeral_notification(
                NotificationCategory::CdImport,
                NotificationSeverity::Error,
                notifications::runtime_error_text(&error).to_owned(),
            );
        }
    }

    pub fn request_cd_import_cancellation(&self) {
        if let Some(cancellation_requested) = &self.cd_import_cancellation {
            cancellation_requested.store(true, Ordering::SeqCst);
        }
    }
}

/// Overlay a track's user-typed title/artist onto its looked-up metadata.
/// A blank (or whitespace-only) override field is ignored, leaving the
/// looked-up / generated value in place.
fn apply_track_override(metadata: &mut TrackMetadata, track_override: Option<&CdTrackOverride>) {
    let Some(track_override) = track_override else {
        return;
    };
    if let Some(title) = non_blank_owned(track_override.title.as_deref()) {
        metadata.title = Some(title);
    }
    if let Some(artist) = non_blank_owned(track_override.artist.as_deref()) {
        metadata.artist = Some(artist);
    }
}

fn non_blank_owned(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|trimmed| !trimmed.is_empty())
        .map(ToOwned::to_owned)
}
