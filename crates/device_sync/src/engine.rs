// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The incremental sync engine: diff the resolved selection against the
//! manifest and what is actually on the device, copy only what changed,
//! write the layout's index/database files, and optionally remove stale
//! files behind the caller's confirmation.

use std::collections::{HashMap, HashSet};

use sustain_domain::{
    AnlzRecord, CatalogueRecord, CollationKey, DeviceLayout, DeviceRelativePath, SyncManifestEntry,
};
use sustain_pioneer::path_hash::AnlzSlot;

use crate::journal::ManifestJournal;
use crate::layout;
use crate::model::{
    GenreBytes, Placement, PreparedSyncRequest, SyncError, SyncFailure, SyncOutcome, SyncPlan,
    SyncProgress, SyncRequest, SyncStage, SyncTrackFailure,
};
use crate::pioneer_export;
use crate::transport::DeviceTransport;

/// The on-device subtree to write under: the Pioneer format owns the drive
/// root (its `PIONEER/` tree is expected there); the other layouts honor the
/// device's validated configured sub-path.
fn device_base(req: &SyncRequest) -> DeviceRelativePath {
    match req.device.layout {
        DeviceLayout::Pioneer => DeviceRelativePath::root(),
        _ => req.device.sub_path.clone(),
    }
}

struct Diff {
    to_write: Vec<usize>,
    /// The placements in `to_write` that replace a file the manifest
    /// already holds in their slot.
    updates: HashSet<usize>,
    unchanged: Vec<usize>,
    /// Manifest files whose slot a placement now claims under another
    /// spelling (an artist or title whose case was corrected). A write
    /// would land in the old directory entry and keep its old name, so
    /// these go before the copies.
    respelled: Vec<DeviceRelativePath>,
    removals: Vec<DeviceRelativePath>,
    /// Recorded playlist files no playlist of this sync writes: the file
    /// of a playlist that left the selection or was renamed.
    stale_playlist_files: Vec<DeviceRelativePath>,
}

impl Diff {
    fn copy_count(&self) -> usize {
        self.to_write.len() - self.updates.len()
    }
}

fn compute_diff(
    req: &SyncRequest,
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    placements: &[Placement],
) -> Result<Diff, SyncError> {
    // Manifest entries by the device file they occupy: an entry from an
    // export planned before paths were compared case-insensitively can
    // share its file with another.
    let mut occupants: HashMap<String, Vec<&SyncManifestEntry>> = HashMap::new();
    for entry in &req.previous_manifest {
        occupants
            .entry(layout::device_path_key(entry.on_device_path.as_str()))
            .or_default()
            .push(entry);
    }
    let desired: HashSet<String> = placements
        .iter()
        .map(|p| layout::device_path_key(p.rel_path.as_str()))
        .collect();

    let mut to_write = Vec::new();
    let mut updates = HashSet::new();
    let mut unchanged = Vec::new();
    let mut respelled = Vec::new();
    for (index, placement) in placements.iter().enumerate() {
        let occupants = occupants
            .get(&layout::device_path_key(placement.rel_path.as_str()))
            .map_or(&[][..], Vec::as_slice);
        let known = occupants
            .iter()
            .find(|e| e.on_device_path == placement.rel_path)
            .map(|e| e.fingerprint.as_str());
        let respelled_before = respelled.len();
        respelled.extend(
            occupants
                .iter()
                .filter(|e| e.on_device_path != placement.rel_path)
                .map(|e| e.on_device_path.clone()),
        );
        let respelt = respelled.len() > respelled_before;
        let path = base.join(&placement.rel_path);
        let expected_size = req.tracks[placement.track_index].source.stat.size_bytes;
        let present = transport
            .regular_file_len(&path)
            .map_err(|error| SyncError::io(path.as_str(), error))?;
        if !respelt
            && known == Some(placement.fingerprint.as_str())
            && present == Some(expected_size)
        {
            unchanged.push(index);
        } else {
            to_write.push(index);
            if !occupants.is_empty() {
                updates.insert(index);
            }
        }
    }

    let removals: Vec<DeviceRelativePath> = req
        .previous_manifest
        .iter()
        .filter(|e| !desired.contains(&layout::device_path_key(e.on_device_path.as_str())))
        .map(|e| e.on_device_path.clone())
        .collect();

    // Compared as the device compares names: a playlist renamed only in
    // case writes the very file its old name recorded, which must stay.
    let written_playlists: HashSet<String> = layout::playlist_files(req)?
        .iter()
        .map(|(path, _playlist)| layout::device_path_key(path.as_str()))
        .collect();
    let stale_playlist_files = req
        .previous_playlist_files
        .iter()
        .filter(|path| !written_playlists.contains(&layout::device_path_key(path.as_str())))
        .cloned()
        .collect();

    Ok(Diff {
        to_write,
        updates,
        unchanged,
        respelled,
        removals,
        stale_playlist_files,
    })
}

/// Compute what a sync would do, without writing anything. The UI shows
/// this — particularly `to_remove` — before confirming a destructive run.
pub fn plan(transport: &dyn DeviceTransport, req: &SyncRequest) -> Result<SyncPlan, SyncError> {
    if req.tracks.is_empty() {
        return Err(SyncError::Empty);
    }
    let base = device_base(req);
    let placements = layout::plan_placements(req)?;
    let diff = compute_diff(req, transport, &base, &placements)?;
    let bytes_to_copy = diff
        .to_write
        .iter()
        .map(|&i| req.tracks[placements[i].track_index].source.stat.size_bytes)
        .sum();
    let bytes_total = placements
        .iter()
        .map(|p| req.tracks[p.track_index].source.stat.size_bytes)
        .sum();
    let genre_bytes = genre_breakdown(req, &placements);
    let blocker = match req.device.layout {
        DeviceLayout::Pioneer => {
            match pioneer_export::ensure_database_is_ours(transport, &base, &req.previous_catalogue)
            {
                Ok(()) => None,
                Err(SyncError::Blocked(blocker)) => Some(blocker),
                Err(error) => return Err(error),
            }
        }
        DeviceLayout::M3u | DeviceLayout::FolderPerPlaylist => None,
    };
    Ok(SyncPlan {
        to_copy: diff.copy_count(),
        to_update: diff.updates.len(),
        to_remove: diff
            .removals
            .into_iter()
            .chain(diff.stale_playlist_files)
            .collect(),
        unchanged: diff.unchanged.len(),
        bytes_to_copy,
        bytes_total,
        genre_bytes,
        blocker,
    })
}

/// Aggregate the placement footprint per genre, mirroring how
/// `bytes_total` is summed so the breakdown adds up to it exactly. A
/// blank or whitespace-only genre tag collapses to `None` ("Unknown").
/// Tags that spell one genre differently ("Deep House", "Deep house":
/// equal as [`CollationKey`]s) are one genre, named by the spelling that
/// carries the most bytes. Ordered largest first, ties broken by genre
/// name for a deterministic (test-stable) result.
fn genre_breakdown(req: &SyncRequest, placements: &[Placement]) -> Vec<GenreBytes> {
    let mut by_spelling: HashMap<Option<&str>, u64> = HashMap::new();
    for placement in placements {
        let track = &req.tracks[placement.track_index];
        let genre = track
            .genre
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty());
        *by_spelling.entry(genre).or_default() += track.source.stat.size_bytes;
    }

    // Each genre: its total, and the spelling to name it by with the bytes
    // that spelling carries.
    let mut by_genre: HashMap<Option<CollationKey>, (u64, Option<&str>, u64)> = HashMap::new();
    for (spelling, bytes) in by_spelling {
        let (total, named, named_bytes) = by_genre
            .entry(spelling.map(CollationKey::new))
            .or_insert((0, spelling, 0));
        *total += bytes;
        if bytes > *named_bytes || (bytes == *named_bytes && spelling < *named) {
            *named = spelling;
            *named_bytes = bytes;
        }
    }

    let mut breakdown: Vec<GenreBytes> = by_genre
        .into_values()
        .map(|(bytes, named, _)| GenreBytes {
            genre: named.map(str::to_owned),
            bytes,
        })
        .collect();
    breakdown.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.genre.cmp(&b.genre)));
    breakdown
}

/// Run the sync. `journal` is told of each change to what the device
/// holds as it is made; `progress` is called as files are processed;
/// `cancel` is polled cooperatively between files and lets a long copy
/// stop early without corrupting the device. The returned manifest preserves files
/// known to be present: unchanged files, successful writes, and every
/// previous file the run left alone — stale files kept or not reached, and
/// files a cancelled run did not get to replace. Recoverable per-track copy
/// failures are reported separately and excluded because the destination
/// state is transport-dependent after a failed publication.
///
/// A run that stops on an error still reports what the device holds, in
/// [`SyncFailure::manifest`]: the files it wrote before the error are on
/// the device too, and a manifest that forgot them would leave them there
/// for good once they leave the selection. The failure is boxed: it is
/// the rare path, and carries every record of the device.
pub fn sync(
    transport: &dyn DeviceTransport,
    req: &PreparedSyncRequest,
    journal: &mut dyn ManifestJournal,
    progress: &mut dyn FnMut(SyncProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<SyncOutcome, Box<SyncFailure>> {
    let untouched = |error: SyncError| {
        Box::new(SyncFailure {
            error,
            manifest: req.previous_manifest.clone(),
            playlist_files: req.previous_playlist_files.clone(),
            catalogue: req.previous_catalogue.clone(),
        })
    };
    if req.tracks.is_empty() {
        return Err(untouched(SyncError::Empty));
    }
    if cancel() {
        return Ok(SyncOutcome {
            cancelled: true,
            manifest: req.previous_manifest.clone(),
            playlist_files: req.previous_playlist_files.clone(),
            catalogue: req.previous_catalogue.clone(),
            ..SyncOutcome::default()
        });
    }
    let base = device_base(req);
    // Before the drive is changed in any way: a Pioneer export that is
    // not Sustain's is left exactly as it is.
    if req.device.layout == DeviceLayout::Pioneer {
        let record = &req.previous_catalogue;
        pioneer_export::ensure_database_is_ours(transport, &base, record).map_err(untouched)?;
        let artwork: Vec<DeviceRelativePath> = req
            .pioneer_assets()
            .map_err(untouched)?
            .artwork
            .files()
            .map(|(name, _)| {
                pioneer_export::artwork_bucket()
                    .join_component(&name)
                    .expect("generated Pioneer artwork filename is safe")
            })
            .collect();
        pioneer_export::ensure_artwork_is_ours(transport, &base, record, &artwork)
            .map_err(untouched)?;
    }
    transport
        .cleanup_stale_temporary_files(&base, cancel)
        .map_err(|error| untouched(SyncError::io(base.as_str(), error)))?;
    transport
        .ensure_dir_all(&base)
        .map_err(|error| untouched(SyncError::io(base.as_str(), error)))?;

    // Register the device early so even a partial sync is recognised next
    // time (the marker always lives at the device root, not the sub-path).
    crate::identity::write_marker_via_transport(transport, &req.device.id)
        .map_err(|error| untouched(SyncError::io(crate::identity::MARKER_FILE, error)))?;

    let placements = layout::plan_placements(req).map_err(untouched)?;
    let diff = compute_diff(req, transport, &base, &placements).map_err(untouched)?;

    let mut run = Run::new(req, &placements, &diff);
    match run.execute(transport, &base, journal, progress, cancel) {
        Ok(()) => Ok(run.into_outcome()),
        Err(error) => Err(Box::new(SyncFailure {
            error,
            manifest: run.device_manifest(),
            playlist_files: run.device_playlist_files(),
            catalogue: run.catalogue.clone(),
        })),
    }
}

/// One sync run's view of the device as it changes it.
struct Run<'a> {
    req: &'a PreparedSyncRequest,
    placements: &'a [Placement],
    diff: &'a Diff,
    /// The analysis pair each previous entry recorded. Until writing the
    /// Pioneer export establishes a new one, that record still describes what is on the
    /// device (an audio rewrite does not touch the pair).
    previous_anlz: HashMap<&'a DeviceRelativePath, &'a AnlzRecord>,
    outcome: SyncOutcome,
    /// Entries for the files the run kept or wrote. `entry_placements`
    /// holds the placement index of each, in step.
    manifest: Vec<SyncManifestEntry>,
    entry_placements: Vec<usize>,
    /// Placements whose file is on the device.
    present: HashSet<usize>,
    written: usize,
    /// What the run settled, so the previous entries it left alone can be
    /// carried over: the device keys of the placements it kept or tried to
    /// write, and the files it deleted.
    claimed: HashSet<String>,
    removed: HashSet<&'a DeviceRelativePath>,
    /// The playlist files the run wrote, and the stale ones it removed.
    playlists_written: Vec<DeviceRelativePath>,
    playlists_removed: HashSet<&'a DeviceRelativePath>,
    /// The Pioneer database and artwork the device holds from Sustain,
    /// kept current as the run writes and removes them.
    catalogue: CatalogueRecord,
}

impl<'a> Run<'a> {
    fn new(req: &'a PreparedSyncRequest, placements: &'a [Placement], diff: &'a Diff) -> Self {
        let previous_anlz = req
            .previous_manifest
            .iter()
            .filter_map(|entry| {
                entry
                    .anlz
                    .as_ref()
                    .map(|anlz| (&entry.on_device_path, anlz))
            })
            .collect();
        let mut run = Self {
            req,
            placements,
            diff,
            previous_anlz,
            outcome: SyncOutcome {
                unchanged: diff.unchanged.len(),
                unplayable: req
                    .tracks
                    .iter()
                    .filter(|track| !layout::plays(req.device.layout, track))
                    .count(),
                ..SyncOutcome::default()
            },
            manifest: Vec::new(),
            entry_placements: Vec::new(),
            present: diff.unchanged.iter().copied().collect(),
            written: 0,
            claimed: HashSet::new(),
            removed: HashSet::new(),
            playlists_written: Vec::new(),
            playlists_removed: HashSet::new(),
            catalogue: req.previous_catalogue.clone(),
        };
        // Unchanged files stay in the manifest as-is.
        for &index in &diff.unchanged {
            run.keep(index);
        }
        run
    }

    /// Record that placement `index`'s file is on the device.
    fn keep(&mut self, index: usize) {
        let placement = &self.placements[index];
        self.manifest.push(manifest_entry(
            self.req,
            placement,
            self.previous_anlz
                .get(&placement.rel_path)
                .map(|&anlz| anlz.clone()),
        ));
        self.entry_placements.push(index);
        self.present.insert(index);
        self.claim(index);
    }

    fn claim(&mut self, index: usize) {
        self.claimed.insert(layout::device_path_key(
            self.placements[index].rel_path.as_str(),
        ));
    }

    fn execute(
        &mut self,
        transport: &dyn DeviceTransport,
        base: &DeviceRelativePath,
        journal: &mut dyn ManifestJournal,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        let (req, placements, diff) = (self.req, self.placements, self.diff);

        // Files whose slot a placement now spells differently go first, and
        // with their analysis pair unless an unchanged entry records it: they
        // are part of writing those placements, so this is not a removal the
        // caller confirms, and not a point to stop at.
        let kept_anlz = recorded_anlz(&self.manifest);
        for rel in &diff.respelled {
            let anlz = self
                .previous_anlz
                .get(rel)
                .copied()
                .filter(|anlz| !kept_anlz.contains(&anlz.dat_path));
            remove_placement(transport, base, rel, anlz)?;
            self.removed.insert(rel);
            journal.forget(rel).map_err(SyncError::Journal)?;
        }

        let total = diff.to_write.len();
        for (done, &placement_index) in diff.to_write.iter().enumerate() {
            if cancel() {
                self.outcome.cancelled = true;
                return Ok(());
            }
            let placement = &placements[placement_index];
            let track = &req.tracks[placement.track_index];
            let dest = base.join(&placement.rel_path);
            // Whatever the write leaves in the slot, the file the manifest
            // knew there is gone.
            self.claim(placement_index);
            if diff.updates.contains(&placement_index) {
                journal
                    .forget(&placement.rel_path)
                    .map_err(SyncError::Journal)?;
            }
            match transport.copy_file(&track.source_path, &dest, &track.source.stat) {
                Ok(()) => {
                    if diff.updates.contains(&placement_index) {
                        self.outcome.updated += 1;
                    } else {
                        self.outcome.copied += 1;
                    }
                    self.written += 1;
                    self.keep(placement_index);
                    let entry = self.manifest.last().expect("the write was just kept");
                    journal.record(entry).map_err(SyncError::Journal)?;
                }
                Err(error) if transport.can_continue_after_copy_error(&error) => {
                    self.outcome.copy_failures.push(SyncTrackFailure {
                        track_id: track.track_id,
                        source_path: track.source_path.clone(),
                        on_device_path: dest,
                        message: error.to_string(),
                    });
                }
                Err(error) => return Err(SyncError::io(&track.source_path, error)),
            }
            progress(SyncProgress {
                stage: SyncStage::Copying,
                completed: done + 1,
                total,
            });
        }

        // Index/database files. The Pioneer/m3u layouts rewrite these every
        // run because the selection (not just the audio) may have changed.
        if layout::always_finalizes(req.device.layout) || self.written > 0 {
            let stage = match req.device.layout {
                DeviceLayout::Pioneer => SyncStage::WritingDatabase,
                _ => SyncStage::WritingPlaylists,
            };
            progress(SyncProgress {
                stage,
                completed: 0,
                total: 1,
            });
            let mut finalized = match req.device.layout {
                DeviceLayout::M3u => {
                    layout::write_m3u_playlists(
                        req,
                        transport,
                        base,
                        placements,
                        &self.present,
                        &mut self.playlists_written,
                    )?;
                    layout::Finalized::default()
                }
                DeviceLayout::FolderPerPlaylist => layout::Finalized::default(),
                DeviceLayout::Pioneer => layout::write_pioneer(
                    req,
                    transport,
                    base,
                    placements,
                    &self.present,
                    &mut self.catalogue,
                    journal,
                )?,
            };
            for (entry, index) in self.manifest.iter_mut().zip(&self.entry_placements) {
                if let Some(anlz) = finalized.anlz.remove(index) {
                    entry.anlz = Some(anlz);
                }
            }
            progress(SyncProgress {
                stage,
                completed: 1,
                total: 1,
            });
        }

        // Removals, only behind the caller's confirmation.
        if req.remove_stale {
            self.remove_stale(transport, base, journal, progress, cancel)?;
        }
        Ok(())
    }

    /// Delete the files the selection no longer holds: stale audio, with
    /// the analysis pairs only it recorded, then the playlist files no
    /// playlist writes any more.
    fn remove_stale(
        &mut self,
        transport: &dyn DeviceTransport,
        base: &DeviceRelativePath,
        journal: &mut dyn ManifestJournal,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &dyn Fn() -> bool,
    ) -> Result<(), SyncError> {
        let diff = self.diff;
        let remove_total = diff.removals.len() + diff.stale_playlist_files.len();
        let mut done = 0;
        // Analysis pairs the new manifest records are never deleted, even
        // if a (corrupt) stale entry names the same slot.
        let kept_anlz = recorded_anlz(&self.manifest);
        for rel in &diff.removals {
            if cancel() {
                self.outcome.cancelled = true;
                return Ok(());
            }
            let anlz = self
                .previous_anlz
                .get(rel)
                .copied()
                .filter(|anlz| !kept_anlz.contains(&anlz.dat_path));
            remove_placement(transport, base, rel, anlz)?;
            self.removed.insert(rel);
            journal.forget(rel).map_err(SyncError::Journal)?;
            self.outcome.removed += 1;
            done += 1;
            progress(SyncProgress {
                stage: SyncStage::Removing,
                completed: done,
                total: remove_total,
            });
        }
        for rel in &diff.stale_playlist_files {
            if cancel() {
                self.outcome.cancelled = true;
                return Ok(());
            }
            remove_file_and_emptied_folders(transport, base, rel)?;
            self.playlists_removed.insert(rel);
            self.outcome.removed += 1;
            done += 1;
            progress(SyncProgress {
                stage: SyncStage::Removing,
                completed: done,
                total: remove_total,
            });
        }
        Ok(())
    }

    /// What the device holds as far as the run knows: the files it kept or
    /// wrote, and every previous file it left alone — a stale file kept
    /// without confirmation or not reached before a stop, or a file the run
    /// did not get to replace.
    fn device_manifest(&self) -> Vec<SyncManifestEntry> {
        let mut manifest = self.manifest.clone();
        manifest.extend(
            self.req
                .previous_manifest
                .iter()
                .filter(|entry| {
                    !self.removed.contains(&entry.on_device_path)
                        && !self
                            .claimed
                            .contains(&layout::device_path_key(entry.on_device_path.as_str()))
                })
                .cloned(),
        );
        manifest
    }

    /// The playlist files the device holds from Sustain as far as the run
    /// knows: those it wrote, and every previously recorded one it neither
    /// removed nor wrote again (under any spelling).
    fn device_playlist_files(&self) -> Vec<DeviceRelativePath> {
        let rewritten: HashSet<String> = self
            .playlists_written
            .iter()
            .map(|path| layout::device_path_key(path.as_str()))
            .collect();
        let mut files = self.playlists_written.clone();
        files.extend(
            self.req
                .previous_playlist_files
                .iter()
                .filter(|path| {
                    !self.playlists_removed.contains(path)
                        && !rewritten.contains(&layout::device_path_key(path.as_str()))
                })
                .cloned(),
        );
        files
    }

    fn into_outcome(self) -> SyncOutcome {
        SyncOutcome {
            manifest: self.device_manifest(),
            playlist_files: self.device_playlist_files(),
            catalogue: self.catalogue.clone(),
            ..self.outcome
        }
    }
}

/// The analysis files `manifest` records.
fn recorded_anlz(manifest: &[SyncManifestEntry]) -> HashSet<DeviceRelativePath> {
    manifest
        .iter()
        .filter_map(|entry| entry.anlz.as_ref().map(|anlz| anlz.dat_path.clone()))
        .collect()
}

fn manifest_entry(
    req: &PreparedSyncRequest,
    placement: &Placement,
    anlz: Option<AnlzRecord>,
) -> SyncManifestEntry {
    SyncManifestEntry {
        track_id: req.tracks[placement.track_index].track_id,
        on_device_path: placement.rel_path.clone(),
        fingerprint: placement.fingerprint.clone(),
        anlz,
    }
}

/// Delete a stale audio file and, when given, the Pioneer analysis pair
/// its entry recorded. Only recorded files are touched: an analysis
/// directory can hold other tracks' pairs, so it is never removed whole,
/// and an entry without a record — or with one that does not name a slot
/// inside the analysis tree — leaves the analysis tree alone. A folder
/// the removal leaves empty goes with its last file.
fn remove_placement(
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    rel: &DeviceRelativePath,
    anlz: Option<&AnlzRecord>,
) -> Result<(), SyncError> {
    remove_file_and_emptied_folders(transport, base, rel)?;
    let slot = anlz.and_then(|anlz| AnlzSlot::from_dat_path(&format!("/{}", anlz.dat_path)));
    if let Some(slot) = slot {
        for path in [slot.dat_path(), slot.ext_path()] {
            remove_file_and_emptied_folders(transport, base, &layout::drive_relative(&path)?)?;
        }
    }
    Ok(())
}

/// Delete the file at `rel` beneath `base`, then each folder on the way
/// up from it that now holds nothing: the album folder of the last track
/// of an album, then the artist folder if that was the artist's last
/// album. A device that lists folders (a car stereo, a deck) would
/// otherwise keep showing a playlist or an artist with nothing in it.
/// Stops at the first folder that still holds anything, and never
/// removes `base` itself.
fn remove_file_and_emptied_folders(
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    rel: &DeviceRelativePath,
) -> Result<(), SyncError> {
    let file = base.join(rel);
    transport
        .remove_file_if_exists(&file)
        .map_err(|error| SyncError::io(file.as_str(), error))?;

    let mut folders: Vec<&str> = rel.components().collect();
    folders.pop();
    while !folders.is_empty() {
        let folder = DeviceRelativePath::new(folders.join("/"))
            .ok_or_else(|| SyncError::planning("a stale file sits under an unsafe folder"))?;
        let folder = base.join(&folder);
        let removed = transport
            .remove_dir_if_empty(&folder)
            .map_err(|error| SyncError::io(folder.as_str(), error))?;
        if !removed {
            break;
        }
        folders.pop();
    }
    Ok(())
}
