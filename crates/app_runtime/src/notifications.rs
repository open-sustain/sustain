// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Central notification surface for user-facing status messages.
//!
//! Every user-visible status message — background task progress,
//! command outcomes, async tag write failures, artwork fetch results —
//! must flow through this module so the UI has a single, predictable
//! source to render. Feature code never pokes the status-bar widget
//! directly.
//!
//! Notifications come in two flavors:
//!
//! - [`NotificationKind::Persistent`] sticks until the producer
//!   explicitly dismisses it. Used for in-progress states (a scan
//!   running, an artwork fetch in flight); the widget paints them
//!   with a spinner and, when the kind says so, a Cancel button.
//!   Several persistents may stack — the most recent is shown; on
//!   dismissal the next one underneath returns to the surface.
//! - [`NotificationKind::Ephemeral`] auto-dismisses after
//!   [`EPHEMERAL_NOTIFICATION_DURATION`]. Used for one-shot outcomes.
//!   Ephemerals briefly preempt the persistent slot for visibility,
//!   then expire and the persistent comes back.
//!
//! The widget renders the head of `ephemeral_queue` if present, else
//! the back of `persistent_stack`. Both lists are pure data; the
//! widget is responsible for animation and timing.

use std::collections::VecDeque;
use std::time::Duration;

/// How long an Ephemeral stays at full opacity once it becomes the
/// displayed head. Product timing decision lives here as the single
/// source of truth; do not duplicate this value at call sites.
pub const EPHEMERAL_NOTIFICATION_DURATION: Duration = Duration::from_secs(4);

/// Duration of the slide+fade carousel transition the widget uses to
/// swap notifications. Co-located with the dismissal duration because
/// the two together describe one product-level timing budget.
pub const NOTIFICATION_TRANSITION: Duration = Duration::from_millis(250);

/// Runaway-safety guard on the ephemeral queue depth. We never evict
/// an un-expired notification at the head; this limit only triggers
/// on producers misbehaving, in which case we drop the newcomer (so
/// the user keeps the ability to read what is already queued).
pub const NOTIFICATION_QUEUE_HARD_CAP: usize = 15;

/// Monotonic, opaque identifier for a notification. Producers keep
/// hold of the id they get back from a push so they can later dismiss
/// the exact notification they created.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct NotificationId(u64);

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum NotificationCategory {
    LibraryScan,
    LibraryImport,
    LibraryConsolidation,
    DuplicateConsolidation,
    LibraryHydration,
    ManagedLibraryFilesystem,
    ArtworkFetch,
    MetadataWrite,
    YoutubeAudioReplacement,
    PlaybackStatistics,
    Command,
    /// Background DSP analysis (BPM / key / waveform) driven by the
    /// `AnalysisScheduler`. Pushed as a persistent notification while
    /// tracks are being analyzed and as an ephemeral summary once the
    /// queue drains.
    AnalysisBackground,
    /// Background network-bound retrieval (artwork / lyrics) driven by
    /// the `OnlineScheduler`. Same lifecycle as
    /// [`Self::AnalysisBackground`] — a persistent notification while
    /// the worker is running, a one-shot summary once it idles.
    OnlineBackground,
    /// Smart Shuffle model lifecycle — cold-start refusal, training
    /// success, training failure. Always ephemeral; the model is
    /// invisible to the user except through these one-shot
    /// notifications.
    SmartShuffle,
    /// Device sync (#23/#24): copy/playlist/database progress while a
    /// sync runs, and the one-shot outcome summary when it finishes.
    DeviceSync,
    /// Persisting `settings.toml` failed during normal operation (e.g. a
    /// debounced volume change could not be written). Always ephemeral —
    /// the in-memory preference still took effect; the user only needs to
    /// know it will not survive a restart.
    Settings,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationSeverity {
    Info,
    Warning,
    Error,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NotificationKind {
    Ephemeral,
    Persistent { cancellable: bool },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Notification {
    pub id: NotificationId,
    pub category: NotificationCategory,
    pub kind: NotificationKind,
    pub severity: NotificationSeverity,
    pub body: String,
}

/// Owns the live persistent stack and ephemeral queue. Held by
/// [`crate::ApplicationRuntime`]; feature code reaches it through the
/// runtime's typed push/dismiss helpers so the observer fires
/// uniformly on every mutation.
#[derive(Debug, Default)]
pub struct NotificationCenter {
    next_id: u64,
    persistent_stack: Vec<Notification>,
    ephemeral_queue: VecDeque<Notification>,
}

impl NotificationCenter {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            persistent_stack: Vec::new(),
            ephemeral_queue: VecDeque::new(),
        }
    }

    /// Currently-displayed persistent notification, or `None` when the
    /// stack is empty. The back of the stack wins so the most recent
    /// in-progress activity is what the user sees.
    pub fn current_persistent(&self) -> Option<&Notification> {
        self.persistent_stack.last()
    }

    pub fn current_ephemeral(&self) -> Option<&Notification> {
        self.ephemeral_queue.front()
    }

    pub fn ephemeral_queue(&self) -> &VecDeque<Notification> {
        &self.ephemeral_queue
    }

    pub fn persistent_stack(&self) -> &[Notification] {
        &self.persistent_stack
    }

    pub fn push_persistent(
        &mut self,
        category: NotificationCategory,
        severity: NotificationSeverity,
        body: String,
        cancellable: bool,
    ) -> NotificationId {
        let id = self.fresh_id();
        self.persistent_stack.push(Notification {
            id,
            category,
            kind: NotificationKind::Persistent { cancellable },
            severity,
            body,
        });
        id
    }

    /// Push an ephemeral, coalescing by category so a burst of similar
    /// outcomes does not stack up. The currently-displayed head is
    /// never preempted — it lives out its full timer regardless of
    /// what arrives next. If a queued (but not yet displayed)
    /// ephemeral in the same category exists, its body is replaced in
    /// place and its position preserved; otherwise the newcomer is
    /// appended to the tail.
    pub fn push_ephemeral(
        &mut self,
        category: NotificationCategory,
        severity: NotificationSeverity,
        body: String,
    ) -> NotificationId {
        let id = self.fresh_id();
        let notification = Notification {
            id,
            category,
            kind: NotificationKind::Ephemeral,
            severity,
            body,
        };

        // Skip the head: it is currently being read by the user and
        // its timer is already running. Anything past it is fair game
        // for in-place replacement so a burst of similar outcomes does
        // not stack up.
        if let Some(slot) = self
            .ephemeral_queue
            .iter_mut()
            .skip(1)
            .find(|queued| queued.category == category)
        {
            *slot = notification;
            return id;
        }

        if self.ephemeral_queue.len() >= NOTIFICATION_QUEUE_HARD_CAP {
            return id;
        }

        self.ephemeral_queue.push_back(notification);
        id
    }

    /// Update the body text of an existing notification in place,
    /// preserving its slot in the persistent stack or ephemeral queue
    /// so the lane does not flicker through a dismiss+repush. Returns
    /// `true` when a matching id was found, `false` otherwise (the
    /// notification was already dismissed or has expired).
    pub fn update_body(&mut self, id: NotificationId, body: String) -> bool {
        if let Some(slot) = self
            .persistent_stack
            .iter_mut()
            .find(|notification| notification.id == id)
        {
            slot.body = body;
            return true;
        }
        if let Some(slot) = self
            .ephemeral_queue
            .iter_mut()
            .find(|notification| notification.id == id)
        {
            slot.body = body;
            return true;
        }
        false
    }

    /// Remove the notification matching `id` from wherever it lives.
    /// No-op if the id is no longer present (already expired, already
    /// dismissed, never existed).
    pub fn dismiss(&mut self, id: NotificationId) {
        if let Some(index) = self
            .persistent_stack
            .iter()
            .position(|notification| notification.id == id)
        {
            self.persistent_stack.remove(index);
            return;
        }
        self.ephemeral_queue
            .retain(|notification| notification.id != id);
    }

    /// Drop the displayed ephemeral once its timer has elapsed. The
    /// widget calls this when it is ready to slide the next item in.
    pub fn expire_current_ephemeral(&mut self) -> Option<Notification> {
        self.ephemeral_queue.pop_front()
    }

    fn fresh_id(&mut self) -> NotificationId {
        // Wrap to 1 on overflow rather than 0 so an uninitialized id
        // is never accidentally valid in debug assertions.
        let id = NotificationId(self.next_id);
        self.next_id = self.next_id.checked_add(1).unwrap_or(1);
        id
    }

    #[cfg(test)]
    fn __test_force_push_ephemeral(
        &mut self,
        category: NotificationCategory,
        body: String,
    ) -> NotificationId {
        let id = self.fresh_id();
        self.ephemeral_queue.push_back(Notification {
            id,
            category,
            kind: NotificationKind::Ephemeral,
            severity: NotificationSeverity::Info,
            body,
        });
        id
    }
}

// User-facing message catalogue. Lives in `app_runtime` so the runtime
// can populate `Notification::body` at the same point it transitions
// its task state. The widget renders the string raw, with no
// case-by-case knowledge of what it means.

use crate::{
    ApplicationRuntimeError, LibraryConsolidationSummary, LibraryImportSummary, LibraryScanSummary,
};

pub fn library_scan_running_text() -> String {
    "Scanning library...".to_owned()
}

pub fn library_import_running_text() -> String {
    "Adding tracks...".to_owned()
}

pub fn library_consolidation_running_text() -> String {
    "Organizing library...".to_owned()
}

pub fn analysis_background_running_text(processed: u32, total: u32) -> String {
    // Always render progress as completed/total so the analysis lane
    // reads the same way as every other background task (device sync,
    // online retrieval). The scheduler snapshots `total` once when the run
    // starts so the denominator stays meaningful while its queue refills.
    format!("Analyzing tracks ({processed}/{total})...")
}

pub fn analysis_background_outcome_text(completed: u32, failed: u32) -> String {
    if failed == 0 {
        format!(
            "Analyzed {} {}.",
            completed,
            pluralize(completed as usize, "track", "tracks"),
        )
    } else {
        format!(
            "Analyzed {} {}, {} {} skipped.",
            completed,
            pluralize(completed as usize, "track", "tracks"),
            failed,
            pluralize(failed as usize, "track", "tracks"),
        )
    }
}

pub fn online_background_running_text(completed: u32, remaining: u32) -> String {
    // Mirror analysis: progress is always completed/total for a uniform
    // read across the notification lane.
    let total = completed.saturating_add(remaining);
    format!("Retrieving online data ({completed}/{total})...")
}

pub fn online_background_outcome_text(completed: u32, failed: u32) -> String {
    if failed == 0 {
        format!(
            "Retrieved online data for {} {}.",
            completed,
            pluralize(completed as usize, "track", "tracks"),
        )
    } else {
        format!(
            "Retrieved online data for {} {}, {} {} skipped.",
            completed,
            pluralize(completed as usize, "track", "tracks"),
            failed,
            pluralize(failed as usize, "track", "tracks"),
        )
    }
}

pub fn analysis_background_persistence_error_text(detail: &str) -> String {
    format!("Analysis paused: the library database rejected a write ({detail}).")
}

pub fn online_background_persistence_error_text(detail: &str) -> String {
    format!("Online retrieval paused: the library database rejected a write ({detail}).")
}

pub fn library_scan_outcome_text(summary: &LibraryScanSummary) -> String {
    // Report what the scan *changed*, not how many tracks the library
    // holds — the live total already shows in the status bar, so restating
    // it here is redundant (follow-up to #71).
    let changes = scan_change_clauses(summary);

    if summary.cancelled {
        // A cancelled scan skips the missing-file sweep, so `changes`
        // here only ever carries additions/updates/failures.
        return match changes {
            Some(changes) => format!("Scan stopped: {changes}."),
            None => "Scan stopped.".to_owned(),
        };
    }
    if summary.missing_reconciliation_skipped {
        return match changes {
            Some(changes) => {
                format!("Scan partial: {changes}; missing-file reconciliation skipped.")
            }
            None => "Scan partial: missing-file reconciliation skipped.".to_owned(),
        };
    }
    match changes {
        Some(changes) => format!("Scan complete: {changes}."),
        None => "Scan complete: no changes.".to_owned(),
    }
}

/// Joins the non-zero change counts ("3 added, 1 updated, 2 missing,
/// 1 failed") for the scan outcome notification, or `None` when the scan
/// changed nothing.
fn scan_change_clauses(summary: &LibraryScanSummary) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    if summary.added_tracks > 0 {
        clauses.push(format!("{} added", summary.added_tracks));
    }
    if summary.updated_tracks > 0 {
        clauses.push(format!("{} updated", summary.updated_tracks));
    }
    if summary.missing_tracks > 0 {
        clauses.push(format!("{} missing", summary.missing_tracks));
    }
    if summary.failed_files > 0 {
        clauses.push(format!("{} failed", summary.failed_files));
    }
    (!clauses.is_empty()).then(|| clauses.join(", "))
}

pub fn library_import_outcome_text(summary: &LibraryImportSummary) -> String {
    if summary.cancelled {
        return format!(
            "Import stopped: {} added before cancel.",
            summary.imported_tracks
        );
    }
    match (
        summary.imported_tracks,
        summary.duplicate_files,
        summary.discovered_files,
    ) {
        (0, 0, 0) => "No audio files were found.".to_owned(),
        (imported, 0, _) => format!("{imported} tracks added."),
        (imported, duplicates, _) => {
            format!("{imported} tracks added, {duplicates} duplicates skipped.")
        }
    }
}

pub fn library_consolidation_outcome_text(summary: &LibraryConsolidationSummary) -> String {
    let mut outcome = if summary.cancelled {
        format!(
            "Library organization stopped: {} moved, {} pending.",
            summary.moved_tracks,
            summary.planned_tracks.saturating_sub(summary.moved_tracks)
        )
    } else {
        format!(
            "Library organized: {} moved, {} already organized, {} missing.",
            summary.moved_tracks, summary.already_organized_tracks, summary.missing_tracks
        )
    };
    if summary.empty_directory_cleanup_failed {
        outcome.push_str(" Some empty folders could not be removed.");
    }
    outcome
}

pub fn managed_library_cleanup_failed_text() -> &'static str {
    "Some empty managed-library folders could not be removed."
}

/// Outcome string emitted after the user changes their library path.
/// `newly_missing` is the number of tracks whose file did not resolve under
/// the new root; `unresolved` counts paths whose reachability could not be
/// proven either way. `total` is the size of the persisted library.
pub fn library_path_change_outcome_text(
    newly_missing: usize,
    unresolved: usize,
    total: usize,
) -> String {
    if total == 0 {
        return "Library folder updated.".to_owned();
    }
    if unresolved > 0 {
        return format!(
            "Library folder updated: {} of {} {} not found; {} could not be checked.",
            newly_missing,
            total,
            pluralize(total, "track", "tracks"),
            unresolved,
        );
    }
    if newly_missing == 0 {
        return format!(
            "Library folder updated: all {} {} found.",
            total,
            pluralize(total, "track", "tracks"),
        );
    }
    format!(
        "Library folder updated: {} of {} {} not found at the new location.",
        newly_missing,
        total,
        pluralize(total, "track", "tracks"),
    )
}

pub fn runtime_error_text(error: &ApplicationRuntimeError) -> &'static str {
    match error {
        ApplicationRuntimeError::BackgroundTaskRunning => {
            "Another background task is already running."
        }
        ApplicationRuntimeError::LibraryScanFailed => "The selected folder could not be scanned.",
        ApplicationRuntimeError::LibraryConsolidationFailed => {
            "The library could not be organized."
        }
        ApplicationRuntimeError::DuplicateConsolidationFailed => {
            "The duplicate tracks could not be consolidated safely."
        }
        ApplicationRuntimeError::DuplicateConsolidationSourceMissing => {
            "One or more of the selected files is missing from disk. Restore or remove it, then consolidate again."
        }
        ApplicationRuntimeError::LibraryServicesUnavailable => {
            "Library scanning is not available in this build."
        }
        ApplicationRuntimeError::LibraryStoreFailed => "The library database could not be updated.",
        ApplicationRuntimeError::LibraryPathUnavailable => "Choose a library folder first.",
        ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(error) => error.user_message(),
        ApplicationRuntimeError::LibraryImportFailed => {
            "The files could not be added to the library."
        }
        ApplicationRuntimeError::LibraryHydrationPending => "The music library is still loading.",
        ApplicationRuntimeError::MetadataWriteFailed => "The track metadata could not be updated.",
        ApplicationRuntimeError::InvalidPlaylistName => "The playlist name is not valid.",
        ApplicationRuntimeError::InvalidPlaylistFolderName => "The folder name is not valid.",
        ApplicationRuntimeError::InvalidSmartPlaylistName => {
            "The smart playlist name is not valid."
        }
        ApplicationRuntimeError::InvalidSmartPlaylistRules => {
            "A smart playlist needs at least one rule."
        }
        ApplicationRuntimeError::PlaylistEntryNotFound
        | ApplicationRuntimeError::PlaylistNotFound => "The playlist could not be updated.",
        ApplicationRuntimeError::PlaylistFolderNotFound => {
            "The playlist folder could not be updated."
        }
        ApplicationRuntimeError::PlaylistFolderWouldCycle => {
            "A folder cannot be moved inside itself."
        }
        ApplicationRuntimeError::SmartPlaylistNotFound => {
            "The smart playlist could not be updated."
        }
        ApplicationRuntimeError::SettingsLoadFailed => "Your settings could not be loaded.",
        ApplicationRuntimeError::SettingsSaveFailed => "Your settings could not be saved.",
        ApplicationRuntimeError::YoutubeAudioDownloadUnavailable => {
            "YouTube audio replacement is not available."
        }
        ApplicationRuntimeError::YoutubeAudioReplacementFailed => {
            "The downloaded audio could not safely replace this track."
        }
        ApplicationRuntimeError::YoutubeAudioReplacementNotEligible => {
            "YouTube replacement is available only for present tracks at or below 192 kbps."
        }
        ApplicationRuntimeError::TrackRelocationFailed => {
            "The replacement track file could not be used."
        }
        ApplicationRuntimeError::TrackReplacementAlreadyInLibrary => {
            "That file is already attached to another library track."
        }
        ApplicationRuntimeError::TrackReplacementOutsideLibrary => {
            "Choose a replacement inside the configured library folder."
        }
        ApplicationRuntimeError::TrackReplacementUnsupported => "Choose a supported audio file.",
        ApplicationRuntimeError::PlaybackFailed
        | ApplicationRuntimeError::PlaybackServiceUnavailable => "Playback is not available.",
        ApplicationRuntimeError::TrackUnavailable => "Track file is missing.",
        ApplicationRuntimeError::TrackTrashFailed => "The track could not be moved to trash.",
        ApplicationRuntimeError::ArtworkFetchingUnavailable => {
            "Remote artwork retrieval is not available in this build."
        }
        ApplicationRuntimeError::ArtworkRejected => {
            "The artwork is unsupported, corrupt, or exceeds Sustain's size limits."
        }
        ApplicationRuntimeError::UnsupportedCommand(_) => "This action is not available yet.",
    }
}

pub fn device_sync_running_text(label: &str) -> String {
    format!("Syncing {label}…")
}

pub fn device_sync_progress_text(progress: sustain_device_sync::SyncProgress) -> String {
    use sustain_device_sync::SyncStage;
    match progress.stage {
        SyncStage::Preparing => {
            format!(
                "Preparing tracks ({}/{})…",
                progress.completed, progress.total
            )
        }
        SyncStage::Copying => {
            format!(
                "Copying tracks ({}/{})…",
                progress.completed, progress.total
            )
        }
        SyncStage::WritingPlaylists => "Writing playlists…".to_owned(),
        SyncStage::WritingDatabase => "Writing device database…".to_owned(),
        SyncStage::Removing => {
            format!(
                "Removing tracks ({}/{})…",
                progress.completed, progress.total
            )
        }
    }
}

pub fn device_sync_outcome_text(outcome: &sustain_device_sync::SyncOutcome) -> String {
    if outcome.cancelled {
        return format!(
            "Sync stopped: {} copied, {} updated.",
            outcome.copied, outcome.updated
        );
    }
    let changed = outcome.copied + outcome.updated;
    if changed == 0 && outcome.removed == 0 {
        return "Device already up to date.".to_owned();
    }
    let mut parts = Vec::new();
    if outcome.copied > 0 {
        parts.push(format!(
            "{} {} added",
            outcome.copied,
            pluralize(outcome.copied, "track", "tracks")
        ));
    }
    if outcome.updated > 0 {
        parts.push(format!("{} updated", outcome.updated));
    }
    if outcome.removed > 0 {
        parts.push(format!("{} removed", outcome.removed));
    }
    format!("Sync complete: {}.", parts.join(", "))
}

fn pluralize(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 { singular } else { plural }
}

#[cfg(test)]
#[path = "notifications_text_tests.rs"]
mod text_tests;

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
