// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! MPRIS / media-key bridge: projects the runtime's now-playing state into
//! desktop metadata and routes incoming MPRIS commands (media keys, MPRIS
//! clients, GNOME shortcuts) back into the application's command surface.

use super::*;

pub(super) fn now_playing_to_mpris_metadata(
    now_playing: &sustain_app_runtime::NowPlaying,
) -> sustain_desktop::NowPlayingMetadata {
    let Some(track) = now_playing.track.as_ref() else {
        return sustain_desktop::NowPlayingMetadata::default();
    };
    sustain_desktop::NowPlayingMetadata {
        track_id: Some(track.id),
        title: track.metadata.title.clone(),
        artist: track.metadata.artist.clone(),
        album: track.metadata.album.clone(),
        album_artist: track.metadata.album_artist.clone(),
        genre: track.metadata.genre.clone(),
        track_number: track.metadata.track_number,
        disc_number: track.metadata.disc_number,
        duration: track.metadata.duration,
    }
}

pub(super) fn install_mpris_command_consumer(
    receiver: Option<MprisCommandReceiver>,
    command_controller: SharedCommandController,
    playback_changed: PlaybackChangedCallback,
    app: gtk::Application,
    window: gtk::ApplicationWindow,
) {
    // No receiver means MPRIS startup failed; the UI just runs without
    // a media-key bridge, and the dropped sender on the desktop side
    // means future try_send calls would silently no-op anyway.
    let Some(receiver) = receiver else {
        return;
    };
    glib::MainContext::default().spawn_local(async move {
        while let Ok(command) = receiver.recv().await {
            handle_mpris_command(
                command,
                &command_controller,
                &playback_changed,
                &app,
                &window,
            );
        }
    });
}

fn handle_mpris_command(
    command: sustain_desktop::MprisCommand,
    command_controller: &SharedCommandController,
    playback_changed: &PlaybackChangedCallback,
    app: &gtk::Application,
    window: &gtk::ApplicationWindow,
) {
    use sustain_desktop::MprisCommand;

    // Mapping MPRIS semantics into Sustain's PlaybackCommand surface:
    //
    // * `Play` is "resume from paused/stopped"; MPRIS clients use it as a
    //   distinct verb from `Pause`/`PlayPause` for state machines that
    //   want to be explicit. Map to `Resume`, which the runtime no-ops
    //   when there is no track loaded — the equivalent of MPRIS's
    //   "Play has no effect" clause for an empty queue.
    // * `Stop` is a hard stop with position reset; map to the existing
    //   `Stop` command.
    // * `Raise` / `Quit` are not playback at all; they are routed to GTK
    //   window/application actions directly.
    let playback_command = match command {
        MprisCommand::Raise => {
            window.present();
            return;
        }
        MprisCommand::Quit => {
            crate::quit_application(app);
            return;
        }
        MprisCommand::PlayPause => PlaybackCommand::TogglePlayPause,
        MprisCommand::Play => PlaybackCommand::Resume,
        MprisCommand::Pause => PlaybackCommand::Pause,
        MprisCommand::Stop => PlaybackCommand::Stop,
        // Treat the desktop-integration Next (media keys, MPRIS clients,
        // GNOME shortcuts) as a user-initiated skip, matching the
        // titlebar Next button. Only the EOS auto-advance path stays on
        // PlayNextTrack so natural track endings never inflate
        // skip_count.
        MprisCommand::Next => PlaybackCommand::SkipCurrentTrack,
        MprisCommand::Previous => PlaybackCommand::PlayPreviousTrack,
    };
    if command_controller.dispatch_succeeded(ApplicationCommand::Playback(playback_command)) {
        playback_changed();
    }
}
