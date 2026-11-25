// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The iPod layout: which iPod Sustain may write, where its audio goes,
//! and the catalogue its firmware reads.
//!
//! An iPod's stock firmware plays only what `iPod_Control/iTunes/iTunesDB`
//! lists, so Sustain writes that catalogue whole. As with a Pioneer
//! export, it writes only over a catalogue it left there itself, or over
//! one that lists no tracks — a freshly restored iPod. A library iTunes or
//! Finder wrote is refused, so Sustain never deletes music it did not put
//! there. See `docs/ipod-sync-design.md`.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use sha2::{Digest, Sha256};
use sustain_domain::{CatalogueRecord, CollationKey, DeviceRelativePath, TrackId};
use sustain_ipod::{
    Checksum, CoverId, CoverSet, Family, FileType, FirewireGuid, IpodLibrary, IpodPlaylist,
    IpodTrack, Profile, Shard, SysInfo,
};

use crate::catalogue;
use crate::journal::ManifestJournal;
use crate::model::{
    Placement, PreparedSyncRequest, SyncBlocker, SyncError, SyncInputTrack, SyncRequest,
};
use crate::transport::DeviceTransport;

const SYSINFO_RELATIVE_PATH: &str = "iPod_Control/Device/SysInfo";
/// `SysInfo` is a few hundred bytes of `Key: value` lines.
const SYSINFO_LIMIT: u64 = 64 * 1024;
/// The largest catalogue read to find out whether it lists any tracks. One
/// that lists none holds only headers and empty lists, a few kilobytes; a
/// larger one Sustain did not record is taken to list tracks, unread.
const EMPTY_CATALOGUE_LIMIT: u64 = 1024 * 1024;
/// The audio lives in this many numbered folders, as iTunes spreads it.
/// One constant: the firmware follows the path the catalogue stores, and a
/// count that changed would move — so re-copy — every track.
const MUSIC_FOLDERS: i64 = 50;
/// The firmware's playable formats top out at 48 kHz.
const MAX_SAMPLE_RATE_HZ: u32 = 48_000;

/// An iPod Sustain can write, identified from the volume being written.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Ipod {
    profile: Profile,
    /// The key of the catalogue's signature, for the families that sign it.
    guid: Option<FirewireGuid>,
}

impl Ipod {
    pub(crate) const fn profile(&self) -> Profile {
        self.profile
    }
}

/// Identify the iPod behind `transport`, or refuse it: a Mac-formatted or
/// read-only volume, a model Sustain does not support, or a model whose
/// catalogue must be signed with a key the device does not reveal.
pub(crate) fn identify(transport: &dyn DeviceTransport) -> Result<Ipod, SyncError> {
    let root = DeviceRelativePath::root();
    let volume = transport
        .volume_facts()
        .map_err(|error| SyncError::io(root.as_str(), error))?;
    if volume.hfs_plus {
        return Err(SyncError::Blocked(SyncBlocker::MacFormattedIpod));
    }
    if volume.read_only {
        return Err(SyncError::Blocked(SyncBlocker::ReadOnlyDevice));
    }
    let sysinfo = read_sysinfo(transport)?;
    let profile = sysinfo
        .model_number
        .as_deref()
        .and_then(Family::from_model_number)
        .ok_or(SyncError::Blocked(SyncBlocker::UnsupportedIpod))?
        .profile();
    let guid = match profile.checksum {
        Checksum::None => None,
        Checksum::Hash58 => Some(
            sysinfo
                .firewire_guid
                .or_else(|| volume.usb_serial.as_deref().and_then(guid_of_serial))
                .ok_or(SyncError::Blocked(SyncBlocker::UnidentifiedIpod))?,
        ),
    };
    Ok(Ipod { profile, guid })
}

/// What the iPod's `SysInfo` says; nothing, when it has none.
fn read_sysinfo(transport: &dyn DeviceTransport) -> Result<SysInfo, SyncError> {
    let path = DeviceRelativePath::new(SYSINFO_RELATIVE_PATH).expect("static SysInfo path is safe");
    let io_error = |error| SyncError::io(path.as_str(), error);
    if transport
        .regular_file_len(&path)
        .map_err(io_error)?
        .is_none()
    {
        return Ok(SysInfo::default());
    }
    let bytes = transport
        .read_file(&path, SYSINFO_LIMIT)
        .map_err(io_error)?;
    Ok(SysInfo::parse(&String::from_utf8_lossy(&bytes)))
}

/// The FireWire GUID a USB serial number carries: its first sixteen hex
/// digits, as libgpod reads it.
fn guid_of_serial(serial: &str) -> Option<FirewireGuid> {
    serial.get(..16).and_then(FirewireGuid::parse)
}

/// Fails with [`SyncBlocker::ForeignIpodLibrary`] unless the iPod's
/// catalogue, if it has one, is one `record` says Sustain left there or
/// lists no tracks.
pub(crate) fn ensure_library_is_ours(
    transport: &dyn DeviceTransport,
    record: &CatalogueRecord,
) -> Result<(), SyncError> {
    let path = itunesdb_path();
    let io_error = |error| SyncError::io(path.as_str(), error);
    let Some(len) = transport.regular_file_len(&path).map_err(io_error)? else {
        return Ok(());
    };
    let recorded_size = record
        .databases
        .iter()
        .any(|database| database.size_bytes == len);
    if recorded_size || len <= EMPTY_CATALOGUE_LIMIT {
        let bytes = transport.read_file(&path, len).map_err(io_error)?;
        if record.databases.contains(&catalogue::identity(&bytes))
            || sustain_ipod::track_count(&bytes) == Some(0)
        {
            return Ok(());
        }
    }
    Err(SyncError::Blocked(SyncBlocker::ForeignIpodLibrary))
}

fn itunesdb_path() -> DeviceRelativePath {
    DeviceRelativePath::new(sustain_ipod::ITUNESDB_RELATIVE_PATH)
        .expect("static iTunesDB path is safe")
}

/// Whether an iPod plays `track`.
pub(crate) fn plays(track: &SyncInputTrack) -> bool {
    FileType::from_extension(&track.extension).is_some()
        && track.sample_rate_hz <= MAX_SAMPLE_RATE_HZ
}

/// Where each track the iPod plays goes:
/// `iPod_Control/Music/F{id mod 50}/S{id in base 36}.{ext}`, the id padded
/// to six digits. The name comes from the track's id alone, so a track
/// keeps its file however its tags change, and two tracks never share one.
/// It is never one iTunes or libgpod writes — four characters, and
/// `libgpod` and digits — so a file they left in a folder is not written
/// over.
pub(crate) fn placements(req: &SyncRequest) -> Result<Vec<Placement>, SyncError> {
    req.tracks
        .iter()
        .enumerate()
        .filter(|(_, track)| plays(track))
        .map(|(track_index, track)| {
            Ok(Placement {
                track_index,
                rel_path: audio_path(track.track_id, &track.extension)?,
                fingerprint: track.source.fingerprint_token(),
            })
        })
        .collect()
}

fn audio_path(track_id: TrackId, extension: &str) -> Result<DeviceRelativePath, SyncError> {
    let id = track_id.get();
    let folder = id.rem_euclid(MUSIC_FOLDERS);
    DeviceRelativePath::new(format!(
        "iPod_Control/Music/F{folder:02}/S{:0>6}.{extension}",
        base36(id.unsigned_abs())
    ))
    .ok_or_else(|| SyncError::planning("generated an unsafe iPod audio path"))
}

fn base36(mut value: u64) -> String {
    const DIGITS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";
    let mut digits = Vec::new();
    loop {
        digits.push(DIGITS[(value % 36) as usize]);
        value /= 36;
        if value == 0 {
            break;
        }
    }
    digits.reverse();
    String::from_utf8(digits).expect("base-36 digits are ASCII")
}

/// Publish the library of the placements in `present` (see
/// [`crate::layout::write_m3u_playlists`]) on `ipod`, once their audio is
/// in place: the cover files and their `ArtworkDB`, then the catalogue,
/// then the removal of the cover files no longer used. `record` is kept
/// current, and journaled before each change it covers.
///
/// Until the new catalogue is published the old one stays in place, so a
/// run that stops part-way at worst leaves it showing the wrong covers,
/// which the next sync puts right. The firmware's own event files go just
/// before the catalogue: their rows name tracks by position in the
/// catalogue they were recorded against, and applied to the new one they
/// would credit plays and ratings to the wrong tracks.
pub(crate) fn write_catalogue(
    req: &PreparedSyncRequest,
    transport: &dyn DeviceTransport,
    placements: &[Placement],
    present: &HashSet<usize>,
    ipod: &Ipod,
    record: &mut CatalogueRecord,
    journal: &mut dyn ManifestJournal,
) -> Result<(), SyncError> {
    let covers = req.ipod_covers()?;
    let library = library(req, &covers.tracks, placements, present)?;
    let catalogue =
        sustain_ipod::write(&library, ipod.profile, ipod.guid).map_err(SyncError::Ipod)?;
    let stale_covers = if ipod.profile.cover_formats.is_empty() {
        Vec::new()
    } else {
        let artwork =
            sustain_ipod::artwork::database(&library.tracks, &covers.covers, ipod.profile)
                .map_err(SyncError::Ipod)?;
        write_covers(
            transport,
            &covers.covers,
            ipod.profile,
            &artwork,
            record,
            journal,
        )?
    };

    for path in sustain_ipod::DEVICE_EVENT_RELATIVE_PATHS {
        let path = DeviceRelativePath::new(path).expect("static iPod event path is safe");
        transport
            .remove_file_if_exists(&path)
            .map_err(|error| SyncError::io(path.as_str(), error))?;
    }
    let path = itunesdb_path();
    // Until the new catalogue is known to be published, either it or the
    // one it replaces may be what the iPod holds.
    let database = catalogue::identity(&catalogue);
    record.databases.insert(database.clone());
    journal
        .record_catalogue(record)
        .map_err(SyncError::Journal)?;
    transport
        .write_file(&path, &catalogue)
        .map_err(|error| SyncError::io(path.as_str(), error))?;
    record.databases = BTreeSet::from([database]);
    journal
        .record_catalogue(record)
        .map_err(SyncError::Journal)?;

    // Only now that no catalogue on the iPod names them do the cover files
    // the previous one used go.
    if !stale_covers.is_empty() {
        for path in stale_covers {
            transport
                .remove_file_if_exists(&path)
                .map_err(|error| SyncError::io(path.as_str(), error))?;
            record.artwork.remove(&path);
        }
        journal
            .record_catalogue(record)
            .map_err(SyncError::Journal)?;
    }
    Ok(())
}

/// Write the `.ithmb` cover files of `covers`, one rendered at a time,
/// then `artwork`, their `ArtworkDB`. Each file is recorded as Sustain's
/// before it is written. Returns the cover files `record` holds that this
/// library no longer uses, for removal once the new catalogue is out.
///
/// A cover file already on the iPod under one of these names is written
/// over even if Sustain did not write it: only an iPod whose catalogue is
/// Sustain's or lists no tracks gets here, and beside such a catalogue an
/// older cover file shows nothing.
fn write_covers(
    transport: &dyn DeviceTransport,
    covers: &CoverSet,
    profile: Profile,
    artwork: &[u8],
    record: &mut CatalogueRecord,
    journal: &mut dyn ManifestJournal,
) -> Result<Vec<DeviceRelativePath>, SyncError> {
    let directory = DeviceRelativePath::new(sustain_ipod::ARTWORK_DIRECTORY)
        .expect("static artwork path is safe");
    let shards: BTreeMap<DeviceRelativePath, Shard> =
        sustain_ipod::artwork::shards(covers, profile)
            .into_iter()
            .map(|shard| {
                let path = directory
                    .join_component(&shard.file_name())
                    .expect("generated cover file name is safe");
                (path, shard)
            })
            .collect();
    let stale = record
        .artwork
        .iter()
        .filter(|path| !shards.contains_key(*path))
        .cloned()
        .collect();
    record.artwork.extend(shards.keys().cloned());
    journal
        .record_catalogue(record)
        .map_err(SyncError::Journal)?;
    for (path, shard) in &shards {
        let bytes = shard.render(covers).map_err(SyncError::IpodCover)?;
        transport
            .write_file(path, &bytes)
            .map_err(|error| SyncError::io(path.as_str(), error))?;
    }
    let path = DeviceRelativePath::new(sustain_ipod::ARTWORK_DB_RELATIVE_PATH)
        .expect("static ArtworkDB path is safe");
    transport
        .write_file(&path, artwork)
        .map_err(|error| SyncError::io(path.as_str(), error))?;
    Ok(stale)
}

/// About how many more bytes the covers of `placements` need on the iPod
/// than the cover files Sustain already left there hold: one cover per
/// album, the most there can be once the covers are read.
pub(crate) fn cover_bytes_to_write(
    transport: &dyn DeviceTransport,
    req: &SyncRequest,
    placements: &[Placement],
    profile: Profile,
) -> Result<u64, SyncError> {
    let albums: HashSet<(CollationKey, CollationKey)> = placements
        .iter()
        .map(|placement| {
            let track = &req.tracks[placement.track_index];
            let artist = track.album_artist.as_deref().unwrap_or(&track.artist);
            (CollationKey::new(artist), CollationKey::new(&track.album))
        })
        .collect();
    let needed = u64::try_from(albums.len())
        .unwrap_or(u64::MAX)
        .saturating_mul(profile.bytes_per_cover());
    let mut held: u64 = 0;
    for path in &req.previous_catalogue.artwork {
        let len = transport
            .regular_file_len(path)
            .map_err(|error| SyncError::io(path.as_str(), error))?;
        held = held.saturating_add(len.unwrap_or(0));
    }
    Ok(needed.saturating_sub(held))
}

/// The catalogue of the present placements, in placement order, and of
/// the selection's playlists.
fn library(
    req: &SyncRequest,
    covers: &[Option<CoverId>],
    placements: &[Placement],
    present: &HashSet<usize>,
) -> Result<IpodLibrary, SyncError> {
    let mut tracks = Vec::with_capacity(present.len());
    let mut row_for: HashMap<usize, usize> = HashMap::with_capacity(present.len());
    for (index, placement) in placements.iter().enumerate() {
        if !present.contains(&index) {
            continue;
        }
        row_for.insert(placement.track_index, tracks.len());
        tracks.push(ipod_track(
            &req.tracks[placement.track_index],
            &placement.rel_path,
            covers[placement.track_index],
        )?);
    }
    let device = req.device.id.as_str();
    let mut occurrences: HashMap<&str, u32> = HashMap::new();
    let playlists = req
        .playlists
        .iter()
        .map(|playlist| {
            let occurrence = occurrences.entry(playlist.name.as_str()).or_default();
            *occurrence += 1;
            IpodPlaylist {
                id: persistent_id(&["playlist", device, &playlist.name, &occurrence.to_string()]),
                name: playlist.name.clone(),
                track_indices: playlist
                    .track_indices
                    .iter()
                    .filter_map(|track_index| row_for.get(track_index).copied())
                    .collect(),
            }
        })
        .collect();
    Ok(IpodLibrary {
        name: req.device.label.clone(),
        library_id: persistent_id(&["library", device]),
        master_playlist_id: persistent_id(&["master playlist", device]),
        tracks,
        playlists,
    })
}

fn ipod_track(
    track: &SyncInputTrack,
    location: &DeviceRelativePath,
    cover: Option<CoverId>,
) -> Result<IpodTrack, SyncError> {
    let out_of_range = |field: &str| {
        SyncError::planning(format!(
            "track {}: {field} out of range for an iPod",
            track.track_id.get()
        ))
    };
    Ok(IpodTrack {
        dbid: track.track_id.get().unsigned_abs(),
        location: location.as_str().to_owned(),
        file_type: FileType::from_extension(&track.extension)
            .ok_or_else(|| out_of_range("format"))?,
        title: Some(track.title.clone()),
        artist: Some(track.artist.clone()),
        album: Some(track.album.clone()),
        album_artist: track.album_artist.clone(),
        genre: track.genre.clone(),
        composer: track.composer.clone(),
        comment: track.comment.clone(),
        sort_title: track.sort_title.clone(),
        sort_artist: track.sort_artist.clone(),
        sort_album: track.sort_album.clone(),
        sort_album_artist: track.sort_album_artist.clone(),
        sort_composer: track.sort_composer.clone(),
        compilation: track.compilation,
        rating_stars: track.rating,
        track_number: track.track_number,
        track_total: track.track_total,
        disc_number: track.disc_number,
        disc_total: track.disc_total,
        year: track.year,
        duration_ms: track.duration_ms,
        size_bytes: u32::try_from(track.source.stat.size_bytes)
            .map_err(|_| out_of_range("file size"))?,
        bitrate_kbps: track.bitrate_kbps,
        sample_rate_hz: u16::try_from(track.sample_rate_hz)
            .map_err(|_| out_of_range("sample rate"))?,
        date_added: track.date_added,
        cover,
    })
}

/// A nonzero 64-bit id derived from `parts`, the same on every sync.
fn persistent_id(parts: &[&str]) -> u64 {
    let mut hasher = Sha256::new();
    for part in parts {
        hasher.update(part.as_bytes());
        hasher.update([0]);
    }
    let digest = hasher.finalize();
    let id = u64::from_le_bytes(
        digest[..8]
            .try_into()
            .expect("a SHA-256 digest has 8 bytes"),
    );
    id.max(1)
}
