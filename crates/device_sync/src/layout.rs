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

use sustain_domain::{DeviceKind, DeviceLayout, DeviceRelativePath, WaveformSegments};
use sustain_pioneer::{
    AnlzInput, PioneerFileType, PioneerPlaylist, PioneerTrack, anlz, artwork::ARTWORK_BUCKET,
    path_hash, pdb,
};

use crate::model::{Placement, PreparedSyncRequest, SyncError, SyncRequest};
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
    let placements = match req.device.layout {
        DeviceLayout::M3u => tree_placements(req, MUSIC_DIR)?,
        DeviceLayout::Pioneer => tree_placements(req, CONTENTS_DIR)?,
        DeviceLayout::FolderPerPlaylist => folder_placements(req)?,
    };
    validate_unique(&placements)?;
    Ok(placements)
}

/// Final guard before any filesystem mutation: every planned path must be
/// unique. The allocators already guarantee this, so a duplicate here is a
/// planner bug — fail closed with a typed error rather than letting the
/// copy step and the manifest silently collapse two tracks onto one file.
fn validate_unique(placements: &[Placement]) -> Result<(), SyncError> {
    let mut seen: HashSet<&str> = HashSet::with_capacity(placements.len());
    for placement in placements {
        if !seen.insert(placement.rel_path.as_str()) {
            return Err(SyncError::planning(format!(
                "duplicate device placement path: {}",
                placement.rel_path
            )));
        }
    }
    Ok(())
}

/// Allocate a unique on-device relative path `{dir}/{name}` for a track.
/// The plain sanitized name is tried first; on collision it disambiguates
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
    if used.insert(rel.clone()) {
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
        if used.insert(rel.clone()) {
            return Ok(rel);
        }
    }
    Err(SyncError::planning(format!(
        "could not allocate a unique device path for track {track_id} under {dir}"
    )))
}

/// Write the layout's index/database files after audio is in place.
/// `written` holds the indices (into `placements`) that were (re)copied
/// this run, so the Pioneer writer can refresh only stale ANLZ files.
pub(crate) fn finalize(
    req: &PreparedSyncRequest,
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    placements: &[Placement],
    written: &HashSet<usize>,
    cancel: &dyn Fn() -> bool,
) -> Result<(), SyncError> {
    match req.device.layout {
        DeviceLayout::M3u => write_m3u_playlists(req, transport, base, placements),
        DeviceLayout::FolderPerPlaylist => Ok(()),
        DeviceLayout::Pioneer => write_pioneer(req, transport, base, placements, written, cancel),
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
    let mut placements = Vec::with_capacity(req.tracks.len());
    for (index, track) in req.tracks.iter().enumerate() {
        let artist =
            crate::sanitize::component(&track.artist, TREE_COMPONENT_CAP, "Unknown Artist");
        let album = crate::sanitize::component(&track.album, TREE_COMPONENT_CAP, "Unknown Album");
        let dir = format!("{root_dir}/{artist}/{album}");
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

fn track_stem(track: &crate::model::SyncInputTrack) -> String {
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
        // files keep their slot and only new tracks are appended.
        let prefix = format!("{folder}/");
        let prior: HashMap<sustain_domain::TrackId, u32> = req
            .previous_manifest
            .iter()
            .filter_map(|entry| {
                let rest = entry.on_device_path.as_str().strip_prefix(&prefix)?;
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

fn unique_name(used: &mut HashSet<String>, base: String) -> String {
    if used.insert(base.clone()) {
        return base;
    }
    for n in 2.. {
        let candidate = format!("{base} ({n})");
        if used.insert(candidate.clone()) {
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
        .map(|p| {
            let rel = p.rel_path.as_str();
            (
                p.track_index,
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
    written: &HashSet<usize>,
    cancel: &dyn Fn() -> bool,
) -> Result<(), SyncError> {
    let assets = req.pioneer_assets()?;
    // One placement per track, in `req.tracks` order, so a track's index
    // is its PDB row index.
    let mut pioneer_tracks = Vec::with_capacity(placements.len());
    for (placement_index, placement) in placements.iter().enumerate() {
        let track = &req.tracks[placement.track_index];
        let track_assets = &assets.tracks[placement.track_index];
        let audio_path = format!("/{}", placement.rel_path);
        let anlz_dat = path_hash::anlz_file(&audio_path, "DAT");

        // Write ANLZ when the audio was (re)written this run or when the
        // .EXT is missing on the device (out-of-band deletion / first run).
        let anlz_dir_rel = DeviceRelativePath::new(
            path_hash::anlz_dir(&audio_path)
                .trim_start_matches('/')
                .to_owned(),
        )
        .ok_or_else(|| SyncError::planning("Pioneer path hash generated an unsafe path"))?;
        let anlz_dir = base.join(&anlz_dir_rel);
        let anlz_ext = anlz_dir
            .join_component("ANLZ0000.EXT")
            .expect("static ANLZ filename is safe");
        let needs_anlz = written.contains(&placement_index)
            || !transport
                .is_regular_file(&anlz_ext)
                .map_err(|error| SyncError::io(anlz_ext.as_str(), error))?;
        if needs_anlz {
            let empty = WaveformSegments {
                segment_duration_ms: 0.0,
                segments: Vec::new(),
            };
            let input = AnlzInput {
                device_audio_path: &audio_path,
                bpm: track.bpm,
                duration_ms: track.duration_ms,
                waveform_preview: track_assets.waveform_preview.as_ref().unwrap_or(&empty),
                waveform_detail: track_assets.waveform_detail.as_ref().unwrap_or(&empty),
            };
            let dat = anlz_dir
                .join_component("ANLZ0000.DAT")
                .expect("static ANLZ filename is safe");
            transport
                .write_file(&dat, &anlz::dat_bytes(&input))
                .map_err(|error| SyncError::io(dat.as_str(), error))?;
            transport
                .write_file(&anlz_ext, &anlz::ext_bytes(&input))
                .map_err(|error| SyncError::io(anlz_ext.as_str(), error))?;
        }

        pioneer_tracks.push(PioneerTrack {
            title: track.title.clone(),
            artist: track.artist.clone(),
            album: track.album.clone(),
            genre: track.genre.clone(),
            bpm: track.bpm,
            key: track.key,
            duration_secs: track.duration_ms / 1000,
            file_size: track.source.stat.size_bytes,
            track_number: track.track_number,
            year: track.year,
            rating: track.rating,
            bitrate_kbps: track.bitrate_kbps,
            sample_rate_hz: track.sample_rate_hz,
            bit_depth: track.bit_depth,
            file_type: PioneerFileType::from_extension(&track.extension),
            artwork_id: track_assets.artwork_id,
            date_added: track.date_added.clone(),
            device_audio_path: audio_path,
            device_anlz_path: anlz_dat,
        });
    }

    // Map req.tracks index -> pioneer row index. With one placement per
    // track in track order this is the identity, but build it explicitly
    // so the mapping is robust.
    let row_for: HashMap<usize, usize> = placements
        .iter()
        .enumerate()
        .map(|(row, p)| (p.track_index, row))
        .collect();
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
    Ok(())
}
