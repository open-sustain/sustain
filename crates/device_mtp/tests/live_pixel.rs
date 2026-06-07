// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Live MTP transport round-trip against a connected Android phone.
//!
//! Ignored by default — it needs real hardware. The real sync drives the
//! transport on a background worker thread (never the GTK main loop), so
//! this exercises it there too, proving gio's synchronous GFile operations
//! work against `gvfsd-mtp` without an application main context.
//!
//! ```sh
//! SUSTAIN_MTP_URI='mtp://Google_Pixel_9_Pro_<serial>/' \
//!   cargo test -p sustain-device-mtp --test live_pixel -- --ignored --nocapture
//! ```

use std::thread;

use sustain_device_mtp::MtpTransport;
use sustain_device_sync::{
    DeviceTransport, MtpTarget, PreparedSyncRequest, SourceSnapshot, SyncInputPlaylist,
    SyncInputTrack, SyncRequest, engine, resolve_source_fingerprint, source_file_stat,
};
use sustain_domain::{
    DeviceKind, DeviceLayout, DeviceRelativePath, FilesPerFolderCap, SyncDevice, SyncDeviceId,
    TrackId,
};

/// Read the phone's volume URI from the environment, skipping (passing)
/// when it is not set so the suite stays green without hardware.
fn target() -> MtpTarget {
    let volume_uri = std::env::var("SUSTAIN_MTP_URI").expect("set SUSTAIN_MTP_URI to the phone");
    let storage = std::env::var("SUSTAIN_MTP_STORAGE")
        .unwrap_or_else(|_| "Internal shared storage".to_owned());
    MtpTarget {
        volume_uri,
        storage,
    }
}

#[test]
#[ignore = "requires a connected Android phone over MTP"]
fn round_trips_on_a_worker_thread() {
    let target = target();

    let source = tempfile::NamedTempFile::new().expect("source file");
    std::fs::write(source.path(), b"sustain mtp copy payload").expect("seed source");
    let source_path = source.path().to_path_buf();
    let expected = source_file_stat(&source_path).expect("source stat");

    let handle = thread::spawn(move || {
        let transport = MtpTransport::open(&target);

        let capacity = transport.capacity().expect("capacity");
        assert!(capacity.total_bytes > 0, "device reports a total size");
        assert!(capacity.available_bytes <= capacity.total_bytes);

        let dir = DeviceRelativePath::new("Music/.sustain-selftest").expect("safe path");
        transport.ensure_dir_all(&dir).expect("mkdir -p");

        // write_file → stat → read_to_string round-trip.
        let marker =
            DeviceRelativePath::new("Music/.sustain-selftest/marker.txt").expect("safe path");
        transport
            .write_file(&marker, b"sustain-mtp-roundtrip")
            .expect("write");
        assert_eq!(
            transport.regular_file_len(&marker).expect("stat"),
            Some("sustain-mtp-roundtrip".len() as u64),
        );
        assert_eq!(
            transport.read_to_string(&marker, 4096).expect("read"),
            "sustain-mtp-roundtrip",
        );

        // copy_file streams a host file to the device under its guard.
        let copied =
            DeviceRelativePath::new("Music/.sustain-selftest/copied.bin").expect("safe path");
        transport
            .copy_file(&source_path, &copied, &expected)
            .expect("copy");
        assert_eq!(
            transport.regular_file_len(&copied).expect("stat copied"),
            Some(expected.size_bytes),
        );

        // Clean up: remove the whole self-test tree.
        assert!(
            transport
                .remove_tree_if_exists(&dir, &|| false)
                .expect("rm tree")
        );
        assert!(
            !transport.is_regular_file(&marker).expect("gone"),
            "self-test tree removed",
        );
    });
    handle.join().expect("worker thread panicked");
}

/// Confirm gio's volume monitor surfaces the connected phone as an MTP
/// device with the expected storage and serial.
#[test]
#[ignore = "requires a connected Android phone over MTP"]
fn discovery_finds_the_phone() {
    // Pump the default main context briefly so the gvfs proxy volume
    // monitor has received the current mount list (mirrors the running GTK
    // loop the real app provides).
    let context = glib::MainContext::default();
    for _ in 0..50 {
        while context.iteration(false) {}
        std::thread::sleep(std::time::Duration::from_millis(10));
    }

    let devices = sustain_device_mtp::discover(&[]);
    println!("discovered {} MTP device(s):", devices.len());
    for device in &devices {
        println!("  {:?} {:?} {:?}", device.label, device.kind, device.target);
    }
    assert!(
        devices
            .iter()
            .any(|d| d.kind == sustain_domain::DeviceKind::Android),
        "the connected phone is discovered as an Android device",
    );
}

/// Drive the real sync engine + M3u Android layout against the phone and
/// confirm the on-device spec layout: audio at
/// `/sdcard/Music/<Artist>/<Album>/<NN Title.ext>` and the `.m3u8` inside
/// `/sdcard/Music` with entries relative to it.
#[test]
#[ignore = "requires a connected Android phone over MTP"]
fn engine_sync_lays_out_android_paths() {
    let target = target();

    let source = tempfile::tempdir().expect("source dir");
    let mut tracks = Vec::new();
    for index in 1..=2 {
        let path = source.path().join(format!("song{index}.mp3"));
        std::fs::write(&path, format!("sustain-e2e-audio-{index}").repeat(64)).expect("seed audio");
        let fingerprint = resolve_source_fingerprint(&path, None).expect("fingerprint");
        tracks.push(SyncInputTrack {
            track_id: TrackId::new(index).expect("id"),
            source_path: path,
            title: format!("E2E Title {index}"),
            artist: "Sustain E2E Artist".to_owned(),
            album: "Sustain E2E Album".to_owned(),
            genre: Some("Test".to_owned()),
            track_number: Some(index as u32),
            year: Some(2026),
            duration_ms: 123_000,
            rating: 0,
            bpm: None,
            key: None,
            bitrate_kbps: Some(320),
            sample_rate_hz: 44_100,
            bit_depth: 16,
            source: SourceSnapshot::resolved(fingerprint),
            date_added: Some("2026-06-07".to_owned()),
            extension: "mp3".to_owned(),
        });
    }

    let device = SyncDevice {
        id: SyncDeviceId::new("e2e-android").expect("id"),
        label: "E2E Pixel".to_owned(),
        kind: DeviceKind::Android,
        layout: DeviceLayout::M3u,
        sub_path: DeviceRelativePath::root(),
        files_per_folder_cap: FilesPerFolderCap::Unlimited,
        volume_id: None,
    };
    let request = SyncRequest {
        device,
        tracks,
        playlists: vec![SyncInputPlaylist {
            name: "Sustain E2E Set".to_owned(),
            track_indices: vec![0, 1],
        }],
        previous_manifest: Vec::new(),
        remove_stale: false,
        export_date: "2026-06-07".to_owned(),
    };
    let prepared = PreparedSyncRequest::new(request, None).expect("prepared");

    let handle = thread::spawn(move || {
        let transport = MtpTransport::open(&target);
        let outcome =
            engine::sync(&transport, &prepared, &mut |_| {}, &|| false).expect("engine sync");
        assert_eq!(outcome.copied, 2, "both tracks copied");

        // Audio at the exact Android spec path.
        let audio = DeviceRelativePath::new(
            "Music/Sustain E2E Artist/Sustain E2E Album/01 E2E Title 1.mp3",
        )
        .expect("safe path");
        assert!(
            transport.is_regular_file(&audio).expect("stat audio"),
            "audio at /sdcard/Music/<Artist>/<Album>/<NN Title>",
        );

        // Playlist inside Music/ with entries relative to it (no `Music/`).
        let playlist = DeviceRelativePath::new("Music/Sustain E2E Set.m3u8").expect("safe path");
        let body = transport
            .read_to_string(&playlist, 64 * 1024)
            .expect("read m3u");
        assert!(body.starts_with("#EXTM3U"));
        assert!(
            body.contains("Sustain E2E Artist/Sustain E2E Album/01 E2E Title 1.mp3"),
            "entries are relative to the Music root: {body}",
        );
        assert!(
            !body.contains("Music/Sustain E2E Artist"),
            "no doubled Music/ prefix in entries: {body}",
        );

        // The identity marker landed at the storage root.
        let marker = DeviceRelativePath::new(".sustain-device-id").expect("safe path");
        assert_eq!(
            transport
                .read_to_string(&marker, 4096)
                .expect("read marker"),
            "e2e-android",
        );

        // Clean up everything this test created.
        let artist = DeviceRelativePath::new("Music/Sustain E2E Artist").expect("safe path");
        transport
            .remove_tree_if_exists(&artist, &|| false)
            .expect("rm artist tree");
        transport
            .remove_file_if_exists(&playlist)
            .expect("rm playlist");
        transport.remove_file_if_exists(&marker).expect("rm marker");
    });
    handle.join().expect("worker thread panicked");
}
