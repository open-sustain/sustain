// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The incremental sync engine: diff the resolved selection against the
//! manifest and what is actually on the device, copy only what changed,
//! write the layout's index/database files, and optionally remove stale
//! files behind the caller's confirmation.

use std::collections::{HashMap, HashSet};

use sustain_domain::{AnlzRecord, DeviceLayout, DeviceRelativePath, SyncManifestEntry};
use sustain_pioneer::path_hash::AnlzSlot;

use crate::layout;
use crate::model::{
    GenreBytes, Placement, PreparedSyncRequest, SyncError, SyncOutcome, SyncPlan, SyncProgress,
    SyncRequest, SyncStage, SyncTrackFailure,
};
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

    Ok(Diff {
        to_write,
        updates,
        unchanged,
        respelled,
        removals,
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
    Ok(SyncPlan {
        to_copy: diff.copy_count(),
        to_update: diff.updates.len(),
        to_remove: diff.removals,
        unchanged: diff.unchanged.len(),
        bytes_to_copy,
        bytes_total,
        genre_bytes,
    })
}

/// Aggregate the placement footprint per genre, mirroring how
/// `bytes_total` is summed so the breakdown adds up to it exactly. A
/// blank or whitespace-only genre tag collapses to `None` ("Unknown").
/// Ordered largest first, ties broken by genre name for a deterministic
/// (test-stable) result.
fn genre_breakdown(req: &SyncRequest, placements: &[Placement]) -> Vec<GenreBytes> {
    let mut by_genre: HashMap<Option<String>, u64> = HashMap::new();
    for placement in placements {
        let track = &req.tracks[placement.track_index];
        let genre = track
            .genre
            .as_deref()
            .map(str::trim)
            .filter(|g| !g.is_empty())
            .map(str::to_owned);
        *by_genre.entry(genre).or_default() += track.source.stat.size_bytes;
    }
    let mut breakdown: Vec<GenreBytes> = by_genre
        .into_iter()
        .map(|(genre, bytes)| GenreBytes { genre, bytes })
        .collect();
    breakdown.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.genre.cmp(&b.genre)));
    breakdown
}

/// Run the sync. `progress` is called as files are processed; `cancel`
/// is polled cooperatively between files and lets a long copy stop early
/// without corrupting the device. The returned manifest preserves files
/// known to be present: unchanged files, successful writes, and every
/// previous file the run left alone — stale files kept or not reached, and
/// files a cancelled run did not get to replace. Recoverable per-track copy
/// failures are reported separately and excluded because the destination
/// state is transport-dependent after a failed publication.
pub fn sync(
    transport: &dyn DeviceTransport,
    req: &PreparedSyncRequest,
    progress: &mut dyn FnMut(SyncProgress),
    cancel: &dyn Fn() -> bool,
) -> Result<SyncOutcome, SyncError> {
    if req.tracks.is_empty() {
        return Err(SyncError::Empty);
    }
    if cancel() {
        return Ok(SyncOutcome {
            cancelled: true,
            manifest: req.previous_manifest.clone(),
            manifest_is_authoritative: true,
            ..SyncOutcome::default()
        });
    }
    let base = device_base(req);
    transport
        .cleanup_stale_temporary_files(&base, cancel)
        .map_err(|error| SyncError::io(base.as_str(), error))?;
    transport
        .ensure_dir_all(&base)
        .map_err(|error| SyncError::io(base.as_str(), error))?;

    // Register the device early so even a partial sync is recognised next
    // time (the marker always lives at the device root, not the sub-path).
    crate::identity::write_marker_via_transport(transport, &req.device.id)
        .map_err(|error| SyncError::io(crate::identity::MARKER_FILE, error))?;

    let placements = layout::plan_placements(req)?;
    let diff = compute_diff(req, transport, &base, &placements)?;

    let mut outcome = SyncOutcome {
        unchanged: diff.unchanged.len(),
        unplayable: req
            .tracks
            .iter()
            .filter(|track| !layout::plays(req.device.layout, track))
            .count(),
        ..SyncOutcome::default()
    };
    // Every present entry starts out with the analysis pair its previous
    // entry recorded: until finalize establishes a new one, that record
    // still describes what is on the device (an audio rewrite does not
    // touch the pair).
    let previous_anlz: HashMap<&DeviceRelativePath, &AnlzRecord> = req
        .previous_manifest
        .iter()
        .filter_map(|entry| {
            entry
                .anlz
                .as_ref()
                .map(|anlz| (&entry.on_device_path, anlz))
        })
        .collect();
    let entry = |index: usize| {
        let placement = &placements[index];
        manifest_entry(
            req,
            placement,
            previous_anlz
                .get(&placement.rel_path)
                .map(|&anlz| anlz.clone()),
        )
    };
    // Unchanged files stay in the manifest as-is. `entry_placements` holds
    // the placement index of each manifest entry, in step with `manifest`.
    let mut manifest: Vec<SyncManifestEntry> = diff.unchanged.iter().map(|&i| entry(i)).collect();
    let mut entry_placements: Vec<usize> = diff.unchanged.clone();
    let mut written = 0_usize;
    let mut present: HashSet<usize> = diff.unchanged.iter().copied().collect();
    // What the run settled, so the previous entries it left alone can be
    // carried over: the device keys of the placements it kept or tried to
    // write, and the files it deleted.
    let mut claimed: HashSet<String> = diff
        .unchanged
        .iter()
        .map(|&index| layout::device_path_key(placements[index].rel_path.as_str()))
        .collect();
    let mut removed: HashSet<&DeviceRelativePath> = HashSet::new();

    // Files whose slot a placement now spells differently go first, and
    // with their analysis pair unless an unchanged entry records it: they
    // are part of writing those placements, so this is not a removal the
    // caller confirms, and not a point to stop at.
    let kept_anlz = recorded_anlz(&manifest);
    for rel in &diff.respelled {
        let anlz = previous_anlz
            .get(rel)
            .copied()
            .filter(|anlz| !kept_anlz.contains(&anlz.dat_path));
        remove_placement(transport, &base, rel, anlz)?;
        removed.insert(rel);
    }

    let total = diff.to_write.len();
    for (done, &placement_index) in diff.to_write.iter().enumerate() {
        if cancel() {
            outcome.cancelled = true;
            break;
        }
        let placement = &placements[placement_index];
        let track = &req.tracks[placement.track_index];
        let dest = base.join(&placement.rel_path);
        claimed.insert(layout::device_path_key(placement.rel_path.as_str()));
        match transport.copy_file(&track.source_path, &dest, &track.source.stat) {
            Ok(()) => {
                if diff.updates.contains(&placement_index) {
                    outcome.updated += 1;
                } else {
                    outcome.copied += 1;
                }
                written += 1;
                present.insert(placement_index);
                manifest.push(entry(placement_index));
                entry_placements.push(placement_index);
            }
            Err(error) if transport.can_continue_after_copy_error(&error) => {
                outcome.copy_failures.push(SyncTrackFailure {
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

    if outcome.cancelled {
        outcome.manifest = with_untouched_entries(req, manifest, &claimed, &removed);
        outcome.manifest_is_authoritative = true;
        return Ok(outcome);
    }

    // Index/database files. The Pioneer/m3u layouts rewrite these every
    // run because the selection (not just the audio) may have changed.
    if layout::always_finalizes(req.device.layout) || written > 0 {
        let stage = match req.device.layout {
            DeviceLayout::Pioneer => SyncStage::WritingDatabase,
            _ => SyncStage::WritingPlaylists,
        };
        progress(SyncProgress {
            stage,
            completed: 0,
            total: 1,
        });
        let mut finalized = layout::finalize(req, transport, &base, &placements, &present, cancel)?;
        for (entry, index) in manifest.iter_mut().zip(&entry_placements) {
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
    if req.remove_stale && !diff.removals.is_empty() {
        // Analysis pairs the new manifest records are never deleted, even
        // if a (corrupt) stale entry names the same slot.
        let kept_anlz = recorded_anlz(&manifest);
        let remove_total = diff.removals.len();
        for (done, rel) in diff.removals.iter().enumerate() {
            if cancel() {
                outcome.cancelled = true;
                break;
            }
            let anlz = previous_anlz
                .get(rel)
                .copied()
                .filter(|anlz| !kept_anlz.contains(&anlz.dat_path));
            remove_placement(transport, &base, rel, anlz)?;
            removed.insert(rel);
            outcome.removed += 1;
            progress(SyncProgress {
                stage: SyncStage::Removing,
                completed: done + 1,
                total: remove_total,
            });
        }
    }

    outcome.manifest = with_untouched_entries(req, manifest, &claimed, &removed);
    outcome.manifest_is_authoritative = true;
    Ok(outcome)
}

/// `manifest` with the previous entries the run left alone: a stale file
/// kept without confirmation or not reached before a cancel, or a file a
/// cancelled run did not get to replace, is still on the device, and the
/// next sync must keep tracking it. `claimed` holds the device keys of the
/// placements the run kept or tried to write, `removed` the files it
/// deleted.
fn with_untouched_entries(
    req: &SyncRequest,
    mut manifest: Vec<SyncManifestEntry>,
    claimed: &HashSet<String>,
    removed: &HashSet<&DeviceRelativePath>,
) -> Vec<SyncManifestEntry> {
    manifest.extend(
        req.previous_manifest
            .iter()
            .filter(|entry| {
                !removed.contains(&entry.on_device_path)
                    && !claimed.contains(&layout::device_path_key(entry.on_device_path.as_str()))
            })
            .cloned(),
    );
    manifest
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
/// inside the analysis tree — leaves the analysis tree alone.
fn remove_placement(
    transport: &dyn DeviceTransport,
    base: &DeviceRelativePath,
    rel: &DeviceRelativePath,
    anlz: Option<&AnlzRecord>,
) -> Result<(), SyncError> {
    let audio_path = base.join(rel);
    transport
        .remove_file_if_exists(&audio_path)
        .map_err(|error| SyncError::io(audio_path.as_str(), error))?;
    let slot = anlz.and_then(|anlz| AnlzSlot::from_dat_path(&format!("/{}", anlz.dat_path)));
    if let Some(slot) = slot {
        for path in [slot.dat_path(), slot.ext_path()] {
            let path = base.join(&layout::drive_relative(&path)?);
            transport
                .remove_file_if_exists(&path)
                .map_err(|error| SyncError::io(path.as_str(), error))?;
        }
    }
    Ok(())
}
