// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The iPod layout through the engine: which iPods are refused before
//! anything is written, where the audio goes, and the catalogue a sync
//! publishes.

use super::*;
use crate::transport::VolumeFacts;

/// An iPod classic 120 GB, which needs a signed catalogue.
const CLASSIC_SYSINFO: &str = "ModelNumStr: xB562\nBuildID: 0x00000000\n";
/// A 5th-generation iPod with video, whose catalogue is unsigned.
const VIDEO_SYSINFO: &str = "ModelNumStr: xA002\n";
/// A classic's USB serial number: its FireWire GUID.
const CLASSIC_SERIAL: &str = "000A27001A2B3C4D";

/// A scratch volume posing as an iPod: `facts` stand for what the
/// filesystem would report.
struct FakeIpod {
    root: DeviceRoot,
    facts: VolumeFacts,
}

impl DeviceTransport for FakeIpod {
    fn capacity(&self) -> io::Result<DeviceCapacity> {
        self.root.capacity()
    }

    fn volume_facts(&self) -> io::Result<VolumeFacts> {
        Ok(self.facts.clone())
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
        self.root.copy_file(source_path, path, expected)
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

/// A FAT32 classic plugged in over USB, at `dest`, with `sysinfo`.
fn ipod(dest: &Path, sysinfo: Option<&str>) -> FakeIpod {
    if let Some(sysinfo) = sysinfo {
        let device = dest.join("iPod_Control/Device");
        std::fs::create_dir_all(&device).expect("create the Device folder");
        std::fs::write(device.join("SysInfo"), sysinfo).expect("write SysInfo");
    }
    FakeIpod {
        root: transport(dest),
        facts: VolumeFacts {
            usb_serial: Some(CLASSIC_SERIAL.to_owned()),
            ..VolumeFacts::default()
        },
    }
}

fn ipod_request(fx: &Fixture) -> SyncRequest {
    let mut req = request(fx, DeviceLayout::Ipod, Vec::new(), false);
    req.device.kind = DeviceKind::Ipod;
    req
}

fn itunesdb(dest: &Path) -> Vec<u8> {
    std::fs::read(dest.join(sustain_ipod::ITUNESDB_RELATIVE_PATH)).expect("read iTunesDB")
}

fn sync_ipod(device: &FakeIpod, req: &SyncRequest) -> Result<SyncOutcome, Box<SyncFailure>> {
    journaled_sync(device, &prepared(req), &mut |_| {}, &|| false)
}

/// What `device` says of `req` before a sync: the blocker the panel shows.
fn blocker(device: &FakeIpod, req: &SyncRequest) -> Option<SyncBlocker> {
    plan(device, req).expect("plan").blocker
}

/// Refused before the drive was changed in any way: no marker, no audio.
fn assert_untouched(dest: &Path, failure: &SyncFailure, expected: SyncBlocker) {
    assert!(
        matches!(failure.error, SyncError::Blocked(blocker) if blocker == expected),
        "refused with {expected:?}, got {}",
        failure.error
    );
    assert!(!dest.join(crate::identity::MARKER_FILE).exists());
    assert!(!dest.join("iPod_Control/Music").exists());
}

#[test]
fn a_restored_classic_gets_its_audio_and_a_signed_catalogue_of_it() {
    let fx = fixture(3);
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let req = ipod_request(&fx);
    assert_eq!(blocker(&device, &req), None);

    let outcome = sync_ipod(&device, &req).expect("sync");

    assert_eq!(outcome.copied, 3);
    let mut paths: Vec<_> = outcome
        .manifest
        .iter()
        .map(|entry| entry.on_device_path.as_str().to_owned())
        .collect();
    paths.sort();
    assert_eq!(
        paths,
        [
            "iPod_Control/Music/F01/S1.mp3",
            "iPod_Control/Music/F02/S2.mp3",
            "iPod_Control/Music/F03/S3.mp3",
        ]
    );
    for path in &paths {
        assert!(fx.dest.path().join(path).is_file());
    }
    let catalogue = itunesdb(fx.dest.path());
    assert_eq!(sustain_ipod::track_count(&catalogue), Some(3));
    assert_eq!(
        outcome.catalogue.databases,
        std::collections::BTreeSet::from([crate::catalogue::identity(&catalogue)]),
        "the record vouches for exactly the catalogue on the iPod"
    );
}

#[test]
fn a_track_id_names_its_file_in_base_36_across_fifty_folders() {
    let mut fx = fixture(1);
    fx.tracks[0].track_id = TrackId::new(36 * 36 + 51).expect("id");
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let outcome = sync_ipod(&device, &ipod_request(&fx)).expect("sync");
    // 1347 is 50 × 26 + 47, and 1 × 36² + 1 × 36 + 15 in base 36.
    assert_eq!(
        outcome.manifest[0].on_device_path.as_str(),
        "iPod_Control/Music/F47/S11F.mp3"
    );
}

#[test]
fn a_resync_writes_over_its_own_catalogue() {
    let fx = fixture(2);
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let first = sync_ipod(&device, &ipod_request(&fx)).expect("first sync");

    let mut fewer = fx.tracks.clone();
    fewer.truncate(1);
    let req = SyncRequest {
        tracks: fewer,
        playlists: Vec::new(),
        ..resync_request(&fx, DeviceLayout::Ipod, &first, true)
    };
    let req = SyncRequest {
        device: ipod_request(&fx).device,
        ..req
    };
    assert_eq!(blocker(&device, &req), None);
    let second = sync_ipod(&device, &req).expect("second sync");

    assert_eq!((second.unchanged, second.removed), (1, 1));
    assert_eq!(
        sustain_ipod::track_count(&itunesdb(fx.dest.path())),
        Some(1)
    );
    assert!(!fx.dest.path().join("iPod_Control/Music/F02").exists());
}

#[test]
fn an_empty_catalogue_sustain_did_not_write_is_replaced() {
    let fx = fixture(1);
    let device = ipod(fx.dest.path(), Some(VIDEO_SYSINFO));
    let restored = sustain_ipod::write(
        &sustain_ipod::IpodLibrary {
            name: "iPod".into(),
            library_id: 1,
            master_playlist_id: 2,
            tracks: Vec::new(),
            playlists: Vec::new(),
        },
        sustain_ipod::Family::Video1.profile(),
        None,
    )
    .expect("write an empty catalogue");
    device
        .write_file(
            &device_path(sustain_ipod::ITUNESDB_RELATIVE_PATH),
            &restored,
        )
        .expect("write the restored catalogue");

    let req = ipod_request(&fx);
    assert_eq!(blocker(&device, &req), None);
    sync_ipod(&device, &req).expect("sync");
    assert_eq!(
        sustain_ipod::track_count(&itunesdb(fx.dest.path())),
        Some(1)
    );
}

#[test]
fn a_library_sustain_did_not_write_is_left_as_it_is() {
    let fx = fixture(2);
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let first = sync_ipod(&device, &ipod_request(&fx)).expect("first sync");
    // iTunes synced the iPod since: a catalogue listing tracks that no
    // record names.
    let path = fx.dest.path().join(sustain_ipod::ITUNESDB_RELATIVE_PATH);
    let mut foreign = itunesdb(fx.dest.path());
    let last = foreign.len() - 1;
    foreign[last] ^= 0xff;
    std::fs::write(&path, &foreign).expect("write the foreign catalogue");
    std::fs::remove_file(fx.dest.path().join(crate::identity::MARKER_FILE))
        .expect("remove the marker");

    let req = SyncRequest {
        device: ipod_request(&fx).device,
        ..resync_request(&fx, DeviceLayout::Ipod, &first, true)
    };
    assert_eq!(
        blocker(&device, &req),
        Some(SyncBlocker::ForeignIpodLibrary)
    );
    let failure = sync_ipod(&device, &req).expect_err("refused");
    assert!(matches!(
        failure.error,
        SyncError::Blocked(SyncBlocker::ForeignIpodLibrary)
    ));
    assert!(!fx.dest.path().join(crate::identity::MARKER_FILE).exists());
    assert_eq!(std::fs::read(&path).expect("read"), foreign);
}

#[test]
fn a_mac_formatted_ipod_is_refused_before_anything_is_written() {
    let fx = fixture(1);
    let mut device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    // Mounted read-only too, as Linux mounts a journaled HFS+ volume: the
    // format is what the user must fix.
    device.facts.hfs_plus = true;
    device.facts.read_only = true;
    let req = ipod_request(&fx);
    assert_eq!(blocker(&device, &req), Some(SyncBlocker::MacFormattedIpod));
    let failure = sync_ipod(&device, &req).expect_err("refused");
    assert_untouched(fx.dest.path(), &failure, SyncBlocker::MacFormattedIpod);
}

#[test]
fn a_read_only_ipod_is_refused_before_anything_is_written() {
    let fx = fixture(1);
    let mut device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    device.facts.read_only = true;
    let failure = sync_ipod(&device, &ipod_request(&fx)).expect_err("refused");
    assert_untouched(fx.dest.path(), &failure, SyncBlocker::ReadOnlyDevice);
}

#[test]
fn an_ipod_of_unknown_model_is_refused_before_anything_is_written() {
    for sysinfo in [None, Some("ModelNumStr: xZ999\n")] {
        let fx = fixture(1);
        let device = ipod(fx.dest.path(), sysinfo);
        let req = ipod_request(&fx);
        assert_eq!(blocker(&device, &req), Some(SyncBlocker::UnsupportedIpod));
        let failure = sync_ipod(&device, &req).expect_err("refused");
        assert_untouched(fx.dest.path(), &failure, SyncBlocker::UnsupportedIpod);
    }
}

#[test]
fn a_classic_without_a_readable_serial_is_refused_and_an_unsigned_model_is_not() {
    let fx = fixture(1);
    let mut device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    device.facts.usb_serial = None;
    let failure = sync_ipod(&device, &ipod_request(&fx)).expect_err("refused");
    assert_untouched(fx.dest.path(), &failure, SyncBlocker::UnidentifiedIpod);

    let fx = fixture(1);
    let mut device = ipod(fx.dest.path(), Some(VIDEO_SYSINFO));
    device.facts.usb_serial = None;
    sync_ipod(&device, &ipod_request(&fx)).expect("an unsigned catalogue needs no serial");
}

#[test]
fn the_serial_in_sysinfo_signs_when_the_usb_serial_is_unreadable() {
    let fx = fixture(1);
    let sysinfo = format!("{CLASSIC_SYSINFO}FirewireGuid: 0x{CLASSIC_SERIAL}\n");
    let mut device = ipod(fx.dest.path(), Some(&sysinfo));
    device.facts.usb_serial = None;
    sync_ipod(&device, &ipod_request(&fx)).expect("sync");
}

#[test]
fn tracks_an_ipod_cannot_play_are_left_off_and_counted() {
    let mut fx = fixture(3);
    fx.tracks[0].extension = "flac".into();
    fx.tracks[1].sample_rate_hz = 96_000;
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let outcome = sync_ipod(&device, &ipod_request(&fx)).expect("sync");
    assert_eq!((outcome.copied, outcome.unplayable), (1, 2));
    assert_eq!(
        sustain_ipod::track_count(&itunesdb(fx.dest.path())),
        Some(1)
    );
}

#[test]
fn the_firmware_event_files_go_before_the_new_catalogue() {
    let fx = fixture(1);
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    let itunes = fx.dest.path().join("iPod_Control/iTunes");
    std::fs::create_dir_all(&itunes).expect("create the iTunes folder");
    for name in ["Play Counts", "OTGPlaylistInfo", "OTGPlaylistInfo_1"] {
        std::fs::write(itunes.join(name), b"rows").expect("write an event file");
    }
    sync_ipod(&device, &ipod_request(&fx)).expect("sync");
    assert!(!itunes.join("Play Counts").exists());
    assert!(!itunes.join("OTGPlaylistInfo").exists());
    assert!(
        itunes.join("OTGPlaylistInfo_1").exists(),
        "the numbered siblings are the firmware's to drop"
    );
}

#[test]
fn a_cancelled_run_publishes_no_catalogue() {
    let fx = fixture(2);
    let device = ipod(fx.dest.path(), Some(CLASSIC_SYSINFO));
    // Stop once the first track is on the iPod.
    let copied = std::cell::Cell::new(false);
    let outcome = journaled_sync(
        &device,
        &prepared(&ipod_request(&fx)),
        &mut |progress| copied.set(copied.get() || progress.stage == SyncStage::Copying),
        &|| copied.get(),
    )
    .expect("cancelled sync");
    assert!(outcome.cancelled);
    assert!(
        !fx.dest
            .path()
            .join(sustain_ipod::ITUNESDB_RELATIVE_PATH)
            .exists()
    );
    assert!(outcome.catalogue.databases.is_empty());
}
