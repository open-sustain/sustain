// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![forbid(unsafe_code)]

//! Sync playlists from the library to external devices (`sustain-device-sync`).
//!
//! Implements the shared device-sync spine of issues #23/#24: device
//! identity and discovery ([`identity`]), an incremental content-aware
//! differ and copy [`engine`], and three on-drive layouts —
//! deduplicated `.m3u8`, one-folder-per-playlist, and Pioneer's
//! `export.pdb` + ANLZ format. The library's database and DSP pipeline
//! are not reached directly: the caller resolves a device's ticked
//! playlists (smart playlists re-evaluated each sync) into the neutral
//! [`model`] inputs and hands them here.

pub mod engine;
pub mod identity;
mod layout;
pub mod model;
mod sanitize;

pub use engine::{plan, sync};
pub use identity::{
    ConnectedDevice, MARKER_FILE, discover, generate_device_id, read_marker, write_marker,
};
pub use model::{
    GenreBytes, Placement, SyncError, SyncInputPlaylist, SyncInputTrack, SyncOutcome, SyncPlan,
    SyncProgress, SyncRequest, SyncStage,
};

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use sustain_domain::{
        DeviceKind, DeviceLayout, FilesPerFolderCap, SyncDevice, SyncDeviceId, TrackId,
    };

    struct Fixture {
        _src: tempfile::TempDir,
        dest: tempfile::TempDir,
        tracks: Vec<SyncInputTrack>,
    }

    fn fixture(count: usize) -> Fixture {
        let src = tempfile::tempdir().expect("src dir");
        let dest = tempfile::tempdir().expect("dest dir");
        let mut tracks = Vec::new();
        for i in 1..=count {
            let path = src.path().join(format!("song{i}.mp3"));
            std::fs::write(&path, format!("audio-data-{i}").repeat(4)).expect("write src");
            tracks.push(SyncInputTrack {
                track_id: TrackId::new(i as i64).expect("id"),
                source_path: path,
                title: format!("Title {i}"),
                artist: format!("Artist {}", (i % 2) + 1),
                album: format!("Album {}", (i % 2) + 1),
                genre: Some("House".into()),
                track_number: Some(i as u32),
                year: Some(2020),
                duration_ms: 200_000,
                rating: 3,
                bpm: Some(128.0),
                key: Some(sustain_domain::MusicalKey::AMinor),
                bitrate_kbps: Some(320),
                sample_rate_hz: 44_100,
                bit_depth: 16,
                file_size: 0,
                date_added: Some("2026-01-01".into()),
                extension: "mp3".into(),
                fingerprint: format!("fp-{i}"),
                waveform_preview: None,
                waveform_detail: None,
                cover_art: None,
            });
        }
        Fixture {
            _src: src,
            dest,
            tracks,
        }
    }

    fn device(layout: DeviceLayout) -> SyncDevice {
        SyncDevice {
            id: SyncDeviceId::new("test-device").expect("id"),
            label: "Test".into(),
            kind: DeviceKind::UsbDrive,
            layout,
            sub_path: String::new(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        }
    }

    fn request(
        fx: &Fixture,
        layout: DeviceLayout,
        prev: Vec<sustain_domain::SyncManifestEntry>,
        remove: bool,
    ) -> SyncRequest {
        SyncRequest {
            device: device(layout),
            mount_path: fx.dest.path().to_path_buf(),
            tracks: fx.tracks.clone(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: (0..fx.tracks.len()).collect(),
            }],
            previous_manifest: prev,
            remove_stale: remove,
            export_date: "2026-01-01".into(),
        }
    }

    fn run(req: &SyncRequest) -> SyncOutcome {
        sync(req, &mut |_| {}, &|| false).expect("sync ok")
    }

    #[test]
    fn m3u_layout_writes_tree_and_playlist() {
        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let outcome = run(&req);
        assert_eq!(outcome.copied, 3);
        assert!(fx.dest.path().join("My Set.m3u8").exists());
        let m3u = std::fs::read_to_string(fx.dest.path().join("My Set.m3u8")).expect("read m3u");
        assert!(m3u.starts_with("#EXTM3U"));
        assert!(m3u.contains("Music/"));
        // Audio tree exists and is deduplicated (3 files).
        let count = walk_files(fx.dest.path().join("Music"));
        assert_eq!(count, 3);
    }

    #[test]
    fn folder_layout_copies_per_playlist_and_is_stable() {
        let fx = fixture(2);
        let req = request(&fx, DeviceLayout::FolderPerPlaylist, Vec::new(), false);
        let first = run(&req);
        assert_eq!(first.copied, 2);
        assert!(fx.dest.path().join("My Set").is_dir());

        // Re-sync with the prior manifest: nothing should be recopied.
        let req2 = request(
            &fx,
            DeviceLayout::FolderPerPlaylist,
            first.manifest.clone(),
            false,
        );
        let second = run(&req2);
        assert_eq!(second.copied, 0);
        assert_eq!(second.updated, 0);
        assert_eq!(second.unchanged, 2);
        // The on-device paths are identical across syncs.
        let mut a: Vec<_> = first.manifest.iter().map(|m| &m.on_device_path).collect();
        let mut b: Vec<_> = second.manifest.iter().map(|m| &m.on_device_path).collect();
        a.sort();
        b.sort();
        assert_eq!(a, b);
    }

    fn named_track(id: i64, title: &str, artist: &str, album: &str) -> SyncInputTrack {
        SyncInputTrack {
            track_id: TrackId::new(id).expect("id"),
            source_path: PathBuf::from(format!("/src/{id}.mp3")),
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            genre: None,
            track_number: None,
            year: None,
            duration_ms: 1000,
            rating: 0,
            bpm: None,
            key: None,
            bitrate_kbps: None,
            sample_rate_hz: 44_100,
            bit_depth: 16,
            file_size: 0,
            date_added: None,
            extension: "mp3".into(),
            fingerprint: format!("fp-{id}"),
            waveform_preview: None,
            waveform_detail: None,
            cover_art: None,
        }
    }

    fn plan_request(
        tracks: Vec<SyncInputTrack>,
        layout: DeviceLayout,
        playlists: Vec<SyncInputPlaylist>,
        prev: Vec<sustain_domain::SyncManifestEntry>,
    ) -> SyncRequest {
        SyncRequest {
            device: device(layout),
            mount_path: PathBuf::from("/mnt/device"),
            tracks,
            playlists,
            previous_manifest: prev,
            remove_stale: false,
            export_date: "2026-01-01".into(),
        }
    }

    fn placement_paths(req: &SyncRequest) -> Vec<String> {
        crate::layout::plan_placements(req)
            .expect("planning succeeds")
            .into_iter()
            .map(|placement| placement.rel_path)
            .collect()
    }

    fn assert_all_unique(paths: &[String]) {
        let set: std::collections::HashSet<&str> = paths.iter().map(String::as_str).collect();
        assert_eq!(
            set.len(),
            paths.len(),
            "planned paths must be unique: {paths:?}"
        );
    }

    #[test]
    fn tree_layout_disambiguates_natural_suffix_collision() {
        // id 12 titled "Foo" disambiguates to "Foo (12)", which the track
        // literally titled "Foo (12)" already reserved — the documented
        // double-collision. The allocator must take a third distinct name.
        let tracks = vec![
            named_track(10, "Foo", "A", "B"),
            named_track(11, "Foo (12)", "A", "B"),
            named_track(12, "Foo", "A", "B"),
        ];
        let req = plan_request(tracks, DeviceLayout::M3u, Vec::new(), Vec::new());
        let paths = placement_paths(&req);
        assert_eq!(paths.len(), 3);
        assert_all_unique(&paths);
    }

    #[test]
    fn tree_layout_keeps_disambiguator_when_the_name_would_truncate() {
        // Two tracks with the same very long title: the plain names collide
        // and a naive append of " (id)" would be truncated off the end,
        // re-colliding. The reserved-suffix allocator must keep them apart.
        let long = "T".repeat(200);
        let tracks = vec![
            named_track(1, &long, "A", "B"),
            named_track(2, &long, "A", "B"),
        ];
        let req = plan_request(tracks, DeviceLayout::Pioneer, Vec::new(), Vec::new());
        let paths = placement_paths(&req);
        assert_eq!(paths.len(), 2);
        assert_all_unique(&paths);
        assert!(
            paths.iter().all(|p| p.len() <= "Contents/A/B/".len() + 120),
            "leaf names stay within the cap: {paths:?}"
        );
    }

    #[test]
    fn tree_layout_disambiguates_titles_that_sanitize_to_one_component() {
        // "AC/DC" and "AC:DC" both sanitize to "AC_DC"; distinct tracks
        // must still land on distinct files.
        let tracks = vec![
            named_track(1, "AC/DC", "A", "B"),
            named_track(2, "AC:DC", "A", "B"),
        ];
        let req = plan_request(tracks, DeviceLayout::M3u, Vec::new(), Vec::new());
        let paths = placement_paths(&req);
        assert_eq!(paths.len(), 2);
        assert_all_unique(&paths);
    }

    #[test]
    fn folder_layout_dedups_a_repeated_playlist_entry() {
        // The playlist lists the same track twice and the prior manifest
        // already placed it: without dedup both entries resolve to the same
        // recovered index and on-device filename, overwriting one copy.
        let tracks = vec![named_track(7, "Only Track", "A", "B")];
        let playlists = vec![SyncInputPlaylist {
            name: "Mix".into(),
            track_indices: vec![0, 0],
        }];
        let prev = vec![sustain_domain::SyncManifestEntry {
            track_id: TrackId::new(7).expect("id"),
            on_device_path: "Mix/001 A - Only Track.mp3".into(),
            fingerprint: "fp-7".into(),
        }];
        let req = plan_request(tracks, DeviceLayout::FolderPerPlaylist, playlists, prev);
        let paths = placement_paths(&req);
        assert_eq!(paths.len(), 1, "a repeated entry is placed once: {paths:?}");
        assert_all_unique(&paths);
    }

    #[test]
    fn pioneer_layout_writes_pdb_and_anlz() {
        let fx = fixture(2);
        let req = request(&fx, DeviceLayout::Pioneer, Vec::new(), false);
        let outcome = run(&req);
        assert_eq!(outcome.copied, 2);
        assert!(fx.dest.path().join("PIONEER/rekordbox/export.pdb").exists());
        assert!(fx.dest.path().join("Contents").is_dir());
        // At least one ANLZ .EXT was written under USBANLZ.
        let exts = walk_files(fx.dest.path().join("PIONEER/USBANLZ"));
        assert!(exts >= 2, "expected per-track ANLZ files, found {exts}");
    }

    #[test]
    fn pioneer_layout_writes_cover_thumbnails() {
        // A 2×2 solid-green PNG — enough for the artwork pipeline to
        // decode, resize, and re-encode.
        const COVER_PNG: &[u8] = &[
            0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48,
            0x44, 0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00,
            0x00, 0xfd, 0xd4, 0x9a, 0x73, 0x00, 0x00, 0x00, 0x0f, 0x49, 0x44, 0x41, 0x54, 0x78,
            0xda, 0x63, 0x60, 0xf8, 0xcf, 0x00, 0x42, 0x10, 0x0a, 0x00, 0x1b, 0xf2, 0x03, 0xfd,
            0xd4, 0x2f, 0x04, 0x80, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42,
            0x60, 0x82,
        ];
        let fx = fixture(2);
        // Both tracks carry the same cover, so de-duplication collapses
        // them to a single artwork id.
        let tracks: Vec<SyncInputTrack> = fx
            .tracks
            .iter()
            .cloned()
            .map(|mut t| {
                t.cover_art = Some(COVER_PNG.to_vec());
                t
            })
            .collect();
        let req = SyncRequest {
            device: device(DeviceLayout::Pioneer),
            mount_path: fx.dest.path().to_path_buf(),
            tracks,
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: Vec::new(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        };
        run(&req);

        let art = fx.dest.path().join("PIONEER/Artwork/00001");
        assert!(art.join("a1.jpg").exists(), "small thumbnail written");
        assert!(art.join("a1_m.jpg").exists(), "large thumbnail written");
        // The shared cover de-duplicates to one id — no second entry.
        assert!(!art.join("a2.jpg").exists(), "shared cover deduplicated");
    }

    #[test]
    fn incremental_resync_copies_nothing_when_unchanged() {
        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let first = run(&req);
        let req2 = request(&fx, DeviceLayout::M3u, first.manifest.clone(), false);
        let second = run(&req2);
        assert_eq!(second.copied, 0);
        assert_eq!(second.unchanged, 3);
    }

    #[test]
    fn removal_only_with_confirmation() {
        let fx = fixture(3);
        // First sync all three.
        let first = run(&request(&fx, DeviceLayout::M3u, Vec::new(), false));

        // Shrink the resolved selection to the first two tracks (the
        // runtime passes only selected tracks as `req.tracks`).
        let shrink = |remove: bool| SyncRequest {
            device: device(DeviceLayout::M3u),
            mount_path: fx.dest.path().to_path_buf(),
            tracks: fx.tracks[..2].to_vec(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: first.manifest.clone(),
            remove_stale: remove,
            export_date: "2026-01-01".into(),
        };

        // Without confirmation, the third file stays and remains tracked.
        let kept = sync(&shrink(false), &mut |_| {}, &|| false).expect("sync");
        assert_eq!(kept.removed, 0);
        assert_eq!(kept.manifest.len(), 3);

        // With confirmation, the stale file is removed.
        let removed = sync(&shrink(true), &mut |_| {}, &|| false).expect("sync");
        assert_eq!(removed.removed, 1);
        assert_eq!(removed.manifest.len(), 2);
    }

    #[test]
    fn marker_is_written_on_sync() {
        let fx = fixture(1);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        run(&req);
        assert_eq!(
            read_marker(fx.dest.path()).map(SyncDeviceId::into_string),
            Some("test-device".to_owned())
        );
    }

    #[test]
    fn plan_breaks_down_footprint_by_genre() {
        let mut fx = fixture(5);
        // Distinct genres + sizes; a None and a whitespace-only tag both
        // collapse into the "Unknown" (None) bucket.
        let specs = [
            (Some("House"), 100u64),
            (Some("Techno"), 300),
            (Some("House"), 50),
            (None, 200),
            (Some("   "), 10),
        ];
        for (track, (genre, size)) in fx.tracks.iter_mut().zip(specs) {
            track.genre = genre.map(str::to_owned);
            track.file_size = size;
        }
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let plan = plan(&req).expect("plan");

        // Largest first; House aggregated (150), Unknown aggregated (210).
        assert_eq!(
            plan.genre_bytes,
            vec![
                GenreBytes {
                    genre: Some("Techno".into()),
                    bytes: 300,
                },
                GenreBytes {
                    genre: None,
                    bytes: 210,
                },
                GenreBytes {
                    genre: Some("House".into()),
                    bytes: 150,
                },
            ]
        );
        // The breakdown accounts for exactly the occupation total.
        let sum: u64 = plan.genre_bytes.iter().map(|g| g.bytes).sum();
        assert_eq!(sum, plan.bytes_total);
    }

    #[test]
    fn empty_selection_is_rejected() {
        let fx = fixture(0);
        let req = SyncRequest {
            device: device(DeviceLayout::M3u),
            mount_path: fx.dest.path().to_path_buf(),
            tracks: Vec::new(),
            playlists: Vec::new(),
            previous_manifest: Vec::new(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        };
        assert!(matches!(
            sync(&req, &mut |_| {}, &|| false),
            Err(SyncError::Empty)
        ));
    }

    fn walk_files(dir: PathBuf) -> usize {
        let mut count = 0;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    count += walk_files(path);
                } else {
                    count += 1;
                }
            }
        }
        count
    }
}
