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
    /// Playback transport failures that happen asynchronously after the
    /// backend accepted a play request.
    Playback,
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
    /// CD import (#25): extraction/encoding progress while a rip runs, and
    /// the one-shot success/partial/failure summary when it finishes.
    CdImport,
    /// Persisting `settings.toml` failed during normal operation (e.g. a
    /// debounced volume change could not be written). Always ephemeral —
    /// the in-memory preference still took effect; the user only needs to
    /// know it will not survive a restart.
    Settings,
    /// The waveform cache could not be opened at startup, so analysis
    /// waveforms are not kept this session. Always ephemeral.
    WaveformCache,
    /// Committed changes to tracks could not be read back from the
    /// library database. Persistent while any such track is retried.
    TrackReconciliation,
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

use sustain_i18n::{gettext, ngettext, npgettext, phrase_list, plural_count, tr_format};

use crate::{
    ApplicationRuntimeError, ImportFailureReason, LibraryConsolidationSummary,
    LibraryImportSummary, LibraryScanSummary, VerifiedDuplicateBatchSummary,
};

/// "{count} {verb}" for a count of tracks, as an item of a list of what a
/// task did ("3 added, 1 updated").
fn tracks_phrase(count: usize, phrase: TracksPhrase) -> String {
    let n = plural_count(count);
    let message = match phrase {
        // Translators: an item of a list of what happened to the library's
        // tracks, as in "Scan complete: 3 added, 1 updated."
        TracksPhrase::Added => npgettext("count of tracks", "{count} added", "{count} added", n),
        TracksPhrase::Updated => {
            npgettext("count of tracks", "{count} updated", "{count} updated", n)
        }
        TracksPhrase::Missing => {
            npgettext("count of tracks", "{count} missing", "{count} missing", n)
        }
        TracksPhrase::Failed => npgettext("count of tracks", "{count} failed", "{count} failed", n),
        TracksPhrase::Moved => npgettext("count of tracks", "{count} moved", "{count} moved", n),
        TracksPhrase::Pending => {
            npgettext("count of tracks", "{count} pending", "{count} pending", n)
        }
        TracksPhrase::AlreadyOrganized => npgettext(
            "count of tracks",
            "{count} already organized",
            "{count} already organized",
            n,
        ),
        TracksPhrase::Copied => npgettext("count of tracks", "{count} copied", "{count} copied", n),
        TracksPhrase::Removed => {
            npgettext("count of tracks", "{count} removed", "{count} removed", n)
        }
    };
    tr_format!(message, count = count)
}

#[derive(Clone, Copy)]
enum TracksPhrase {
    Added,
    Updated,
    Missing,
    Failed,
    Moved,
    Pending,
    AlreadyOrganized,
    Copied,
    Removed,
}

/// "{count} track(s) failed", the one track-count phrase that names its noun.
fn tracks_failed_phrase(count: usize) -> String {
    tr_format!(
        ngettext(
            "{count} track failed",
            "{count} tracks failed",
            plural_count(count)
        ),
        count = count
    )
}

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
        total = total
    )
}

pub fn library_consolidation_running_text() -> String {
    gettext("Organizing library...")
}

pub fn cd_import_running_text() -> String {
    gettext("Importing CD...")
}

pub fn cd_import_progress_text(completed: usize, total: usize) -> String {
    tr_format!(
        gettext("Importing CD ({completed}/{total})..."),
        completed = completed,
        total = total
    )
}

pub fn cd_import_outcome_text(
    imported: usize,
    requested: usize,
    cancelled: bool,
    failure: Option<&str>,
) -> String {
    if let Some(failure) = failure {
        return tr_format!(
            ngettext(
                "Imported {imported} of {requested} track: {failure}",
                "Imported {imported} of {requested} tracks: {failure}",
                plural_count(requested)
            ),
            imported = imported,
            requested = requested,
            failure = failure
        );
    }
    if cancelled {
        return tr_format!(
            ngettext(
                "CD import stopped: {imported} track imported.",
                "CD import stopped: {imported} tracks imported.",
                plural_count(imported)
            ),
            imported = imported
        );
    }
    tr_format!(
        ngettext(
            "Imported {imported} track from CD.",
            "Imported {imported} tracks from CD.",
            plural_count(imported)
        ),
        imported = imported
    )
}

pub fn analysis_background_running_text(processed: u32, total: u32) -> String {
    // Always render progress as completed/total so the analysis lane
    // reads the same way as every other background task (device sync,
    // online retrieval). The scheduler snapshots `total` once when the run
    // starts so the denominator stays meaningful while its queue refills.
    tr_format!(
        gettext("Analyzing tracks ({processed}/{total})..."),
        processed = processed,
        total = total
    )
}

pub fn analysis_background_outcome_text(completed: u32, failed: u32) -> String {
    match (completed, failed) {
        (_, 0) => tr_format!(
            ngettext(
                "Analyzed {completed} track.",
                "Analyzed {completed} tracks.",
                completed
            ),
            completed = completed
        ),
        (0, _) => tr_format!(
            ngettext(
                "{failed} track could not be analyzed.",
                "{failed} tracks could not be analyzed.",
                failed
            ),
            failed = failed
        ),
        _ => tr_format!(
            // Translators: both halves are the two phrases below, as in
            // "Analyzed 12 tracks; 2 tracks could not be analyzed."
            gettext("{analyzed}; {not_analyzed}."),
            analyzed = tr_format!(
                ngettext(
                    "Analyzed {completed} track",
                    "Analyzed {completed} tracks",
                    completed
                ),
                completed = completed
            ),
            not_analyzed = tr_format!(
                ngettext(
                    "{failed} track could not be analyzed",
                    "{failed} tracks could not be analyzed",
                    failed
                ),
                failed = failed
            )
        ),
    }
}

/// How serious the end of an analysis batch is: a track that could not be
/// analyzed has no BPM, key or beat grid to show for it.
pub fn analysis_background_outcome_severity(failed: u32) -> NotificationSeverity {
    if failed == 0 {
        NotificationSeverity::Info
    } else {
        NotificationSeverity::Warning
    }
}

pub fn online_background_running_text(completed: u32, remaining: u32) -> String {
    // Mirror analysis: progress is always completed/total for a uniform
    // read across the notification lane.
    let total = completed.saturating_add(remaining);
    tr_format!(
        gettext("Retrieving online data ({completed}/{total})..."),
        completed = completed,
        total = total
    )
}

pub fn online_background_outcome_text(completed: u32, failed: u32) -> String {
    if failed == 0 {
        return tr_format!(
            ngettext(
                "Retrieved online data for {completed} track.",
                "Retrieved online data for {completed} tracks.",
                completed
            ),
            completed = completed
        );
    }
    tr_format!(
        // Translators: {tracks} is "{count} track(s)" and {skipped} is
        // "{count} track(s) skipped", as in "Retrieved online data for
        // 40 tracks, 2 tracks skipped."
        gettext("Retrieved online data for {tracks}, {skipped}."),
        tracks = tr_format!(
            ngettext("{count} track", "{count} tracks", completed),
            count = completed
        ),
        skipped = tr_format!(
            ngettext("{count} track skipped", "{count} tracks skipped", failed),
            count = failed
        )
    )
}

pub fn analysis_background_persistence_error_text(detail: &str) -> String {
    tr_format!(
        gettext("Analysis paused: the library database rejected a write ({detail})."),
        detail = detail
    )
}

pub fn online_background_persistence_error_text(detail: &str) -> String {
    tr_format!(
        gettext("Online retrieval paused: the library database rejected a write ({detail})."),
        detail = detail
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
                changes = changes
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
/// changed nothing.
fn scan_change_clauses(summary: &LibraryScanSummary) -> Option<String> {
    let clauses: Vec<String> = [
        (summary.added_tracks, TracksPhrase::Added),
        (summary.updated_tracks, TracksPhrase::Updated),
        (summary.missing_tracks, TracksPhrase::Missing),
        (summary.failed_files, TracksPhrase::Failed),
    ]
    .into_iter()
    .filter(|&(count, _)| count > 0)
    .map(|(count, phrase)| tracks_phrase(count, phrase))
    .collect();
    (!clauses.is_empty()).then(|| phrase_list(&clauses))
}

pub fn library_import_outcome_text(summary: &LibraryImportSummary) -> String {
    if summary.stopped_on_error {
        let changes =
            library_import_change_clauses(summary).unwrap_or_else(|| gettext("no tracks added"));
        return tr_format!(
            gettext("Import stopped after an error: {changes}."),
            changes = changes
        );
    }
    if summary.cancelled {
        return tr_format!(
            ngettext(
                "Import stopped: {count} added before cancel.",
                "Import stopped: {count} added before cancel.",
                plural_count(summary.imported_tracks)
            ),
            count = summary.imported_tracks
        );
    }
    match (
        summary.imported_tracks,
        summary.duplicate_files,
        summary.discovered_files,
        summary.failed_files,
    ) {
        (0, 0, 0, 0) => gettext("No audio files were found."),
        _ => match library_import_change_clauses(summary) {
            Some(changes) => tr_format!(
                // Translators: {changes} is a list such as "3 tracks added,
                // 1 duplicate file skipped", made a sentence of its own.
                gettext("{changes}."),
                changes = changes
            ),
            None => gettext("No changes."),
        },
    }
}

fn library_import_change_clauses(summary: &LibraryImportSummary) -> Option<String> {
    let mut clauses = Vec::new();
    if summary.imported_tracks > 0 {
        clauses.push(tr_format!(
            ngettext(
                "{count} track added",
                "{count} tracks added",
                plural_count(summary.imported_tracks)
            ),
            count = summary.imported_tracks
        ));
    }
    if summary.duplicate_files > 0 {
        clauses.push(tr_format!(
            ngettext(
                "{count} duplicate file skipped",
                "{count} duplicate files skipped",
                plural_count(summary.duplicate_files)
            ),
            count = summary.duplicate_files
        ));
    }
    if summary.failed_files > 0 {
        clauses.push(tr_format!(
            ngettext(
                "{count} file could not be imported",
                "{count} files could not be imported",
                plural_count(summary.failed_files)
            ),
            count = summary.failed_files
        ));
    }
    (!clauses.is_empty()).then(|| phrase_list(&clauses))
}

/// Why an import left a file out, as the end-of-import list shows it.
pub fn import_failure_reason_text(reason: ImportFailureReason) -> String {
    match reason {
        ImportFailureReason::Unreadable => gettext("The file could not be read."),
        ImportFailureReason::UnreadableTags => gettext("Its tags could not be read."),
        ImportFailureReason::CopyFailed => {
            gettext("It could not be copied into the library folder.")
        }
        ImportFailureReason::NoDestination => {
            gettext("No free file name was left for it in the library folder.")
        }
        ImportFailureReason::OutsideLibraryFolder => {
            gettext("It is outside the library folder, and the library keeps files where they are.")
        }
    }
}

pub fn library_consolidation_outcome_text(summary: &LibraryConsolidationSummary) -> String {
    let outcome = if summary.cancelled {
        tr_format!(
            gettext("Library organization stopped: {moved}, {pending}."),
            moved = tracks_phrase(summary.moved_tracks, TracksPhrase::Moved),
            pending = tracks_phrase(
                summary.planned_tracks.saturating_sub(summary.moved_tracks),
                TracksPhrase::Pending
            )
        )
    } else {
        tr_format!(
            gettext("Library organized: {moved}, {organized}, {missing}."),
            moved = tracks_phrase(summary.moved_tracks, TracksPhrase::Moved),
            organized = tracks_phrase(
                summary.already_organized_tracks,
                TracksPhrase::AlreadyOrganized
            ),
            missing = tracks_phrase(summary.missing_tracks, TracksPhrase::Missing)
        )
    };
    if summary.empty_directory_cleanup_failed {
        sentences(&[outcome, gettext("Some empty folders could not be removed.")])
    } else {
        outcome
    }
}

/// Sentences translated one by one, as one message.
fn sentences(sentences: &[String]) -> String {
    sentences.join(" ")
}

pub fn verified_duplicate_progress_text(completed: usize, total: usize) -> String {
    tr_format!(
        gettext("Merging verified duplicates ({completed}/{total})..."),
        completed = completed,
        total = total
    )
}

/// One sentence per thing the user should know: what merged, then each
/// reason a group was left alone.
pub fn verified_duplicate_outcome_text(summary: &VerifiedDuplicateBatchSummary) -> String {
    let merged = summary.merges.len();
    let removed = summary.removed_tracks();
    let mut outcome = vec![tr_format!(
        // Translators: {groups} is "{count} verified duplicate group(s)" and
        // {tracks} is "{count} track(s)".
        gettext("Merged {groups}, removing {tracks}."),
        groups = tr_format!(
            ngettext(
                "{count} verified duplicate group",
                "{count} verified duplicate groups",
                plural_count(merged)
            ),
            count = merged
        ),
        tracks = tr_format!(
            ngettext("{count} track", "{count} tracks", plural_count(removed)),
            count = removed
        )
    )];
    let left_alone: Vec<String> = [
        (summary.changed_groups, GroupsPhrase::Changed),
        (summary.unverified_groups, GroupsPhrase::Unverified),
        (summary.failed_groups, GroupsPhrase::Failed),
        (summary.cancelled_groups, GroupsPhrase::Cancelled),
    ]
    .into_iter()
    .filter(|&(count, _)| count > 0)
    .map(|(count, phrase)| groups_phrase(count, phrase))
    .collect();
    if !left_alone.is_empty() {
        outcome.push(tr_format!(
            gettext("Left alone: {groups}."),
            groups = phrase_list(&left_alone)
        ));
    }
    if summary.cleanup_failed() {
        outcome.push(gettext(
            "Sustain retained recovery files because cleanup could not finish safely.",
        ));
    }
    sentences(&outcome)
}

#[derive(Clone, Copy)]
enum GroupsPhrase {
    Changed,
    Unverified,
    Failed,
    Cancelled,
}

/// "{count} {state}" for a count of duplicate groups left alone.
fn groups_phrase(count: usize, phrase: GroupsPhrase) -> String {
    let n = plural_count(count);
    let message = match phrase {
        // Translators: an item of the list of duplicate groups a merge left
        // alone, as in "Left alone: 2 changed since the preview, 1 failed."
        GroupsPhrase::Changed => npgettext(
            "count of duplicate groups",
            "{count} changed since the preview",
            "{count} changed since the preview",
            n,
        ),
        GroupsPhrase::Unverified => npgettext(
            "count of duplicate groups",
            "{count} no longer verified",
            "{count} no longer verified",
            n,
        ),
        GroupsPhrase::Failed => npgettext(
            "count of duplicate groups",
            "{count} failed",
            "{count} failed",
            n,
        ),
        GroupsPhrase::Cancelled => npgettext(
            "count of duplicate groups",
            "{count} cancelled",
            "{count} cancelled",
            n,
        ),
    };
    tr_format!(message, count = count)
}

pub fn managed_library_cleanup_failed_text() -> String {
    gettext("Some empty managed-library folders could not be removed.")
}

pub fn waveform_cache_unavailable_text() -> String {
    gettext("The waveform cache could not be opened. Waveforms will not be kept this session.")
}

pub fn metadata_write_retry_text() -> String {
    gettext("Some changes could not be mirrored to audio files. Sustain will retry.")
}

pub fn track_reconciliation_retry_text() -> String {
    gettext("Some track changes could not be read from the library database. Sustain will retry.")
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
                plural_count(total)
            ),
            missing = newly_missing,
            total = total,
            unresolved = unresolved
        );
    }
    if newly_missing == 0 {
        return tr_format!(
            ngettext(
                "Library folder updated: all {total} track found.",
                "Library folder updated: all {total} tracks found.",
                plural_count(total)
            ),
            total = total
        );
    }
    tr_format!(
        ngettext(
            "Library folder updated: {missing} of {total} track not found at the new location.",
            "Library folder updated: {missing} of {total} tracks not found at the new location.",
            plural_count(total)
        ),
        missing = newly_missing,
        total = total
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
        ApplicationRuntimeError::CdBackendUnavailable => {
            gettext("CD ripping is not available in this build.")
        }
        ApplicationRuntimeError::CdDiscChanged => {
            gettext("The disc changed. Re-insert the original disc and try again.")
        }
        ApplicationRuntimeError::CdImportFailed => gettext("The CD could not be imported."),
        ApplicationRuntimeError::CdImportNoTracksSelected => {
            gettext("Select at least one track to import.")
        }
        ApplicationRuntimeError::AudioCdDriveBusy => {
            gettext("The disc is being imported. Play it once the import is done.")
        }
    }
}

pub fn device_sync_running_text(label: &str) -> String {
    tr_format!(gettext("Syncing {label}…"), label = label)
}

pub fn device_sync_progress_text(progress: sustain_device_sync::SyncProgress) -> String {
    use sustain_device_sync::SyncStage;
    let (completed, total) = (progress.completed, progress.total);
    match progress.stage {
        SyncStage::Preparing => tr_format!(
            gettext("Preparing tracks ({completed}/{total})…"),
            completed = completed,
            total = total
        ),
        SyncStage::Copying => tr_format!(
            gettext("Copying tracks ({completed}/{total})…"),
            completed = completed,
            total = total
        ),
        SyncStage::WritingPlaylists => gettext("Writing playlists…"),
        SyncStage::WritingDatabase => gettext("Writing device database…"),
        SyncStage::Removing => tr_format!(
            gettext("Removing tracks ({completed}/{total})…"),
            completed = completed,
            total = total
        ),
    }
}

pub fn device_sync_outcome_text(outcome: &sustain_device_sync::SyncOutcome) -> String {
    let failed = outcome.copy_failures.len();
    if outcome.cancelled {
        let mut parts = vec![
            tracks_phrase(outcome.copied, TracksPhrase::Copied),
            tracks_phrase(outcome.updated, TracksPhrase::Updated),
        ];
        if failed > 0 {
            parts.push(tracks_failed_phrase(failed));
        }
        return tr_format!(
            gettext("Sync stopped: {changes}."),
            changes = phrase_list(&parts)
        );
    }
    let changed = outcome.copied + outcome.updated;
    let unplayable = (outcome.unplayable > 0).then(|| {
        tr_format!(
            ngettext(
                "{count} track left off: the device cannot play its format",
                "{count} tracks left off: the device cannot play their format",
                plural_count(outcome.unplayable)
            ),
            count = outcome.unplayable
        )
    });
    if changed == 0 && outcome.removed == 0 && failed == 0 {
        return match unplayable {
            Some(unplayable) => tr_format!(
                gettext("Device already up to date; {unplayable}."),
                unplayable = unplayable
            ),
            None => gettext("Device already up to date."),
        };
    }
    let mut parts = Vec::new();
    if outcome.copied > 0 {
        parts.push(tr_format!(
            ngettext(
                "{count} track added",
                "{count} tracks added",
                plural_count(outcome.copied)
            ),
            count = outcome.copied
        ));
    }
    if outcome.updated > 0 {
        parts.push(tracks_phrase(outcome.updated, TracksPhrase::Updated));
    }
    if outcome.removed > 0 {
        parts.push(tracks_phrase(outcome.removed, TracksPhrase::Removed));
    }
    if failed > 0 {
        parts.push(tracks_failed_phrase(failed));
    }
    let changes = phrase_list(&parts);
    match (failed == 0, unplayable) {
        (true, None) => tr_format!(gettext("Sync complete: {changes}."), changes = changes),
        (true, Some(unplayable)) => tr_format!(
            gettext("Sync complete: {changes}; {unplayable}."),
            changes = changes,
            unplayable = unplayable
        ),
        (false, None) => tr_format!(gettext("Sync partial: {changes}."), changes = changes),
        (false, Some(unplayable)) => tr_format!(
            gettext("Sync partial: {changes}; {unplayable}."),
            changes = changes,
            unplayable = unplayable
        ),
    }
}

#[cfg(test)]
#[path = "notifications_text_tests.rs"]
mod text_tests;

#[cfg(test)]
#[path = "notifications_tests.rs"]
mod tests;
