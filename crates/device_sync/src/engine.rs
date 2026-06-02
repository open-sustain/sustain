// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The incremental sync engine: diff the resolved selection against the
//! manifest and what is actually on the device, copy only what changed,
//! write the layout's index/database files, and optionally remove stale
//! files behind the caller's confirmation.

use std::collections::HashSet;

use sustain_domain::{DeviceLayout, DeviceRelativePath, SyncManifestEntry};
use sustain_pioneer::path_hash;

use crate::device_root::DeviceRoot;
use crate::layout;
use crate::model::{
    GenreBytes, Placement, PreparedSyncRequest, SyncError, SyncOutcome, SyncPlan, SyncProgress,
    SyncRequest, SyncStage,
};

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
    unchanged: Vec<usize>,
    removals: Vec<DeviceRelativePath>,
    copy_count: usize,
    update_count: usize,
}

fn compute_diff(
    req: &SyncRequest,
    root: &DeviceRoot,
    base: &DeviceRelativePath,
    placements: &[Placement],
) -> Result<Diff, SyncError> {
    use std::collections::HashMap;
    let prev: HashMap<&str, &str> = req
        .previous_manifest
        .iter()
        .map(|e| (e.on_device_path.as_str(), e.fingerprint.as_str()))
        .collect();
    let desired: HashSet<&str> = placements.iter().map(|p| p.rel_path.as_str()).collect();

    let mut to_write = Vec::new();
    let mut unchanged = Vec::new();
    let mut copy_count = 0;
    let mut update_count = 0;
    for (index, placement) in placements.iter().enumerate() {
        let known = prev.get(placement.rel_path.as_str());
        let path = base.join(&placement.rel_path);
        let expected_size = req.tracks[placement.track_index].source.stat.size_bytes;
        let present = root
            .regular_file_len(&path)
            .map_err(|error| SyncError::io(path.as_str(), error))?;
        if known == Some(&placement.fingerprint.as_str()) && present == Some(expected_size) {
            unchanged.push(index);
        } else {
            to_write.push(index);
            if known.is_some() {
                update_count += 1;
            } else {
                copy_count += 1;
            }
        }
    }

    let removals: Vec<DeviceRelativePath> = req
        .previous_manifest
        .iter()
        .filter(|e| !desired.contains(e.on_device_path.as_str()))
        .map(|e| e.on_device_path.clone())
        .collect();

    Ok(Diff {
        to_write,
        unchanged,
        removals,
        copy_count,
        update_count,
    })
}

/// Compute what a sync would do, without writing anything. The UI shows
/// this — particularly `to_remove` — before confirming a destructive run.
pub fn plan(req: &SyncRequest) -> Result<SyncPlan, SyncError> {
    if req.tracks.is_empty() {
        return Err(SyncError::Empty);
    }
    let root =
        DeviceRoot::open(&req.mount_path).map_err(|error| SyncError::io(&req.mount_path, error))?;
    let base = device_base(req);
    let placements = layout::plan_placements(req)?;
    let diff = compute_diff(req, &root, &base, &placements)?;
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
        to_copy: diff.copy_count,
        to_update: diff.update_count,
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
    use std::collections::HashMap;
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
/// without corrupting the device (the manifest returned reflects exactly
/// what is on the device at the stopping point).
pub fn sync(
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
    let root =
        DeviceRoot::open(&req.mount_path).map_err(|error| SyncError::io(&req.mount_path, error))?;
    let base = device_base(req);
    root.cleanup_stale_temporary_files(&base, cancel)
        .map_err(|error| SyncError::io(base.as_str(), error))?;
    root.ensure_dir_all(&base)
        .map_err(|error| SyncError::io(base.as_str(), error))?;

    // Register the device early so even a partial sync is recognised next
    // time (the marker always lives at the mount root, not the sub-path).
    crate::identity::write_marker_to_root(&root, &req.device.id)
        .map_err(|error| SyncError::io(crate::identity::MARKER_FILE, error))?;

    let placements = layout::plan_placements(req)?;
    let diff = compute_diff(req, &root, &base, &placements)?;

    let mut outcome = SyncOutcome {
        unchanged: diff.unchanged.len(),
        ..SyncOutcome::default()
    };
    // Unchanged files stay in the manifest as-is.
    let mut manifest: Vec<SyncManifestEntry> = diff
        .unchanged
        .iter()
        .map(|&i| manifest_entry(req, &placements[i]))
        .collect();
    let mut written: HashSet<usize> = HashSet::new();

    let total = diff.to_write.len();
    for (done, &placement_index) in diff.to_write.iter().enumerate() {
        if cancel() {
            outcome.cancelled = true;
            break;
        }
        let placement = &placements[placement_index];
        let track = &req.tracks[placement.track_index];
        let dest = base.join(&placement.rel_path);
        root.copy_file(&track.source_path, &dest, &track.source.stat)
            .map_err(|error| SyncError::io(&track.source_path, error))?;

        let is_update = req
            .previous_manifest
            .iter()
            .any(|m| m.on_device_path == placement.rel_path);
        if is_update {
            outcome.updated += 1;
        } else {
            outcome.copied += 1;
        }
        written.insert(placement_index);
        manifest.push(manifest_entry(req, placement));
        progress(SyncProgress {
            stage: SyncStage::Copying,
            completed: done + 1,
            total,
        });
    }

    if outcome.cancelled {
        outcome.manifest = manifest;
        outcome.manifest_is_authoritative = true;
        return Ok(outcome);
    }

    // Index/database files. The Pioneer/m3u layouts rewrite these every
    // run because the selection (not just the audio) may have changed.
    if layout::always_finalizes(req.device.layout) || !written.is_empty() {
        let stage = match req.device.layout {
            DeviceLayout::Pioneer => SyncStage::WritingDatabase,
            _ => SyncStage::WritingPlaylists,
        };
        progress(SyncProgress {
            stage,
            completed: 0,
            total: 1,
        });
        layout::finalize(req, &root, &base, &placements, &written, cancel)?;
        progress(SyncProgress {
            stage,
            completed: 1,
            total: 1,
        });
    }

    // Removals, only behind the caller's confirmation.
    if req.remove_stale && !diff.removals.is_empty() {
        let remove_total = diff.removals.len();
        for (done, rel) in diff.removals.iter().enumerate() {
            if cancel() {
                outcome.cancelled = true;
                retain_unremoved_manifest_entries(req, &diff.removals[done..], &mut manifest);
                break;
            }
            remove_placement(req, &root, &base, rel, cancel)?;
            outcome.removed += 1;
            progress(SyncProgress {
                stage: SyncStage::Removing,
                completed: done + 1,
                total: remove_total,
            });
        }
    } else {
        // Stale files left in place remain part of the manifest so the
        // next sync still tracks them.
        for rel in &diff.removals {
            if let Some(entry) = req
                .previous_manifest
                .iter()
                .find(|m| &m.on_device_path == rel)
            {
                manifest.push(entry.clone());
            }
        }
    }

    outcome.manifest = manifest;
    outcome.manifest_is_authoritative = true;
    Ok(outcome)
}

fn manifest_entry(req: &PreparedSyncRequest, placement: &Placement) -> SyncManifestEntry {
    SyncManifestEntry {
        track_id: req.tracks[placement.track_index].track_id,
        on_device_path: placement.rel_path.clone(),
        fingerprint: placement.fingerprint.clone(),
    }
}

/// Delete a stale file (and, for Pioneer, its orphaned ANLZ directory).
/// Best-effort: a failed delete does not abort the sync.
fn remove_placement(
    req: &SyncRequest,
    root: &DeviceRoot,
    base: &DeviceRelativePath,
    rel: &DeviceRelativePath,
    cancel: &dyn Fn() -> bool,
) -> Result<(), SyncError> {
    let audio_path = base.join(rel);
    root.remove_file_if_exists(&audio_path)
        .map_err(|error| SyncError::io(audio_path.as_str(), error))?;
    if req.device.layout == DeviceLayout::Pioneer {
        let anlz_dir = path_hash::anlz_dir(&format!("/{rel}"));
        let anlz_dir = DeviceRelativePath::new(anlz_dir.trim_start_matches('/').to_owned())
            .ok_or_else(|| SyncError::planning("Pioneer path hash generated an unsafe path"))?;
        let anlz_path = base.join(&anlz_dir);
        root.remove_tree_if_exists(&anlz_path, cancel)
            .map_err(|error| SyncError::io(anlz_path.as_str(), error))?;
    }
    Ok(())
}

fn retain_unremoved_manifest_entries(
    req: &SyncRequest,
    removals: &[DeviceRelativePath],
    manifest: &mut Vec<SyncManifestEntry>,
) {
    for rel in removals {
        if let Some(entry) = req
            .previous_manifest
            .iter()
            .find(|entry| &entry.on_device_path == rel)
        {
            manifest.push(entry.clone());
        }
    }
}
