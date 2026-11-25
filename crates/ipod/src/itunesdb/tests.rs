// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The writer's output is walked record by record: every length nests,
//! every count matches what follows, and every cross-reference resolves.

use std::collections::HashSet;

use super::{WriteError, device_path, text_record, track_count, write};
use crate::hash58::FirewireGuid;
use crate::model::{FileType, IpodLibrary, IpodPlaylist, IpodTrack};
use crate::profile::{Checksum, Family, Profile};

fn track(dbid: u64, title: &str, artist: &str, album: &str) -> IpodTrack {
    IpodTrack {
        dbid,
        location: format!("iPod_Control/Music/F{:02}/S{dbid}.mp3", dbid % 50),
        file_type: FileType::Mp3,
        title: Some(title.to_owned()),
        artist: Some(artist.to_owned()),
        album: Some(album.to_owned()),
        album_artist: None,
        genre: Some("Electronic".to_owned()),
        composer: None,
        comment: None,
        sort_title: None,
        sort_artist: None,
        sort_album: None,
        sort_album_artist: None,
        sort_composer: None,
        compilation: false,
        rating_stars: 4,
        track_number: Some(1),
        track_total: Some(10),
        disc_number: None,
        disc_total: None,
        year: Some(1997),
        duration_ms: 240_000,
        size_bytes: 9_600_000,
        bitrate_kbps: Some(320),
        sample_rate_hz: 44_100,
        date_added: Some(1_700_000_000),
    }
}

fn library() -> IpodLibrary {
    IpodLibrary {
        name: "Julien's iPod".to_owned(),
        library_id: 0x5355_5354_4149_4e00,
        master_playlist_id: 1,
        tracks: vec![
            track(10, "Teardrop", "Massive Attack", "Mezzanine"),
            track(11, "Angel", "Massive Attack", "Mezzanine"),
            track(12, "Joga", "Björk", "Homogenic"),
            track(13, "Sexy Boy", "Air", "Moon Safari"),
        ],
        playlists: vec![
            IpodPlaylist {
                id: 2,
                name: "Night drive".to_owned(),
                // A track twice: playlists keep duplicates.
                track_indices: vec![2, 0, 2],
            },
            IpodPlaylist {
                id: 3,
                name: "Empty".to_owned(),
                track_indices: Vec::new(),
            },
        ],
    }
}

fn unsigned() -> Profile {
    Family::Mini2.profile()
}

fn signed() -> Profile {
    Family::Classic2.profile()
}

fn guid() -> FirewireGuid {
    FirewireGuid::parse("000A27001C0FFEE5").expect("valid GUID")
}

fn u32_at(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("four bytes"))
}

/// A record found while walking the tree.
struct Found {
    offset: usize,
    header_length: usize,
    total_length: usize,
}

/// The record at `offset`, which must carry `tag` and fit in `end`.
fn record(bytes: &[u8], offset: usize, tag: &[u8; 4], end: usize) -> Found {
    assert_eq!(&bytes[offset..offset + 4], tag, "tag at {offset:#x}");
    let header_length = u32_at(bytes, offset + 4) as usize;
    let total_length = u32_at(bytes, offset + 8) as usize;
    assert!(total_length >= header_length, "{tag:?} at {offset:#x}");
    assert!(
        offset + total_length <= end,
        "{tag:?} at {offset:#x} overruns"
    );
    Found {
        offset,
        header_length,
        total_length,
    }
}

/// The children of a container record, which must exactly fill it.
fn text_records(bytes: &[u8], parent: &Found, first: usize, count: u32) -> Vec<(u32, Vec<u8>)> {
    let end = parent.offset + parent.total_length;
    let mut offset = first;
    let mut children = Vec::new();
    for _ in 0..count {
        let child = record(bytes, offset, b"mhod", end);
        children.push((
            u32_at(bytes, offset + 12),
            bytes[offset..offset + child.total_length].to_vec(),
        ));
        offset += child.total_length;
    }
    children
}

struct Walked {
    dataset_kinds: Vec<u32>,
    track_ids: Vec<u32>,
    /// The playlists of each playlist dataset.
    playlists: Vec<Vec<WalkedPlaylist>>,
    album_count: u32,
    artist_count: u32,
}

struct WalkedPlaylist {
    master: bool,
    entries: Vec<u32>,
    indexes: Vec<WalkedIndex>,
}

/// A browse index and its jump table: the sort code, the positions, and
/// the runs as (letter, start, count).
struct WalkedIndex {
    sort: u32,
    positions: Vec<u32>,
    runs: Vec<(u16, u32, u32)>,
}

fn walk(bytes: &[u8]) -> Walked {
    let root = record(bytes, 0, b"mhbd", bytes.len());
    assert_eq!(root.header_length, 0xf4);
    assert_eq!(root.total_length, bytes.len());
    let mut walked = Walked {
        dataset_kinds: Vec::new(),
        track_ids: Vec::new(),
        playlists: Vec::new(),
        album_count: 0,
        artist_count: 0,
    };
    let mut offset = root.header_length;
    for _ in 0..u32_at(bytes, 0x14) {
        let dataset = record(bytes, offset, b"mhsd", bytes.len());
        assert_eq!(dataset.header_length, 0x60);
        let kind = u32_at(bytes, offset + 12);
        walked.dataset_kinds.push(kind);
        let end = offset + dataset.total_length;
        let list = offset + dataset.header_length;
        let count = u32_at(bytes, list + 8);
        let mut child = list + 0x5c;
        match kind {
            1 | 6 | 10 => {
                assert_eq!(&bytes[list..list + 4], b"mhlt");
                for _ in 0..count {
                    let track = record(bytes, child, b"mhit", end);
                    assert_eq!(track.header_length, 0x248);
                    walked.track_ids.push(u32_at(bytes, child + 0x10));
                    let texts = text_records(
                        bytes,
                        &track,
                        child + track.header_length,
                        u32_at(bytes, child + 12),
                    );
                    let used: usize = texts.iter().map(|(_, record)| record.len()).sum();
                    assert_eq!(track.header_length + used, track.total_length);
                    child += track.total_length;
                }
            }
            2 | 3 | 5 => {
                assert_eq!(&bytes[list..list + 4], b"mhlp");
                let mut playlists = Vec::new();
                for _ in 0..count {
                    let playlist = record(bytes, child, b"mhyp", end);
                    assert_eq!(playlist.header_length, 0x6c);
                    assert_eq!(u32_at(bytes, child + 0x2c), 1, "sorted manually");
                    let records = text_records(
                        bytes,
                        &playlist,
                        child + playlist.header_length,
                        u32_at(bytes, child + 12),
                    );
                    let mut indexes = Vec::new();
                    for pair in records.chunks(2).skip(1) {
                        let (index, positions) = (&pair[0].1, u32_at(&pair[0].1, 28));
                        assert_eq!(pair[0].0, 52);
                        assert_eq!(pair[1].0, 53);
                        let sort = u32_at(index, 24);
                        let positions: Vec<u32> = (0..positions as usize)
                            .map(|entry| u32_at(index, 72 + 4 * entry))
                            .collect();
                        let jump = &pair[1].1;
                        assert_eq!(u32_at(jump, 24), sort);
                        let runs = (0..u32_at(jump, 28) as usize)
                            .map(|run| {
                                let at = 40 + 12 * run;
                                (
                                    u16::from_le_bytes([jump[at], jump[at + 1]]),
                                    u32_at(jump, at + 4),
                                    u32_at(jump, at + 8),
                                )
                            })
                            .collect();
                        indexes.push(WalkedIndex {
                            sort,
                            positions,
                            runs,
                        });
                    }
                    let mut entry = child
                        + playlist.header_length
                        + records
                            .iter()
                            .map(|(_, record)| record.len())
                            .sum::<usize>();
                    let mut entries = Vec::new();
                    for position in 0..u32_at(bytes, child + 0x10) {
                        let item = record(bytes, entry, b"mhip", end);
                        entries.push(u32_at(bytes, entry + 0x18));
                        let place = text_records(bytes, &item, entry + item.header_length, 1);
                        assert_eq!(place[0].0, 100);
                        assert_eq!(u32_at(&place[0].1, 24), position);
                        entry += item.total_length;
                    }
                    assert_eq!(entry, child + playlist.total_length);
                    playlists.push(WalkedPlaylist {
                        master: bytes[child + 0x14] == 1,
                        entries,
                        indexes,
                    });
                    child += playlist.total_length;
                }
                walked.playlists.push(playlists);
            }
            4 => {
                assert_eq!(&bytes[list..list + 4], b"mhla");
                walked.album_count = count;
                for _ in 0..count {
                    let album = record(bytes, child, b"mhia", end);
                    child += album.total_length;
                }
            }
            8 => {
                assert_eq!(&bytes[list..list + 4], b"mhli");
                walked.artist_count = count;
                for _ in 0..count {
                    let artist = record(bytes, child, b"mhii", end);
                    child += artist.total_length;
                }
            }
            other => unreachable!("the writer emits no dataset {other}"),
        }
        assert_eq!(child, end, "dataset {kind} is exactly filled");
        offset = end;
    }
    assert_eq!(offset, bytes.len());
    walked
}

#[test]
fn the_catalogue_is_a_well_formed_tree_in_libgpods_dataset_order() {
    let library = library();
    let bytes = write(&library, unsigned(), None).expect("writes");
    let walked = walk(&bytes);
    assert_eq!(walked.dataset_kinds, vec![1, 3, 2, 4, 8, 6, 10, 5]);
    assert_eq!(walked.track_ids, vec![52, 53, 54, 55]);
    // Three albums (Mezzanine counted once), three artists.
    assert_eq!(walked.album_count, 3);
    assert_eq!(walked.artist_count, 3);
}

#[test]
fn both_playlist_datasets_list_the_master_first_then_the_playlists_in_order() {
    let bytes = write(&library(), unsigned(), None).expect("writes");
    let walked = walk(&bytes);
    // Datasets 3 and 2 carry the playlists; dataset 5 none.
    assert_eq!(walked.playlists.len(), 3);
    assert!(walked.playlists[2].is_empty());
    for playlists in &walked.playlists[..2] {
        assert_eq!(playlists.len(), 3);
        assert!(playlists[0].master);
        assert_eq!(playlists[0].entries, vec![52, 53, 54, 55]);
        assert!(!playlists[1].master);
        assert_eq!(playlists[1].entries, vec![54, 52, 54]);
        assert!(playlists[2].entries.is_empty());
        assert!(playlists[1].indexes.is_empty());
    }
}

#[test]
fn the_master_playlist_indexes_every_browse_menu_with_covering_jump_runs() {
    let bytes = write(&library(), unsigned(), None).expect("writes");
    let master = &walk(&bytes).playlists[1][0];
    let sorts: Vec<u32> = master.indexes.iter().map(|index| index.sort).collect();
    assert_eq!(sorts, vec![0x03, 0x05, 0x04, 0x07, 0x12]);
    for WalkedIndex {
        sort,
        positions,
        runs,
    } in &master.indexes
    {
        let distinct: HashSet<u32> = positions.iter().copied().collect();
        assert_eq!(
            distinct,
            (0..4).collect(),
            "menu {sort:#x} is a permutation"
        );
        let mut next = 0;
        for &(_, start, count) in runs {
            assert_eq!(start, next, "menu {sort:#x} runs are contiguous");
            assert!(count > 0);
            next += count;
        }
        assert_eq!(next, 4, "menu {sort:#x} runs cover the index");
    }
    // By artist: Air, Björk, then Massive Attack's album by track order
    // (both are track 1, so by title: Angel before Teardrop).
    let by_artist = &master.indexes[1];
    assert_eq!(by_artist.positions, vec![3, 2, 1, 0]);
    let letters: Vec<u16> = by_artist
        .runs
        .iter()
        .map(|&(letter, _, _)| letter)
        .collect();
    assert_eq!(
        letters,
        vec![u16::from(b'A'), u16::from(b'B'), u16::from(b'M')]
    );
}

#[test]
fn an_empty_library_has_a_master_playlist_without_indexes() {
    let mut library = library();
    library.tracks.clear();
    library.playlists.clear();
    let bytes = write(&library, signed(), Some(guid())).expect("writes");
    let walked = walk(&bytes);
    assert!(walked.track_ids.is_empty());
    let master = &walked.playlists[1][0];
    assert!(master.master && master.entries.is_empty() && master.indexes.is_empty());
    assert_eq!(track_count(&bytes), Some(0));
}

#[test]
fn track_fields_land_at_their_offsets() {
    let bytes = write(&library(), signed(), Some(guid())).expect("writes");
    // The first track record follows the root, the dataset and the list.
    let mhit = 0xf4 + 0x60 + 0x5c;
    assert_eq!(&bytes[mhit..mhit + 4], b"mhit");
    assert_eq!(u32_at(&bytes, mhit + 0x14), 1, "visible");
    assert_eq!(
        &bytes[mhit + 0x18..mhit + 0x1c],
        b" 3PM",
        "MP3 marker, little-endian"
    );
    assert_eq!(bytes[mhit + 0x1f], 80, "four stars");
    assert_eq!(u32_at(&bytes, mhit + 0x28), 240_000, "length in ms");
    assert_eq!(u32_at(&bytes, mhit + 0x3c), 44_100 << 16, "sample rate");
    assert_eq!(
        u32_at(&bytes, mhit + 0x68),
        1_700_000_000 + 2_082_844_800,
        "added, as Mac time"
    );
    assert_eq!(
        &bytes[mhit + 0x70..mhit + 0x78],
        &10u64.to_le_bytes(),
        "dbid"
    );
    assert_eq!(
        &bytes[mhit + 0xa8..mhit + 0xb0],
        &10u64.to_le_bytes(),
        "dbid2"
    );
    assert_eq!(
        u32_at(&bytes, mhit + 0xd0),
        1,
        "audio, on a video-capable iPod"
    );
    assert_eq!(u32_at(&bytes, mhit + 0x120), 1, "first album");
    assert_eq!(u32_at(&bytes, mhit + 0x1e0), 1, "first artist");

    let older = write(&library(), unsigned(), None).expect("writes");
    assert_eq!(u32_at(&older, mhit + 0xd0), 0, "no media type before video");
}

#[test]
fn text_is_utf16_without_a_terminator() {
    let record = text_record(1, "Jóga").expect("encodes");
    assert_eq!(u32_at(&record, 4), 24);
    assert_eq!(u32_at(&record, 8), 48);
    assert_eq!(u32_at(&record, 12), 1);
    assert_eq!(u32_at(&record, 24), 1, "UTF-16");
    assert_eq!(u32_at(&record, 28), 8, "byte length");
    assert_eq!(&record[40..], &[b'J', 0, 0xf3, 0, b'g', 0, b'a', 0]);
}

#[test]
fn locations_become_colon_separated_device_paths() {
    assert_eq!(
        device_path("iPod_Control/Music/F07/S2K.mp3"),
        ":iPod_Control:Music:F07:S2K.mp3"
    );
}

#[test]
fn a_signed_catalogue_differs_from_the_unsigned_one_only_in_its_signature() {
    let library = library();
    let unsigned_classic = Profile {
        checksum: Checksum::None,
        ..signed()
    };
    let plain = write(&library, unsigned_classic, None).expect("writes");
    let signed_bytes = write(&library, signed(), Some(guid())).expect("writes");
    assert_eq!(
        u16::from_le_bytes([signed_bytes[0x30], signed_bytes[0x31]]),
        1
    );
    assert_ne!(signed_bytes[0x58..0x6c], [0u8; 20]);
    let mut unsigned_again = signed_bytes.clone();
    unsigned_again[0x30..0x32].fill(0);
    unsigned_again[0x58..0x6c].fill(0);
    assert_eq!(unsigned_again, plain);
}

#[test]
fn a_hash58_device_without_a_guid_is_refused() {
    assert_eq!(
        write(&library(), signed(), None),
        Err(WriteError::MissingFirewireGuid)
    );
    assert_eq!(signed().checksum, Checksum::Hash58);
}

#[test]
fn ids_must_be_nonzero_and_unique() {
    let mut library = library();
    library.tracks[1].dbid = 10;
    assert_eq!(
        write(&library, unsigned(), None),
        Err(WriteError::InvalidTrackId { dbid: 10 })
    );
    let mut library = self::library();
    library.tracks[0].dbid = 0;
    assert_eq!(
        write(&library, unsigned(), None),
        Err(WriteError::InvalidTrackId { dbid: 0 })
    );
    let mut library = self::library();
    library.playlists[0].id = library.master_playlist_id;
    assert_eq!(
        write(&library, unsigned(), None),
        Err(WriteError::InvalidPlaylistId { id: 1 })
    );
}

#[test]
fn a_playlist_cannot_name_a_missing_track() {
    let mut library = library();
    library.playlists[0].track_indices.push(4);
    assert_eq!(
        write(&library, unsigned(), None),
        Err(WriteError::UnknownTrack {
            playlist: "Night drive".to_owned(),
            index: 4
        })
    );
}

#[test]
fn track_count_reads_sustains_and_libgpods_catalogues() {
    let bytes = write(&library(), signed(), Some(guid())).expect("writes");
    assert_eq!(track_count(&bytes), Some(4));
    let libgpod = include_bytes!("../../tests/fixtures/libgpod-empty-classic.itunesdb");
    assert_eq!(track_count(libgpod), Some(0));
}

#[test]
fn track_count_does_not_follow_a_damaged_catalogue() {
    let bytes = write(&library(), unsigned(), None).expect("writes");
    assert_eq!(track_count(&[]), None);
    assert_eq!(track_count(&bytes[..bytes.len() - 1]), None, "truncated");
    let mut wrong_tag = bytes.clone();
    wrong_tag[0xf4] = b'x';
    assert_eq!(track_count(&wrong_tag), None, "no dataset");
    let mut overlong = bytes.clone();
    overlong[0xf4 + 8..0xf4 + 12].copy_from_slice(&u32::MAX.to_le_bytes());
    assert_eq!(track_count(&overlong), None, "dataset overruns the file");
    let mut many = bytes;
    many[0x14..0x18].copy_from_slice(&u32::MAX.to_le_bytes());
    // Still finds the track list: it is the first dataset.
    assert_eq!(track_count(&many), Some(4));
}
