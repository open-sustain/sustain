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
//
// Every string here is localized through `sustain_i18n`: static messages go
// through `gettext`, interpolated ones through `tr_format!` over a
// `gettext`/`ngettext` template, and count-dependent ones through `ngettext`
// so each target language selects its own plural form. Because word order
// varies across languages, a sentence that carries more than one independent
// count renders each count as its own `ngettext` phrase and injects the
// finished phrases into an outer template, rather than gluing fragments
// together positionally.

use sustain_i18n::{gettext, ngettext, tr_format};

use crate::{
    ApplicationRuntimeError, LibraryConsolidationSummary, LibraryImportSummary, LibraryScanSummary,
};

pub fn library_scan_running_text() -> String {
    gettext("Scanning library...")
}

pub fn library_import_running_text() -> String {
    gettext("Adding tracks...")
}

pub fn library_import_progress_text(processed: usize, total: usize) -> String {
    tr_format!(
        gettext("Adding tracks ({processed}/{total})..."),
        processed = processed,
        total = total,
    )
}

pub fn library_consolidation_running_text() -> String {
    gettext("Organizing library...")
}

pub fn analysis_background_running_text(processed: u32, total: u32) -> String {
    // Always render progress as completed/total so the analysis lane
    // reads the same way as every other background task (device sync,
    // online retrieval). The scheduler snapshots `total` once when the run
    // starts so the denominator stays meaningful while its queue refills.
    tr_format!(
        gettext("Analyzing tracks ({processed}/{total})..."),
        processed = processed,
        total = total,
    )
}

pub fn analysis_background_outcome_text(completed: u32, failed: u32) -> String {
    if failed == 0 {
        tr_format!(
            ngettext(
                "Analyzed {completed} track.",
                "Analyzed {completed} tracks.",
                completed,
            ),
            completed = completed,
        )
    } else {
        let skipped = skipped_tracks_phrase(failed);
        tr_format!(
            // Translators: {skipped} is an already-localized phrase such as
            // "1 track skipped"; keep the placeholder verbatim.
            ngettext(
                "Analyzed {completed} track, {skipped}.",
                "Analyzed {completed} tracks, {skipped}.",
                completed,
            ),
            completed = completed,
            skipped = skipped,
        )
    }
}

pub fn online_background_running_text(completed: u32, remaining: u32) -> String {
    // Mirror analysis: progress is always completed/total for a uniform
    // read across the notification lane.
    let total = completed.saturating_add(remaining);
    tr_format!(
        gettext("Retrieving online data ({completed}/{total})..."),
        completed = completed,
        total = total,
    )
}

pub fn online_background_outcome_text(completed: u32, failed: u32) -> String {
    if failed == 0 {
        tr_format!(
            ngettext(
                "Retrieved online data for {completed} track.",
                "Retrieved online data for {completed} tracks.",
                completed,
            ),
            completed = completed,
        )
    } else {
        let skipped = skipped_tracks_phrase(failed);
        tr_format!(
            // Translators: {skipped} is an already-localized phrase such as
            // "1 track skipped"; keep the placeholder verbatim.
            ngettext(
                "Retrieved online data for {completed} track, {skipped}.",
                "Retrieved online data for {completed} tracks, {skipped}.",
                completed,
            ),
            completed = completed,
            skipped = skipped,
        )
    }
}

/// The "N tracks skipped" sub-phrase shared by the analysis and online-retrieval
/// failure summaries. Rendered on its own so its plural form is correct
/// independently of the surrounding sentence's primary count.
fn skipped_tracks_phrase(skipped: u32) -> String {
    tr_format!(
        // Translators: a terse phrase, e.g. "1 track skipped"; tracks that failed
        // analysis or retrieval and were skipped.
        ngettext(
            "{skipped} track skipped",
            "{skipped} tracks skipped",
            skipped
        ),
        skipped = skipped,
    )
}

pub fn analysis_background_persistence_error_text(detail: &str) -> String {
    tr_format!(
        gettext("Analysis paused: the library database rejected a write ({detail})."),
        detail = detail,
    )
}

pub fn online_background_persistence_error_text(detail: &str) -> String {
    tr_format!(
        gettext("Online retrieval paused: the library database rejected a write ({detail})."),
        detail = detail,
    )
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
            Some(changes) => tr_format!(gettext("Scan stopped: {changes}."), changes = changes),
            None => gettext("Scan stopped."),
        };
    }
    if summary.missing_reconciliation_skipped {
        return match changes {
            Some(changes) => tr_format!(
                gettext("Scan partial: {changes}; missing-file reconciliation skipped."),
                changes = changes,
            ),
            None => gettext("Scan partial: missing-file reconciliation skipped."),
        };
    }
    match changes {
        Some(changes) => tr_format!(gettext("Scan complete: {changes}."), changes = changes),
        None => gettext("Scan complete: no changes."),
    }
}

/// Joins the non-zero change counts ("3 added, 1 updated, 2 missing,
/// 1 failed") for the scan outcome notification, or `None` when the scan
/// changed nothing. Each clause is pluralized on its own count so every
/// target language picks the right form.
fn scan_change_clauses(summary: &LibraryScanSummary) -> Option<String> {
    let mut clauses: Vec<String> = Vec::new();
    if summary.added_tracks > 0 {
        clauses.push(tr_format!(
            // Translators: a terse change-count clause joined into a scan summary,
            // e.g. "3 added"; refers to tracks added to the library.
            ngettext(
                "{added} added",
                "{added} added",
                summary.added_tracks as u32
            ),
            added = summary.added_tracks,
        ));
    }
    if summary.updated_tracks > 0 {
        clauses.push(tr_format!(
            // Translators: a terse change-count clause, e.g. "1 updated"; tracks
            // whose metadata changed.
            ngettext(
                "{updated} updated",
                "{updated} updated",
                summary.updated_tracks as u32
            ),
            updated = summary.updated_tracks,
        ));
    }
    if summary.missing_tracks > 0 {
        clauses.push(tr_format!(
            // Translators: a terse change-count clause, e.g. "2 missing"; tracks
            // no longer found on disk.
            ngettext(
                "{missing} missing",
                "{missing} missing",
                summary.missing_tracks as u32
            ),
            missing = summary.missing_tracks,
        ));
    }
    if summary.failed_files > 0 {
        clauses.push(tr_format!(
            // Translators: a terse change-count clause, e.g. "1 failed"; files
            // that could not be read.
            ngettext(
                "{failed} failed",
                "{failed} failed",
                summary.failed_files as u32
            ),
            failed = summary.failed_files,
        ));
    }
    (!clauses.is_empty()).then(|| clauses.join(", "))
}

pub fn library_import_outcome_text(summary: &LibraryImportSummary) -> String {
    if summary.cancelled {
        return tr_format!(
            ngettext(
                "Import stopped: {imported} added before cancel.",
                "Import stopped: {imported} added before cancel.",
                summary.imported_tracks as u32,
            ),
            imported = summary.imported_tracks,
        );
    }
    match (
        summary.imported_tracks,
        summary.duplicate_files,
        summary.discovered_files,
    ) {
        (0, 0, 0) => gettext("No audio files were found."),
        (imported, 0, _) => tr_format!(
            ngettext(
                "{imported} track added.",
                "{imported} tracks added.",
                imported as u32
            ),
            imported = imported,
        ),
        (imported, duplicates, _) => {
            let skipped = tr_format!(
                // Translators: a terse change-count clause, e.g. "2 duplicates
                // skipped"; files skipped because they were already in the library.
                ngettext(
                    "{duplicates} duplicate skipped",
                    "{duplicates} duplicates skipped",
                    duplicates as u32,
                ),
                duplicates = duplicates,
            );
            tr_format!(
                // Translators: {skipped} is an already-localized phrase such as
                // "2 duplicates skipped"; keep the placeholder verbatim.
                ngettext(
                    "{imported} track added, {skipped}.",
                    "{imported} tracks added, {skipped}.",
                    imported as u32,
                ),
                imported = imported,
                skipped = skipped,
            )
        }
    }
}

pub fn library_consolidation_outcome_text(summary: &LibraryConsolidationSummary) -> String {
    let mut outcome = if summary.cancelled {
        let pending = summary.planned_tracks.saturating_sub(summary.moved_tracks);
        tr_format!(
            // Translators: {moved} and {pending} are already-localized phrases
            // such as "1 moved" / "2 pending"; keep the placeholders verbatim.
            gettext("Library organization stopped: {moved}, {pending}."),
            moved = moved_tracks_phrase(summary.moved_tracks),
            pending = pending_tracks_phrase(pending),
        )
    } else {
        tr_format!(
            // Translators: {moved}, {organized} and {missing} are
            // already-localized phrases such as "1 moved" / "0 already
            // organized" / "0 missing"; keep the placeholders verbatim.
            gettext("Library organized: {moved}, {organized}, {missing}."),
            moved = moved_tracks_phrase(summary.moved_tracks),
            organized = already_organized_tracks_phrase(summary.already_organized_tracks),
            missing = missing_tracks_phrase(summary.missing_tracks),
        )
    };
    if summary.empty_directory_cleanup_failed {
        outcome.push(' ');
        outcome.push_str(&gettext("Some empty folders could not be removed."));
    }
    outcome
}

/// "N moved" sub-phrase for library-organization summaries.
fn moved_tracks_phrase(moved: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "1 moved"; tracks
        // relocated into the organized library.
        ngettext("{moved} moved", "{moved} moved", moved as u32),
        moved = moved,
    )
}

/// "N pending" sub-phrase for a cancelled library-organization run.
fn pending_tracks_phrase(pending: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "2 pending"; tracks not
        // yet relocated when organization was cancelled.
        ngettext("{pending} pending", "{pending} pending", pending as u32),
        pending = pending,
    )
}

/// "N already organized" sub-phrase for library-organization summaries.
fn already_organized_tracks_phrase(organized: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "0 already organized";
        // tracks that were already in their managed location.
        ngettext(
            "{organized} already organized",
            "{organized} already organized",
            organized as u32,
        ),
        organized = organized,
    )
}

/// "N missing" sub-phrase shared by scan and organization summaries.
fn missing_tracks_phrase(missing: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "0 missing"; tracks no
        // longer found on disk.
        ngettext("{missing} missing", "{missing} missing", missing as u32),
        missing = missing,
    )
}

pub fn managed_library_cleanup_failed_text() -> String {
    gettext("Some empty managed-library folders could not be removed.")
}

pub fn metadata_write_retry_text() -> String {
    gettext("Some changes could not be mirrored to audio files. Sustain will retry.")
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
        return gettext("Library folder updated.");
    }
    if unresolved > 0 {
        return tr_format!(
            ngettext(
                "Library folder updated: {missing} of {total} track not found; {unresolved} could not be checked.",
                "Library folder updated: {missing} of {total} tracks not found; {unresolved} could not be checked.",
                total as u32,
            ),
            missing = newly_missing,
            total = total,
            unresolved = unresolved,
        );
    }
    if newly_missing == 0 {
        return tr_format!(
            ngettext(
                "Library folder updated: all {total} track found.",
                "Library folder updated: all {total} tracks found.",
                total as u32,
            ),
            total = total,
        );
    }
    tr_format!(
        ngettext(
            "Library folder updated: {missing} of {total} track not found at the new location.",
            "Library folder updated: {missing} of {total} tracks not found at the new location.",
            total as u32,
        ),
        missing = newly_missing,
        total = total,
    )
}

pub fn runtime_error_text(error: &ApplicationRuntimeError) -> String {
    match error {
        ApplicationRuntimeError::BackgroundTaskRunning => {
            gettext("Another background task is already running.")
        }
        ApplicationRuntimeError::LibraryScanFailed => {
            gettext("The selected folder could not be scanned.")
        }
        ApplicationRuntimeError::LibraryConsolidationFailed => {
            gettext("The library could not be organized.")
        }
        ApplicationRuntimeError::DuplicateConsolidationFailed => {
            gettext("The duplicate tracks could not be consolidated safely.")
        }
        ApplicationRuntimeError::DuplicateConsolidationSourceMissing => gettext(
            "One or more of the selected files is missing from disk. Restore or remove it, then consolidate again.",
        ),
        ApplicationRuntimeError::LibraryServicesUnavailable => {
            gettext("Library scanning is not available in this build.")
        }
        ApplicationRuntimeError::LibraryStoreFailed => {
            gettext("The library database could not be updated.")
        }
        ApplicationRuntimeError::LibraryPathUnavailable => {
            gettext("Choose a library folder first.")
        }
        ApplicationRuntimeError::ManagedLibraryFilesystemUnsupported(error) => error.user_message(),
        ApplicationRuntimeError::LibraryImportFailed => {
            gettext("The files could not be added to the library.")
        }
        ApplicationRuntimeError::LibraryHydrationPending => {
            gettext("The music library is still loading.")
        }
        ApplicationRuntimeError::MetadataWriteFailed => {
            gettext("The track metadata could not be updated.")
        }
        ApplicationRuntimeError::InvalidPlaylistName => gettext("The playlist name is not valid."),
        ApplicationRuntimeError::InvalidPlaylistFolderName => {
            gettext("The folder name is not valid.")
        }
        ApplicationRuntimeError::InvalidSmartPlaylistName => {
            gettext("The smart playlist name is not valid.")
        }
        ApplicationRuntimeError::InvalidSmartPlaylistRules => {
            gettext("A smart playlist needs at least one rule.")
        }
        ApplicationRuntimeError::PlaylistEntryNotFound
        | ApplicationRuntimeError::PlaylistNotFound => {
            gettext("The playlist could not be updated.")
        }
        ApplicationRuntimeError::PlaylistFolderNotFound => {
            gettext("The playlist folder could not be updated.")
        }
        ApplicationRuntimeError::PlaylistFolderWouldCycle => {
            gettext("A folder cannot be moved inside itself.")
        }
        ApplicationRuntimeError::SmartPlaylistNotFound => {
            gettext("The smart playlist could not be updated.")
        }
        ApplicationRuntimeError::SettingsLoadFailed => {
            gettext("Your settings could not be loaded.")
        }
        ApplicationRuntimeError::SettingsSaveFailed => gettext("Your settings could not be saved."),
        ApplicationRuntimeError::YoutubeAudioDownloadUnavailable => {
            gettext("YouTube audio replacement is not available.")
        }
        ApplicationRuntimeError::YoutubeAudioReplacementFailed => {
            gettext("The downloaded audio could not safely replace this track.")
        }
        ApplicationRuntimeError::YoutubeAudioReplacementNotEligible => gettext(
            "YouTube replacement is available only for present tracks at or below 192 kbps.",
        ),
        ApplicationRuntimeError::TrackRelocationFailed => {
            gettext("The replacement track file could not be used.")
        }
        ApplicationRuntimeError::TrackReplacementAlreadyInLibrary => {
            gettext("That file is already attached to another library track.")
        }
        ApplicationRuntimeError::TrackReplacementOutsideLibrary => {
            gettext("Choose a replacement inside the configured library folder.")
        }
        ApplicationRuntimeError::TrackReplacementUnsupported => {
            gettext("Choose a supported audio file.")
        }
        ApplicationRuntimeError::PlaybackFailed
        | ApplicationRuntimeError::PlaybackServiceUnavailable => {
            gettext("Playback is not available.")
        }
        ApplicationRuntimeError::TrackUnavailable => gettext("Track file is missing."),
        ApplicationRuntimeError::TrackTrashFailed => {
            gettext("The track could not be moved to trash.")
        }
        ApplicationRuntimeError::ArtworkFetchingUnavailable => {
            gettext("Remote artwork retrieval is not available in this build.")
        }
        ApplicationRuntimeError::ArtworkRejected => {
            gettext("The artwork is unsupported, corrupt, or exceeds Sustain's size limits.")
        }
        ApplicationRuntimeError::UnsupportedCommand(_) => {
            gettext("This action is not available yet.")
        }
    }
}

pub fn device_sync_running_text(label: &str) -> String {
    tr_format!(gettext("Syncing {label}…"), label = label)
}

pub fn device_sync_progress_text(progress: sustain_device_sync::SyncProgress) -> String {
    use sustain_device_sync::SyncStage;
    match progress.stage {
        SyncStage::Preparing => tr_format!(
            gettext("Preparing tracks ({completed}/{total})…"),
            completed = progress.completed,
            total = progress.total,
        ),
        SyncStage::Copying => tr_format!(
            gettext("Copying tracks ({completed}/{total})…"),
            completed = progress.completed,
            total = progress.total,
        ),
        SyncStage::WritingPlaylists => gettext("Writing playlists…"),
        SyncStage::WritingDatabase => gettext("Writing device database…"),
        SyncStage::Removing => tr_format!(
            gettext("Removing tracks ({completed}/{total})…"),
            completed = progress.completed,
            total = progress.total,
        ),
    }
}

pub fn device_sync_outcome_text(outcome: &sustain_device_sync::SyncOutcome) -> String {
    if outcome.cancelled {
        return tr_format!(
            // Translators: {copied} and {updated} are already-localized phrases
            // such as "5 copied" / "3 updated"; keep the placeholders verbatim.
            gettext("Sync stopped: {copied}, {updated}."),
            copied = copied_tracks_phrase(outcome.copied),
            updated = updated_tracks_phrase(outcome.updated),
        );
    }
    let changed = outcome.copied + outcome.updated;
    if changed == 0 && outcome.removed == 0 {
        return gettext("Device already up to date.");
    }
    let mut parts = Vec::new();
    if outcome.copied > 0 {
        parts.push(tr_format!(
            // Translators: a terse change-count clause, e.g. "5 tracks added";
            // tracks newly copied to the device.
            ngettext(
                "{copied} track added",
                "{copied} tracks added",
                outcome.copied as u32
            ),
            copied = outcome.copied,
        ));
    }
    if outcome.updated > 0 {
        parts.push(updated_tracks_phrase(outcome.updated));
    }
    if outcome.removed > 0 {
        parts.push(tr_format!(
            // Translators: a terse change-count clause, e.g. "2 removed"; tracks
            // deleted from the device.
            ngettext(
                "{removed} removed",
                "{removed} removed",
                outcome.removed as u32
            ),
            removed = outcome.removed,
        ));
    }
    tr_format!(
        gettext("Sync complete: {changes}."),
        changes = parts.join(", ")
    )
}

/// "N copied" sub-phrase for a cancelled device sync.
fn copied_tracks_phrase(copied: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "5 copied"; tracks copied
        // to the device before the sync was cancelled.
        ngettext("{copied} copied", "{copied} copied", copied as u32),
        copied = copied,
    )
}

/// "N updated" sub-phrase shared by the device-sync summaries.
fn updated_tracks_phrase(updated: usize) -> String {
    tr_format!(
        // Translators: a terse change-count clause, e.g. "3 updated"; tracks
        // re-copied because they changed.
        ngettext("{updated} updated", "{updated} updated", updated as u32),
        updated = updated,
    )
}

#[cfg(test)]
#[path = "notifications_text_tests.rs"]
mod text_tests;

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
