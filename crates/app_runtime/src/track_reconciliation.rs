// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The tracks whose committed change the runtime has not read back yet.
//!
//! A worker that commits a change to a track announces the track's id on
//! the track-updated channel, and the runtime adopts the change by
//! reading the row back from the library store. When that read fails,
//! the change is in SQLite but in neither the runtime nor the views. The
//! id is then kept here, and announced again on the same channel after a
//! backoff, until a read of the track succeeds or finds it gone (#276).
//!
//! Each retry reads the row as SQLite holds it then, so a retry adopts
//! whatever was committed last and never replays an older snapshot; a
//! track deleted in the meantime reads back absent, which resolves it
//! without bringing it back. Ids are kept once each, so the retries never
//! outgrow the library, and a single timer thread at a time announces
//! all of them.

use std::{
    collections::BTreeSet,
    sync::{Arc, Mutex, PoisonError},
    thread,
    time::Duration,
};

use crate::{NotificationId, TrackId};

/// The wait before the first retry, doubled after each failed one.
const FIRST_RETRY_DELAY: Duration = Duration::from_secs(1);

/// The longest wait between two retries, so a store that keeps failing is
/// retried at a slow steady pace rather than not at all.
const MAX_RETRY_DELAY: Duration = Duration::from_secs(60);

#[derive(Default)]
pub(crate) struct TrackReconciliation {
    pending: Arc<Mutex<Pending>>,
    /// Retries scheduled since no id was last left unresolved; sets the
    /// backoff.
    retries: u32,
    /// The notification shown while ids are unresolved.
    notification: Option<NotificationId>,
}

/// The state the runtime shares with the retry timer.
#[derive(Default)]
struct Pending {
    unresolved: BTreeSet<TrackId>,
    retry_scheduled: bool,
}

impl TrackReconciliation {
    /// Keep `track_id`, whose read back failed, and make sure a retry is
    /// scheduled on `sink`; without a sink, the id waits for its next
    /// announcement. Returns whether the id was not kept already.
    pub(crate) fn read_failed(
        &mut self,
        track_id: TrackId,
        sink: Option<&async_channel::Sender<TrackId>>,
    ) -> bool {
        let (newly_unresolved, schedule) = {
            let mut pending = self.lock();
            let newly_unresolved = pending.unresolved.insert(track_id);
            let schedule = sink.is_some() && !pending.retry_scheduled;
            pending.retry_scheduled |= schedule;
            (newly_unresolved, schedule)
        };
        if schedule && let Some(sink) = sink {
            let delay = retry_delay(self.retries);
            self.retries = self.retries.saturating_add(1);
            self.schedule_retry(delay, sink.clone());
        }
        newly_unresolved
    }

    /// Forget `track_id`, read back or found gone. Returns whether no id
    /// is left unresolved.
    pub(crate) fn resolved(&mut self, track_id: TrackId) -> bool {
        let settled = {
            let mut pending = self.lock();
            pending.unresolved.remove(&track_id);
            pending.unresolved.is_empty()
        };
        if settled {
            self.retries = 0;
        }
        settled
    }

    #[cfg(test)]
    pub(crate) fn unresolved_count(&self) -> usize {
        self.lock().unresolved.len()
    }

    pub(crate) fn notification(&self) -> Option<NotificationId> {
        self.notification
    }

    pub(crate) fn set_notification(&mut self, notification: Option<NotificationId>) {
        self.notification = notification;
    }

    fn schedule_retry(&self, delay: Duration, sink: async_channel::Sender<TrackId>) {
        let pending = Arc::clone(&self.pending);
        let spawned = thread::Builder::new()
            .name("sustain-track-reconciliation".to_owned())
            .spawn(move || {
                thread::sleep(delay);
                let unresolved = {
                    let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                    pending.retry_scheduled = false;
                    pending.unresolved.clone()
                };
                for track_id in unresolved {
                    // A closed channel means the UI shell is gone and no
                    // one is left to adopt the change.
                    if sink.send_blocking(track_id).is_err() {
                        return;
                    }
                }
            });
        if let Err(error) = spawned {
            eprintln!("Sustain: could not schedule a track reconciliation retry: {error}");
            self.lock().retry_scheduled = false;
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Pending> {
        // Pending holds plain values that every critical section leaves
        // consistent, so a panic elsewhere cannot leave it half-updated.
        self.pending.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// The wait before retry number `retries` (counting from zero).
fn retry_delay(retries: u32) -> Duration {
    FIRST_RETRY_DELAY
        .checked_mul(1_u32.checked_shl(retries).unwrap_or(u32::MAX))
        .map_or(MAX_RETRY_DELAY, |delay| delay.min(MAX_RETRY_DELAY))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track_id(id: i64) -> TrackId {
        TrackId::new(id).expect("non-zero track id")
    }

    #[test]
    fn retries_back_off_up_to_a_steady_pace() {
        assert_eq!(retry_delay(0), Duration::from_secs(1));
        assert_eq!(retry_delay(1), Duration::from_secs(2));
        assert_eq!(retry_delay(5), Duration::from_secs(32));
        assert_eq!(retry_delay(6), MAX_RETRY_DELAY);
        assert_eq!(retry_delay(40), MAX_RETRY_DELAY);
    }

    #[test]
    fn failed_reads_are_kept_once_each_until_resolved() {
        let mut reconciliation = TrackReconciliation::default();

        assert!(reconciliation.read_failed(track_id(1), None));
        assert!(!reconciliation.read_failed(track_id(1), None));
        assert!(reconciliation.read_failed(track_id(2), None));
        assert_eq!(reconciliation.unresolved_count(), 2);

        assert!(!reconciliation.resolved(track_id(1)));
        assert!(reconciliation.resolved(track_id(2)));
        assert_eq!(reconciliation.unresolved_count(), 0);
    }

    #[test]
    fn one_retry_announces_every_unresolved_track_once() {
        let (sink, announced) = async_channel::unbounded();
        let mut reconciliation = TrackReconciliation::default();

        reconciliation.read_failed(track_id(1), Some(&sink));
        // The retry for the first failure is already scheduled: the
        // second failure joins it rather than scheduling its own.
        reconciliation.read_failed(track_id(2), Some(&sink));

        let first = announced.recv_blocking().expect("first retry");
        let second = announced.recv_blocking().expect("second retry");
        assert_eq!((first, second), (track_id(1), track_id(2)));
        assert!(announced.is_empty(), "each track is announced once");
        assert_eq!(
            reconciliation.unresolved_count(),
            2,
            "a retry announces tracks; only a successful read resolves them"
        );
    }
}
