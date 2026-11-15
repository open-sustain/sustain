// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use sustain_cd_import::{RawTocTrack, TocSnapshot};
use sustain_domain::{
    PlayableId, PlaybackCommand, PlaybackOptions, PlaybackQueueSource, PlaybackState, RepeatMode,
    ShuffleMode,
};
use sustain_playback::NullPlaybackService;

use super::{AudioCdDescription, AudioCdTrack};
use crate::{
    ApplicationCommand, ApplicationRuntime, ApplicationRuntimeError, CdTrackOverride,
    NowPlayingItem, OpticalDiscoveryResult,
};

const DEVICE: &str = "/dev/sr0";

fn disc() -> TocSnapshot {
    TocSnapshot::from_raw(
        PathBuf::from(DEVICE),
        "disc-id".to_owned(),
        String::new(),
        &[
            RawTocTrack {
                number: 1,
                offset: 150,
                sectors: 15_000,
            },
            RawTocTrack {
                number: 2,
                offset: 15_150,
                sectors: 18_000,
            },
            RawTocTrack {
                number: 3,
                offset: 33_150,
                sectors: 7_500,
            },
        ],
    )
}

fn playing_runtime() -> ApplicationRuntime {
    let mut runtime =
        ApplicationRuntime::new().with_playback_service(Box::new(NullPlaybackService::new()));
    runtime.optical_discs = vec![disc()];
    runtime
}

fn description() -> AudioCdDescription {
    AudioCdDescription {
        snapshot: disc(),
        release: None,
        overrides: BTreeMap::new(),
    }
}

fn now_playing_disc_track(runtime: &ApplicationRuntime) -> AudioCdTrack {
    match runtime.now_playing().item {
        Some(NowPlayingItem::AudioCd(track)) => Some(track),
        _ => None,
    }
    .expect("a disc track is playing")
}

fn disc_track(number: u32) -> PlayableId {
    PlayableId::AudioCdTrack { number }
}

fn playback(runtime: &mut ApplicationRuntime, command: PlaybackCommand) {
    runtime
        .handle_command(ApplicationCommand::Playback(command))
        .expect("playback command");
}

#[test]
fn playing_a_disc_track_queues_the_whole_disc_in_order() {
    let mut runtime = playing_runtime();

    runtime.play_audio_cd(description(), 2).expect("play disc");

    assert_eq!(runtime.playback_state().playable(), Some(disc_track(2)));
    assert_eq!(
        runtime.playback_queue.source(),
        &PlaybackQueueSource::AudioCd
    );
    assert_eq!(
        runtime.playback_queue.ordered_track_ids(),
        [disc_track(1), disc_track(2), disc_track(3)]
    );
    assert_eq!(runtime.playback_queue_next_track_id(), Some(disc_track(3)));
}

#[test]
fn the_disc_plays_on_through_the_transport() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 2).expect("play disc");

    playback(&mut runtime, PlaybackCommand::PlayNextTrack);
    assert_eq!(runtime.playback_state().playable(), Some(disc_track(3)));

    playback(&mut runtime, PlaybackCommand::PlayPreviousTrack);
    assert_eq!(runtime.playback_state().playable(), Some(disc_track(2)));
}

#[test]
fn now_playing_shows_the_disc_track_as_a_rip_would_tag_it() {
    let mut runtime = playing_runtime();
    let mut disc_description = description();
    disc_description.overrides.insert(
        1,
        CdTrackOverride {
            title: Some("Opening".to_owned()),
            artist: None,
        },
    );

    runtime
        .play_audio_cd(disc_description, 1)
        .expect("play disc");

    let track = now_playing_disc_track(&runtime);
    assert_eq!(track.number, 1);
    assert_eq!(track.metadata.title.as_deref(), Some("Opening"));
    assert_eq!(track.metadata.duration, Some(Duration::from_secs(200)));
}

#[test]
fn a_disc_track_keeps_no_listening_statistics() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");

    runtime.on_playback_tick().expect("tick");

    assert!(runtime.playback_session.is_none());
}

#[test]
fn smart_shuffle_plays_a_disc_in_pure_shuffle() {
    let mut runtime = playing_runtime();
    playback(
        &mut runtime,
        PlaybackCommand::SetShuffleMode(ShuffleMode::Smart),
    );

    runtime.play_audio_cd(description(), 1).expect("play disc");

    assert!(!runtime.playback_queue_uses_lazy_continuation());
    assert_eq!(
        runtime.playback_options(),
        PlaybackOptions {
            shuffle_mode: ShuffleMode::Smart,
            repeat_mode: RepeatMode::Off,
        }
    );
    assert!(runtime.playback_queue_next_track_id().is_some());
}

#[test]
fn a_disc_being_ripped_cannot_be_played() {
    let mut runtime = playing_runtime();
    runtime.ripping_disc = Some(disc().identity());

    assert_eq!(
        runtime.play_audio_cd(description(), 1),
        Err(ApplicationRuntimeError::AudioCdDriveBusy)
    );
    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);
    assert!(runtime.playing_disc().is_none());
}

#[test]
fn a_track_the_disc_does_not_hold_is_refused() {
    let mut runtime = playing_runtime();

    assert_eq!(
        runtime.play_audio_cd(description(), 9),
        Err(ApplicationRuntimeError::TrackUnavailable)
    );
    assert!(runtime.playing_disc().is_none());
}

#[test]
fn ejecting_the_disc_stops_it_and_empties_it_from_the_queue() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");

    let generation = runtime.optical_discovery_generation;
    assert!(runtime.apply_optical_discovery(OpticalDiscoveryResult {
        generation,
        discs: Vec::new(),
    }));

    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);
    assert!(runtime.playback_queue.ordered_track_ids().is_empty());
    assert!(runtime.playing_disc().is_none());
    assert!(runtime.now_playing().item.is_none());
}

#[test]
fn rediscovering_the_same_disc_keeps_it_playing() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");
    let other_disc = TocSnapshot::from_raw(
        PathBuf::from("/dev/sr1"),
        "other".to_owned(),
        String::new(),
        &[RawTocTrack {
            number: 1,
            offset: 150,
            sectors: 7_500,
        }],
    );

    let generation = runtime.optical_discovery_generation;
    assert!(runtime.apply_optical_discovery(OpticalDiscoveryResult {
        generation,
        discs: vec![disc(), other_disc],
    }));

    assert_eq!(runtime.playback_state().playable(), Some(disc_track(1)));
    assert!(runtime.playing_disc().is_some());
}

#[test]
fn ripping_the_drive_takes_it_from_playback() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");

    runtime.release_disc_drive(&PathBuf::from(DEVICE));

    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);
    assert!(runtime.playback_queue.ordered_track_ids().is_empty());
    assert!(runtime.playing_disc().is_none());
}

#[test]
fn a_new_description_renames_the_disc_being_played() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 2).expect("play disc");
    let mut renamed = description();
    renamed.overrides.insert(
        2,
        CdTrackOverride {
            title: Some("Interlude".to_owned()),
            artist: Some("Someone".to_owned()),
        },
    );

    assert!(runtime.redescribe_playing_disc(&renamed));

    let track = now_playing_disc_track(&runtime);
    assert_eq!(track.metadata.title.as_deref(), Some("Interlude"));
    assert_eq!(track.metadata.artist.as_deref(), Some("Someone"));
    assert_eq!(
        runtime.playing_audio_cd_track().map(|(_, number)| number),
        Some(2)
    );
}

#[test]
fn another_disc_description_leaves_the_disc_being_played_alone() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");
    let other = AudioCdDescription {
        snapshot: TocSnapshot::from_raw(
            PathBuf::from(DEVICE),
            "another-disc".to_owned(),
            String::new(),
            &[RawTocTrack {
                number: 1,
                offset: 150,
                sectors: 7_500,
            }],
        ),
        release: None,
        overrides: BTreeMap::new(),
    };

    assert!(!runtime.redescribe_playing_disc(&other));
}

#[test]
fn a_library_track_that_cannot_play_leaves_the_disc_playing() {
    let mut runtime = playing_runtime();
    runtime.play_audio_cd(description(), 1).expect("play disc");

    let library_play =
        runtime.handle_command(ApplicationCommand::Playback(PlaybackCommand::PlayTrack {
            track_id: sustain_domain::TrackId::new(1).expect("track id"),
            queue: sustain_domain::PlaybackQueueRequest {
                source: PlaybackQueueSource::Library,
                ordered_track_ids: Vec::new(),
            },
        }));

    // No such library track: the disc keeps playing.
    assert_eq!(library_play, Err(ApplicationRuntimeError::TrackUnavailable));
    assert_eq!(
        runtime.playing_audio_cd_track().map(|(_, number)| number),
        Some(1)
    );
}

#[test]
fn a_disc_no_longer_in_its_drive_is_refused() {
    let mut runtime = playing_runtime();
    let swapped = AudioCdDescription {
        snapshot: TocSnapshot::from_raw(
            PathBuf::from(DEVICE),
            "swapped-out".to_owned(),
            String::new(),
            &[RawTocTrack {
                number: 1,
                offset: 150,
                sectors: 7_500,
            }],
        ),
        release: None,
        overrides: BTreeMap::new(),
    };

    assert_eq!(
        runtime.play_audio_cd(swapped, 1),
        Err(ApplicationRuntimeError::CdDiscChanged)
    );
    assert_eq!(runtime.playback_state(), PlaybackState::Stopped);
    assert!(runtime.playing_disc().is_none());
}

#[test]
fn a_disc_that_fails_to_start_leaves_nothing_of_it_queued() {
    // A drive path the backend cannot open: starting the disc fails.
    let unreadable = TocSnapshot::from_raw(
        PathBuf::new(),
        "disc-id".to_owned(),
        String::new(),
        &[RawTocTrack {
            number: 1,
            offset: 150,
            sectors: 7_500,
        }],
    );
    let mut runtime =
        ApplicationRuntime::new().with_playback_service(Box::new(NullPlaybackService::new()));
    runtime.optical_discs = vec![unreadable.clone()];

    let started = runtime.play_audio_cd(
        AudioCdDescription {
            snapshot: unreadable,
            release: None,
            overrides: BTreeMap::new(),
        },
        1,
    );

    assert_eq!(started, Err(ApplicationRuntimeError::PlaybackFailed));
    assert!(runtime.playing_disc().is_none());
    assert!(runtime.playback_queue.ordered_track_ids().is_empty());
    assert_eq!(runtime.playback_queue_next_track_id(), None);
}
