// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Audio CD playback: playing the tracks of an inserted disc straight from
//! the drive, without ripping them (#197).
//!
//! A disc track is a [`PlayableId::AudioCdTrack`], so it plays through the
//! same queue, transport, Now Playing and media keys as a library track.
//! The runtime keeps the disc being played — its drive and each track's
//! metadata, worked out exactly as a rip would tag it — for as long as its
//! tracks can be played. Nothing of it reaches the library: a disc track
//! has no play count, rating or Smart Shuffle history.
//!
//! The drive serves one reader at a time. Ripping the disc stops its
//! playback, and the disc cannot be played while it is ripped. Ejecting it
//! stops its playback and takes its tracks out of the queue.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;

use sustain_cd_import::{DiscIdentity, TocSnapshot};
use sustain_domain::{
    PlayableId, PlaybackLocation, PlaybackQueue, PlaybackQueueSource, PlaybackSource, TrackMetadata,
};
use sustain_metadata_remote::DiscRelease;

use crate::cd_import::{CdTrackOverride, disc_track_metadata};
use crate::playback::playback_shuffle_seed;
use crate::{ApplicationRuntime, ApplicationRuntimeError, ApplicationRuntimeResult};

/// An inserted disc as the CD page describes it: what its tracks are
/// called is worked out from it exactly as a rip of them would be tagged.
pub struct AudioCdDescription {
    /// The disc snapshot the CD page was built from.
    pub snapshot: TocSnapshot,
    /// The chosen MusicBrainz release, or `None` for the fallback identity.
    pub release: Option<DiscRelease>,
    /// Per-track title/artist the user typed in the CD page.
    pub overrides: BTreeMap<u32, CdTrackOverride>,
}

impl AudioCdDescription {
    fn tracks(&self) -> Vec<AudioCdTrack> {
        self.snapshot
            .tracks
            .iter()
            .map(|toc_track| {
                let mut metadata = disc_track_metadata(
                    &self.snapshot,
                    self.release.as_ref(),
                    self.overrides.get(&toc_track.number),
                    toc_track.number,
                );
                metadata.duration = Some(toc_track.duration());
                AudioCdTrack {
                    number: toc_track.number,
                    metadata,
                }
            })
            .collect()
    }
}

/// One track of the disc being played.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioCdTrack {
    /// Physical track number on the disc.
    pub number: u32,
    /// The track's metadata, as a rip of it would be tagged, with its
    /// duration from the disc's table of contents.
    pub metadata: TrackMetadata,
}

/// The disc whose tracks the player plays.
pub(crate) struct PlayingDisc {
    identity: DiscIdentity,
    tracks: Vec<AudioCdTrack>,
}

impl PlayingDisc {
    fn track(&self, number: u32) -> Option<&AudioCdTrack> {
        self.tracks.iter().find(|track| track.number == number)
    }

    fn playables(&self) -> impl Iterator<Item = PlayableId> + '_ {
        self.tracks.iter().map(|track| PlayableId::AudioCdTrack {
            number: track.number,
        })
    }
}

impl ApplicationRuntime {
    /// Play track `track_number` of `disc`, queueing the whole disc in
    /// its track order. Refused while the disc is ripped: the drive serves
    /// one reader at a time.
    pub fn play_audio_cd(
        &mut self,
        disc: AudioCdDescription,
        track_number: u32,
    ) -> ApplicationRuntimeResult<()> {
        let identity = disc.snapshot.identity();
        if self
            .ripping_disc()
            .is_some_and(|ripping| ripping.device_path() == identity.device_path())
        {
            return Err(ApplicationRuntimeError::AudioCdDriveBusy);
        }
        // The page may still show a disc that was swapped out: play only
        // the disc discovery last saw in the drive.
        if !self
            .optical_discs()
            .iter()
            .any(|inserted| inserted.is_same_disc(&identity))
        {
            return Err(ApplicationRuntimeError::CdDiscChanged);
        }
        let start = PlayableId::AudioCdTrack {
            number: track_number,
        };
        let disc = PlayingDisc {
            identity,
            tracks: disc.tracks(),
        };
        if disc.track(track_number).is_none() {
            return Err(ApplicationRuntimeError::TrackUnavailable);
        }
        let queue = PlaybackQueue::new(
            PlaybackQueueSource::AudioCd,
            disc.playables().collect(),
            start,
            self.playback_queue.options(),
            playback_shuffle_seed(),
        );

        // The disc and its queue take over before the backend is asked to
        // play: a failure, now or reported later by the backend, then ends
        // the disc's playback instead of falling back on the queue it
        // replaced.
        let device = disc.identity.device_path().to_path_buf();
        self.playing_disc = Some(disc);
        self.playback_queue = queue;
        if let Err(error) = self.play_playable(start) {
            self.release_disc_drive(&device);
            return Err(error);
        }
        Ok(())
    }

    /// Adopt a new description of the disc being played — a MusicBrainz
    /// release arrived or was chosen, or the user renamed a track — so Now
    /// Playing and the queue name its tracks as the CD page does. Returns
    /// whether `disc` is the disc being played.
    pub fn redescribe_playing_disc(&mut self, disc: &AudioCdDescription) -> bool {
        let identity = disc.snapshot.identity();
        let Some(playing) = self
            .playing_disc
            .as_mut()
            .filter(|playing| playing.identity == identity)
        else {
            return false;
        };
        playing.tracks = disc.tracks();
        true
    }

    /// The track of the disc being played that the player is on, if a
    /// disc track is loading, playing or paused.
    pub fn playing_audio_cd_track(&self) -> Option<(&DiscIdentity, u32)> {
        let PlayableId::AudioCdTrack { number } = self.playback_state().playable()? else {
            return None;
        };
        Some((self.playing_disc()?, number))
    }

    /// The disc whose tracks the player plays, if any.
    pub fn playing_disc(&self) -> Option<&DiscIdentity> {
        self.playing_disc.as_ref().map(|disc| &disc.identity)
    }

    /// Track `number` of the disc being played.
    pub fn audio_cd_track(&self, number: u32) -> Option<&AudioCdTrack> {
        self.playing_disc.as_ref()?.track(number)
    }

    /// Start a disc track: no file to check and no listening session —
    /// a disc track keeps no statistics.
    pub(crate) fn play_audio_cd_track(&mut self, number: u32) -> ApplicationRuntimeResult<()> {
        let device = self
            .playing_disc
            .as_ref()
            .filter(|disc| disc.track(number).is_some())
            .map(|disc| disc.identity.device_path().to_path_buf())
            .ok_or(ApplicationRuntimeError::TrackUnavailable)?;
        self.settle_current_playing_session();
        self.playback_service()?
            .play_track(PlaybackSource {
                track_id: PlayableId::AudioCdTrack { number },
                location: PlaybackLocation::AudioCd {
                    device,
                    track_number: number,
                },
            })
            .map_err(|_| ApplicationRuntimeError::PlaybackFailed)?;
        self.playback_session = None;
        Ok(())
    }

    /// Give up the disc being played in `device` — its drive is about to
    /// be ripped or ejected, or the disc left it: stop its playback, take
    /// its tracks out of the queue and forget it. Nothing happens when no
    /// disc of that drive is being played.
    pub fn release_disc_drive(&mut self, device: &Path) {
        let Some(disc) = self
            .playing_disc
            .take_if(|disc| disc.identity.device_path() == device)
        else {
            return;
        };
        let playing_disc_track = self
            .playback_state()
            .playable()
            .is_some_and(|playable| playable.library_track_id().is_none());
        if playing_disc_track && let Some(service) = self.playback_service.as_deref() {
            let _ = service.stop();
        }
        let disc_tracks: HashSet<PlayableId> = disc.playables().collect();
        self.playback_queue.remove_tracks(&disc_tracks);
    }

    /// Give up the disc being played if discovery no longer sees it in
    /// its drive.
    pub(crate) fn release_ejected_disc(&mut self) {
        let Some(identity) = self.playing_disc().cloned() else {
            return;
        };
        let inserted = self
            .optical_discs()
            .iter()
            .any(|snapshot| snapshot.is_same_disc(&identity));
        if !inserted {
            self.release_disc_drive(identity.device_path());
        }
    }
}

#[cfg(test)]
#[path = "audio_cd_playback_tests.rs"]
mod tests;
