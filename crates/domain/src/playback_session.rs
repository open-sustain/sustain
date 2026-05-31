// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::time::Duration;

use crate::TrackId;

/// Hard ceiling on the play threshold for long-form tracks (podcasts,
/// DJ mixes, audiobook chapters). Once the listener has spent this long
/// on a track, the play counts even if the track is longer than 20
/// minutes (in which case the duration/2 rule would otherwise require
/// >10 min of listening).
const PLAY_THRESHOLD_CEILING: Duration = Duration::from_secs(10 * 60);

/// Tracks how much of the currently playing track the listener has
/// actually heard, and whether a play has been registered for this
/// listening session.
///
/// A "play" is registered exactly once per session, the first time the
/// cumulative listened time crosses the threshold returned by
/// [`PlaybackSession::play_threshold`]. After that, further listening
/// in the same session does NOT increment the play count again. A new
/// session begins each time playback starts for a track.
///
/// Cumulative listened time is intentionally distinct from raw playback
/// position: seeking forward does NOT count as listening, pausing and
/// resuming preserves accumulated time, and replaying a section adds to
/// the cumulative total.
#[derive(Clone, Debug, PartialEq)]
pub struct PlaybackSession {
    session_id: u64,
    track_id: TrackId,
    duration: Duration,
    listened: Duration,
    accounting_baseline: Option<Duration>,
    play_registration_pending: bool,
    play_registered: bool,
}

impl PlaybackSession {
    /// Begin a new session for the given track. A fresh session has
    /// zero listened time and has not yet registered a play.
    pub const fn new(session_id: u64, track_id: TrackId, duration: Duration) -> Self {
        Self {
            session_id,
            track_id,
            duration,
            listened: Duration::ZERO,
            accounting_baseline: None,
            play_registration_pending: false,
            play_registered: false,
        }
    }

    /// Threshold of cumulative listened time at which a play registers.
    /// Returns `min(duration / 2, 10 minutes)`: short tracks must be
    /// heard halfway through, long tracks need only 10 minutes of
    /// actual listening.
    pub fn play_threshold(duration: Duration) -> Duration {
        let half = duration / 2;
        half.min(PLAY_THRESHOLD_CEILING)
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    pub fn track_id(&self) -> TrackId {
        self.track_id
    }

    pub fn duration(&self) -> Duration {
        self.duration
    }

    pub fn listened(&self) -> Duration {
        self.listened
    }

    pub fn is_play_registered(&self) -> bool {
        self.play_registered
    }

    pub fn is_play_registration_pending(&self) -> bool {
        self.play_registration_pending
    }

    /// Anchor future accounting at `now` without counting any interval
    /// before it. Called when playback starts or resumes.
    pub fn resume_accounting(&mut self, now: Duration) {
        self.accounting_baseline = Some(now);
    }

    /// Account real monotonic elapsed time while playback is active.
    /// Delayed callbacks therefore add the full interval rather than one
    /// assumed timer period. A backwards-moving injected clock safely adds
    /// zero while still re-anchoring the baseline.
    pub fn account_playing_until(&mut self, now: Duration) {
        if let Some(previous) = self.accounting_baseline.replace(now) {
            self.listened = self.listened.saturating_add(now.saturating_sub(previous));
        }
    }

    /// Account listening up to `now`, then stop the baseline. Paused time
    /// cannot leak into the next resume interval.
    pub fn freeze_accounting(&mut self, now: Duration) {
        if self.accounting_baseline.is_some() {
            self.account_playing_until(now);
            self.accounting_baseline = None;
        }
    }

    /// Returns true when the cumulative listened time has reached the
    /// play threshold and no registration is already queued or committed.
    pub fn should_begin_play_registration(&self) -> bool {
        if self.play_registration_pending || self.play_registered {
            return false;
        }
        if self.duration.is_zero() {
            return false;
        }
        self.listened >= Self::play_threshold(self.duration)
    }

    /// Reserve this session's one play-registration attempt before SQLite
    /// persistence begins. Returns false when the threshold has not been
    /// reached or an attempt was already reserved.
    pub fn begin_play_registration(&mut self) -> bool {
        if !self.should_begin_play_registration() {
            return false;
        }
        self.play_registration_pending = true;
        true
    }

    /// Confirm the reserved registration after authoritative persistence.
    pub fn confirm_play_registration(&mut self) {
        self.play_registration_pending = false;
        self.play_registered = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track() -> TrackId {
        TrackId::new(1).expect("valid test track id")
    }

    #[test]
    fn play_threshold_is_half_duration_for_short_tracks() {
        let three_minutes = Duration::from_secs(180);
        assert_eq!(
            PlaybackSession::play_threshold(three_minutes),
            Duration::from_secs(90)
        );
    }

    #[test]
    fn play_threshold_caps_at_ten_minutes_for_long_tracks() {
        let one_hour = Duration::from_secs(3600);
        assert_eq!(
            PlaybackSession::play_threshold(one_hour),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn play_threshold_for_twenty_minute_track_is_ten_minutes() {
        let twenty_minutes = Duration::from_secs(20 * 60);
        assert_eq!(
            PlaybackSession::play_threshold(twenty_minutes),
            Duration::from_secs(600)
        );
    }

    #[test]
    fn fresh_session_does_not_register_play() {
        let session = PlaybackSession::new(1, track(), Duration::from_secs(180));
        assert!(!session.should_begin_play_registration());
        assert!(!session.is_play_registered());
    }

    #[test]
    fn play_registers_once_threshold_crossed() {
        let mut session = PlaybackSession::new(1, track(), Duration::from_secs(180));
        session.resume_accounting(Duration::ZERO);
        session.account_playing_until(Duration::from_secs(89));
        assert!(!session.should_begin_play_registration());
        session.account_playing_until(Duration::from_secs(90));
        assert!(session.should_begin_play_registration());
    }

    #[test]
    fn play_does_not_re_register_within_same_session() {
        let mut session = PlaybackSession::new(1, track(), Duration::from_secs(180));
        session.resume_accounting(Duration::ZERO);
        session.account_playing_until(Duration::from_secs(120));
        assert!(session.begin_play_registration());
        assert!(!session.should_begin_play_registration());
        session.confirm_play_registration();
        assert!(!session.should_begin_play_registration());
        session.account_playing_until(Duration::from_secs(180));
        assert!(!session.should_begin_play_registration());
    }

    #[test]
    fn long_track_registers_at_ten_minute_ceiling() {
        let mut session = PlaybackSession::new(1, track(), Duration::from_secs(3600));
        session.resume_accounting(Duration::ZERO);
        session.account_playing_until(Duration::from_secs(599));
        assert!(!session.should_begin_play_registration());
        session.account_playing_until(Duration::from_secs(600));
        assert!(session.should_begin_play_registration());
    }

    #[test]
    fn zero_duration_track_never_registers_play() {
        let mut session = PlaybackSession::new(1, track(), Duration::ZERO);
        session.resume_accounting(Duration::ZERO);
        session.account_playing_until(Duration::from_secs(60));
        assert!(!session.should_begin_play_registration());
    }

    #[test]
    fn delayed_accounting_uses_real_monotonic_delta() {
        let mut session = PlaybackSession::new(1, track(), Duration::from_secs(180));
        session.resume_accounting(Duration::from_secs(10));
        session.account_playing_until(Duration::from_secs(75));

        assert_eq!(session.listened(), Duration::from_secs(65));
    }

    #[test]
    fn frozen_accounting_excludes_paused_time() {
        let mut session = PlaybackSession::new(1, track(), Duration::from_secs(180));
        session.resume_accounting(Duration::from_secs(10));
        session.freeze_accounting(Duration::from_secs(20));
        session.resume_accounting(Duration::from_secs(100));
        session.account_playing_until(Duration::from_secs(105));

        assert_eq!(session.listened(), Duration::from_secs(15));
    }
}
