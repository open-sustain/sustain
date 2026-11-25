// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! The `iTunesDB` catalogue.
//!
//! The file is a tree of records. Each starts with a four-letter tag, its
//! header length and either its total length (children included) or, for a
//! list, its child count; numbers are little-endian, text is UTF-16LE. The
//! root `mhbd` holds eight datasets (`mhsd`), in libgpod's order:
//!
//! | type | holds |
//! |---|---|
//! | 1 | the tracks (`mhlt` → `mhit` → `mhod`) |
//! | 3 | the playlists, in the layout podcasts once needed |
//! | 2 | the playlists (`mhlp` → `mhyp` → `mhip`) |
//! | 4 | the albums (`mhla` → `mhia`) |
//! | 8 | the artists (`mhli` → `mhii`) |
//! | 6, 10 | empty track lists |
//! | 5 | an empty playlist list |
//!
//! The master playlist comes first in both playlist datasets. It lists
//! every track, and carries the browse indexes and jump tables (the `sort` module).
//! Every record's header is written at its full libgpod size, and every
//! field at its libgpod offset; a field Sustain has no value for is zero.

use std::collections::{HashMap, HashSet};

use crate::artwork::Images;
use crate::hash58::{self, FirewireGuid};
use crate::model::{FileType, IpodLibrary, IpodPlaylist, IpodTrack};
use crate::profile::{Checksum, Profile};
use crate::record::{Header, Open, Out, WriteError, u32_len};
use crate::sort::{BrowseIndexes, Menu};

/// The catalogue format version: iTunes 9.2's, the one libgpod writes.
const DATABASE_VERSION: u32 = 0x30;
/// libgpod numbers tracks from 52 when it rewrites a catalogue.
const FIRST_TRACK_ID: u32 = 52;
/// Seconds from the Mac epoch (1904-01-01) to the Unix epoch.
const MAC_EPOCH_OFFSET: i64 = 2_082_844_800;
/// libgpod's rating step: one star is 20.
const RATING_PER_STAR: u8 = 20;

// Text record types.
const TITLE: u32 = 1;
const PATH: u32 = 2;
const ALBUM: u32 = 3;
const ARTIST: u32 = 4;
const GENRE: u32 = 5;
const KIND: u32 = 6;
const COMMENT: u32 = 8;
const COMPOSER: u32 = 12;
const ALBUM_ARTIST: u32 = 22;
const SORT_ARTIST: u32 = 23;
const SORT_TITLE: u32 = 27;
const SORT_ALBUM: u32 = 28;
const SORT_ALBUM_ARTIST: u32 = 29;
const SORT_COMPOSER: u32 = 30;
const BROWSE_INDEX: u32 = 52;
const JUMP_TABLE: u32 = 53;
const PLAYLIST_ENTRY: u32 = 100;
const ALBUM_LIST_ALBUM: u32 = 200;
const ALBUM_LIST_ARTIST: u32 = 201;
const ALBUM_LIST_SORT_ARTIST: u32 = 202;
const ARTIST_LIST_ARTIST: u32 = 300;

/// Write `library` as the catalogue of a device with `profile`, signed when
/// the profile requires it (`guid` must then be given).
pub fn write(
    library: &IpodLibrary,
    profile: Profile,
    guid: Option<FirewireGuid>,
) -> Result<Vec<u8>, WriteError> {
    validate(library)?;
    let guid = match profile.checksum {
        Checksum::None => None,
        Checksum::Hash58 => Some(guid.ok_or(WriteError::MissingFirewireGuid)?),
    };
    let groups = Groups::new(&library.tracks);
    let images = Images::new(&library.tracks, profile)?;
    let mut out = Out::default();
    let root = out.open_record(&mhbd_header(library));
    write_tracks(&mut out, library, &groups, &images.track_links, profile)?;
    write_playlists(&mut out, library, 3)?;
    write_playlists(&mut out, library, 2)?;
    write_albums(&mut out, library, &groups)?;
    write_artists(&mut out, library, &groups)?;
    write_empty_track_list(&mut out, 6)?;
    write_empty_track_list(&mut out, 10)?;
    write_empty_playlist_list(&mut out)?;
    out.close_record(root)?;
    let mut bytes = out.bytes;
    if let Some(guid) = guid {
        hash58::sign_written(&mut bytes, guid);
    }
    Ok(bytes)
}

fn validate(library: &IpodLibrary) -> Result<(), WriteError> {
    let mut dbids = HashSet::with_capacity(library.tracks.len());
    for track in &library.tracks {
        if track.dbid == 0 || !dbids.insert(track.dbid) {
            return Err(WriteError::InvalidTrackId { dbid: track.dbid });
        }
    }
    let mut ids = HashSet::with_capacity(library.playlists.len() + 1);
    for id in std::iter::once(library.master_playlist_id)
        .chain(library.playlists.iter().map(|playlist| playlist.id))
    {
        if id == 0 || !ids.insert(id) {
            return Err(WriteError::InvalidPlaylistId { id });
        }
    }
    for playlist in &library.playlists {
        if let Some(&index) = playlist
            .track_indices
            .iter()
            .find(|&&index| index >= library.tracks.len())
        {
            return Err(WriteError::UnknownTrack {
                playlist: playlist.name.clone(),
                index,
            });
        }
    }
    // Every count and position the records hold is 32-bit.
    if u32::try_from(library.tracks.len())
        .ok()
        .and_then(|count| count.checked_add(FIRST_TRACK_ID))
        .is_none()
    {
        return Err(WriteError::TooLarge);
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Records
// ---------------------------------------------------------------------

fn mhbd_header(library: &IpodLibrary) -> Vec<u8> {
    let mut header = Header::new(b"mhbd", 0xf4);
    header
        // Not compressed: 1. The late Classic rejects 2.
        .u32(0x0c, 1)
        .u32(0x10, DATABASE_VERSION)
        .u32(0x14, 8)
        .u64(0x18, library.library_id)
        // The platform that wrote the catalogue: 1 is a Mac, 2 Windows.
        // libgpod writes 1 for a catalogue it creates, whatever the
        // volume's format, and a Mac syncing a Windows-formatted iPod
        // writes the same, so 1 is the value known to work on FAT32.
        .u16(0x20, 1)
        // The language, "en" as libgpod writes it.
        .u16(0x46, 0x656e)
        .u64(0x48, library.library_id);
    header.0
}

/// A dataset of `kind`; its children follow.
fn open_dataset(out: &mut Out, kind: u32) -> Open {
    let mut header = Header::new(b"mhsd", 0x60);
    header.u32(0x0c, kind);
    out.open_record(&header.0)
}

/// A list header (`mhlt`, `mhlp`, `mhla`, `mhli`): it holds its child
/// count where other records hold their total length.
fn list_header(tag: &[u8; 4], count: usize) -> Result<Vec<u8>, WriteError> {
    let mut header = Header::new(tag, 0x5c);
    header.u32(0x08, u32_len(count)?);
    Ok(header.0)
}

/// A text record: a 24-byte header, then the encoding (1, UTF-16), the
/// text's byte length, two words libgpod writes as 1 and 0, and the text
/// without a terminator.
fn text_record(kind: u32, text: &str) -> Result<Vec<u8>, WriteError> {
    let utf16: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let text_length = u32_len(utf16.len())?;
    let mut header = Header::new(b"mhod", 0x18);
    header
        .u32(
            0x08,
            text_length.checked_add(40).ok_or(WriteError::TooLarge)?,
        )
        .u32(0x0c, kind);
    let mut record = header.0;
    for word in [1, text_length, 1, 0] {
        record.extend_from_slice(&u32::to_le_bytes(word));
    }
    record.extend_from_slice(&utf16);
    Ok(record)
}

/// Append the text records of `fields` that hold text, returning how many.
fn write_text_records(out: &mut Out, fields: &[(u32, Option<&str>)]) -> Result<u32, WriteError> {
    let mut count = 0;
    for &(kind, text) in fields {
        if let Some(text) = text.filter(|text| !text.is_empty()) {
            out.extend(&text_record(kind, text)?);
            count += 1;
        }
    }
    Ok(count)
}

// ---------------------------------------------------------------------
// Albums and artists
// ---------------------------------------------------------------------

/// The albums and artists of the library, numbered from 1 in order of first
/// appearance, as libgpod numbers them; each track's album and artist id;
/// and the first track of each, whose text names it.
struct Groups {
    track_album: Vec<u32>,
    track_artist: Vec<u32>,
    track_composer: Vec<u32>,
    albums: Vec<usize>,
    artists: Vec<usize>,
}

impl Groups {
    fn new(tracks: &[IpodTrack]) -> Self {
        let mut groups = Self {
            track_album: Vec::with_capacity(tracks.len()),
            track_artist: Vec::with_capacity(tracks.len()),
            track_composer: Vec::with_capacity(tracks.len()),
            albums: Vec::new(),
            artists: Vec::new(),
        };
        let mut albums: HashMap<(&str, &str), u32> = HashMap::new();
        let mut artists: HashMap<&str, u32> = HashMap::new();
        let mut composers: HashMap<&str, u32> = HashMap::new();
        for (index, track) in tracks.iter().enumerate() {
            // An album is one title by one album artist (the artist when no
            // album artist is set): two "Greatest Hits" stay apart.
            let album = present(track.album.as_deref()).map_or(0, |album| {
                let by = present(track.album_artist.as_deref())
                    .or(present(track.artist.as_deref()))
                    .unwrap_or_default();
                number(&mut albums, (album, by), &mut groups.albums, index)
            });
            let artist = present(track.artist.as_deref()).map_or(0, |artist| {
                number(&mut artists, artist, &mut groups.artists, index)
            });
            let composer = present(track.composer.as_deref()).map_or(0, |composer| {
                let next = u32_count(composers.len()) + 1;
                *composers.entry(composer).or_insert(next)
            });
            groups.track_album.push(album);
            groups.track_artist.push(artist);
            groups.track_composer.push(composer);
        }
        groups
    }
}

fn number<K: std::hash::Hash + Eq>(
    ids: &mut HashMap<K, u32>,
    key: K,
    firsts: &mut Vec<usize>,
    index: usize,
) -> u32 {
    *ids.entry(key).or_insert_with(|| {
        firsts.push(index);
        u32_count(firsts.len())
    })
}

/// A count already bounded by [`validate`].
fn u32_count(count: usize) -> u32 {
    u32::try_from(count).expect("the writer bounds every count to 32 bits")
}

fn present(text: Option<&str>) -> Option<&str> {
    text.filter(|text| !text.is_empty())
}

fn write_albums(out: &mut Out, library: &IpodLibrary, groups: &Groups) -> Result<(), WriteError> {
    let dataset = open_dataset(out, 4);
    out.extend(&list_header(b"mhla", groups.albums.len())?);
    for (id, &first) in (1..).zip(&groups.albums) {
        let track = &library.tracks[first];
        let mut header = Header::new(b"mhia", 0x58);
        header.u32(0x10, id).u64(0x14, u64::from(id)).u32(0x1c, 2);
        let record = out.open_record(&header.0);
        let count = write_text_records(
            out,
            &[
                (ALBUM_LIST_ALBUM, track.album.as_deref()),
                (
                    ALBUM_LIST_ARTIST,
                    present(track.album_artist.as_deref()).or(track.artist.as_deref()),
                ),
                (
                    ALBUM_LIST_SORT_ARTIST,
                    present(track.sort_album_artist.as_deref()).or(track.sort_artist.as_deref()),
                ),
            ],
        )?;
        out.set_u32(record, 0x0c, count);
        out.close_record(record)?;
    }
    out.close_record(dataset)
}

fn write_artists(out: &mut Out, library: &IpodLibrary, groups: &Groups) -> Result<(), WriteError> {
    let dataset = open_dataset(out, 8);
    out.extend(&list_header(b"mhli", groups.artists.len())?);
    for (id, &first) in (1..).zip(&groups.artists) {
        let mut header = Header::new(b"mhii", 0x50);
        header.u32(0x10, id).u64(0x14, u64::from(id)).u32(0x1c, 2);
        let record = out.open_record(&header.0);
        let count = write_text_records(
            out,
            &[(ARTIST_LIST_ARTIST, library.tracks[first].artist.as_deref())],
        )?;
        out.set_u32(record, 0x0c, count);
        out.close_record(record)?;
    }
    out.close_record(dataset)
}

// ---------------------------------------------------------------------
// Tracks
// ---------------------------------------------------------------------

fn write_tracks(
    out: &mut Out,
    library: &IpodLibrary,
    groups: &Groups,
    image_links: &[u32],
    profile: Profile,
) -> Result<(), WriteError> {
    let dataset = open_dataset(out, 1);
    out.extend(&list_header(b"mhlt", library.tracks.len())?);
    for (index, track) in library.tracks.iter().enumerate() {
        let record = out.open_record(&mhit_header(
            track,
            index,
            groups,
            image_links[index],
            profile,
        ));
        let path = device_path(&track.location);
        let count = write_text_records(
            out,
            &[
                (TITLE, track.title.as_deref()),
                (ARTIST, track.artist.as_deref()),
                (ALBUM, track.album.as_deref()),
                (KIND, Some(track.file_type.kind())),
                (COMMENT, track.comment.as_deref()),
                (PATH, Some(&path)),
                (GENRE, track.genre.as_deref()),
                (COMPOSER, track.composer.as_deref()),
                (ALBUM_ARTIST, track.album_artist.as_deref()),
                (SORT_ARTIST, track.sort_artist.as_deref()),
                (SORT_TITLE, track.sort_title.as_deref()),
                (SORT_ALBUM, track.sort_album.as_deref()),
                (SORT_ALBUM_ARTIST, track.sort_album_artist.as_deref()),
                (SORT_COMPOSER, track.sort_composer.as_deref()),
            ],
        )?;
        out.set_u32(record, 0x0c, count);
        out.close_record(record)?;
    }
    out.close_record(dataset)
}

/// The track's id in this catalogue: its position, numbered from 52.
fn track_id(index: usize) -> u32 {
    u32_count(index) + FIRST_TRACK_ID
}

/// A device path as the catalogue spells it: colon-separated from the root
/// (`:iPod_Control:Music:F07:S2K.mp3`).
fn device_path(location: &str) -> String {
    let mut path = String::with_capacity(location.len() + 1);
    for component in location
        .split('/')
        .filter(|component| !component.is_empty())
    {
        path.push(':');
        path.push_str(component);
    }
    path
}

fn mac_time(unix: Option<i64>) -> u32 {
    unix.and_then(|seconds| seconds.checked_add(MAC_EPOCH_OFFSET))
        .and_then(|seconds| u32::try_from(seconds).ok())
        .unwrap_or(0)
}

fn mhit_header(
    track: &IpodTrack,
    index: usize,
    groups: &Groups,
    image_link: u32,
    profile: Profile,
) -> Vec<u8> {
    let (type1, type2, unk126, unk144): (u8, u8, u16, u16) = match track.file_type {
        // The two type bytes are as iTunes writes them for a constant
        // bitrate MP3 (0, 1) and for MPEG-4 audio (0, 0); it sets the
        // first to 1 for a variable bitrate MP3. The firmware plays
        // without them: libgpod leaves both 0 on every track it adds,
        // variable bitrate or not. The two words after them are libgpod's
        // defaults per format.
        FileType::Mp3 => (0, 1, 0xffff, 0x000c),
        FileType::Mpeg4 => (0, 0, 0xffff, 0x0033),
        FileType::Wav | FileType::Aiff => (0, 0, 0, 0),
    };
    let added = mac_time(track.date_added);
    let mut header = Header::new(b"mhit", 0x248);
    header
        .u32(0x10, track_id(index))
        // Visible.
        .u32(0x14, 1)
        .bytes(0x18, &marker_le(track.file_type))
        .u8(0x1c, type1)
        .u8(0x1d, type2)
        .u8(0x1e, u8::from(track.compilation))
        .u8(0x1f, track.rating_stars.min(5) * RATING_PER_STAR)
        .u32(0x24, track.size_bytes)
        .u32(0x28, track.duration_ms)
        .u32(0x2c, track.track_number.unwrap_or(0))
        .u32(0x30, track.track_total.unwrap_or(0))
        .u32(0x34, track.year.unwrap_or(0))
        .u32(0x38, track.bitrate_kbps.unwrap_or(0))
        .u32(0x3c, u32::from(track.sample_rate_hz) << 16)
        .u32(0x5c, track.disc_number.unwrap_or(0))
        .u32(0x60, track.disc_total.unwrap_or(0))
        .u32(0x68, added)
        .u64(0x70, track.dbid)
        .u16(0x7e, unk126)
        .bytes(0x88, &f32::from(track.sample_rate_hz).to_le_bytes())
        .u16(0x90, unk144)
        .u64(0xa8, track.dbid)
        // Not to be marked as an unplayed podcast.
        .u8(0xb2, 1)
        .u32(0xd0, u32::from(profile.plays_video))
        .u32(0x120, groups.track_album[index])
        .u32(0x12c, track.size_bytes)
        .u64(0x134, 0x8080_8080_8080)
        .u32(0x168, 1)
        .u32(0x1e0, groups.track_artist[index])
        .u32(0x1f4, groups.track_composer[index]);
    if let Some(cover) = track.cover.filter(|_| image_link != 0) {
        // One cover, the size of the image it came from, "has artwork",
        // and the image the firmware looks it up by.
        header
            .u16(0x7c, 1)
            .u32(0x80, cover.source_bytes())
            .u8(0xa4, 1)
            .u32(0x160, image_link);
    }
    header.0
}

/// The file-type marker as the record stores it: a big-endian
/// four-character code written as a little-endian word.
fn marker_le(file_type: FileType) -> [u8; 4] {
    let mut marker = file_type.marker();
    marker.reverse();
    marker
}

// ---------------------------------------------------------------------
// Playlists
// ---------------------------------------------------------------------

fn write_playlists(out: &mut Out, library: &IpodLibrary, kind: u32) -> Result<(), WriteError> {
    let dataset = open_dataset(out, kind);
    out.extend(&list_header(b"mhlp", library.playlists.len() + 1)?);
    let master = IpodPlaylist {
        id: library.master_playlist_id,
        name: library.name.clone(),
        track_indices: (0..library.tracks.len()).collect(),
    };
    write_playlist(out, &master, Some(&library.tracks))?;
    for playlist in &library.playlists {
        write_playlist(out, playlist, None)?;
    }
    out.close_record(dataset)
}

/// Write one playlist; `master_tracks` is the track list when it is the
/// master playlist, which also carries the browse indexes. The creation
/// time (`0x18`) stays 0, "no time" as libgpod spells it, so that the same
/// library always yields the same bytes.
fn write_playlist(
    out: &mut Out,
    playlist: &IpodPlaylist,
    master_tracks: Option<&[IpodTrack]>,
) -> Result<(), WriteError> {
    let indexed = master_tracks.filter(|tracks| !tracks.is_empty());
    let mut header = Header::new(b"mhyp", 0x6c);
    header
        .u32(0x0c, if indexed.is_some() { 12 } else { 2 })
        .u32(0x10, u32_len(playlist.track_indices.len())?)
        .u8(0x14, u8::from(master_tracks.is_some()))
        .u64(0x1c, playlist.id)
        // One text record: the name.
        .u16(0x28, 1)
        // Sorted manually: in the order the entries are listed.
        .u32(0x2c, 1);
    let record = out.open_record(&header.0);
    out.extend(&text_record(TITLE, &playlist.name)?);
    out.extend(&column_preferences());
    if let Some(tracks) = indexed {
        let indexes = BrowseIndexes::new(tracks);
        for menu in Menu::ALL {
            let (positions, runs) = indexes.index(menu);
            out.extend(&browse_index(menu, &positions)?);
            out.extend(&jump_table(menu, &runs)?);
        }
    }
    for (position, &index) in playlist.track_indices.iter().enumerate() {
        let mut entry = Header::new(b"mhip", 0x4c);
        entry.u32(0x0c, 1).u32(0x18, track_id(index));
        let item = out.open_record(&entry.0);
        let mut place = Header::new(b"mhod", 0x18);
        place.u32(0x08, 44).u32(0x0c, PLAYLIST_ENTRY);
        let mut place = place.0;
        place.extend_from_slice(&u32_len(position)?.to_le_bytes());
        place.extend_from_slice(&[0u8; 16]);
        out.extend(&place);
        out.close_record(item)?;
    }
    out.close_record(record)
}

/// The column-preferences record every playlist carries, as libgpod writes
/// it: without one, iTunes shows the playlist empty.
fn column_preferences() -> Vec<u8> {
    const BODY: [u32; 42] = [
        0, 0, 0, 0, 0x01_0084, 0x05, 0x09, 0x03, 0x12_0001, 0, 0, 0, 0xc8_0002, 0, 0, 0, 0x3c_000d,
        0, 0, 0, 0x7d_0004, 0, 0, 0, 0x7d_0003, 0, 0, 0, 0x64_0008, 0, 0, 0, 0x64_0017, 0x01, 0, 0,
        0x50_0014, 0x01, 0, 0, 0x7d_0015, 0x01,
    ];
    let mut header = Header::new(b"mhod", 0x18);
    header.u32(0x08, 0x288).u32(0x0c, PLAYLIST_ENTRY);
    let mut record = header.0;
    for word in BODY {
        record.extend_from_slice(&word.to_le_bytes());
    }
    record.resize(0x288, 0);
    record
}

fn browse_index(menu: Menu, positions: &[u32]) -> Result<Vec<u8>, WriteError> {
    let count = u32_len(positions.len())?;
    let mut header = Header::new(b"mhod", 0x18);
    header
        .u32(
            0x08,
            count
                .checked_mul(4)
                .and_then(|bytes| bytes.checked_add(72))
                .ok_or(WriteError::TooLarge)?,
        )
        .u32(0x0c, BROWSE_INDEX);
    let mut record = header.0;
    record.extend_from_slice(&menu.code().to_le_bytes());
    record.extend_from_slice(&count.to_le_bytes());
    record.extend_from_slice(&[0u8; 40]);
    for position in positions {
        record.extend_from_slice(&position.to_le_bytes());
    }
    Ok(record)
}

fn jump_table(menu: Menu, runs: &[crate::sort::JumpRun]) -> Result<Vec<u8>, WriteError> {
    let count = u32_len(runs.len())?;
    let mut header = Header::new(b"mhod", 0x18);
    header
        .u32(
            0x08,
            count
                .checked_mul(12)
                .and_then(|bytes| bytes.checked_add(40))
                .ok_or(WriteError::TooLarge)?,
        )
        .u32(0x0c, JUMP_TABLE);
    let mut record = header.0;
    record.extend_from_slice(&menu.code().to_le_bytes());
    record.extend_from_slice(&count.to_le_bytes());
    record.extend_from_slice(&[0u8; 8]);
    for run in runs {
        record.extend_from_slice(&run.letter.to_le_bytes());
        record.extend_from_slice(&[0u8; 2]);
        record.extend_from_slice(&run.start.to_le_bytes());
        record.extend_from_slice(&run.count.to_le_bytes());
    }
    Ok(record)
}

fn write_empty_track_list(out: &mut Out, kind: u32) -> Result<(), WriteError> {
    let dataset = open_dataset(out, kind);
    out.extend(&list_header(b"mhlt", 0)?);
    out.close_record(dataset)
}

fn write_empty_playlist_list(out: &mut Out) -> Result<(), WriteError> {
    let dataset = open_dataset(out, 5);
    out.extend(&list_header(b"mhlp", 0)?);
    out.close_record(dataset)
}

// ---------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------

/// How many tracks the catalogue `bytes` lists, or `None` when they are not
/// a catalogue this reader can follow — which callers must treat as "not
/// empty". The input is untrusted: every offset is bounds-checked.
pub fn track_count(bytes: &[u8]) -> Option<u32> {
    let root = Record::at(bytes, 0, b"mhbd")?;
    let children = read_u32(bytes, 0x14)?;
    let mut offset = root.header_length;
    for _ in 0..children {
        let dataset = Record::at(bytes, offset, b"mhsd")?;
        if dataset.total_length < dataset.header_length
            || offset + dataset.total_length > root.total_length
        {
            return None;
        }
        if read_u32(bytes, offset + 0x0c)? == 1 {
            let list = offset + dataset.header_length;
            if bytes.get(list..list + 4)? != b"mhlt" {
                return None;
            }
            return read_u32(bytes, list + 8);
        }
        offset += dataset.total_length;
    }
    None
}

struct Record {
    header_length: usize,
    total_length: usize,
}

impl Record {
    fn at(bytes: &[u8], offset: usize, tag: &[u8; 4]) -> Option<Self> {
        if bytes.get(offset..offset.checked_add(4)?)? != tag {
            return None;
        }
        let header_length = usize::try_from(read_u32(bytes, offset + 4)?).ok()?;
        let total_length = usize::try_from(read_u32(bytes, offset + 8)?).ok()?;
        if header_length < 12
            || total_length < header_length
            || offset.checked_add(total_length)? > bytes.len()
        {
            return None;
        }
        Some(Self {
            header_length,
            total_length,
        })
    }
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let word = bytes.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes(word.try_into().ok()?))
}

#[cfg(test)]
mod tests;
