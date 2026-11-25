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

mod device_root;
pub mod engine;
pub mod identity;
pub mod journal;
mod layout;
pub mod model;
mod pioneer_export;
mod sanitize;
mod source;
pub mod transport;

pub use device_root::DeviceRoot;
pub use engine::{plan, sync};
pub use identity::{
    ConnectedDevice, MARKER_FILE, discover, generate_device_id, read_marker,
    read_marker_via_transport, write_marker, write_marker_via_transport,
};
pub use journal::{JournalError, ManifestJournal};
pub use layout::device_path_key;
pub use model::{
    DeviceCapacity, GenreBytes, PioneerTrackAnalysis, Placement, PreparedPioneerAssets,
    PreparedSyncRequest, SourceSnapshot, SyncBlocker, SyncError, SyncFailure, SyncInputPlaylist,
    SyncInputTrack, SyncOutcome, SyncPlan, SyncProgress, SyncRequest, SyncStage, SyncTrackFailure,
};
pub use source::{ensure_source_unchanged, resolve_source_fingerprint, source_file_stat};
pub use transport::{DeviceTarget, DeviceTransport, DirectoryWorkBudget, MtpTarget};

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        io,
        path::{Path, PathBuf},
        time::Duration,
    };
    use sustain_domain::{
        AnlzRecord, BeatGrid, Bpm, CatalogueRecord, DeviceKind, DeviceLayout, DeviceRelativePath,
        FilesPerFolderCap, SourceFileStat, SyncDevice, SyncDeviceId, TrackId,
    };

    struct Fixture {
        _src: tempfile::TempDir,
        dest: tempfile::TempDir,
        tracks: Vec<SyncInputTrack>,
    }

    // A 2×2 solid-green PNG — enough for the artwork pipeline to
    // decode, resize, and re-encode.
    const COVER_PNG: &[u8] = &[
        0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44,
        0x52, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00, 0x02, 0x08, 0x02, 0x00, 0x00, 0x00, 0xfd,
        0xd4, 0x9a, 0x73, 0x00, 0x00, 0x00, 0x0f, 0x49, 0x44, 0x41, 0x54, 0x78, 0xda, 0x63, 0x60,
        0xf8, 0xcf, 0x00, 0x42, 0x10, 0x0a, 0x00, 0x1b, 0xf2, 0x03, 0xfd, 0xd4, 0x2f, 0x04, 0x80,
        0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae, 0x42, 0x60, 0x82,
    ];

    fn fixture(count: usize) -> Fixture {
        let src = tempfile::tempdir().expect("src dir");
        let dest = tempfile::tempdir().expect("dest dir");
        let mut tracks = Vec::new();
        for i in 1..=count {
            let path = src.path().join(format!("song{i}.mp3"));
            std::fs::write(&path, format!("audio-data-{i}").repeat(4)).expect("write src");
            let source = SourceSnapshot::resolved(
                resolve_source_fingerprint(&path, None).expect("resolve source fingerprint"),
            );
            tracks.push(SyncInputTrack {
                track_id: TrackId::new(i as i64).expect("id"),
                source_path: path,
                title: format!("Title {i}"),
                artist: format!("Artist {}", (i % 2) + 1),
                album: format!("Album {}", (i % 2) + 1),
                genre: Some("House".into()),
                composer: None,
                comment: None,
                track_number: Some(i as u32),
                disc_number: None,
                year: Some(2020),
                duration_ms: 200_000,
                rating: 3,
                bpm: Bpm::from_whole(128),
                key: Some(sustain_domain::MusicalKey::AMinor),
                bitrate_kbps: Some(320),
                sample_rate_hz: 44_100,
                bit_depth: 16,
                source,
                date_added: Some("2026-01-01".into()),
                extension: "mp3".into(),
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
            sub_path: DeviceRelativePath::root(),
            files_per_folder_cap: FilesPerFolderCap::Unlimited,
            volume_id: None,
        }
    }

    /// Open a filesystem transport on a scratch device root.
    fn transport(dest: &Path) -> DeviceRoot {
        DeviceRoot::open(dest).expect("open scratch device root")
    }

    /// A journal replaying each change over the manifest a run starts
    /// from.
    struct ReplayJournal {
        manifest: std::collections::BTreeMap<String, sustain_domain::SyncManifestEntry>,
        catalogue: CatalogueRecord,
    }

    impl ReplayJournal {
        fn over(req: &SyncRequest) -> Self {
            Self {
                manifest: req
                    .previous_manifest
                    .iter()
                    .map(|entry| (entry.on_device_path.as_str().to_owned(), entry.clone()))
                    .collect(),
                catalogue: req.previous_catalogue.clone(),
            }
        }

        fn manifest(&self) -> Vec<sustain_domain::SyncManifestEntry> {
            self.manifest.values().cloned().collect()
        }
    }

    impl ManifestJournal for ReplayJournal {
        fn record(
            &mut self,
            entry: &sustain_domain::SyncManifestEntry,
        ) -> Result<(), JournalError> {
            self.manifest
                .insert(entry.on_device_path.as_str().to_owned(), entry.clone());
            Ok(())
        }

        fn forget(&mut self, path: &DeviceRelativePath) -> Result<(), JournalError> {
            self.manifest.remove(path.as_str());
            Ok(())
        }

        fn record_catalogue(&mut self, record: &CatalogueRecord) -> Result<(), JournalError> {
            self.catalogue = record.clone();
            Ok(())
        }
    }

    /// The files a manifest tracks, by path, without the analysis pairs
    /// only a finished Pioneer run records.
    fn tracked_files(
        manifest: &[sustain_domain::SyncManifestEntry],
    ) -> Vec<(String, TrackId, String)> {
        let mut files: Vec<_> = manifest
            .iter()
            .map(|entry| {
                (
                    entry.on_device_path.as_str().to_owned(),
                    entry.track_id,
                    entry.fingerprint.clone(),
                )
            })
            .collect();
        files.sort();
        files
    }

    /// Run [`sync`], checking that its journal, replayed over the manifest
    /// the run starts from, gives the manifest the run reports — however
    /// the run ends.
    fn journaled_sync(
        transport: &dyn DeviceTransport,
        req: &PreparedSyncRequest,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &dyn Fn() -> bool,
    ) -> Result<SyncOutcome, Box<SyncFailure>> {
        journaled_sync_keeping_journal(transport, req, progress, cancel).0
    }

    /// [`journaled_sync`], also returning what its journal holds: all
    /// that survives a run whose process dies.
    fn journaled_sync_keeping_journal(
        transport: &dyn DeviceTransport,
        req: &PreparedSyncRequest,
        progress: &mut dyn FnMut(SyncProgress),
        cancel: &dyn Fn() -> bool,
    ) -> (Result<SyncOutcome, Box<SyncFailure>>, ReplayJournal) {
        let mut journal = ReplayJournal::over(req);
        let result = sync(transport, req, &mut journal, progress, cancel);
        let (reported, reported_export) = match &result {
            Ok(outcome) => (&outcome.manifest, &outcome.catalogue),
            Err(failure) => (&failure.manifest, &failure.catalogue),
        };
        assert_eq!(
            tracked_files(&journal.manifest()),
            tracked_files(reported),
            "the journal replays to the manifest the run reports"
        );
        // The journal vouches for at least what the run reports.
        assert!(
            journal
                .catalogue
                .databases
                .is_superset(&reported_export.databases)
                && journal
                    .catalogue
                    .artwork
                    .is_superset(&reported_export.artwork),
            "the journal vouches for the Pioneer export the run reports"
        );
        (result, journal)
    }

    /// A device whose copy to `fail_path` fails, either as an error the
    /// sync can carry on past (MTP's per-file failures) or as one that
    /// stops it (a full or unplugged drive).
    struct CopyFailureTransport {
        root: DeviceRoot,
        fail_path: DeviceRelativePath,
        recoverable: bool,
    }

    impl CopyFailureTransport {
        fn recoverable(root: DeviceRoot, fail_path: DeviceRelativePath) -> Self {
            Self {
                root,
                fail_path,
                recoverable: true,
            }
        }

        fn fatal(root: DeviceRoot, fail_path: DeviceRelativePath) -> Self {
            Self {
                root,
                fail_path,
                recoverable: false,
            }
        }
    }

    impl DeviceTransport for CopyFailureTransport {
        fn capacity(&self) -> io::Result<DeviceCapacity> {
            self.root.capacity()
        }

        fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()> {
            self.root.ensure_dir_all(path)
        }

        fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.root.is_regular_file(path)
        }

        fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>> {
            self.root.regular_file_len(path)
        }

        fn read_file(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<Vec<u8>> {
            self.root.read_file(path, limit)
        }

        fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
            self.root.write_file(path, bytes)
        }

        fn copy_file(
            &self,
            source_path: &Path,
            path: &DeviceRelativePath,
            expected: &SourceFileStat,
        ) -> io::Result<()> {
            if path == &self.fail_path {
                return Err(io::Error::other("injected copy failure"));
            }
            self.root.copy_file(source_path, path, expected)
        }

        fn can_continue_after_copy_error(&self, _error: &io::Error) -> bool {
            self.recoverable
        }

        fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.root.remove_file_if_exists(path)
        }

        fn remove_dir_if_empty(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.root.remove_dir_if_empty(path)
        }

        fn remove_tree_if_exists(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<bool> {
            self.root.remove_tree_if_exists(path, cancel)
        }

        fn cleanup_stale_temporary_files(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<()> {
            self.root.cleanup_stale_temporary_files(path, cancel)
        }
    }

    /// A device that ignores letter case, as FAT32 and exFAT do: every
    /// path names the file of its lowercase spelling.
    struct CaseInsensitiveTransport(DeviceRoot);

    fn folded(path: &DeviceRelativePath) -> DeviceRelativePath {
        DeviceRelativePath::new(path.as_str().to_lowercase()).expect("lowercase keeps a path safe")
    }

    impl DeviceTransport for CaseInsensitiveTransport {
        fn capacity(&self) -> io::Result<DeviceCapacity> {
            self.0.capacity()
        }

        fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()> {
            self.0.ensure_dir_all(&folded(path))
        }

        fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.0.is_regular_file(&folded(path))
        }

        fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>> {
            self.0.regular_file_len(&folded(path))
        }

        fn read_file(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<Vec<u8>> {
            self.0.read_file(&folded(path), limit)
        }

        fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
            self.0.write_file(&folded(path), bytes)
        }

        fn copy_file(
            &self,
            source_path: &Path,
            path: &DeviceRelativePath,
            expected: &SourceFileStat,
        ) -> io::Result<()> {
            self.0.copy_file(source_path, &folded(path), expected)
        }

        fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.0.remove_file_if_exists(&folded(path))
        }

        fn remove_dir_if_empty(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            self.0.remove_dir_if_empty(&folded(path))
        }

        fn remove_tree_if_exists(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<bool> {
            self.0.remove_tree_if_exists(&folded(path), cancel)
        }

        fn cleanup_stale_temporary_files(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<()> {
            self.0.cleanup_stale_temporary_files(&folded(path), cancel)
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
            tracks: fx.tracks.clone(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: (0..fx.tracks.len()).collect(),
            }],
            previous_manifest: prev,
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: remove,
            export_date: "2026-01-01".into(),
        }
    }

    /// A request for the same tracks, carrying everything `previous` left
    /// on the device.
    fn resync_request(
        fx: &Fixture,
        layout: DeviceLayout,
        previous: &SyncOutcome,
        remove: bool,
    ) -> SyncRequest {
        SyncRequest {
            previous_playlist_files: previous.playlist_files.clone(),
            previous_catalogue: previous.catalogue.clone(),
            ..request(fx, layout, previous.manifest.clone(), remove)
        }
    }

    /// Resolved fixtures always carry a content hash, so promoting a request
    /// to the mutating engine cannot fail; the helper unwraps for the tests.
    fn prepared(req: &SyncRequest) -> PreparedSyncRequest {
        let analyses = vec![PioneerTrackAnalysis::default(); req.tracks.len()];
        prepared_with(req, analyses)
    }

    /// `req` prepared with `analyses`, one per track, when it targets a
    /// Pioneer layout.
    fn prepared_with(
        req: &SyncRequest,
        analyses: Vec<PioneerTrackAnalysis>,
    ) -> PreparedSyncRequest {
        let pioneer_assets = (req.device.layout == DeviceLayout::Pioneer).then(|| {
            let mut assets = PreparedPioneerAssets::new();
            for analysis in analyses {
                assets.push_track(analysis, None);
            }
            assets
        });
        PreparedSyncRequest::new(req.clone(), pioneer_assets)
            .expect("fixture sources are all resolved")
    }

    fn run(dest: &Path, req: &SyncRequest) -> SyncOutcome {
        run_prepared(dest, &prepared(req))
    }

    fn run_prepared(dest: &Path, prepared: &PreparedSyncRequest) -> SyncOutcome {
        journaled_sync(&transport(dest), prepared, &mut |_| {}, &|| false).expect("sync ok")
    }

    fn device_path(path: &str) -> DeviceRelativePath {
        DeviceRelativePath::new(path).expect("safe path")
    }

    /// A request for the same tracks with one playlist per name, carrying
    /// what `previous` left on the device.
    fn playlists_request(
        fx: &Fixture,
        layout: DeviceLayout,
        names: &[&str],
        previous: &SyncOutcome,
        remove: bool,
    ) -> SyncRequest {
        SyncRequest {
            playlists: names
                .iter()
                .map(|name| SyncInputPlaylist {
                    name: (*name).into(),
                    track_indices: (0..fx.tracks.len()).collect(),
                })
                .collect(),
            previous_manifest: previous.manifest.clone(),
            previous_playlist_files: previous.playlist_files.clone(),
            previous_catalogue: previous.catalogue.clone(),
            ..request(fx, layout, Vec::new(), remove)
        }
    }

    #[test]
    fn the_file_of_a_playlist_left_out_goes_only_with_confirmation() {
        let fx = fixture(2);
        let first = run(
            fx.dest.path(),
            &playlists_request(
                &fx,
                DeviceLayout::M3u,
                &["Mix", "Chill"],
                &SyncOutcome::default(),
                false,
            ),
        );
        assert_eq!(
            first.playlist_files,
            [device_path("Mix.m3u8"), device_path("Chill.m3u8")]
        );
        // A playlist file Sustain did not write is never its to remove.
        std::fs::write(fx.dest.path().join("Mine.m3u8"), "#EXTM3U\n").expect("user playlist");

        // "Mix" renamed, "Chill" unticked.
        let renamed =
            |remove| playlists_request(&fx, DeviceLayout::M3u, &["Mix 2026"], &first, remove);
        let planned = plan(&transport(fx.dest.path()), &renamed(true)).expect("plan");
        assert_eq!(
            planned.to_remove,
            [device_path("Mix.m3u8"), device_path("Chill.m3u8")]
        );

        let kept = run(fx.dest.path(), &renamed(false));
        assert_eq!(kept.removed, 0);
        assert!(fx.dest.path().join("Mix.m3u8").is_file());
        assert_eq!(
            kept.playlist_files,
            [
                device_path("Mix 2026.m3u8"),
                device_path("Mix.m3u8"),
                device_path("Chill.m3u8")
            ]
        );

        let removed = run(
            fx.dest.path(),
            &playlists_request(&fx, DeviceLayout::M3u, &["Mix 2026"], &kept, true),
        );
        assert_eq!(removed.removed, 2);
        assert_eq!(removed.playlist_files, [device_path("Mix 2026.m3u8")]);
        assert!(!fx.dest.path().join("Mix.m3u8").exists());
        assert!(!fx.dest.path().join("Chill.m3u8").exists());
        assert!(fx.dest.path().join("Mix 2026.m3u8").is_file());
        assert!(fx.dest.path().join("Mine.m3u8").is_file());
        // The tracks stay: the remaining playlist holds them all.
        assert_eq!(removed.manifest.len(), 2);
    }

    #[test]
    fn a_playlist_renamed_in_case_keeps_its_file_on_a_device_that_ignores_case() {
        let fx = fixture(1);
        let device = || CaseInsensitiveTransport(transport(fx.dest.path()));
        let first = journaled_sync(
            &device(),
            &prepared(&playlists_request(
                &fx,
                DeviceLayout::M3u,
                &["mix"],
                &SyncOutcome::default(),
                false,
            )),
            &mut |_| {},
            &|| false,
        )
        .expect("sync");

        let req = playlists_request(&fx, DeviceLayout::M3u, &["Mix"], &first, true);
        assert!(plan(&device(), &req).expect("plan").to_remove.is_empty());
        let second =
            journaled_sync(&device(), &prepared(&req), &mut |_| {}, &|| false).expect("sync");

        assert_eq!(second.removed, 0);
        assert_eq!(second.playlist_files, [device_path("Mix.m3u8")]);
        assert!(fx.dest.path().join("mix.m3u8").is_file());
    }

    #[test]
    fn leaving_the_m3u_layout_makes_every_playlist_file_stale() {
        let fx = fixture(1);
        let first = run(
            fx.dest.path(),
            &playlists_request(
                &fx,
                DeviceLayout::M3u,
                &["Mix"],
                &SyncOutcome::default(),
                false,
            ),
        );

        let outcome = run(
            fx.dest.path(),
            &SyncRequest {
                previous_manifest: Vec::new(),
                ..playlists_request(&fx, DeviceLayout::FolderPerPlaylist, &["Mix"], &first, true)
            },
        );

        assert!(outcome.playlist_files.is_empty());
        assert!(!fx.dest.path().join("Mix.m3u8").exists());
    }

    #[test]
    fn playlists_of_one_name_each_get_a_playlist_file() {
        let fx = fixture(2);
        let mut req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        req.playlists = ["Mix", "mix", "Mix"]
            .into_iter()
            .enumerate()
            .map(|(index, name)| SyncInputPlaylist {
                name: name.into(),
                track_indices: vec![index % 2],
            })
            .collect();
        run(fx.dest.path(), &req);

        let mut playlists: Vec<String> = std::fs::read_dir(fx.dest.path())
            .expect("read device")
            .map(|entry| {
                entry
                    .expect("entry")
                    .file_name()
                    .into_string()
                    .expect("name")
            })
            .filter(|name| name.to_lowercase().starts_with("mix"))
            .collect();
        playlists.sort();
        assert_eq!(playlists, ["Mix (3).m3u8", "Mix.m3u8", "mix (2).m3u8"]);
    }

    #[test]
    fn a_tag_holding_a_line_break_stays_on_its_playlist_line() {
        let mut fx = fixture(2);
        fx.tracks[0].title = "First line\r\nMusic/not-a-track.mp3".to_owned();
        fx.tracks[0].artist = "Tab\there\u{85}and there".to_owned();
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        run(fx.dest.path(), &req);

        let m3u = std::fs::read_to_string(fx.dest.path().join("My Set.m3u8")).expect("read m3u");
        let lines: Vec<&str> = m3u.split(['\n', '\r', '\u{85}']).collect();
        // The header, then a description and a path per track, then the
        // empty piece after the last line.
        assert_eq!(lines.len(), 6, "{m3u:?}");
        assert_eq!(
            lines[1].split_once(',').map(|(_, text)| text),
            Some("Tab here and there - First line  Music/not-a-track.mp3")
        );
        for path in [lines[2], lines[4]] {
            assert!(fx.dest.path().join(path).is_file(), "{path:?}");
        }
    }

    #[test]
    fn an_android_entry_starting_with_a_hash_is_not_a_comment() {
        let mut fx = fixture(1);
        fx.tracks[0].artist = "#1 Dads".to_owned();
        let mut req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        req.device.kind = DeviceKind::Android;
        run(fx.dest.path(), &req);

        let m3u =
            std::fs::read_to_string(fx.dest.path().join("Music/My Set.m3u8")).expect("read m3u");
        let entries: Vec<&str> = m3u.lines().filter(|line| !line.starts_with('#')).collect();
        assert_eq!(entries.len(), 1, "{m3u:?}");
        assert!(entries[0].starts_with("./#1 Dads/"), "{m3u:?}");
        assert!(fx.dest.path().join("Music").join(entries[0]).is_file());
    }

    #[test]
    fn m3u_layout_writes_tree_and_playlist() {
        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let outcome = run(fx.dest.path(), &req);
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
        let first = run(fx.dest.path(), &req);
        assert_eq!(first.copied, 2);
        assert!(fx.dest.path().join("My Set").is_dir());

        // Re-sync with the prior manifest: nothing should be recopied.
        let req2 = request(
            &fx,
            DeviceLayout::FolderPerPlaylist,
            first.manifest.clone(),
            false,
        );
        let second = run(fx.dest.path(), &req2);
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
            composer: None,
            comment: None,
            track_number: None,
            disc_number: None,
            year: None,
            duration_ms: 1000,
            rating: 0,
            bpm: None,
            key: None,
            bitrate_kbps: None,
            sample_rate_hz: 44_100,
            bit_depth: 16,
            // Fake source path that is never copied — these tracks only
            // exercise layout planning, so a provisional stat (no hash) is
            // enough and keeps the request out of the mutating engine.
            source: SourceSnapshot::provisional(SourceFileStat {
                device: 1,
                inode: id as u64,
                size_bytes: 0,
                modified_at_ns: 0,
                changed_at_ns: 0,
            }),
            date_added: None,
            extension: "mp3".into(),
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
            tracks,
            playlists,
            previous_manifest: prev,
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        }
    }

    fn placement_paths(req: &SyncRequest) -> Vec<String> {
        crate::layout::plan_placements(req)
            .expect("planning succeeds")
            .into_iter()
            .map(|placement| placement.rel_path.as_str().to_owned())
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
    fn tree_layout_keeps_paths_that_differ_only_in_case_apart() {
        // One file on a FAT32 or exFAT stick: the second track would
        // overwrite the first. They share the first track's folders.
        let tracks = vec![
            named_track(1, "Intro", "Daft Punk", "Discovery"),
            named_track(2, "intro", "daft punk", "discovery"),
        ];
        let req = plan_request(tracks, DeviceLayout::Pioneer, Vec::new(), Vec::new());
        let paths = placement_paths(&req);
        assert_eq!(
            paths,
            [
                "Contents/Daft Punk/Discovery/Intro.mp3",
                "Contents/Daft Punk/Discovery/intro (2).mp3",
            ]
        );
    }

    #[test]
    fn folder_layout_keeps_playlists_that_differ_only_in_case_apart() {
        let tracks = vec![
            named_track(1, "One", "A", "B"),
            named_track(2, "Two", "A", "B"),
        ];
        let playlists = vec![
            SyncInputPlaylist {
                name: "Techno".into(),
                track_indices: vec![0],
            },
            SyncInputPlaylist {
                name: "techno".into(),
                track_indices: vec![1],
            },
        ];
        let req = plan_request(
            tracks,
            DeviceLayout::FolderPerPlaylist,
            playlists,
            Vec::new(),
        );
        assert_eq!(
            placement_paths(&req),
            ["Techno/001 A - One.mp3", "techno (2)/001 A - Two.mp3"]
        );
    }

    #[test]
    fn folder_layout_keeps_a_disambiguated_playlist_folder_within_its_cap() {
        let tracks = vec![
            named_track(1, "One", "A", "B"),
            named_track(2, "Two", "A", "B"),
        ];
        let name = "Late Night Motorway Driving Mix Vol";
        let playlists = vec![
            SyncInputPlaylist {
                name: name.into(),
                track_indices: vec![0],
            },
            SyncInputPlaylist {
                name: name.into(),
                track_indices: vec![1],
            },
        ];
        let req = plan_request(
            tracks,
            DeviceLayout::FolderPerPlaylist,
            playlists,
            Vec::new(),
        );
        let folders: Vec<String> = placement_paths(&req)
            .iter()
            .map(|path| path.split('/').next().unwrap_or_default().to_owned())
            .collect();
        assert_eq!(
            folders,
            [
                "Late Night Motorway Driving Mix",
                "Late Night Motorway Driving (2)"
            ]
        );
        assert!(folders.iter().all(|folder| folder.chars().count() <= 32));
    }

    #[test]
    fn a_disambiguated_playlist_file_keeps_its_extension_within_the_cap() {
        let fx = fixture(1);
        let mut req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let name = "N".repeat(60);
        req.playlists = vec![
            SyncInputPlaylist {
                name: name.clone(),
                track_indices: vec![0],
            },
            SyncInputPlaylist {
                name,
                track_indices: vec![0],
            },
        ];
        let files: Vec<String> = crate::layout::playlist_files(&req)
            .expect("playlist files")
            .into_iter()
            .map(|(path, _playlist)| path.as_str().to_owned())
            .collect();
        assert_eq!(files.len(), 2);
        assert_ne!(files[0], files[1]);
        for file in &files {
            assert!(file.ends_with(".m3u8"), "{file}");
            assert!(file.chars().count() <= 60, "{file}");
        }
    }

    #[test]
    fn tree_layout_fits_a_long_multibyte_title_in_one_device_name() {
        // 100 kanji are 300 UTF-8 bytes: within the 120-character cap,
        // past the 255 bytes a Linux filesystem takes in one name.
        let mut fx = fixture(1);
        fx.tracks[0].title = "夜".repeat(100);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let paths = placement_paths(&req);
        let leaf = paths[0].rsplit('/').next().unwrap_or_default();
        assert!(leaf.len() <= 255, "{} bytes: {leaf}", leaf.len());
        assert!(leaf.ends_with(".mp3"));

        let outcome = run(fx.dest.path(), &req);
        assert!(outcome.copy_failures.is_empty());
        assert!(fx.dest.path().join(&paths[0]).is_file());
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
            on_device_path: DeviceRelativePath::new("Mix/001 A - Only Track.mp3")
                .expect("safe path"),
            fingerprint: "fp-7".into(),
            anlz: None,
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
        let outcome = run(fx.dest.path(), &req);
        assert_eq!(outcome.copied, 2);
        assert!(fx.dest.path().join("PIONEER/rekordbox/export.pdb").exists());
        assert!(fx.dest.path().join("Contents").is_dir());
        // At least one ANLZ .EXT was written under USBANLZ.
        let exts = walk_files(fx.dest.path().join("PIONEER/USBANLZ"));
        assert!(exts >= 2, "expected per-track ANLZ files, found {exts}");
    }

    #[test]
    fn pioneer_layout_seeds_colour_waveform_settings_but_keeps_existing_ones() {
        let fx = fixture(1);
        let settings = fx.dest.path().join("PIONEER/DEVSETTING.DAT");
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        assert_eq!(
            std::fs::read(&settings).expect("settings seeded"),
            sustain_pioneer::device_setting::colour_waveform_bytes()
        );

        // Settings changed on a player (or by rekordbox) survive a resync.
        std::fs::write(&settings, b"set on the player").expect("player settings");
        run(
            fx.dest.path(),
            &resync_request(&fx, DeviceLayout::Pioneer, &first, false),
        );
        assert_eq!(
            std::fs::read(&settings).expect("settings"),
            b"set on the player"
        );
    }

    #[test]
    fn pioneer_layout_leaves_off_formats_the_player_cannot_play() {
        let mut fx = fixture(3);
        fx.tracks[2].extension = "opus".into();
        let opus_id = fx.tracks[2].track_id;

        let outcome = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        assert_eq!((outcome.copied, outcome.unplayable), (2, 1));
        assert!(
            outcome
                .manifest
                .iter()
                .all(|entry| entry.track_id != opus_id)
        );
        assert_eq!(walk_files(fx.dest.path().join("Contents")), 2);

        // Other layouts copy every format.
        let outcome = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, Vec::new(), false),
        );
        assert_eq!((outcome.copied, outcome.unplayable), (3, 0));
    }

    /// Two tracks whose Pioneer audio paths hash to the same `USBANLZ`
    /// directory (a pair observed on a real rekordbox export).
    fn colliding_pioneer_fixture() -> Fixture {
        let mut fx = fixture(2);
        let rude_boy = &mut fx.tracks[0];
        rude_boy.artist = "Rihanna".into();
        rude_boy.album = "Rated R".into();
        rude_boy.track_number = Some(8);
        rude_boy.title = "Rude Boy".into();
        rude_boy.extension = "flac".into();
        let aquarius = &mut fx.tracks[1];
        aquarius.artist = "Abel Ramos".into();
        aquarius.album = String::new();
        aquarius.track_number = None;
        aquarius.title = "Aquarius (Randy Katana Remix)".into();
        fx
    }

    fn anlz_of(manifest: &[sustain_domain::SyncManifestEntry], audio: &str) -> AnlzRecord {
        manifest
            .iter()
            .find(|entry| entry.on_device_path.as_str() == audio)
            .and_then(|entry| entry.anlz.clone())
            .unwrap_or_else(|| panic!("{audio} records an analysis pair"))
    }

    /// The UTF-16BE audio path a `.DAT` file's `PPTH` section names.
    fn names_audio_path(dat: &[u8], audio: &str) -> bool {
        let utf16: Vec<u8> = format!("/{audio}")
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        dat.windows(utf16.len()).any(|window| window == utf16)
    }

    const RUDE_BOY: &str = "Contents/Rihanna/Rated R/08 Rude Boy.flac";
    const AQUARIUS: &str = "Contents/Abel Ramos/Unknown Album/Aquarius (Randy Katana Remix).mp3";

    #[test]
    fn pioneer_colliding_tracks_own_distinct_anlz_slots() {
        let fx = colliding_pioneer_fixture();
        let outcome = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );

        let rude_boy = anlz_of(&outcome.manifest, RUDE_BOY);
        let aquarius = anlz_of(&outcome.manifest, AQUARIUS);
        let (rude_boy_dir, _) = rude_boy.dat_path.as_str().rsplit_once('/').expect("dir");
        let (aquarius_dir, _) = aquarius.dat_path.as_str().rsplit_once('/').expect("dir");
        assert_eq!(rude_boy_dir, aquarius_dir, "the fixture paths collide");
        assert!(rude_boy.dat_path.as_str().ends_with("/ANLZ0000.DAT"));
        assert!(aquarius.dat_path.as_str().ends_with("/ANLZ0001.DAT"));
        for (record, audio) in [(&rude_boy, RUDE_BOY), (&aquarius, AQUARIUS)] {
            let dat = std::fs::read(fx.dest.path().join(record.dat_path.as_str())).expect("dat");
            assert!(names_audio_path(&dat, audio), "{audio} owns its own pair");
            let ext = fx
                .dest
                .path()
                .join(record.dat_path.as_str().replace(".DAT", ".EXT"));
            assert!(ext.is_file(), "{audio} has its .EXT");
        }
        let pdb = std::fs::read(fx.dest.path().join("PIONEER/rekordbox/export.pdb")).expect("pdb");
        for record in [&rude_boy, &aquarius] {
            let analyze_path = format!("/{}", record.dat_path);
            assert!(
                pdb.windows(analyze_path.len())
                    .any(|window| window == analyze_path.as_bytes()),
                "the PDB points at {analyze_path}"
            );
        }
    }

    #[test]
    fn pioneer_resync_rewrites_an_anlz_pair_only_when_its_bytes_change() {
        let mut fx = colliding_pioneer_fixture();
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        let record = anlz_of(&first.manifest, RUDE_BOY);
        let dat = fx.dest.path().join(record.dat_path.as_str());
        // A marker the engine would erase if it rewrote the pair, longer
        // than the pair, as cues a player adds make it: only a pair cut
        // short is rewritten.
        let marker = vec![0xA5; std::fs::metadata(&dat).expect("dat").len() as usize + 64];
        std::fs::write(&dat, &marker).expect("mark dat");

        let second = run(
            fx.dest.path(),
            &resync_request(&fx, DeviceLayout::Pioneer, &first, false),
        );
        assert_eq!(anlz_of(&second.manifest, RUDE_BOY), record);
        assert_eq!(std::fs::read(&dat).expect("dat"), marker);

        // A pair cut short is written again.
        std::fs::write(&dat, b"").expect("truncate dat");
        let truncated = run(
            fx.dest.path(),
            &resync_request(&fx, DeviceLayout::Pioneer, &second, false),
        );
        assert_eq!(anlz_of(&truncated.manifest, RUDE_BOY), record);
        assert!(names_audio_path(
            &std::fs::read(&dat).expect("dat"),
            RUDE_BOY
        ));

        // New analysis reaches the device although the audio is unchanged.
        let tempo = Bpm::from_whole(124).expect("tempo");
        fx.tracks[0].bpm = Some(tempo);
        let analyses = vec![
            PioneerTrackAnalysis {
                beat_grid: Some(BeatGrid::new(tempo, Duration::ZERO, 1).expect("grid")),
                ..PioneerTrackAnalysis::default()
            },
            PioneerTrackAnalysis::default(),
        ];
        let third = run_prepared(
            fx.dest.path(),
            &prepared_with(
                &resync_request(&fx, DeviceLayout::Pioneer, &second, false),
                analyses.clone(),
            ),
        );
        assert_eq!(third.unchanged, 2);
        let rewritten = anlz_of(&third.manifest, RUDE_BOY);
        assert_eq!(rewritten.dat_path, record.dat_path, "the slot is sticky");
        assert_ne!(rewritten.digest, record.digest);
        assert!(names_audio_path(
            &std::fs::read(&dat).expect("dat"),
            RUDE_BOY
        ));

        // A missing file is restored even when the record matches.
        let ext = dat.with_extension("EXT");
        std::fs::remove_file(&ext).expect("remove ext");
        let fourth = run_prepared(
            fx.dest.path(),
            &prepared_with(
                &resync_request(&fx, DeviceLayout::Pioneer, &third, false),
                analyses,
            ),
        );
        assert_eq!(anlz_of(&fourth.manifest, RUDE_BOY), rewritten);
        assert!(ext.is_file());
    }

    #[test]
    fn pioneer_removal_deletes_only_the_removed_tracks_anlz_pair() {
        let mut fx = colliding_pioneer_fixture();
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        let removed = anlz_of(&first.manifest, RUDE_BOY);
        let kept = anlz_of(&first.manifest, AQUARIUS);

        fx.tracks.remove(0);
        let second = run(
            fx.dest.path(),
            &resync_request(&fx, DeviceLayout::Pioneer, &first, true),
        );
        assert_eq!(second.removed, 1);
        assert_eq!(anlz_of(&second.manifest, AQUARIUS), kept);
        let removed_dat = fx.dest.path().join(removed.dat_path.as_str());
        assert!(!removed_dat.exists());
        assert!(!removed_dat.with_extension("EXT").exists());
        let kept_dat = fx.dest.path().join(kept.dat_path.as_str());
        assert!(names_audio_path(
            &std::fs::read(&kept_dat).expect("dat"),
            AQUARIUS
        ));
        assert!(kept_dat.with_extension("EXT").is_file());
    }

    #[test]
    fn pioneer_stale_track_left_in_place_keeps_its_anlz_record() {
        let mut fx = colliding_pioneer_fixture();
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        let stale = anlz_of(&first.manifest, RUDE_BOY);

        fx.tracks.remove(0);
        let second = run(
            fx.dest.path(),
            &resync_request(&fx, DeviceLayout::Pioneer, &first, false),
        );
        assert_eq!(anlz_of(&second.manifest, RUDE_BOY), stale);
        assert!(fx.dest.path().join(stale.dat_path.as_str()).is_file());
    }

    #[test]
    fn pioneer_entry_without_anlz_record_never_deletes_analysis_files() {
        let mut fx = colliding_pioneer_fixture();
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        // A manifest written before entries recorded their pair.
        let legacy: Vec<_> = first
            .manifest
            .iter()
            .cloned()
            .map(|entry| sustain_domain::SyncManifestEntry {
                anlz: None,
                ..entry
            })
            .collect();
        let rude_boy_dat = fx
            .dest
            .path()
            .join(anlz_of(&first.manifest, RUDE_BOY).dat_path.as_str());

        fx.tracks.remove(0);
        let second = run(
            fx.dest.path(),
            &SyncRequest {
                previous_manifest: legacy,
                ..resync_request(&fx, DeviceLayout::Pioneer, &first, true)
            },
        );
        assert_eq!(second.removed, 1);
        // The surviving track takes the lowest free slot, which the removed
        // track's unrecorded pair occupied; nothing was deleted under it.
        let aquarius = anlz_of(&second.manifest, AQUARIUS);
        let aquarius_dat = fx.dest.path().join(aquarius.dat_path.as_str());
        assert_eq!(aquarius_dat, rude_boy_dat);
        assert!(names_audio_path(
            &std::fs::read(&aquarius_dat).expect("dat"),
            AQUARIUS
        ));
        assert!(aquarius_dat.with_extension("EXT").is_file());
    }

    /// Every regular file under `dir` with its bytes.
    fn snapshot(dir: &Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let mut files = std::collections::BTreeMap::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(current) = pending.pop() {
            for entry in std::fs::read_dir(&current).expect("read dir") {
                let path = entry.expect("dir entry").path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let bytes = std::fs::read(&path).expect("read file");
                    files.insert(path, bytes);
                }
            }
        }
        files
    }

    /// `req` prepared with the same cover on every track.
    fn prepared_with_covers(req: &SyncRequest) -> PreparedSyncRequest {
        let mut assets = PreparedPioneerAssets::new();
        for _ in &req.tracks {
            assets.push_track(PioneerTrackAnalysis::default(), Some(COVER_PNG));
        }
        PreparedSyncRequest::new(req.clone(), Some(assets)).expect("prepared Pioneer request")
    }

    fn sync_blocked(dest: &Path, req: &PreparedSyncRequest) -> Box<SyncFailure> {
        let failure = journaled_sync(&transport(dest), req, &mut |_| {}, &|| false)
            .expect_err("the sync is blocked");
        assert!(
            matches!(
                failure.error,
                SyncError::Blocked(SyncBlocker::ForeignPioneerExport)
            ),
            "{failure}"
        );
        failure
    }

    fn pdb_on(dest: &Path) -> PathBuf {
        dest.join(sustain_pioneer::PDB_RELATIVE_PATH)
    }

    fn artwork_on(dest: &Path, name: &str) -> PathBuf {
        dest.join(sustain_pioneer::artwork::ARTWORK_BUCKET)
            .join(name)
    }

    #[test]
    fn a_pioneer_export_sustain_did_not_write_blocks_the_sync_and_stays_untouched() {
        let fx = fixture(2);
        let dest = fx.dest.path();
        std::fs::create_dir_all(pdb_on(dest).parent().expect("parent")).expect("rekordbox dir");
        std::fs::write(pdb_on(dest), b"a rekordbox catalog").expect("foreign pdb");
        // A temporary file left by an interrupted write: the sync would
        // clean it up first thing, were it let in.
        std::fs::write(dest.join(".sustain-write-123-456.tmp"), b"partial").expect("temp");
        let before = snapshot(dest);

        let req = request(&fx, DeviceLayout::Pioneer, Vec::new(), true);
        assert_eq!(
            plan(&transport(dest), &req).expect("plan").blocker,
            Some(SyncBlocker::ForeignPioneerExport)
        );
        let failure = sync_blocked(dest, &prepared_with_covers(&req));
        assert!(failure.manifest.is_empty());
        assert_eq!(failure.catalogue, CatalogueRecord::default());
        assert_eq!(
            snapshot(dest),
            before,
            "the drive is left exactly as it was"
        );

        // Removing the export is the deliberate act that lets a sync in.
        std::fs::remove_dir_all(dest.join("PIONEER")).expect("remove PIONEER");
        assert_eq!(plan(&transport(dest), &req).expect("plan").blocker, None);
        let outcome = journaled_sync(
            &transport(dest),
            &prepared_with_covers(&req),
            &mut |_| {},
            &|| false,
        )
        .expect("sync ok");
        assert_eq!(outcome.copied, 2);
        assert_eq!(
            outcome.catalogue.databases,
            std::collections::BTreeSet::from([sustain_pioneer::database_identity(
                &std::fs::read(pdb_on(dest)).expect("pdb")
            )])
        );
    }

    #[test]
    fn sustains_own_export_is_written_over_until_something_else_changes_it() {
        let mut fx = fixture(2);
        let dest = fx.dest.path();
        let first = run(
            dest,
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        fx.tracks[0].title = "Retitled".into();
        let again = resync_request(&fx, DeviceLayout::Pioneer, &first, false);
        assert_eq!(plan(&transport(dest), &again).expect("plan").blocker, None);
        let second = run(dest, &again);
        // Once published, only the new database is Sustain's.
        assert_eq!(
            second.catalogue.databases,
            std::collections::BTreeSet::from([sustain_pioneer::database_identity(
                &std::fs::read(pdb_on(dest)).expect("pdb")
            )])
        );

        // A player recording history writes into the database.
        let mut changed = std::fs::read(pdb_on(dest)).expect("pdb");
        changed.extend_from_slice(b"history");
        std::fs::write(pdb_on(dest), &changed).expect("player write");
        let before = snapshot(dest);
        let after_player = resync_request(&fx, DeviceLayout::Pioneer, &second, false);
        assert_eq!(
            plan(&transport(dest), &after_player).expect("plan").blocker,
            Some(SyncBlocker::ForeignPioneerExport)
        );
        let failure = sync_blocked(dest, &prepared(&after_player));
        assert_eq!(failure.catalogue, second.catalogue);
        assert_eq!(failure.manifest, second.manifest);
        assert_eq!(snapshot(dest), before);
    }

    #[test]
    fn a_device_with_no_record_of_its_export_holds_a_foreign_one() {
        let fx = fixture(1);
        let dest = fx.dest.path();
        let first = run(
            dest,
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        // As a device synced before Sustain kept the record.
        let unrecorded = SyncRequest {
            previous_catalogue: CatalogueRecord::default(),
            ..resync_request(&fx, DeviceLayout::Pioneer, &first, false)
        };
        assert_eq!(
            plan(&transport(dest), &unrecorded).expect("plan").blocker,
            Some(SyncBlocker::ForeignPioneerExport)
        );
    }

    #[test]
    fn a_deleted_export_is_written_afresh() {
        let fx = fixture(2);
        let dest = fx.dest.path();
        let first = journaled_sync(
            &transport(dest),
            &prepared_with_covers(&request(&fx, DeviceLayout::Pioneer, Vec::new(), false)),
            &mut |_| {},
            &|| false,
        )
        .expect("first sync");
        std::fs::remove_dir_all(dest.join("PIONEER")).expect("remove PIONEER");

        let again = resync_request(&fx, DeviceLayout::Pioneer, &first, false);
        assert_eq!(plan(&transport(dest), &again).expect("plan").blocker, None);
        let second = journaled_sync(
            &transport(dest),
            &prepared_with_covers(&again),
            &mut |_| {},
            &|| false,
        )
        .expect("resync");
        assert!(pdb_on(dest).is_file());
        assert!(artwork_on(dest, "a1.jpg").is_file());
        assert_eq!(second.catalogue.artwork, first.catalogue.artwork);
    }

    #[test]
    fn artwork_sustain_did_not_write_is_left_in_place() {
        let fx = fixture(2);
        let dest = fx.dest.path();
        std::fs::create_dir_all(artwork_on(dest, "")).expect("artwork dir");
        std::fs::write(artwork_on(dest, "a9.jpg"), b"someone else's").expect("artwork");

        let first = journaled_sync(
            &transport(dest),
            &prepared_with_covers(&request(&fx, DeviceLayout::Pioneer, Vec::new(), false)),
            &mut |_| {},
            &|| false,
        )
        .expect("first sync");
        let written = std::collections::BTreeSet::from([
            device_path("PIONEER/Artwork/00001/a1.jpg"),
            device_path("PIONEER/Artwork/00001/a1_m.jpg"),
        ]);
        assert_eq!(first.catalogue.artwork, written);

        // Covers dropped: Sustain's own thumbnails go, the other file stays.
        let second = run(
            dest,
            &resync_request(&fx, DeviceLayout::Pioneer, &first, false),
        );
        assert!(second.catalogue.artwork.is_empty());
        assert!(!artwork_on(dest, "a1.jpg").exists());
        assert!(!artwork_on(dest, "a1_m.jpg").exists());
        assert_eq!(
            std::fs::read(artwork_on(dest, "a9.jpg")).expect("kept"),
            b"someone else's"
        );
    }

    /// A device that counts the files read from it, and whose write of
    /// `failing_write`'s file fails — after publishing it, as a failed
    /// directory sync does, or before.
    struct FaultyDevice {
        inner: DeviceRoot,
        failing_write: Option<FailingWrite>,
        reads: std::cell::Cell<usize>,
    }

    struct FailingWrite {
        path: &'static str,
        after_publishing: bool,
    }

    impl FaultyDevice {
        fn failing(dest: &Path, path: &'static str, after_publishing: bool) -> Self {
            Self {
                inner: transport(dest),
                failing_write: Some(FailingWrite {
                    path,
                    after_publishing,
                }),
                reads: std::cell::Cell::new(0),
            }
        }
    }

    impl DeviceTransport for FaultyDevice {
        fn capacity(&self) -> io::Result<DeviceCapacity> {
            self.inner.capacity()
        }

        fn ensure_dir_all(&self, path: &DeviceRelativePath) -> io::Result<()> {
            DeviceTransport::ensure_dir_all(&self.inner, path)
        }

        fn is_regular_file(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            DeviceTransport::is_regular_file(&self.inner, path)
        }

        fn regular_file_len(&self, path: &DeviceRelativePath) -> io::Result<Option<u64>> {
            DeviceTransport::regular_file_len(&self.inner, path)
        }

        fn read_file(&self, path: &DeviceRelativePath, limit: u64) -> io::Result<Vec<u8>> {
            self.reads.set(self.reads.get() + 1);
            DeviceTransport::read_file(&self.inner, path, limit)
        }

        fn write_file(&self, path: &DeviceRelativePath, bytes: &[u8]) -> io::Result<()> {
            let Some(failing) = self
                .failing_write
                .as_ref()
                .filter(|failing| failing.path == path.as_str())
            else {
                return DeviceTransport::write_file(&self.inner, path, bytes);
            };
            if failing.after_publishing {
                DeviceTransport::write_file(&self.inner, path, bytes)?;
            }
            Err(io::Error::other("directory sync failed"))
        }

        fn copy_file(
            &self,
            source: &Path,
            path: &DeviceRelativePath,
            expected: &SourceFileStat,
        ) -> io::Result<()> {
            DeviceTransport::copy_file(&self.inner, source, path, expected)
        }

        fn remove_file_if_exists(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            DeviceTransport::remove_file_if_exists(&self.inner, path)
        }

        fn remove_dir_if_empty(&self, path: &DeviceRelativePath) -> io::Result<bool> {
            DeviceTransport::remove_dir_if_empty(&self.inner, path)
        }

        fn remove_tree_if_exists(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<bool> {
            DeviceTransport::remove_tree_if_exists(&self.inner, path, cancel)
        }

        fn cleanup_stale_temporary_files(
            &self,
            path: &DeviceRelativePath,
            cancel: &dyn Fn() -> bool,
        ) -> io::Result<()> {
            DeviceTransport::cleanup_stale_temporary_files(&self.inner, path, cancel)
        }
    }

    #[test]
    fn an_export_whose_process_dies_leaves_a_record_that_vouches_for_it() {
        let mut fx = fixture(2);
        let dest = fx.dest.path();
        let first = run(
            dest,
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );

        // The second run publishes the first of its covers, then dies
        // before reporting: only what it journaled survives.
        fx.tracks[0].title = "Retitled".into();
        let failing = FaultyDevice::failing(dest, "PIONEER/Artwork/00001/a1_m.jpg", false);
        let (result, journal) = journaled_sync_keeping_journal(
            &failing,
            &prepared_with_covers(&resync_request(&fx, DeviceLayout::Pioneer, &first, false)),
            &mut |_| {},
            &|| false,
        );
        result.expect_err("the cover write failed");
        assert!(artwork_on(dest, "a1.jpg").is_file());

        let survived = SyncRequest {
            previous_manifest: journal.manifest(),
            previous_catalogue: journal.catalogue.clone(),
            ..request(&fx, DeviceLayout::Pioneer, Vec::new(), false)
        };
        assert_eq!(
            plan(&transport(dest), &survived).expect("plan").blocker,
            None
        );
        journaled_sync(
            &transport(dest),
            &prepared_with_covers(&survived),
            &mut |_| {},
            &|| false,
        )
        .expect("the next sync carries on");
    }

    #[test]
    fn a_database_write_that_fails_leaves_either_database_vouched_for() {
        for after_publishing in [true, false] {
            let mut fx = fixture(1);
            let dest = fx.dest.path();
            let first = run(
                dest,
                &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
            );
            fx.tracks[0].title = "Retitled".into();
            let failing =
                FaultyDevice::failing(dest, sustain_pioneer::PDB_RELATIVE_PATH, after_publishing);
            let failure = journaled_sync(
                &failing,
                &prepared(&resync_request(&fx, DeviceLayout::Pioneer, &first, false)),
                &mut |_| {},
                &|| false,
            )
            .expect_err("the database write failed");
            let on_drive =
                sustain_pioneer::database_identity(&std::fs::read(pdb_on(dest)).expect("pdb"));
            assert!(
                failure.catalogue.databases.contains(&on_drive),
                "after_publishing: {after_publishing}"
            );
            let next = SyncRequest {
                previous_manifest: failure.manifest.clone(),
                previous_catalogue: failure.catalogue.clone(),
                ..request(&fx, DeviceLayout::Pioneer, Vec::new(), false)
            };
            run(dest, &next);
        }
    }

    #[test]
    fn a_database_of_a_size_sustain_never_wrote_is_not_read() {
        let fx = fixture(1);
        let dest = fx.dest.path();
        let first = run(
            dest,
            &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
        );
        let again = resync_request(&fx, DeviceLayout::Pioneer, &first, false);
        let device = FaultyDevice {
            inner: transport(dest),
            failing_write: None,
            reads: std::cell::Cell::new(0),
        };
        assert_eq!(plan(&device, &again).expect("plan").blocker, None);
        assert_eq!(
            device.reads.get(),
            1,
            "its own database is read to be recognised"
        );

        let mut grown = std::fs::read(pdb_on(dest)).expect("pdb");
        grown.extend_from_slice(b"history");
        std::fs::write(pdb_on(dest), grown).expect("player write");
        assert_eq!(
            plan(&device, &again).expect("plan").blocker,
            Some(SyncBlocker::ForeignPioneerExport)
        );
        assert_eq!(
            device.reads.get(),
            1,
            "a database of another size is not read"
        );
    }

    #[test]
    fn a_catalog_sustain_never_writes_blocks_the_sync() {
        for catalog in sustain_pioneer::FOREIGN_CATALOG_RELATIVE_PATHS {
            let fx = fixture(1);
            let dest = fx.dest.path();
            let first = run(
                dest,
                &request(&fx, DeviceLayout::Pioneer, Vec::new(), false),
            );
            // rekordbox converted Sustain's export, leaving its database be.
            std::fs::write(dest.join(catalog), b"OneLibrary").expect("catalog");
            let before = snapshot(dest);
            let again = resync_request(&fx, DeviceLayout::Pioneer, &first, true);
            assert_eq!(
                plan(&transport(dest), &again).expect("plan").blocker,
                Some(SyncBlocker::ForeignPioneerExport),
                "{catalog}"
            );
            sync_blocked(dest, &prepared(&again));
            assert_eq!(snapshot(dest), before, "{catalog}");
        }
    }

    #[test]
    fn covers_the_drive_database_names_outlive_a_failed_publication() {
        let fx = fixture(1);
        let dest = fx.dest.path();
        let first = journaled_sync(
            &transport(dest),
            &prepared_with_covers(&request(&fx, DeviceLayout::Pioneer, Vec::new(), false)),
            &mut |_| {},
            &|| false,
        )
        .expect("first sync");
        // The cover leaves the selection, but the new database never lands.
        let failing = FaultyDevice::failing(dest, sustain_pioneer::PDB_RELATIVE_PATH, false);
        let failure = journaled_sync(
            &failing,
            &prepared(&resync_request(&fx, DeviceLayout::Pioneer, &first, false)),
            &mut |_| {},
            &|| false,
        )
        .expect_err("the database write failed");
        assert!(artwork_on(dest, "a1.jpg").is_file());
        assert!(artwork_on(dest, "a1_m.jpg").is_file());
        assert_eq!(failure.catalogue.artwork, first.catalogue.artwork);
    }

    #[test]
    fn artwork_a_sync_would_write_over_blocks_it() {
        let fx = fixture(1);
        let dest = fx.dest.path();
        std::fs::create_dir_all(artwork_on(dest, "")).expect("artwork dir");
        std::fs::write(artwork_on(dest, "a1_m.jpg"), b"someone else's").expect("artwork");
        let before = snapshot(dest);

        sync_blocked(
            dest,
            &prepared_with_covers(&request(&fx, DeviceLayout::Pioneer, Vec::new(), false)),
        );
        assert_eq!(snapshot(dest), before);
    }

    #[test]
    fn pioneer_layout_writes_cover_thumbnails() {
        let mut repeated_assets = PreparedPioneerAssets::new();
        for _ in 0..10_000 {
            repeated_assets.push_track(PioneerTrackAnalysis::default(), Some(COVER_PNG));
        }
        assert_eq!(repeated_assets.track_count(), 10_000);
        assert_eq!(
            repeated_assets.artwork_count(),
            1,
            "repeated source artwork must retain one rendered thumbnail set"
        );

        let fx = fixture(2);
        // Both tracks carry the same cover, so de-duplication collapses
        // them to a single artwork id.
        let req = SyncRequest {
            device: device(DeviceLayout::Pioneer),
            tracks: fx.tracks.clone(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: Vec::new(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        };
        let mut assets = PreparedPioneerAssets::new();
        for _ in &req.tracks {
            assets.push_track(PioneerTrackAnalysis::default(), Some(COVER_PNG));
        }
        assert_eq!(assets.artwork_count(), 1);
        journaled_sync(
            &transport(fx.dest.path()),
            &PreparedSyncRequest::new(req, Some(assets)).expect("prepared Pioneer request"),
            &mut |_| {},
            &|| false,
        )
        .expect("sync ok");

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
        let first = run(fx.dest.path(), &req);
        let req2 = request(&fx, DeviceLayout::M3u, first.manifest.clone(), false);
        let second = run(fx.dest.path(), &req2);
        assert_eq!(second.copied, 0);
        assert_eq!(second.unchanged, 3);
    }

    #[test]
    fn a_case_correction_keeps_the_track_on_a_case_insensitive_device() {
        let mut fx = fixture(1);
        fx.tracks[0].artist = "daft punk".into();
        let device = || CaseInsensitiveTransport(transport(fx.dest.path()));
        let first = journaled_sync(
            &device(),
            &prepared(&request(&fx, DeviceLayout::Pioneer, Vec::new(), true)),
            &mut |_| {},
            &|| false,
        )
        .expect("first sync");

        // The old spelling is not stale: it is the file the new one names.
        fx.tracks[0].artist = "Daft Punk".into();
        let corrected = journaled_sync(
            &device(),
            &prepared(&resync_request(&fx, DeviceLayout::Pioneer, &first, true)),
            &mut |_| {},
            &|| false,
        )
        .expect("corrected sync");
        assert_eq!((corrected.updated, corrected.removed), (1, 0));
        assert_eq!(corrected.manifest.len(), 1);
        let entry = &corrected.manifest[0];
        assert_eq!(
            entry.on_device_path.as_str(),
            "Contents/Daft Punk/Album 2/01 Title 1.mp3"
        );
        assert_eq!(
            device()
                .regular_file_len(&entry.on_device_path)
                .expect("read the device"),
            Some(fx.tracks[0].source.stat.size_bytes),
        );
        let anlz = entry.anlz.as_ref().expect("the track keeps its analysis");
        assert!(
            device()
                .is_regular_file(&anlz.dat_path)
                .expect("read the device")
        );
    }

    #[test]
    fn incremental_resync_replaces_a_truncated_destination() {
        let fx = fixture(2);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let first = run(fx.dest.path(), &req);
        let truncated = fx
            .dest
            .path()
            .join(first.manifest[0].on_device_path.as_str());
        std::fs::write(&truncated, b"partial").expect("truncate destination");

        let req2 = request(&fx, DeviceLayout::M3u, first.manifest.clone(), false);
        let second = run(fx.dest.path(), &req2);
        assert_eq!(second.updated, 1);
        assert_eq!(second.unchanged, 1);
        assert_eq!(
            std::fs::read(&truncated).expect("read repaired destination"),
            std::fs::read(&fx.tracks[0].source_path).expect("read source"),
        );
    }

    #[test]
    fn recoverable_copy_failure_keeps_syncing_and_persists_successes() {
        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let fail_path =
            DeviceRelativePath::new("Music/Artist 1/Album 1/02 Title 2.mp3").expect("safe path");
        let transport = CopyFailureTransport::recoverable(transport(fx.dest.path()), fail_path);

        let outcome = journaled_sync(&transport, &prepared(&req), &mut |_| {}, &|| false)
            .expect("recoverable copy failure does not abort");

        assert_eq!(outcome.copied, 2);
        assert_eq!(outcome.updated, 0);
        assert_eq!(outcome.unchanged, 0);
        assert_eq!(outcome.copy_failures.len(), 1);
        assert_eq!(outcome.copy_failures[0].track_id, fx.tracks[1].track_id);
        assert_eq!(outcome.manifest.len(), 2);
        assert!(
            outcome
                .manifest
                .iter()
                .all(|entry| entry.track_id != fx.tracks[1].track_id),
            "failed track must not be persisted as present"
        );

        assert!(
            fx.dest
                .path()
                .join("Music/Artist 2/Album 2/01 Title 1.mp3")
                .exists()
        );
        assert!(
            !fx.dest
                .path()
                .join("Music/Artist 1/Album 1/02 Title 2.mp3")
                .exists()
        );
        assert!(
            fx.dest
                .path()
                .join("Music/Artist 2/Album 2/03 Title 3.mp3")
                .exists()
        );

        let m3u = std::fs::read_to_string(fx.dest.path().join("My Set.m3u8")).expect("read m3u");
        assert!(m3u.contains("Title 1"));
        assert!(!m3u.contains("Title 2"));
        assert!(m3u.contains("Title 3"));
    }

    #[test]
    fn external_source_change_is_recopied_on_the_next_sync() {
        // #100: a source file changed after the previous sync (here without
        // any rescan of the library) must be detected and re-copied. The old
        // size-only fingerprint missed content changes; the SHA-256 catches
        // them.
        let fx = fixture(2);
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, Vec::new(), false),
        );
        assert_eq!(first.copied, 2);

        // An external tool rewrites track 1's bytes. Re-observe the source so
        // the new request carries its fresh content hash.
        let mut tracks = fx.tracks.clone();
        std::fs::write(&tracks[0].source_path, b"externally-rewritten-bytes").expect("rewrite");
        tracks[0].source = SourceSnapshot::resolved(
            resolve_source_fingerprint(&tracks[0].source_path, None).expect("re-resolve"),
        );

        let req = SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks,
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: first.manifest.clone(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        };
        let second = run(fx.dest.path(), &req);
        // Only the rewritten track is re-copied; the untouched one is skipped.
        assert_eq!(second.updated, 1);
        assert_eq!(second.unchanged, 1);
        assert_eq!(second.copied, 0);
    }

    #[test]
    fn removal_only_with_confirmation() {
        let fx = fixture(3);
        // First sync all three.
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, Vec::new(), false),
        );

        // Shrink the resolved selection to the first two tracks (the
        // runtime passes only selected tracks as `req.tracks`).
        let shrink = |remove: bool| SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks: fx.tracks[..2].to_vec(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: first.manifest.clone(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: remove,
            export_date: "2026-01-01".into(),
        };

        // Without confirmation, the third file stays and remains tracked.
        let kept = journaled_sync(
            &transport(fx.dest.path()),
            &prepared(&shrink(false)),
            &mut |_| {},
            &|| false,
        )
        .expect("sync");
        assert_eq!(kept.removed, 0);
        assert_eq!(kept.manifest.len(), 3);

        // With confirmation, the stale file is removed.
        let removed = journaled_sync(
            &transport(fx.dest.path()),
            &prepared(&shrink(true)),
            &mut |_| {},
            &|| false,
        )
        .expect("sync");
        assert_eq!(removed.removed, 1);
        assert_eq!(removed.manifest.len(), 2);
    }

    #[test]
    fn cancellation_before_removal_keeps_stale_manifest_rows() {
        let fx = fixture(3);
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, Vec::new(), false),
        );
        let shrink = SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks: fx.tracks[..2].to_vec(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1],
            }],
            previous_manifest: first.manifest.clone(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: true,
            export_date: "2026-01-01".into(),
        };

        let outcome = journaled_sync(
            &transport(fx.dest.path()),
            &prepared(&shrink),
            &mut |_| {},
            &|| true,
        )
        .expect("cancelled sync succeeds");
        assert!(outcome.cancelled);
        assert_eq!(outcome.removed, 0);
        assert_eq!(outcome.manifest, first.manifest);
    }

    #[test]
    fn a_folder_emptied_by_stale_removal_goes_with_its_last_file() {
        let mut fx = fixture(4);
        let folders = [
            ("A", "First"),
            ("A", "Second"),
            ("B", "Third"),
            ("C", "Fourth"),
        ];
        for (track, (artist, album)) in fx.tracks.iter_mut().zip(folders) {
            track.artist = artist.into();
            track.album = album.into();
        }
        let first_request = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let first = run(fx.dest.path(), &first_request);
        let music = fx.dest.path().join("Music");
        // Something of the listener's own in one of the folders.
        std::fs::write(music.join("C/Fourth/cover.jpg"), b"art").expect("own file");

        // Only the first track stays ticked.
        let mut next = request(&fx, DeviceLayout::M3u, first.manifest.clone(), true);
        next.tracks.truncate(1);
        next.playlists[0].track_indices = vec![0];
        let outcome = run(fx.dest.path(), &next);
        assert_eq!(outcome.removed, 3);

        // The artist keeps the album that still holds a track; the artist
        // whose only album emptied goes; a folder holding anything else stays.
        assert!(music.join("A/First").is_dir());
        assert!(!music.join("A/Second").exists());
        assert!(!music.join("B").exists());
        assert!(music.join("C/Fourth/cover.jpg").is_file());

        // Nothing left to sync: the folders go, the device root stays.
        let mut last = request(&fx, DeviceLayout::M3u, outcome.manifest.clone(), true);
        last.tracks = vec![fx.tracks[3].clone()];
        last.playlists[0].track_indices = vec![0];
        run(fx.dest.path(), &last);
        assert!(!music.join("A").exists());
        assert!(fx.dest.path().is_dir());
    }

    #[test]
    fn cancellation_mid_copy_keeps_tracking_the_files_left_on_the_device() {
        let mut fx = fixture(4);
        let mut first_request = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        first_request.tracks.truncate(3);
        first_request.playlists[0].track_indices = vec![0, 1, 2];
        let first = run(fx.dest.path(), &first_request);

        // Track 1 is dropped (stale), track 4 is new and copied first, and
        // the run stops before rewriting the changed track 3.
        std::fs::write(&fx.tracks[2].source_path, b"edited audio").expect("edit source");
        fx.tracks[2].source = SourceSnapshot::resolved(
            resolve_source_fingerprint(&fx.tracks[2].source_path, None)
                .expect("resolve edited source"),
        );
        let next = SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks: vec![
                fx.tracks[1].clone(),
                fx.tracks[3].clone(),
                fx.tracks[2].clone(),
            ],
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0, 1, 2],
            }],
            previous_manifest: first.manifest.clone(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: true,
            export_date: "2026-01-01".into(),
        };
        let copied = std::cell::Cell::new(false);
        let outcome = journaled_sync(
            &transport(fx.dest.path()),
            &prepared(&next),
            &mut |progress| copied.set(progress.stage == SyncStage::Copying),
            &|| copied.get(),
        )
        .expect("cancelled sync succeeds");
        assert!(outcome.cancelled);
        assert_eq!((outcome.copied, outcome.updated), (1, 0));

        // Every file on the device stays in the manifest: track 2 as
        // before, the one write, and the stale file and the unreached
        // file under their previous entries.
        let tracked: std::collections::HashSet<&str> = outcome
            .manifest
            .iter()
            .map(|entry| entry.on_device_path.as_str())
            .collect();
        let on_device: std::collections::HashSet<&str> = first
            .manifest
            .iter()
            .chain(&outcome.manifest)
            .map(|entry| entry.on_device_path.as_str())
            .filter(|path| fx.dest.path().join(path).is_file())
            .collect();
        assert_eq!(tracked, on_device);
        assert_eq!(outcome.manifest.len(), 4);
    }

    #[test]
    fn a_sync_stopped_by_an_error_keeps_tracking_the_files_it_wrote() {
        let fx = fixture(3);
        let mut first_request = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        first_request.tracks.truncate(1);
        first_request.playlists[0].track_indices = vec![0];
        let first = run(fx.dest.path(), &first_request);

        // The drive fills up on the third track, after the second was
        // written.
        let grown = request(&fx, DeviceLayout::M3u, first.manifest, false);
        let placements = crate::layout::plan_placements(&grown).expect("plan");
        let transport =
            CopyFailureTransport::fatal(transport(fx.dest.path()), placements[2].rel_path.clone());
        let failure = journaled_sync(&transport, &prepared(&grown), &mut |_| {}, &|| false)
            .expect_err("a fatal copy error stops the sync");
        assert!(matches!(failure.error, SyncError::Io { .. }));
        let tracked: Vec<TrackId> = failure.manifest.iter().map(|e| e.track_id).collect();
        assert_eq!(tracked, vec![fx.tracks[0].track_id, fx.tracks[1].track_id]);

        // So dropping the second track from the selection removes its file.
        let shrunk = SyncRequest {
            previous_manifest: failure.manifest,
            remove_stale: true,
            ..first_request
        };
        let outcome = run(fx.dest.path(), &shrunk);
        assert_eq!(outcome.removed, 1);
        assert_eq!(walk_files(fx.dest.path().join("Music")), 1);
    }

    #[test]
    fn an_export_that_never_reports_back_resumes_from_its_journal() {
        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let placements = crate::layout::plan_placements(&req).expect("plan");

        // The run stops on the third file, and its manifest is lost with
        // the process: only the journal survives.
        let transport =
            CopyFailureTransport::fatal(transport(fx.dest.path()), placements[2].rel_path.clone());
        let mut journal = ReplayJournal::over(&req);
        sync(
            &transport,
            &prepared(&req),
            &mut journal,
            &mut |_| {},
            &|| false,
        )
        .expect_err("a fatal copy error stops the sync");

        let resumed = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, journal.manifest(), false),
        );
        assert_eq!((resumed.unchanged, resumed.copied), (2, 1));
    }

    #[test]
    fn a_change_the_journal_cannot_record_stops_the_sync() {
        struct FullJournal;

        impl ManifestJournal for FullJournal {
            fn record(
                &mut self,
                _entry: &sustain_domain::SyncManifestEntry,
            ) -> Result<(), JournalError> {
                Err("database is full".into())
            }

            fn forget(&mut self, _path: &DeviceRelativePath) -> Result<(), JournalError> {
                Ok(())
            }

            fn record_catalogue(&mut self, _record: &CatalogueRecord) -> Result<(), JournalError> {
                Ok(())
            }
        }

        let fx = fixture(3);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let failure = sync(
            &transport(fx.dest.path()),
            &prepared(&req),
            &mut FullJournal,
            &mut |_| {},
            &|| false,
        )
        .expect_err("an unrecorded change stops the sync");
        assert!(matches!(failure.error, SyncError::Journal(_)));
        // The file written before the stop is still reported.
        assert_eq!(failure.manifest.len(), 1);
        assert_eq!(walk_files(fx.dest.path().join("Music")), 1);
    }

    #[cfg(unix)]
    #[test]
    fn failed_stale_removal_aborts_the_sync() {
        use std::os::unix::fs::symlink;

        let fx = fixture(2);
        let first = run(
            fx.dest.path(),
            &request(&fx, DeviceLayout::M3u, Vec::new(), false),
        );
        let stale = fx
            .dest
            .path()
            .join(first.manifest[1].on_device_path.as_str());
        std::fs::remove_file(&stale).expect("remove destination");
        let host = tempfile::NamedTempFile::new().expect("host file");
        std::fs::write(host.path(), b"host-data").expect("seed host file");
        symlink(host.path(), &stale).expect("replace destination with symlink");

        let shrink = SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks: fx.tracks[..1].to_vec(),
            playlists: vec![SyncInputPlaylist {
                name: "My Set".into(),
                track_indices: vec![0],
            }],
            previous_manifest: first.manifest,
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: true,
            export_date: "2026-01-01".into(),
        };
        assert!(
            journaled_sync(
                &transport(fx.dest.path()),
                &prepared(&shrink),
                &mut |_| {},
                &|| false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(host.path()).expect("read host file"),
            b"host-data"
        );
    }

    #[test]
    fn marker_is_written_on_sync() {
        let fx = fixture(1);
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        run(fx.dest.path(), &req);
        assert_eq!(
            read_marker(fx.dest.path()).map(SyncDeviceId::into_string),
            Some("test-device".to_owned())
        );
    }

    #[cfg(unix)]
    #[test]
    fn marker_symlink_is_rejected_without_touching_its_host_target() {
        use std::os::unix::fs::symlink;

        let dest = tempfile::tempdir().expect("device dir");
        let host = tempfile::NamedTempFile::new().expect("host file");
        std::fs::write(host.path(), "host-data").expect("seed host file");
        symlink(host.path(), dest.path().join(MARKER_FILE)).expect("marker symlink");

        let id = SyncDeviceId::new("device-id").expect("id");
        assert!(write_marker(dest.path(), &id).is_err());
        assert_eq!(
            std::fs::read_to_string(host.path()).expect("read host file"),
            "host-data"
        );
        assert_eq!(read_marker(dest.path()), None);
    }

    #[cfg(unix)]
    #[test]
    fn sync_aborts_when_the_marker_cannot_be_published() {
        use std::os::unix::fs::symlink;

        let fx = fixture(1);
        let host = tempfile::NamedTempFile::new().expect("host file");
        std::fs::write(host.path(), "host-data").expect("seed host file");
        symlink(host.path(), fx.dest.path().join(MARKER_FILE)).expect("marker symlink");

        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        assert!(
            journaled_sync(
                &transport(fx.dest.path()),
                &prepared(&req),
                &mut |_| {},
                &|| false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(host.path()).expect("read host file"),
            "host-data"
        );
        assert!(!fx.dest.path().join("Music").exists());
    }

    #[cfg(unix)]
    #[test]
    fn sync_rejects_symlinked_intermediate_destination_without_writing_host_files() {
        use std::os::unix::fs::symlink;

        let fx = fixture(1);
        let host = tempfile::tempdir().expect("host dir");
        std::fs::create_dir(fx.dest.path().join("Music")).expect("music dir");
        symlink(host.path(), fx.dest.path().join("Music/Artist 2")).expect("artist symlink");

        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        assert!(
            journaled_sync(
                &transport(fx.dest.path()),
                &prepared(&req),
                &mut |_| {},
                &|| false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_dir(host.path())
                .expect("read host dir")
                .count(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn sync_rejects_symlinked_final_destination_without_touching_host_file() {
        use std::os::unix::fs::symlink;

        let fx = fixture(1);
        let host = tempfile::NamedTempFile::new().expect("host file");
        std::fs::write(host.path(), "host-data").expect("seed host file");
        let album = fx.dest.path().join("Music/Artist 2/Album 2");
        std::fs::create_dir_all(&album).expect("album dir");
        symlink(host.path(), album.join("01 Title 1.mp3")).expect("track symlink");

        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        assert!(
            journaled_sync(
                &transport(fx.dest.path()),
                &prepared(&req),
                &mut |_| {},
                &|| false
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(host.path()).expect("read host file"),
            "host-data"
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
            track.source.stat.size_bytes = size;
        }
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let plan = plan(&transport(fx.dest.path()), &req).expect("plan");

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
    fn spellings_of_one_genre_share_a_footprint_segment() {
        let mut fx = fixture(4);
        let specs = [
            ("deep house", 100u64),
            ("Deep House", 300),
            ("DEEP HOUSE ", 50),
            ("Deep Hous\u{e9}", 20),
        ];
        for (track, (genre, size)) in fx.tracks.iter_mut().zip(specs) {
            track.genre = Some(genre.to_owned());
            track.source.stat.size_bytes = size;
        }
        let req = request(&fx, DeviceLayout::M3u, Vec::new(), false);
        let plan = plan(&transport(fx.dest.path()), &req).expect("plan");

        // Named by the spelling carrying the most bytes; an accent makes a
        // different genre, not a different spelling.
        assert_eq!(
            plan.genre_bytes,
            vec![
                GenreBytes {
                    genre: Some("Deep House".into()),
                    bytes: 450,
                },
                GenreBytes {
                    genre: Some("Deep Hous\u{e9}".into()),
                    bytes: 20,
                },
            ]
        );
    }

    #[test]
    fn empty_selection_is_rejected() {
        let fx = fixture(0);
        let req = SyncRequest {
            device: device(DeviceLayout::M3u),
            tracks: Vec::new(),
            playlists: Vec::new(),
            previous_manifest: Vec::new(),
            previous_playlist_files: Vec::new(),
            previous_catalogue: sustain_domain::CatalogueRecord::default(),
            remove_stale: false,
            export_date: "2026-01-01".into(),
        };
        let failure = journaled_sync(
            &transport(fx.dest.path()),
            &prepared(&req),
            &mut |_| {},
            &|| false,
        )
        .expect_err("an empty selection fails");
        assert!(matches!(failure.error, SyncError::Empty));
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
