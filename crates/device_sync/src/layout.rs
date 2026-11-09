// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Per-layout path planning and finalization.
//!
//! Each layout turns the resolved track set + playlists into a list of
//! [`Placement`]s (which source track goes to which on-device path) and,
//! after the audio has been copied, writes its index/database files:
//!
//! - **M3u** — a deduplicated `Music/Artist/Album` tree plus one
//!   `.m3u8` per playlist referencing relative paths.
//! - **FolderPerPlaylist** — one folder per playlist with real copies
//!   (not deduplicated), stable per-track indices recorded so re-syncs
//!   do not reshuffle, optional per-folder file cap with `01/`, `02/`
//!   subfolder splits.
//! - **Pioneer** — a deduplicated `Contents/Artist/Album` tree plus the
//!   `export.pdb` database and per-track ANLZ waveform files.

use std::collections::{BTreeSet, HashMap, HashSet};

use sustain_domain::{AnlzRecord, DeviceKind, DeviceLayout, DeviceRelativePath, WaveformSegments};
use sustain_pioneer::{
    AnlzInput, PioneerFileType, PioneerPlaylist, PioneerTrack, anlz, artwork::ARTWORK_BUCKET,
    device_setting, path_hash, pdb,
};

use crate::model::{Placement, PreparedSyncRequest, SyncError, SyncInputTrack, SyncRequest};
use crate::transport::DeviceTransport;

const MUSIC_DIR: &str = "Music";
const CONTENTS_DIR: &str = "Contents";
/// Component cap for the m3u/Pioneer trees (FAT-safe, generous).
const TREE_COMPONENT_CAP: usize = 60;
/// Filename cap for the m3u/Pioneer tree leaf names.
const TREE_FILENAME_CAP: usize = 120;
/// Component cap for the folder-per-playlist layout (car-stereo target).
const FOLDER_COMPONENT_CAP: usize = 32;
/// Disambiguation attempt ceiling before a layout reports a planning
/// error. The immutable track id resolves the documented collisions in
/// attempt zero; the extra counter covers the pathological case where even
/// the id-tagged, length-capped name is already taken. The ceiling is far
/// beyond any real library and only exists to guarantee the loop
/// terminates rather than spins.
const MAX_DISAMBIGUATION_ATTEMPTS: u32 = 10_000;

/// Compute the desired placements for a request's layout. Fails closed
/// with [`SyncError::Planning`] if a unique destination cannot be produced
/// or the final set still holds a duplicate path — never a silent
/// overwrite.
pub fn plan_placements(req: &SyncRequest) -> Result<Vec<Placement>, SyncError> {
    let mut placements = match req.device.layout {
        DeviceLayout::M3u => tree_placements(req, MUSIC_DIR)?,
        DeviceLayout::Pioneer => tree_placements(req, CONTENTS_DIR)?,
        DeviceLayout::FolderPerPlaylist => folder_placements(req)?,
    };
    placements.retain(|placement| plays(req.device.layout, &req.tracks[placement.track_index]));
    validate_unique(&placements)?;
    Ok(placements)
}

/// Whether the player a layout targets can play `track`. Pioneer players
/// read only the formats rekordbox exports; anything else would sit in
/// the player's browser and refuse to load. The other layouts feed
/// players Sustain knows nothing about, so every track goes.
pub(crate) fn plays(layout: DeviceLayout, track: &SyncInputTrack) -> bool {
    match layout {
        DeviceLayout::Pioneer => {
            PioneerFileType::from_extension(&track.extension) != PioneerFileType::Unknown
        }
        DeviceLayout::M3u | DeviceLayout::FolderPerPlaylist => true,
    }
}

/// The identity of an on-device path. The filesystems a sync writes to —
/// FAT32 and exFAT sticks, Android shared storage — ignore letter case,
/// so two paths that differ only in case name one file. The planner and
/// the differ compare device paths by this key, never by spelling.
///
/// Letters fold through their single-letter uppercase, the mapping
/// exFAT's up-case table carries; a letter whose uppercase is several
/// letters (`ß`) stays as it is, as it does on the device.
pub(crate) fn device_path_key(path: &str) -> String {
    path.chars()
        .map(|letter| {
            let mut upper = letter.to_uppercase();
            match (upper.next(), upper.next()) {
                (Some(single), None) => single,
                _ => letter,
            }
        })
        .collect()
}

/// Final guard before any filesystem mutation: every planned path must be
/// unique on the device. The allocators already guarantee this, so a duplicate here is a
/// planner bug — fail closed with a typed error rather than letting the
/// copy step and the manifest silently collapse two tracks onto one file.
fn validate_unique(placements: &[Placement]) -> Result<(), SyncError> {
    let mut seen: HashSet<String> = HashSet::with_capacity(placements.len());
    for placement in placements {
        if !seen.insert(device_path_key(placement.rel_path.as_str())) {
            return Err(SyncError::planning(format!(
                "duplicate device placement path: {}",
                placement.rel_path
            )));
        }
    }
    Ok(())
}

/// Allocate a unique on-device relative path `{dir}/{name}` for a track;
/// `used` holds the [`device_path_key`]s already taken. The plain sanitized name is tried first; on collision it disambiguates
/// with the immutable track id, reserving room for the suffix before the
/// length cap so truncation can never drop it. An extra counter covers the
/// rare case where even the id-tagged name is already taken (sanitization
/// or truncation can collapse distinct stems). Returns a planning error
/// only if no unique name can be produced within the attempt ceiling.
fn allocate_unique_path(
    used: &mut HashSet<String>,
    dir: &str,
    stem: &str,
    extension: &str,
    track_id: i64,
    name_cap: usize,
) -> Result<String, SyncError> {
    let name = crate::sanitize::filename(stem, extension, name_cap);
    let rel = format!("{dir}/{name}");
    if used.insert(device_path_key(&rel)) {
        return Ok(rel);
    }
    for attempt in 0..MAX_DISAMBIGUATION_ATTEMPTS {
        let suffix = if attempt == 0 {
            format!(" ({track_id})")
        } else {
            format!(" ({track_id}-{})", attempt + 1)
        };
        let name = crate::sanitize::filename_with_suffix(stem, &suffix, extension, name_cap);
        let rel = format!("{dir}/{name}");
        if used.insert(device_path_key(&rel)) {
            return Ok(rel);
        }
    }
    Err(SyncError::planning(format!(
        "could not allocate a unique device path for track {track_id} under {dir}"
    )))
}

/// What [`finalize`] established on the device beyond the audio files.
#[derive(Default)]
pub(crate) struct Finalized {
    /// The Pioneer analysis pair each present placement (by index into
    /// `placements`) now owns, written or verified current this run.
    pub(crate) anlz: HashMap<usize, AnlzRecord>,
}

/// Write the layout's index/database files after audio is in place.
/// `present` holds the indices (into `placements`) that are known to exist
/// on the device after the copy phase: unchanged files plus successful
/// writes from this run. Recoverable copy failures are intentionally absent.
pub(crate) fn finalize(
    req: &PreparedSyncRequest,
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    placements: &[Placement],
    present: &HashSet<usize>,
    cancel: &dyn Fn() -> bool,
) -> Result<Finalized, SyncError> {
    match req.device.layout {
        DeviceLayout::M3u => {
            write_m3u_playlists(req, transport, base, placements, present)?;
            Ok(Finalized::default())
        }
        DeviceLayout::FolderPerPlaylist => Ok(Finalized::default()),
        DeviceLayout::Pioneer => write_pioneer(req, transport, base, placements, present, cancel),
    }
}

/// True when the layout writes index/database files in [`finalize`]
/// even if no audio changed (the selection itself may have changed).
pub fn always_finalizes(layout: DeviceLayout) -> bool {
    matches!(layout, DeviceLayout::M3u | DeviceLayout::Pioneer)
}

// ---------------------------------------------------------------------
// Deduplicated tree layouts (m3u, Pioneer)
// ---------------------------------------------------------------------

fn tree_placements(req: &SyncRequest, root_dir: &str) -> Result<Vec<Placement>, SyncError> {
    let mut used: HashSet<String> = HashSet::new();
    let mut spellings: HashMap<String, String> = HashMap::new();
    let mut placements = Vec::with_capacity(req.tracks.len());
    for (index, track) in req.tracks.iter().enumerate() {
        let artist =
            crate::sanitize::component(&track.artist, TREE_COMPONENT_CAP, "Unknown Artist");
        let album = crate::sanitize::component(&track.album, TREE_COMPONENT_CAP, "Unknown Album");
        let artist_dir = first_spelling(&mut spellings, format!("{root_dir}/{artist}"));
        let dir = first_spelling(&mut spellings, format!("{artist_dir}/{album}"));
        let rel = allocate_unique_path(
            &mut used,
            &dir,
            &track_stem(track),
            &track.extension,
            track.track_id.get(),
            TREE_FILENAME_CAP,
        )?;
        placements.push(Placement {
            track_index: index,
            rel_path: DeviceRelativePath::new(rel)
                .ok_or_else(|| SyncError::planning("generated an unsafe tree placement"))?,
            fingerprint: track.source.fingerprint_token(),
        });
    }
    Ok(placements)
}

/// The spelling the plan already gave the directory `path` names on the
/// device. Tracks tagged `Daft Punk` and `daft punk` share one folder on
/// a case-insensitive filesystem, so they share one spelling in the plan
/// too: the first one met.
fn first_spelling(spellings: &mut HashMap<String, String>, path: String) -> String {
    spellings
        .entry(device_path_key(&path))
        .or_insert(path)
        .clone()
}

fn track_stem(track: &SyncInputTrack) -> String {
    match track.track_number {
        Some(n) if n > 0 => format!("{n:02} {}", track.title),
        _ => track.title.clone(),
    }
}

// ---------------------------------------------------------------------
// Folder-per-playlist layout
// ---------------------------------------------------------------------

fn folder_placements(req: &SyncRequest) -> Result<Vec<Placement>, SyncError> {
    let cap = req.device.files_per_folder_cap.limit();
    let mut used_folders: HashSet<String> = HashSet::new();
    let mut placements = Vec::new();

    for playlist in &req.playlists {
        let folder = unique_name(
            &mut used_folders,
            crate::sanitize::component(&playlist.name, FOLDER_COMPONENT_CAP, "Playlist"),
        );

        // Recover stable indices from the previous manifest so existing
        // files keep their slot and only new tracks are appended. The
        // folder is matched as the device sees it, whatever its case.
        let prefix = device_path_key(&format!("{folder}/"));
        let prior: HashMap<sustain_domain::TrackId, u32> = req
            .previous_manifest
            .iter()
            .filter_map(|entry| {
                let path = device_path_key(entry.on_device_path.as_str());
                let rest = path.strip_prefix(&prefix)?;
                let file = rest.rsplit('/').next()?;
                Some((entry.track_id, leading_number(file)?))
            })
            .collect();

        let mut used_idx: BTreeSet<u32> = prior.values().copied().collect();
        let mut next_idx = used_idx.iter().max().copied().unwrap_or(0);
        let mut assignments: Vec<(usize, u32)> = Vec::with_capacity(playlist.track_indices.len());
        // Place each track once per folder: a playlist listing the same
        // track twice would otherwise resolve both entries to the same
        // recovered index (and thus the same on-device filename), silently
        // overwriting one copy with the other.
        let mut placed_ids: HashSet<sustain_domain::TrackId> = HashSet::new();
        for &track_index in &playlist.track_indices {
            let track_id = req.tracks[track_index].track_id;
            if !placed_ids.insert(track_id) {
                continue;
            }
            let idx = match prior.get(&track_id) {
                Some(&existing) => existing,
                None => {
                    next_idx += 1;
                    used_idx.insert(next_idx);
                    next_idx
                }
            };
            assignments.push((track_index, idx));
        }

        let max_idx = assignments.iter().map(|(_, i)| *i).max().unwrap_or(0);
        let width = max_idx.to_string().len().max(3);
        for (track_index, idx) in assignments {
            let track = &req.tracks[track_index];
            let stem = format!("{idx:0width$} {} - {}", track.artist, track.title);
            let name = crate::sanitize::filename(&stem, &track.extension, FOLDER_COMPONENT_CAP);
            let rel = match cap {
                Some(c) if max_idx > c => {
                    let sub = (idx - 1) / c + 1;
                    format!("{folder}/{sub:02}/{name}")
                }
                _ => format!("{folder}/{name}"),
            };
            placements.push(Placement {
                track_index,
                rel_path: DeviceRelativePath::new(rel).ok_or_else(|| {
                    SyncError::planning("generated an unsafe folder-per-playlist placement")
                })?,
                fingerprint: track.source.fingerprint_token(),
            });
        }
    }
    Ok(placements)
}

fn leading_number(name: &str) -> Option<u32> {
    let digits: String = name.chars().take_while(char::is_ascii_digit).collect();
    digits.parse().ok()
}

/// `base`, or `base (n)` when the device already holds that name in
/// `used` (by [`device_path_key`]).
fn unique_name(used: &mut HashSet<String>, base: String) -> String {
    if used.insert(device_path_key(&base)) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base} ({n})");
        if used.insert(device_path_key(&candidate)) {
            return candidate;
        }
    }
    unreachable!("name space is unbounded")
}

// ---------------------------------------------------------------------
// m3u index files
// ---------------------------------------------------------------------

fn write_m3u_playlists(
    req: &SyncRequest,
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    placements: &[Placement],
    present: &HashSet<usize>,
) -> Result<(), SyncError> {
    // Android players auto-discover playlists where their relative entries
    // resolve, so the `.m3u8` is dropped inside the `Music/` tree with
    // entries relative to it (`Artist/Album/Track`). Every other target
    // keeps the playlist at the device root with `Music/`-prefixed entries
    // (the player reads them from the drive root).
    let android = req.device.kind == DeviceKind::Android;
    let playlist_dir = if android {
        DeviceRelativePath::new(MUSIC_DIR).expect("static Music dir is safe")
    } else {
        DeviceRelativePath::root()
    };
    let entry_prefix = if android {
        format!("{MUSIC_DIR}/")
    } else {
        String::new()
    };

    // track index -> its on-device path, made relative to the playlist's
    // own location so each entry resolves next to the `.m3u8`.
    let path_for: HashMap<usize, &str> = placements
        .iter()
        .enumerate()
        .filter(|(index, _placement)| present.contains(index))
        .map(|(_index, placement)| {
            let rel = placement.rel_path.as_str();
            (
                placement.track_index,
                rel.strip_prefix(&entry_prefix).unwrap_or(rel),
            )
        })
        .collect();

    let mut used: HashSet<String> = HashSet::new();
    for playlist in &req.playlists {
        let stem = crate::sanitize::component(&playlist.name, TREE_COMPONENT_CAP, "Playlist");
        let name = unique_name(&mut used, format!("{stem}.m3u8"));
        let mut body = String::from("#EXTM3U\n");
        for &track_index in &playlist.track_indices {
            let Some(rel) = path_for.get(&track_index) else {
                continue;
            };
            let track = &req.tracks[track_index];
            body.push_str(&format!(
                "#EXTINF:{},{} - {}\n{}\n",
                track.duration_ms / 1000,
                track.artist,
                track.title,
                rel,
            ));
        }
        let relative = playlist_dir
            .join_component(&name)
            .ok_or_else(|| SyncError::planning("generated an unsafe playlist path"))?;
        let dest = base.join(&relative);
        transport
            .write_file(&dest, body.as_bytes())
            .map_err(|error| SyncError::io(dest.as_str(), error))?;
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Pioneer database + ANLZ
// ---------------------------------------------------------------------

fn write_pioneer(
    req: &PreparedSyncRequest,
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    placements: &[Placement],
    present: &HashSet<usize>,
    cancel: &dyn Fn() -> bool,
) -> Result<Finalized, SyncError> {
    let assets = req.pioneer_assets()?;
    // Present placements in placement order: their PDB row order.
    let exported: Vec<usize> = (0..placements.len())
        .filter(|index| present.contains(index))
        .collect();
    let audio_paths: Vec<String> = exported
        .iter()
        .map(|&index| format!("/{}", placements[index].rel_path))
        .collect();

    // Analysis slots. Each exported track keeps the slot its previous
    // record names; every slot still recorded for a track that is *not*
    // exported is reserved, because its record (and possibly its audio)
    // stays in the manifest — handing that slot to another track would
    // make the record lie about the bytes on the device.
    let previous: HashMap<&DeviceRelativePath, &AnlzRecord> = req
        .previous_manifest
        .iter()
        .filter_map(|entry| {
            entry
                .anlz
                .as_ref()
                .map(|anlz| (&entry.on_device_path, anlz))
        })
        .collect();
    let exported_paths: HashSet<&DeviceRelativePath> = exported
        .iter()
        .map(|&index| &placements[index].rel_path)
        .collect();
    let recorded: Vec<Option<String>> = exported
        .iter()
        .map(|&index| {
            previous
                .get(&placements[index].rel_path)
                .map(|anlz| format!("/{}", anlz.dat_path))
        })
        .collect();
    let reserved: Vec<String> = previous
        .iter()
        .filter(|(path, _)| !exported_paths.contains(*path))
        .map(|(_, anlz)| format!("/{}", anlz.dat_path))
        .collect();
    let slot_requests: Vec<(&str, Option<&str>)> = audio_paths
        .iter()
        .zip(&recorded)
        .map(|(audio, recorded)| (audio.as_str(), recorded.as_deref()))
        .collect();
    let reserved: Vec<&str> = reserved.iter().map(String::as_str).collect();
    let slots = path_hash::assign_slots(&slot_requests, &reserved)
        .map_err(|error| SyncError::planning(error.to_string()))?;

    let empty = WaveformSegments {
        segment_duration_ms: 0.0,
        segments: Vec::new(),
    };
    let mut finalized = Finalized::default();
    let mut pioneer_tracks = Vec::with_capacity(exported.len());
    let mut row_for: HashMap<usize, usize> = HashMap::new();
    for ((&placement_index, audio_path), slot) in exported.iter().zip(&audio_paths).zip(&slots) {
        let placement = &placements[placement_index];
        let track = &req.tracks[placement.track_index];
        let track_assets = &assets.tracks[placement.track_index];

        // The pair is regenerated every run and written only when the
        // device does not already hold exactly these bytes in this slot:
        // a new grid or waveform reaches the drive even though the audio
        // file is unchanged, and an unchanged pair is not rewritten.
        let analysis = &track_assets.analysis;
        let input = AnlzInput {
            device_audio_path: audio_path,
            beat_grid: analysis
                .beat_grid
                .and_then(|grid| grid.for_library_tempo(track.bpm)),
            timeline_delay: analysis.timeline_delay,
            duration_ms: track.duration_ms,
            waveform_preview: analysis.waveform_preview.as_ref().unwrap_or(&empty),
            waveform_detail: analysis.waveform_detail.as_ref().unwrap_or(&empty),
        };
        let dat_bytes = anlz::dat_bytes(&input);
        let ext_bytes = anlz::ext_bytes(&input);
        let record = AnlzRecord {
            dat_path: drive_relative(&slot.dat_path())?,
            digest: anlz::pair_digest(&dat_bytes, &ext_bytes),
        };
        let dat = base.join(&record.dat_path);
        let ext = base.join(&drive_relative(&slot.ext_path())?);
        let current = previous.get(&placement.rel_path) == Some(&&record)
            && is_file(transport, &dat)?
            && is_file(transport, &ext)?;
        if !current {
            transport
                .write_file(&dat, &dat_bytes)
                .map_err(|error| SyncError::io(dat.as_str(), error))?;
            transport
                .write_file(&ext, &ext_bytes)
                .map_err(|error| SyncError::io(ext.as_str(), error))?;
        }

        row_for.insert(placement.track_index, pioneer_tracks.len());
        pioneer_tracks.push(PioneerTrack {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            genre: track.genre.clone(),
            composer: track.composer.clone(),
            comment: track.comment.clone(),
            bpm: track.bpm,
            key: track.key,
            duration_secs: track.duration_ms / 1000,
            file_size: track.source.stat.size_bytes,
            track_number: track.track_number,
            disc_number: track.disc_number,
            year: track.year,
            rating: track.rating,
            bitrate_kbps: track.bitrate_kbps,
            sample_rate_hz: track.sample_rate_hz,
            bit_depth: track.bit_depth,
            file_type: PioneerFileType::from_extension(&track.extension),
            artwork_id: track_assets.artwork_id,
            date_added: track.date_added.clone(),
            device_audio_path: audio_path.clone(),
            device_anlz_path: slot.dat_path(),
        });
        finalized.anlz.insert(placement_index, record);
    }

    let pioneer_playlists: Vec<PioneerPlaylist> = req
        .playlists
        .iter()
        .map(|playlist| PioneerPlaylist {
            name: playlist.name.clone(),
            entries: playlist
                .track_indices
                .iter()
                .filter_map(|ti| row_for.get(ti).copied())
                .collect(),
        })
        .collect();

    // Render the cover thumbnails onto the drive (clearing any stale set
    // from a previous, differently-numbered export) before stamping the
    // matching id↔path rows into the PDB.
    let artwork_bucket = base.join(
        &DeviceRelativePath::new(ARTWORK_BUCKET)
            .expect("static Pioneer artwork bucket path is safe"),
    );
    transport
        .remove_tree_if_exists(&artwork_bucket, cancel)
        .map_err(|error| SyncError::io(artwork_bucket.as_str(), error))?;
    if !assets.artwork.is_empty() {
        transport
            .ensure_dir_all(&artwork_bucket)
            .map_err(|error| SyncError::io(artwork_bucket.as_str(), error))?;
        for (name, bytes) in assets.artwork.files() {
            let path = artwork_bucket
                .join_component(&name)
                .expect("generated Pioneer artwork filename is safe");
            transport
                .write_file(&path, bytes)
                .map_err(|error| SyncError::io(path.as_str(), error))?;
        }
    }
    let artwork_rows = assets.artwork.rows();

    let pdb_path = base.join(
        &DeviceRelativePath::new(sustain_pioneer::PDB_RELATIVE_PATH)
            .expect("static Pioneer PDB path is safe"),
    );
    let bytes = pdb::build(
        &pioneer_tracks,
        &pioneer_playlists,
        &artwork_rows,
        &req.export_date,
    )
    .map_err(SyncError::Pdb)?;
    transport
        .write_file(&pdb_path, &bytes)
        .map_err(|error| SyncError::io(pdb_path.as_str(), error))?;

    // Without a settings file, players show the waveforms in BLUE
    // monochrome. A drive that has one keeps it: from then on the
    // settings belong to whoever set them, on a player or in rekordbox.
    let settings_path = base.join(
        &DeviceRelativePath::new(device_setting::DEVSETTING_RELATIVE_PATH)
            .expect("static Pioneer settings path is safe"),
    );
    if !is_file(transport, &settings_path)? {
        transport
            .write_file(&settings_path, &device_setting::colour_waveform_bytes())
            .map_err(|error| SyncError::io(settings_path.as_str(), error))?;
    }
    Ok(finalized)
}

/// A drive-root Pioneer path (`/PIONEER/...`) as a device-relative path.
pub(crate) fn drive_relative(path: &str) -> Result<DeviceRelativePath, SyncError> {
    DeviceRelativePath::new(path.trim_start_matches('/').to_owned())
        .ok_or_else(|| SyncError::planning("Pioneer path hash generated an unsafe path"))
}

fn is_file(transport: &dyn DeviceTransport, path: &DeviceRelativePath) -> Result<bool, SyncError> {
    transport
        .is_regular_file(path)
        .map_err(|error| SyncError::io(path.as_str(), error))
}
