// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

use std::{cell::RefCell, rc::Rc, time::Duration};

use gst::glib;
use gst::prelude::*;
use gstreamer as gst;
pub use sustain_domain::{PlaybackCommand, PlaybackState, TrackPlaybackSource, VolumePercent};

pub type PlaybackResult<T> = Result<T, PlaybackError>;

const APPLICATION_ID: &str = "io.github.open_sustain.sustain";
const APPLICATION_NAME: &str = "Sustain";
const PULSE_SINK_FACTORY: &str = "pulsesink";
const PULSE_STREAM_PROPERTIES_NAME: &str = "props";

/// Invoked when the currently playing track finishes naturally (end-of-stream).
/// Not invoked for manual stops, pauses, seeks, or `play_track` replacements —
/// only when the audio runs to its end.
pub type TrackEndedCallback = Box<dyn Fn()>;
type SharedTrackEndedCallback = Rc<dyn Fn()>;

/// A fatal asynchronous backend failure reported after a play request was
/// accepted, such as a decoder error discovered during preroll.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlaybackBackendError {
    pub track_id: Option<sustain_domain::TrackId>,
    pub message: String,
}

pub type PlaybackErrorCallback = Box<dyn Fn(PlaybackBackendError)>;
type SharedPlaybackErrorCallback = Rc<dyn Fn(PlaybackBackendError)>;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PlaybackError {
    BackendUnavailable,
    MissingSourcePath,
    PlaybackFailed,
    SourceUriFailed,
}

pub trait PlaybackService {
    fn play_track(&self, source: TrackPlaybackSource) -> PlaybackResult<()>;
    fn pause(&self) -> PlaybackResult<()>;
    fn resume(&self) -> PlaybackResult<()>;
    fn stop(&self) -> PlaybackResult<()>;
    fn seek(&self, position: Duration) -> PlaybackResult<()>;
    fn set_volume(&self, volume: VolumePercent) -> PlaybackResult<()>;
    fn volume(&self) -> VolumePercent;
    fn state(&self) -> PlaybackState;
    fn set_on_track_ended(&self, callback: TrackEndedCallback);
    fn set_on_playback_error(&self, callback: PlaybackErrorCallback);
}

#[derive(Default)]
pub struct NullPlaybackService {
    state: RefCell<PlaybackState>,
    volume: RefCell<VolumePercent>,
    on_track_ended: RefCell<Option<TrackEndedCallback>>,
    on_playback_error: RefCell<Option<PlaybackErrorCallback>>,
}

impl NullPlaybackService {
    pub fn new() -> Self {
        Self::default()
    }
}

impl PlaybackService for NullPlaybackService {
    fn play_track(&self, source: TrackPlaybackSource) -> PlaybackResult<()> {
        if source.path.as_os_str().is_empty() {
            return Err(PlaybackError::MissingSourcePath);
        }

        self.state.replace(PlaybackState::Playing {
            track_id: source.track_id,
            position: Duration::ZERO,
        });
        Ok(())
    }

    fn pause(&self) -> PlaybackResult<()> {
        let current = self.state();
        if let PlaybackState::Playing { track_id, position } = current {
            self.state
                .replace(PlaybackState::Paused { track_id, position });
        }
        Ok(())
    }

    fn resume(&self) -> PlaybackResult<()> {
        let current = self.state();
        if let PlaybackState::Paused { track_id, position } = current {
            self.state
                .replace(PlaybackState::Playing { track_id, position });
        }
        Ok(())
    }

    fn stop(&self) -> PlaybackResult<()> {
        self.state.replace(PlaybackState::Stopped);
        Ok(())
    }

    fn seek(&self, position: Duration) -> PlaybackResult<()> {
        let next = match self.state() {
            PlaybackState::Playing { track_id, .. } => {
                PlaybackState::Playing { track_id, position }
            }
            PlaybackState::Paused { track_id, .. } => PlaybackState::Paused { track_id, position },
            other => other,
        };
        self.state.replace(next);
        Ok(())
    }

    fn set_volume(&self, volume: VolumePercent) -> PlaybackResult<()> {
        self.volume.replace(volume);
        Ok(())
    }

    fn volume(&self) -> VolumePercent {
        *self.volume.borrow()
    }

    fn state(&self) -> PlaybackState {
        self.state.borrow().clone()
    }

    fn set_on_track_ended(&self, callback: TrackEndedCallback) {
        self.on_track_ended.replace(Some(callback));
    }

    fn set_on_playback_error(&self, callback: PlaybackErrorCallback) {
        self.on_playback_error.replace(Some(callback));
    }
}

pub struct GStreamerPlaybackService {
    playbin: gst::Element,
    state: Rc<RefCell<PlaybackState>>,
    volume: RefCell<VolumePercent>,
    on_track_ended: Rc<RefCell<Option<SharedTrackEndedCallback>>>,
    on_playback_error: Rc<RefCell<Option<SharedPlaybackErrorCallback>>>,
    // Bus watch is removed when the guard drops; keep it alive for the
    // lifetime of the service so EOS messages keep reaching us.
    _bus_watch: gst::bus::BusWatchGuard,
}

impl GStreamerPlaybackService {
    pub fn new() -> PlaybackResult<Self> {
        gst::init().map_err(|_| PlaybackError::BackendUnavailable)?;
        let playbin = gst::ElementFactory::make("playbin")
            .build()
            .map_err(|_| PlaybackError::BackendUnavailable)?;
        if let Some(audio_sink) = pulse_audio_sink() {
            playbin.set_property("audio-sink", &audio_sink);
        }

        let state = Rc::new(RefCell::new(PlaybackState::Stopped));
        let on_track_ended: Rc<RefCell<Option<SharedTrackEndedCallback>>> =
            Rc::new(RefCell::new(None));
        let on_playback_error: Rc<RefCell<Option<SharedPlaybackErrorCallback>>> =
            Rc::new(RefCell::new(None));

        let bus = playbin.bus().ok_or(PlaybackError::BackendUnavailable)?;
        let on_eos = on_track_ended.clone();
        let on_error = on_playback_error.clone();
        let bus_state = state.clone();
        let bus_playbin = playbin.clone();
        let bus_watch = bus
            .add_watch_local(move |_bus, message| {
                match message.view() {
                    gst::MessageView::Eos(_) => {
                        let callback = on_eos.borrow().clone();
                        if let Some(callback) = callback {
                            callback();
                        }
                    }
                    gst::MessageView::Error(error) => {
                        let track_id = playback_track_id(&bus_state.borrow());
                        let backend_error = PlaybackBackendError {
                            track_id,
                            message: gst_error_message(error),
                        };
                        let _ = bus_playbin.set_state(gst::State::Null);
                        bus_state.replace(PlaybackState::Stopped);

                        let callback = on_error.borrow().clone();
                        if let Some(callback) = callback {
                            callback(backend_error);
                        }
                    }
                    gst::MessageView::StateChanged(state_changed)
                        if message_source_is_playbin(message, &bus_playbin)
                            && state_changed.current() == gst::State::Playing =>
                    {
                        let loading_track_id = match *bus_state.borrow() {
                            PlaybackState::Loading { track_id } => Some(track_id),
                            _ => None,
                        };
                        if let Some(track_id) = loading_track_id {
                            bus_state.replace(PlaybackState::Playing {
                                track_id,
                                position: Duration::ZERO,
                            });
                        }
                    }
                    gst::MessageView::Warning(warning) => {
                        eprintln!(
                            "Sustain: GStreamer warning: {}",
                            gst_warning_message(warning)
                        );
                    }
                    _ => {}
                }
                glib::ControlFlow::Continue
            })
            .map_err(|_| PlaybackError::BackendUnavailable)?;

        Ok(Self {
            playbin,
            state,
            volume: RefCell::new(VolumePercent::default()),
            on_track_ended,
            on_playback_error,
            _bus_watch: bus_watch,
        })
    }
}

impl PlaybackService for GStreamerPlaybackService {
    fn play_track(&self, source: TrackPlaybackSource) -> PlaybackResult<()> {
        if source.path.as_os_str().is_empty() {
            return Err(PlaybackError::MissingSourcePath);
        }

        let uri = gst::glib::filename_to_uri(&source.path, None)
            .map_err(|_| PlaybackError::SourceUriFailed)?;

        self.playbin
            .set_state(gst::State::Null)
            .map_err(|_| PlaybackError::PlaybackFailed)?;
        self.playbin.set_property("uri", uri.as_str());
        self.state.replace(PlaybackState::Loading {
            track_id: source.track_id,
        });
        let state_change = self
            .playbin
            .set_state(gst::State::Playing)
            .map_err(|_| PlaybackError::PlaybackFailed)?;
        if !matches!(state_change, gst::StateChangeSuccess::Async) {
            self.state.replace(PlaybackState::Playing {
                track_id: source.track_id,
                position: Duration::ZERO,
            });
        }

        Ok(())
    }

    fn pause(&self) -> PlaybackResult<()> {
        let current = self.state();
        let PlaybackState::Playing { track_id, position } = current else {
            return Ok(());
        };
        self.playbin
            .set_state(gst::State::Paused)
            .map_err(|_| PlaybackError::PlaybackFailed)?;
        self.state
            .replace(PlaybackState::Paused { track_id, position });

        Ok(())
    }

    fn resume(&self) -> PlaybackResult<()> {
        let current = self.state();
        let PlaybackState::Paused { track_id, position } = current else {
            return Ok(());
        };
        self.playbin
            .set_state(gst::State::Playing)
            .map_err(|_| PlaybackError::PlaybackFailed)?;
        self.state
            .replace(PlaybackState::Playing { track_id, position });

        Ok(())
    }

    fn stop(&self) -> PlaybackResult<()> {
        self.playbin
            .set_state(gst::State::Null)
            .map_err(|_| PlaybackError::PlaybackFailed)?;
        self.state.replace(PlaybackState::Stopped);

        Ok(())
    }

    fn seek(&self, position: Duration) -> PlaybackResult<()> {
        let next = match self.state() {
            PlaybackState::Playing { track_id, .. } => {
                PlaybackState::Playing { track_id, position }
            }
            PlaybackState::Paused { track_id, .. } => PlaybackState::Paused { track_id, position },
            other => {
                self.state.replace(other);
                return Ok(());
            }
        };
        self.playbin
            .seek_simple(
                gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
                clock_time_from_duration(position),
            )
            .map_err(|_| PlaybackError::PlaybackFailed)?;

        self.state.replace(next);

        Ok(())
    }

    fn set_volume(&self, volume: VolumePercent) -> PlaybackResult<()> {
        self.playbin.set_property("volume", volume.as_scalar());
        self.volume.replace(volume);
        Ok(())
    }

    fn volume(&self) -> VolumePercent {
        *self.volume.borrow()
    }

    fn state(&self) -> PlaybackState {
        match self.state.borrow().clone() {
            PlaybackState::Playing { track_id, position } => PlaybackState::Playing {
                track_id,
                position: self.current_position().unwrap_or(position),
            },
            PlaybackState::Paused { track_id, position } => PlaybackState::Paused {
                track_id,
                position: self.current_position().unwrap_or(position),
            },
            state => state,
        }
    }

    fn set_on_track_ended(&self, callback: TrackEndedCallback) {
        self.on_track_ended.replace(Some(Rc::from(callback)));
    }

    fn set_on_playback_error(&self, callback: PlaybackErrorCallback) {
        self.on_playback_error.replace(Some(Rc::from(callback)));
    }
}

impl GStreamerPlaybackService {
    fn current_position(&self) -> Option<Duration> {
        self.playbin
            .query_position::<gst::ClockTime>()
            .map(duration_from_clock_time)
    }
}

impl Drop for GStreamerPlaybackService {
    fn drop(&mut self) {
        let _ = self.playbin.set_state(gst::State::Null);
    }
}

fn clock_time_from_duration(duration: Duration) -> gst::ClockTime {
    gst::ClockTime::from_nseconds(duration.as_nanos().min(u128::from(u64::MAX)) as u64)
}

fn duration_from_clock_time(clock_time: gst::ClockTime) -> Duration {
    Duration::from_nanos(clock_time.nseconds())
}

fn playback_track_id(state: &PlaybackState) -> Option<sustain_domain::TrackId> {
    match state {
        PlaybackState::Loading { track_id }
        | PlaybackState::Playing { track_id, .. }
        | PlaybackState::Paused { track_id, .. } => Some(*track_id),
        PlaybackState::Stopped => None,
    }
}

fn message_source_is_playbin(message: &gst::MessageRef, playbin: &gst::Element) -> bool {
    message
        .src()
        .is_some_and(|source| source.as_ptr() == playbin.upcast_ref::<gst::Object>().as_ptr())
}

fn gst_error_message(error: &gst::message::Error) -> String {
    match error.debug() {
        Some(debug) => format!("{} ({debug})", error.error()),
        None => error.error().to_string(),
    }
}

fn gst_warning_message(warning: &gst::message::Warning) -> String {
    match warning.debug() {
        Some(debug) => format!("{} ({debug})", warning.error()),
        None => warning.error().to_string(),
    }
}

fn pulse_audio_sink() -> Option<gst::Element> {
    let sink = gst::ElementFactory::make(PULSE_SINK_FACTORY).build().ok()?;
    sink.set_property("client-name", APPLICATION_NAME);
    sink.set_property("stream-properties", pulse_stream_properties());
    Some(sink)
}

fn pulse_stream_properties() -> gst::Structure {
    gst::Structure::builder(PULSE_STREAM_PROPERTIES_NAME)
        .field("application.id", APPLICATION_ID)
        .field("application.name", APPLICATION_NAME)
        .field("media.role", "music")
        .build()
}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, time::Duration};

    use gstreamer as gst;
    use sustain_domain::TrackId;

    use super::{
        APPLICATION_ID, APPLICATION_NAME, NullPlaybackService, PULSE_STREAM_PROPERTIES_NAME,
        PlaybackError, PlaybackService, PlaybackState, VolumePercent, pulse_stream_properties,
    };
    use crate::TrackPlaybackSource;

    #[test]
    fn null_service_starts_stopped() {
        let playback = NullPlaybackService::new();

        assert_eq!(playback.state(), PlaybackState::Stopped);
    }

    #[test]
    fn null_service_tracks_basic_state_transitions() {
        let playback = NullPlaybackService::new();
        let track_id = positive_track_id();

        assert_eq!(
            playback.play_track(TrackPlaybackSource::new(
                track_id,
                PathBuf::from("/music/track.flac")
            )),
            Ok(())
        );
        assert_eq!(
            playback.state(),
            PlaybackState::Playing {
                track_id,
                position: Duration::ZERO
            }
        );

        assert_eq!(playback.seek(Duration::from_secs(42)), Ok(()));
        assert_eq!(
            playback.state(),
            PlaybackState::Playing {
                track_id,
                position: Duration::from_secs(42)
            }
        );

        assert_eq!(playback.pause(), Ok(()));
        assert_eq!(
            playback.state(),
            PlaybackState::Paused {
                track_id,
                position: Duration::from_secs(42)
            }
        );

        assert_eq!(playback.resume(), Ok(()));
        assert_eq!(
            playback.state(),
            PlaybackState::Playing {
                track_id,
                position: Duration::from_secs(42)
            }
        );

        assert_eq!(playback.stop(), Ok(()));
        assert_eq!(playback.state(), PlaybackState::Stopped);
    }

    #[test]
    fn null_service_rejects_missing_source_path() {
        let playback = NullPlaybackService::new();

        assert_eq!(
            playback.play_track(TrackPlaybackSource::new(
                positive_track_id(),
                PathBuf::new()
            )),
            Err(PlaybackError::MissingSourcePath)
        );
    }

    #[test]
    fn null_service_tracks_volume() {
        let playback = NullPlaybackService::new();
        let volume = VolumePercent::new(42).expect("valid test volume");

        assert_eq!(playback.set_volume(volume), Ok(()));

        assert_eq!(playback.volume(), volume);
    }

    #[test]
    fn pulse_stream_properties_identify_sustain_as_music() {
        gst::init().expect("GStreamer init");
        let properties = pulse_stream_properties();

        assert_eq!(properties.name().as_str(), PULSE_STREAM_PROPERTIES_NAME);
        assert_eq!(
            properties
                .get::<String>("application.id")
                .expect("application id property"),
            APPLICATION_ID
        );
        assert_eq!(
            properties
                .get::<String>("application.name")
                .expect("application name property"),
            APPLICATION_NAME
        );
        assert_eq!(
            properties
                .get::<String>("media.role")
                .expect("media role property"),
            "music"
        );
    }

    fn positive_track_id() -> TrackId {
        match TrackId::new(1) {
            Some(track_id) => track_id,
            None => unreachable!("hard-coded positive track id should be valid"),
        }
    }
}
