// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Pure per-track metadata planning for a CD rip.
//!
//! Given the probed TOC and the chosen MusicBrainz release (or `None` for
//! the offline/no-match fallback), produce the editable [`TrackMetadata`]
//! Sustain will both tag the encoded file with and store as the
//! authoritative library row. Technical fields (duration, bitrate, …) are
//! deliberately left unset here: they are captured by reading the finished
//! file back, not guessed from the request.

use sustain_cd_import::TocSnapshot;
use sustain_domain::TrackMetadata;
use sustain_metadata_remote::DiscRelease;

const FALLBACK_ALBUM: &str = "Audio CD";
const FALLBACK_ARTIST: &str = "Unknown Artist";

/// Build the editable metadata for one physical track.
///
/// `release` is the user-chosen MusicBrainz release, or `None` for the
/// fallback. The per-track title/artist come from the release track that
/// maps to `physical_number` by ordered position within the medium; missing
/// values fall back to `Track NN` / the release artist / `Unknown Artist`.
pub(crate) fn build_track_metadata(
    snapshot: &TocSnapshot,
    release: Option<&DiscRelease>,
    physical_number: u32,
) -> TrackMetadata {
    // The release's ordered tracks line up with the disc's ordered audio
    // tracks (the compatibility check guaranteed equal counts), so map by
    // the physical track's index within the TOC rather than by raw number.
    let ordered_index = snapshot
        .tracks
        .iter()
        .position(|track| track.number == physical_number);
    let disc_track =
        release.and_then(|release| ordered_index.and_then(|index| release.tracks.get(index)));

    let title = disc_track
        .and_then(|track| non_blank(track.title.as_deref()))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("Track {physical_number:02}"));

    let release_artist = release.and_then(|release| non_blank(release.artist_credit.as_deref()));
    let artist = disc_track
        .and_then(|track| non_blank(track.artist_credit.as_deref()))
        .or(release_artist)
        .unwrap_or(FALLBACK_ARTIST)
        .to_owned();

    let album = release
        .and_then(|release| non_blank(release.title.as_deref()))
        .unwrap_or(FALLBACK_ALBUM)
        .to_owned();

    let track_total = release
        .map(|release| release.track_total)
        .unwrap_or_else(|| snapshot.audio_track_count());

    TrackMetadata {
        title: Some(title),
        artist: Some(artist),
        album: Some(album),
        album_artist: release_artist.map(ToOwned::to_owned),
        genre: None,
        track_number: Some(physical_number),
        track_total: Some(track_total),
        disc_number: release.and_then(|release| release.disc_number),
        disc_total: release.and_then(|release| release.disc_total),
        year: release.and_then(|release| release.year),
        compilation: release.map(|release| release.is_compilation),
        ..TrackMetadata::default()
    }
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|inner| !inner.trim().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use sustain_cd_import::RawTocTrack;
    use sustain_metadata_remote::DiscTrack;

    fn two_track_snapshot() -> TocSnapshot {
        TocSnapshot::from_raw(
            std::path::PathBuf::from("/dev/sr0"),
            "disc-id".to_owned(),
            String::new(),
            &[
                RawTocTrack {
                    number: 1,
                    offset: 150,
                    sectors: 13350,
                },
                RawTocTrack {
                    number: 2,
                    offset: 13500,
                    sectors: 18000,
                },
            ],
        )
    }

    fn disc_track(position: u32, title: &str, artist: Option<&str>) -> DiscTrack {
        DiscTrack {
            position,
            title: Some(title.to_owned()),
            artist_credit: artist.map(ToOwned::to_owned),
            duration_ms: Some(180_000),
            recording_mbid: None,
        }
    }

    fn release() -> DiscRelease {
        DiscRelease {
            release_mbid: "rel".to_owned(),
            release_group_mbid: None,
            title: Some("The Album".to_owned()),
            artist_credit: Some("The Band".to_owned()),
            year: Some(1999),
            date: Some("1999".to_owned()),
            country: None,
            label: None,
            format: Some("CD".to_owned()),
            disc_number: Some(1),
            disc_total: None,
            track_total: 2,
            is_compilation: false,
            tracks: vec![
                disc_track(1, "Opener", None),
                disc_track(2, "Closer", Some("Guest")),
            ],
        }
    }

    #[test]
    fn musicbrainz_metadata_maps_per_track() {
        let snapshot = two_track_snapshot();
        let release = release();

        let first = build_track_metadata(&snapshot, Some(&release), 1);
        assert_eq!(first.title.as_deref(), Some("Opener"));
        // No per-track artist -> falls back to the release artist.
        assert_eq!(first.artist.as_deref(), Some("The Band"));
        assert_eq!(first.album.as_deref(), Some("The Album"));
        assert_eq!(first.album_artist.as_deref(), Some("The Band"));
        assert_eq!(first.track_number, Some(1));
        assert_eq!(first.track_total, Some(2));
        assert_eq!(first.disc_number, Some(1));
        assert_eq!(first.year, Some(1999));
        assert_eq!(first.compilation, Some(false));

        let second = build_track_metadata(&snapshot, Some(&release), 2);
        assert_eq!(second.title.as_deref(), Some("Closer"));
        // Per-track artist wins over the release artist.
        assert_eq!(second.artist.as_deref(), Some("Guest"));
    }

    #[test]
    fn fallback_metadata_uses_audio_cd_placeholders() {
        let snapshot = two_track_snapshot();

        let first = build_track_metadata(&snapshot, None, 1);
        assert_eq!(first.title.as_deref(), Some("Track 01"));
        assert_eq!(first.artist.as_deref(), Some("Unknown Artist"));
        assert_eq!(first.album.as_deref(), Some("Audio CD"));
        assert_eq!(first.album_artist, None);
        assert_eq!(first.track_number, Some(1));
        assert_eq!(first.track_total, Some(2));
        assert_eq!(first.disc_number, None);
        assert_eq!(first.compilation, None);
        // No audio-stream values are guessed; they come from reading the
        // finished file back.
        assert_eq!(first.duration, None);
        assert_eq!(first.bitrate_kbps, None);
    }
}
