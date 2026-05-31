// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `export.pdb` writer — Pioneer's embedded DeviceSQL track database.
//!
//! The PDB is a paged store: a 4096-byte file header (page 0) listing
//! 20 table pointers, then per-table header pages and data pages. Each
//! data page packs variable-length rows into a heap growing up from
//! offset `0x28`, with a "row group" footer growing down from the page
//! end that records each row's offset. Multi-byte fields are
//! little-endian.
//!
//! This is a clean re-derivation of the maintainer's hardware-validated
//! reference exporter: the page layout, the (reverse-engineered) page
//! header formulas, and the constant "magic" field values are preserved
//! because they are what real XDJ/CDJ hardware and Rekordbox 5 accept,
//! but the code is restructured around an in-memory page buffer and a
//! single shared data-page builder rather than the reference's
//! seek-and-patch passes. The `columns` and the three `history` tables
//! are content-independent yet structurally load-bearing (the hardware
//! refuses a drive whose history tables are empty); their data pages are
//! carried over verbatim as validated reference blobs.

use std::collections::HashMap;
use std::ops::Range;

use crate::device_sql::encode;
use crate::key;
use crate::model::{PioneerArtwork, PioneerPlaylist, PioneerTrack};

const PAGE_SIZE: usize = 4096;
const HEAP_START: usize = 0x28;
const NUM_TABLES: u32 = 20;
/// Standard file length (pages 0–40) when no table needs overflow
/// pages. Empty-candidate pointers may reference pages 41–52, which the
/// hardware tolerates as out-of-file "grow here" hints.
const STANDARD_PAGES: usize = 41;
/// First page available for overflow allocation. Pages 41–52 are the
/// reserved empty-candidate zone and must stay zeroed.
const OVERFLOW_START: u32 = 53;

// Content-independent but structurally required reference pages.
const REF_COLUMNS: &[u8; PAGE_SIZE] = include_bytes!("reference/columns.bin");
const REF_HISTORY: &[u8; PAGE_SIZE] = include_bytes!("reference/history.bin");
const REF_HISTORY_ENTRIES: &[u8; PAGE_SIZE] = include_bytes!("reference/history_entries.bin");
const REF_HISTORY_PLAYLISTS: &[u8; PAGE_SIZE] = include_bytes!("reference/history_playlists.bin");

// Table type discriminants (DeviceSQL table ids).
const T_TRACKS: u32 = 0x00;
const T_GENRES: u32 = 0x01;
const T_ARTISTS: u32 = 0x02;
const T_ALBUMS: u32 = 0x03;
const T_LABELS: u32 = 0x04;
const T_KEYS: u32 = 0x05;
const T_COLORS: u32 = 0x06;
const T_PLAYLIST_TREE: u32 = 0x07;
const T_PLAYLIST_ENTRIES: u32 = 0x08;
const T_ARTWORK: u32 = 0x0D;
const T_COLUMNS: u32 = 0x10;
const T_HISTORY_PLAYLISTS: u32 = 0x11;
const T_HISTORY_ENTRIES: u32 = 0x12;
const T_HISTORY: u32 = 0x13;

/// Error from building the PDB. The only failure mode is a single row
/// too large to fit on a page, which would indicate a pathological
/// metadata string; everything else is infallible buffer assembly.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PdbError {
    RowTooLarge { table: &'static str, bytes: usize },
}

impl std::fmt::Display for PdbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RowTooLarge { table, bytes } => {
                write!(f, "{table} row of {bytes} bytes does not fit in a PDB page")
            }
        }
    }
}

impl std::error::Error for PdbError {}

/// Build the complete `export.pdb` byte image for the given tracks,
/// playlists, and cover-art rows. `analyze_date` is the `YYYY-MM-DD`
/// string stamped into each track's analyze-date field. `artworks` are
/// the unique covers (id↔path) a track's `artwork_id` references; pass
/// an empty slice to leave the artwork table empty (header only).
pub fn build(
    tracks: &[PioneerTrack],
    playlists: &[PioneerPlaylist],
    artworks: &[PioneerArtwork],
    analyze_date: &str,
) -> Result<Vec<u8>, PdbError> {
    let entities = Entities::build(tracks);

    // Serialize every row up front; sizes drive page chunking.
    let mut track_rows = Vec::with_capacity(tracks.len());
    for (index, track) in tracks.iter().enumerate() {
        track_rows.push(track_row(track, &entities, index, analyze_date));
    }
    let mut artist_rows: Vec<Vec<u8>> = entities
        .artists
        .iter()
        .enumerate()
        .map(|(i, name)| artist_row(name, (i + 1) as u32))
        .collect();
    let mut album_rows: Vec<Vec<u8>> = entities
        .albums
        .iter()
        .enumerate()
        .map(|(i, name)| album_row(name, (i + 1) as u32))
        .collect();
    let mut genre_rows: Vec<Vec<u8>> = entities
        .genres
        .iter()
        .enumerate()
        .map(|(i, name)| genre_row(name, (i + 1) as u32))
        .collect();
    let mut tree_rows: Vec<Vec<u8>> = playlists
        .iter()
        .enumerate()
        .map(|(i, p)| playlist_tree_row(&p.name, i))
        .collect();
    let mut entry_rows = playlist_entry_rows(playlists);
    let artwork_rows: Vec<Vec<u8>> = artworks.iter().map(artwork_row).collect();

    let track_chunks = chunk_rows("tracks", &track_rows)?;
    let artist_chunks = chunk_rows("artists", &artist_rows)?;
    let album_chunks = chunk_rows("albums", &album_rows)?;
    let genre_chunks = chunk_rows("genres", &genre_rows)?;
    let tree_chunks = chunk_rows("playlist tree", &tree_rows)?;
    let entry_chunks = chunk_rows("playlist entries", &entry_rows)?;

    // Allocate page numbers: fixed first pages, overflow from page 53.
    let mut next = OVERFLOW_START;
    let tracks_alloc = assign(2, track_chunks.len(), 51, &mut next);
    let genres_alloc = assign(4, genre_chunks.len(), 48, &mut next);
    let artists_alloc = assign(6, artist_chunks.len(), 47, &mut next);
    let albums_alloc = assign(8, album_chunks.len(), 49, &mut next);
    let tree_alloc = assign(16, tree_chunks.len(), 46, &mut next);
    let entries_alloc = assign(18, entry_chunks.len(), 52, &mut next);
    // Artwork: header at the fixed page 27, first data page at the fixed
    // page 28, with any further pages and the empty-candidate drawn from
    // the overflow counter — page 28 has no reserved out-of-file slot in
    // the 41–52 zone (those are all taken), so its empty-candidate must
    // be a real page, which also forces the file to grow past page 40.
    let artwork_chunks = if artworks.is_empty() {
        Vec::new()
    } else {
        chunk_rows("artwork", &artwork_rows)?
    };
    let artwork_alloc = if artworks.is_empty() {
        None
    } else {
        Some(assign_artwork(28, artwork_chunks.len(), &mut next))
    };

    let num_pages = if next > OVERFLOW_START {
        next as usize
    } else {
        STANDARD_PAGES
    };
    let mut file = FileBuf::new(num_pages);

    // --- Tracks (type 0x00) ---
    file.set_header(
        1,
        T_TRACKS,
        Some(tracks_alloc.first()),
        Some(tracks_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_TRACKS,
        &tracks_alloc,
        &track_chunks,
        &mut track_rows,
        TableCfg {
            base_seq: 10,
            seq_step: 5,
            plus_one_at_11: true,
            index_shift: true,
        },
    )?;

    // --- Genres (type 0x01) ---
    file.set_header(
        3,
        T_GENRES,
        Some(genres_alloc.first()),
        Some(genres_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_GENRES,
        &genres_alloc,
        &genre_chunks,
        &mut genre_rows,
        TableCfg {
            base_seq: 8,
            seq_step: 5,
            plus_one_at_11: true,
            index_shift: false,
        },
    )?;

    // --- Artists (type 0x02) ---
    file.set_header(
        5,
        T_ARTISTS,
        Some(artists_alloc.first()),
        Some(artists_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_ARTISTS,
        &artists_alloc,
        &artist_chunks,
        &mut artist_rows,
        TableCfg {
            base_seq: 7,
            seq_step: 5,
            plus_one_at_11: true,
            index_shift: true,
        },
    )?;

    // --- Albums (type 0x03) ---
    file.set_header(
        7,
        T_ALBUMS,
        Some(albums_alloc.first()),
        Some(albums_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_ALBUMS,
        &albums_alloc,
        &album_chunks,
        &mut album_rows,
        TableCfg {
            base_seq: 9,
            seq_step: 5,
            plus_one_at_11: true,
            index_shift: true,
        },
    )?;

    // --- Labels (type 0x04): empty, header only ---
    file.set_header(9, T_LABELS, Some(10), None);

    // --- Keys (type 0x05) ---
    file.set_header(11, T_KEYS, Some(12), Some(12));
    write_keys_page(&mut file, tracks.len());

    // --- Colors (type 0x06) ---
    file.set_header(13, T_COLORS, Some(14), Some(14));
    write_colors_page(&mut file);

    // --- Playlist tree (type 0x07) ---
    file.set_header(
        15,
        T_PLAYLIST_TREE,
        Some(tree_alloc.first()),
        Some(tree_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_PLAYLIST_TREE,
        &tree_alloc,
        &tree_chunks,
        &mut tree_rows,
        TableCfg {
            base_seq: 6,
            seq_step: 1,
            plus_one_at_11: false,
            index_shift: false,
        },
    )?;

    // --- Playlist entries (type 0x08) ---
    file.set_header(
        17,
        T_PLAYLIST_ENTRIES,
        Some(entries_alloc.first()),
        Some(entries_alloc.first()),
    );
    write_rows_table(
        &mut file,
        T_PLAYLIST_ENTRIES,
        &entries_alloc,
        &entry_chunks,
        &mut entry_rows,
        TableCfg {
            base_seq: 11,
            seq_step: 5,
            plus_one_at_11: true,
            index_shift: false,
        },
    )?;

    // --- Empty placeholder tables (header only) ---
    file.set_header(19, 0x09, Some(20), None);
    file.set_header(21, 0x0A, Some(22), None);
    file.set_header(23, 0x0B, Some(24), None);
    file.set_header(25, 0x0C, Some(26), None);
    file.set_header(29, 0x0E, Some(30), None);
    file.set_header(31, 0x0F, Some(32), None);

    // --- Artwork (type 0x0D) ---
    // With covers present the header at page 27 points at the data page;
    // without, it stays the header-only placeholder (next/empty = 28).
    match &artwork_alloc {
        Some(alloc) => {
            file.set_header(27, T_ARTWORK, Some(alloc.first()), Some(alloc.first()));
            write_artwork_pages(&mut file, alloc, &artwork_chunks, &artwork_rows)?;
        }
        None => file.set_header(27, T_ARTWORK, Some(28), None),
    }

    // --- Columns (type 0x10): reference data page ---
    file.set_header(33, T_COLUMNS, Some(34), Some(34));
    file.set_page(34, REF_COLUMNS);

    // --- History tables (types 0x11–0x13): reference data pages ---
    file.set_header(35, T_HISTORY_PLAYLISTS, Some(36), Some(36));
    file.set_page(36, REF_HISTORY_PLAYLISTS);
    file.set_header(37, T_HISTORY_ENTRIES, Some(38), Some(38));
    file.set_page(38, REF_HISTORY_ENTRIES);
    write_history_header(&mut file, tracks.len());
    let history_sequence = 10u32 + (tracks.len().saturating_sub(1) as u32) * 5;
    file.set_page(40, REF_HISTORY);
    file.patch_u32(40, 0x10, history_sequence); // align the DB sequence stamp

    // --- File header (page 0) and table pointers ---
    write_file_header(
        &mut file,
        &entities,
        tracks,
        playlists,
        num_pages,
        &TablePointers {
            tracks_last: tracks_alloc.last,
            tracks_empty: tracks_alloc.empty,
            genres_last: genres_alloc.last,
            genres_empty: genres_alloc.empty,
            artists_last: artists_alloc.last,
            artists_empty: artists_alloc.empty,
            albums_last: albums_alloc.last,
            albums_empty: albums_alloc.empty,
            tree_last: tree_alloc.last,
            tree_empty: tree_alloc.empty,
            entries_last: entries_alloc.last,
            entries_empty: entries_alloc.empty,
            artwork_last: artwork_alloc.as_ref().map_or(27, |a| a.last),
            artwork_empty: artwork_alloc.as_ref().map_or(28, |a| a.empty),
        },
    );

    Ok(file.into_bytes())
}

// ---------------------------------------------------------------------
// Page buffer
// ---------------------------------------------------------------------

struct FileBuf {
    data: Vec<u8>,
}

impl FileBuf {
    fn new(num_pages: usize) -> Self {
        Self {
            data: vec![0u8; num_pages * PAGE_SIZE],
        }
    }

    fn page_mut(&mut self, index: u32) -> &mut [u8] {
        let start = index as usize * PAGE_SIZE;
        &mut self.data[start..start + PAGE_SIZE]
    }

    fn set_page(&mut self, index: u32, bytes: &[u8; PAGE_SIZE]) {
        self.page_mut(index).copy_from_slice(bytes);
    }

    fn patch_u32(&mut self, index: u32, offset: usize, value: u32) {
        let start = index as usize * PAGE_SIZE + offset;
        self.data[start..start + 4].copy_from_slice(&value.to_le_bytes());
    }

    /// Write a table's header page.
    fn set_header(
        &mut self,
        page_index: u32,
        table_type: u32,
        next_page: Option<u32>,
        first_data: Option<u32>,
    ) {
        let next = next_page.unwrap_or(0);
        let header = PageHeader {
            table_type,
            page_index,
            next_page: next,
            num_rows_small: 0,
            num_rows_large: 0x1FFF,
            unknown3: 0,
            unknown4: 0,
            page_flags: 0x64,
            unknown1: 1,
            unknown2: 0,
            unknown5: 0x1FFF,
            unknown6: 0x03EC,
            unknown7: 0,
        };
        let page = self.page_mut(page_index);
        write_page_header(page, &header, 0, 0);
        write_header_content(page, page_index, first_data, false);
    }

    fn into_bytes(self) -> Vec<u8> {
        self.data
    }
}

// ---------------------------------------------------------------------
// Page allocation
// ---------------------------------------------------------------------

struct Alloc {
    data_pages: Vec<u32>,
    empty: u32,
    last: u32,
}

impl Alloc {
    fn first(&self) -> u32 {
        self.data_pages[0]
    }
}

/// Assign data pages to a table: the fixed `first` page plus overflow
/// pages drawn from the monotonic `next` counter. A table that fits one
/// page keeps its reserved empty-candidate; an overflowing table gets a
/// fresh trailing empty page so no two tables collide.
fn assign(first: u32, num_chunks: usize, reserved_empty: u32, next: &mut u32) -> Alloc {
    let mut data_pages = vec![first];
    for _ in 1..num_chunks {
        data_pages.push(*next);
        *next += 1;
    }
    let empty = if num_chunks > 1 {
        let e = *next;
        *next += 1;
        e
    } else {
        reserved_empty
    };
    let last = *data_pages.last().unwrap_or(&first);
    Alloc {
        data_pages,
        empty,
        last,
    }
}

/// Allocate the artwork table's pages. Unlike [`assign`], the
/// empty-candidate is always drawn from the overflow counter (never a
/// reserved 41–52 slot): the fixed first data page 28 has no reserved
/// out-of-file companion, so its empty-candidate must be a real page,
/// which in turn extends the file past the standard 41 pages.
fn assign_artwork(first: u32, num_chunks: usize, next: &mut u32) -> Alloc {
    let mut data_pages = vec![first];
    for _ in 1..num_chunks {
        data_pages.push(*next);
        *next += 1;
    }
    let empty = *next;
    *next += 1;
    let last = *data_pages.last().unwrap_or(&first);
    Alloc {
        data_pages,
        empty,
        last,
    }
}

/// Greedily pack rows into pages by exact size, accounting for the
/// row-group footer that grows with row count. Always yields at least
/// one (possibly empty) chunk so every table writes a data page.
fn chunk_rows(table: &'static str, rows: &[Vec<u8>]) -> Result<Vec<Range<usize>>, PdbError> {
    if rows.is_empty() {
        // A single empty page so the table still has a data page.
        return Ok(std::iter::once(0..0).collect());
    }
    let mut chunks = Vec::new();
    let mut start = 0;
    let mut heap = 0usize;
    let mut count = 0usize;
    for (i, row) in rows.iter().enumerate() {
        let aligned = align4(row.len());
        let footer = row_group_bytes(count + 1);
        let single = HEAP_START + aligned + row_group_bytes(1);
        if single > PAGE_SIZE {
            return Err(PdbError::RowTooLarge {
                table,
                bytes: row.len(),
            });
        }
        if count > 0 && HEAP_START + heap + aligned + footer > PAGE_SIZE {
            chunks.push(start..i);
            start = i;
            heap = 0;
            count = 0;
        }
        heap += aligned;
        count += 1;
    }
    chunks.push(start..rows.len());
    Ok(chunks)
}

// ---------------------------------------------------------------------
// Data-page assembly
// ---------------------------------------------------------------------

struct PageHeader {
    table_type: u32,
    page_index: u32,
    next_page: u32,
    num_rows_small: u8,
    num_rows_large: u16,
    unknown3: u8,
    unknown4: u8,
    page_flags: u8,
    unknown1: u32,
    unknown2: u32,
    unknown5: u16,
    unknown6: u16,
    unknown7: u16,
}

fn write_page_header(page: &mut [u8], h: &PageHeader, free_size: u16, used_size: u16) {
    page[0x04..0x08].copy_from_slice(&h.page_index.to_le_bytes());
    page[0x08..0x0C].copy_from_slice(&h.table_type.to_le_bytes());
    page[0x0C..0x10].copy_from_slice(&h.next_page.to_le_bytes());
    page[0x10..0x14].copy_from_slice(&h.unknown1.to_le_bytes());
    page[0x14..0x18].copy_from_slice(&h.unknown2.to_le_bytes());
    page[0x18] = h.num_rows_small;
    page[0x19] = h.unknown3;
    page[0x1A] = h.unknown4;
    page[0x1B] = h.page_flags;
    page[0x1C..0x1E].copy_from_slice(&free_size.to_le_bytes());
    page[0x1E..0x20].copy_from_slice(&used_size.to_le_bytes());
    page[0x20..0x22].copy_from_slice(&h.unknown5.to_le_bytes());
    page[0x22..0x24].copy_from_slice(&h.num_rows_large.to_le_bytes());
    page[0x24..0x26].copy_from_slice(&h.unknown6.to_le_bytes());
    page[0x26..0x28].copy_from_slice(&h.unknown7.to_le_bytes());
}

/// Fill a header page's pointer pattern (offset `0x28` onward).
fn write_header_content(page: &mut [u8], header_page: u32, first_data: Option<u32>, history: bool) {
    let mut w = HEAP_START;
    let put = |page: &mut [u8], w: &mut usize, value: u32| {
        page[*w..*w + 4].copy_from_slice(&value.to_le_bytes());
        *w += 4;
    };
    put(page, &mut w, header_page);
    put(page, &mut w, first_data.unwrap_or(0x03FF_FFFF));
    put(page, &mut w, 0x03FF_FFFF);
    put(page, &mut w, 0);
    if history {
        page[w..w + 4].copy_from_slice(&[0x01, 0x00, 0xFF, 0x1F]);
        w += 4;
        page[w..w + 4].copy_from_slice(&[0x40, 0x01, 0x00, 0x00]);
        w += 4;
    } else {
        put(page, &mut w, 0x1FFF_0000);
    }
    let pattern = [0xF8, 0xFF, 0xFF, 0x1F];
    while w < PAGE_SIZE - 20 {
        page[w..w + 4].copy_from_slice(&pattern);
        w += 4;
    }
    // The final 20 bytes stay zero.
}

#[derive(Clone, Copy)]
enum RowGroupUnknown {
    HighBit,
    Identity,
}

/// Lay rows into a page heap, write the page header, and append the
/// reverse-ordered row-group footer.
fn build_data_page(
    table: &'static str,
    header: &mut PageHeader,
    rows: &[Vec<u8>],
    rg: RowGroupUnknown,
    page: &mut [u8],
) -> Result<(), PdbError> {
    let mut heap = Vec::new();
    let mut offsets = Vec::with_capacity(rows.len());
    for row in rows {
        offsets.push(heap.len() as u16);
        heap.extend_from_slice(row);
        while heap.len() % 4 != 0 {
            heap.push(0);
        }
    }
    let footer = row_group_bytes(rows.len());
    if HEAP_START + heap.len() + footer > PAGE_SIZE {
        return Err(PdbError::RowTooLarge {
            table,
            bytes: heap.len(),
        });
    }
    let free_size = (PAGE_SIZE - HEAP_START - heap.len() - footer) as u16;
    let used_size = heap.len() as u16;
    write_page_header(page, header, free_size, used_size);
    page[HEAP_START..HEAP_START + heap.len()].copy_from_slice(&heap);
    write_row_groups(page, rows.len(), &offsets, rg);
    Ok(())
}

fn write_row_groups(page: &mut [u8], num_rows: usize, offsets: &[u16], rg: RowGroupUnknown) {
    let footer = row_group_bytes(num_rows);
    let mut pos = PAGE_SIZE - footer;
    let num_groups = num_rows.div_ceil(16);
    for group in (0..num_groups).rev() {
        let start = group * 16;
        let end = (start + 16).min(num_rows);
        let k = end - start;
        let mut flags: u16 = 0;
        for slot in 0..k {
            flags |= 1 << slot;
        }
        for slot in (0..k).rev() {
            page[pos..pos + 2].copy_from_slice(&offsets[start + slot].to_le_bytes());
            pos += 2;
        }
        page[pos..pos + 2].copy_from_slice(&flags.to_le_bytes());
        pos += 2;
        let unknown = match rg {
            RowGroupUnknown::Identity => flags,
            RowGroupUnknown::HighBit => high_bit(flags),
        };
        page[pos..pos + 2].copy_from_slice(&unknown.to_le_bytes());
        pos += 2;
    }
}

fn high_bit(flags: u16) -> u16 {
    if flags == 0 || flags == 0xFFFF {
        0
    } else {
        let idx = 15u16.saturating_sub(flags.leading_zeros() as u16);
        1u16 << idx
    }
}

fn row_group_bytes(num_rows: usize) -> usize {
    let full = num_rows / 16;
    let partial = num_rows % 16;
    full * 36 + if partial > 0 { partial * 2 + 4 } else { 0 }
}

fn align4(n: usize) -> usize {
    (n + 3) & !3
}

struct TableCfg {
    base_seq: u32,
    seq_step: u32,
    plus_one_at_11: bool,
    index_shift: bool,
}

/// Write all data pages of a uniformly-formatted table (tracks,
/// artists, albums, genres, playlist tree/entries).
fn write_rows_table(
    file: &mut FileBuf,
    table_type: u32,
    alloc: &Alloc,
    chunks: &[Range<usize>],
    rows: &mut [Vec<u8>],
    cfg: TableCfg,
) -> Result<(), PdbError> {
    let table = table_name(table_type);
    let mut cumulative = cfg.base_seq;
    for (chunk_index, range) in chunks.iter().enumerate() {
        let n = range.len();
        let is_last = chunk_index + 1 == chunks.len();
        let page_num = alloc.data_pages[chunk_index];
        let next_page = if is_last {
            alloc.empty
        } else {
            alloc.data_pages[chunk_index + 1]
        };

        let base = if chunk_index == 0 {
            cumulative + (n.saturating_sub(1) as u32) * cfg.seq_step
        } else {
            cumulative + (n as u32) * cfg.seq_step
        };
        let sequence = if cfg.plus_one_at_11 && n >= 11 && is_last {
            base + 1
        } else {
            base
        };
        cumulative = sequence;

        if cfg.index_shift {
            for (local, row_index) in range.clone().enumerate() {
                let shift = (local as u16).wrapping_mul(0x20);
                rows[row_index][2..4].copy_from_slice(&shift.to_le_bytes());
            }
        }

        let mut header = PageHeader {
            table_type,
            page_index: page_num,
            next_page,
            num_rows_small: n.min(255) as u8,
            num_rows_large: if n == 0 { 0 } else { (n - 1) as u16 },
            unknown3: ((n % 8) * 0x20) as u8,
            unknown4: if n >= 10 { n.div_ceil(16) as u8 } else { 0 },
            page_flags: 0x24,
            unknown1: sequence,
            unknown2: 0,
            unknown5: 0x0001,
            unknown6: 0,
            unknown7: 0,
        };
        build_data_page(
            table,
            &mut header,
            &rows[range.clone()],
            RowGroupUnknown::HighBit,
            file.page_mut(page_num),
        )?;
    }
    Ok(())
}

fn write_keys_page(file: &mut FileBuf, track_count: usize) {
    let (unknown1, unknown3) = if track_count <= 1 {
        (0x0Au32, 0x20u8)
    } else {
        (0x1Bu32, 0x60u8)
    };
    let rows: Vec<Vec<u8>> = key::KEY_TABLE
        .iter()
        .map(|(id, name)| {
            let mut row = Vec::new();
            row.extend_from_slice(&id.to_le_bytes());
            row.extend_from_slice(&id.to_le_bytes());
            row.extend_from_slice(&encode(name));
            row
        })
        .collect();
    let n = rows.len();
    let mut header = PageHeader {
        table_type: T_KEYS,
        page_index: 12,
        next_page: 50,
        num_rows_small: n as u8,
        num_rows_large: (n - 1) as u16,
        unknown3,
        unknown4: 0,
        page_flags: 0x24,
        unknown1,
        unknown2: 0,
        unknown5: 0x0001,
        unknown6: 0,
        unknown7: 0,
    };
    // 24 fixed rows always fit one page.
    let _ = build_data_page(
        "keys",
        &mut header,
        &rows,
        RowGroupUnknown::HighBit,
        file.page_mut(12),
    );
}

fn write_colors_page(file: &mut FileBuf) {
    let colors = [
        (1u8, "Pink"),
        (2, "Red"),
        (3, "Orange"),
        (4, "Yellow"),
        (5, "Green"),
        (6, "Aqua"),
        (7, "Blue"),
        (8, "Purple"),
    ];
    let rows: Vec<Vec<u8>> = colors
        .iter()
        .map(|(index, name)| {
            let mut row = Vec::new();
            row.extend_from_slice(&0u32.to_le_bytes());
            row.push(0);
            row.push(*index);
            row.extend_from_slice(&0u16.to_le_bytes());
            row.extend_from_slice(&encode(name));
            row
        })
        .collect();
    let mut header = PageHeader {
        table_type: T_COLORS,
        page_index: 14,
        next_page: 42,
        num_rows_small: rows.len() as u8,
        num_rows_large: 0,
        unknown3: 0,
        unknown4: 1,
        page_flags: 0x24,
        unknown1: 0x0002,
        unknown2: 0,
        unknown5: 0x0008,
        unknown6: 0,
        unknown7: 0,
    };
    let _ = build_data_page(
        "colors",
        &mut header,
        &rows,
        RowGroupUnknown::Identity,
        file.page_mut(14),
    );
}

/// Write the artwork table's data page(s). Faithful to the reference
/// exporter, which differs from the generic [`write_rows_table`] in two
/// ways for this table: `unknown4` is always zero, and the per-page
/// sequence uses the plain base-8 / step-5 accumulation below (no
/// "+1 at 11 rows" adjustment).
fn write_artwork_pages(
    file: &mut FileBuf,
    alloc: &Alloc,
    chunks: &[Range<usize>],
    rows: &[Vec<u8>],
) -> Result<(), PdbError> {
    let mut cumulative = 8u32;
    for (chunk_index, range) in chunks.iter().enumerate() {
        let n = range.len();
        let is_last = chunk_index + 1 == chunks.len();
        let page_num = alloc.data_pages[chunk_index];
        let next_page = if is_last {
            alloc.empty
        } else {
            alloc.data_pages[chunk_index + 1]
        };

        let sequence = cumulative + (n.saturating_sub(1) as u32) * 5;
        cumulative = sequence + 5;

        let mut header = PageHeader {
            table_type: T_ARTWORK,
            page_index: page_num,
            next_page,
            num_rows_small: n.min(255) as u8,
            num_rows_large: if n == 0 { 0 } else { (n - 1) as u16 },
            unknown3: ((n % 8) * 0x20) as u8,
            unknown4: 0,
            page_flags: 0x24,
            unknown1: sequence,
            unknown2: 0,
            unknown5: 0x0001,
            unknown6: 0,
            unknown7: 0,
        };
        build_data_page(
            "artwork",
            &mut header,
            &rows[range.clone()],
            RowGroupUnknown::HighBit,
            file.page_mut(page_num),
        )?;
    }
    Ok(())
}

fn write_history_header(file: &mut FileBuf, track_count: usize) {
    let (unk5, num_rows_large) = if track_count <= 1 {
        (0x0001u16, 0x0000u16)
    } else {
        (0x1FFFu16, 0x1FFFu16)
    };
    let sequence = 10u32 + (track_count.saturating_sub(1) as u32) * 5;
    let header = PageHeader {
        table_type: T_HISTORY,
        page_index: 39,
        next_page: 40,
        num_rows_small: 0,
        num_rows_large,
        unknown3: 0,
        unknown4: 0,
        page_flags: 0x64,
        unknown1: sequence,
        unknown2: 0,
        unknown5: unk5,
        unknown6: 0x03EC,
        unknown7: 0x0001,
    };
    let page = file.page_mut(39);
    write_page_header(page, &header, 0, 0);
    write_header_content(page, 39, Some(40), true);
}

fn table_name(table_type: u32) -> &'static str {
    match table_type {
        T_TRACKS => "tracks",
        T_GENRES => "genres",
        T_ARTISTS => "artists",
        T_ALBUMS => "albums",
        T_PLAYLIST_TREE => "playlist tree",
        T_PLAYLIST_ENTRIES => "playlist entries",
        _ => "table",
    }
}

// ---------------------------------------------------------------------
// Row serializers
// ---------------------------------------------------------------------

struct Entities {
    artists: Vec<String>,
    albums: Vec<String>,
    genres: Vec<String>,
    artist_id: HashMap<String, u32>,
    album_id: HashMap<String, u32>,
    genre_id: HashMap<String, u32>,
}

impl Entities {
    fn build(tracks: &[PioneerTrack]) -> Self {
        let mut e = Self {
            artists: Vec::new(),
            albums: Vec::new(),
            genres: Vec::new(),
            artist_id: HashMap::new(),
            album_id: HashMap::new(),
            genre_id: HashMap::new(),
        };
        for track in tracks {
            intern(&mut e.artists, &mut e.artist_id, &track.artist);
            intern(&mut e.albums, &mut e.album_id, &track.album);
            if let Some(genre) = &track.genre {
                if !genre.is_empty() {
                    intern(&mut e.genres, &mut e.genre_id, genre);
                }
            }
        }
        e
    }
}

fn intern(list: &mut Vec<String>, map: &mut HashMap<String, u32>, value: &str) {
    if !map.contains_key(value) {
        let id = (list.len() + 1) as u32;
        list.push(value.to_owned());
        map.insert(value.to_owned(), id);
    }
}

fn genre_row(name: &str, id: u32) -> Vec<u8> {
    let mut row = Vec::new();
    row.extend_from_slice(&id.to_le_bytes());
    row.extend_from_slice(&encode(name));
    row
}

/// Minimum artwork-row stride (id + DeviceSQL path, zero-padded). The
/// reference packs rows at this fixed size; matching it keeps the
/// row-group offsets identical for the short paths it ever produces. A
/// pathologically long path simply yields a longer, 4-byte-aligned row,
/// which the offset-addressed page layout still parses correctly.
const ARTWORK_ROW_SIZE: usize = 36;

fn artwork_row(artwork: &PioneerArtwork) -> Vec<u8> {
    let mut row = Vec::with_capacity(ARTWORK_ROW_SIZE);
    row.extend_from_slice(&artwork.id.to_le_bytes());
    row.extend_from_slice(&encode(&artwork.path));
    while row.len() < ARTWORK_ROW_SIZE {
        row.push(0);
    }
    row
}

fn artist_row(name: &str, id: u32) -> Vec<u8> {
    let mut row = Vec::new();
    row.extend_from_slice(&0x60u16.to_le_bytes());
    row.extend_from_slice(&0u16.to_le_bytes()); // index_shift, patched per page
    row.extend_from_slice(&id.to_le_bytes());
    row.push(0x03);
    row.push(10); // name offset
    row.extend_from_slice(&encode(name));
    while row.len() < 28 {
        row.push(0);
    }
    row
}

fn album_row(name: &str, id: u32) -> Vec<u8> {
    let mut row = Vec::new();
    row.extend_from_slice(&0x80u16.to_le_bytes());
    row.extend_from_slice(&0u16.to_le_bytes()); // index_shift, patched per page
    row.extend_from_slice(&0u32.to_le_bytes()); // unknown
    row.extend_from_slice(&0u32.to_le_bytes()); // artist_id (unused)
    row.extend_from_slice(&id.to_le_bytes());
    row.extend_from_slice(&0u32.to_le_bytes()); // unknown
    row.push(0x03);
    row.push(22); // name offset
    row.extend_from_slice(&encode(name));
    while row.len() < 40 {
        row.push(0);
    }
    row
}

fn playlist_tree_row(name: &str, index: usize) -> Vec<u8> {
    let mut row = Vec::new();
    row.extend_from_slice(&0u32.to_le_bytes()); // parent_id (root)
    row.extend_from_slice(&0u32.to_le_bytes()); // unknown
    row.extend_from_slice(&(index as u32).to_le_bytes()); // sort order
    row.extend_from_slice(&((index + 1) as u32).to_le_bytes()); // playlist id
    row.extend_from_slice(&0u32.to_le_bytes()); // node_is_folder = 0
    row.extend_from_slice(&encode(name)); // inline name
    row
}

fn playlist_entry_rows(playlists: &[PioneerPlaylist]) -> Vec<Vec<u8>> {
    let mut rows = Vec::new();
    for (playlist_index, playlist) in playlists.iter().enumerate() {
        let playlist_id = (playlist_index + 1) as u32;
        for (position, &track_index) in playlist.entries.iter().enumerate() {
            let mut row = Vec::new();
            row.extend_from_slice(&((position + 1) as u32).to_le_bytes());
            row.extend_from_slice(&((track_index + 1) as u32).to_le_bytes());
            row.extend_from_slice(&playlist_id.to_le_bytes());
            rows.push(row);
        }
    }
    rows
}

fn track_row(
    track: &PioneerTrack,
    entities: &Entities,
    index: usize,
    analyze_date: &str,
) -> Vec<u8> {
    let track_id = (index + 1) as u32;
    let artist_id = entities.artist_id.get(&track.artist).copied().unwrap_or(0);
    let album_id = entities.album_id.get(&track.album).copied().unwrap_or(0);
    let genre_id = track
        .genre
        .as_ref()
        .and_then(|g| entities.genre_id.get(g))
        .copied()
        .unwrap_or(1);
    let key_id = track.key.map(key::rekordbox_id).unwrap_or(1);
    let sample_rate = if track.sample_rate_hz == 0 {
        44_100
    } else {
        track.sample_rate_hz
    };
    let tempo = track.bpm.map(|b| (b * 100.0).round() as u32).unwrap_or(0);

    let mut row = Vec::with_capacity(0x88);
    let u16 = |row: &mut Vec<u8>, v: u16| row.extend_from_slice(&v.to_le_bytes());
    let u32_ = |row: &mut Vec<u8>, v: u32| row.extend_from_slice(&v.to_le_bytes());

    u16(&mut row, 0x0024); // subtype
    u16(&mut row, 0); // index_shift, patched per page
    // bitmask: the validated reference writes 0x0700 here (the documented
    // 0x1FF803DE value describes Rekordbox's own export but is not needed
    // for hardware acceptance).
    u32_(&mut row, 0x0700);
    u32_(&mut row, sample_rate);
    u32_(&mut row, 0); // composer_id
    u32_(&mut row, track.file_size.min(u32::MAX as u64) as u32);
    u32_(&mut row, (track_id + 5) | 0x100); // u2 (waveform-ready flag in bit 8)
    u16(&mut row, 0xE5B6); // u3
    u16(&mut row, 0x6A76); // u4
    u32_(&mut row, track.artwork_id); // artwork_id (0 = none)
    u32_(&mut row, key_id);
    u32_(&mut row, 0); // original_artist_id
    u32_(&mut row, 0); // label_id
    u32_(&mut row, 0); // remixer_id
    u32_(&mut row, track.bitrate_kbps.unwrap_or(0));
    u32_(&mut row, track.track_number.unwrap_or(0));
    u32_(&mut row, tempo);
    u32_(&mut row, genre_id);
    u32_(&mut row, album_id);
    u32_(&mut row, artist_id);
    u32_(&mut row, track_id);
    u16(&mut row, 0); // disc_number
    u16(&mut row, 0); // play_count
    u16(
        &mut row,
        track.year.unwrap_or(0).min(u16::MAX as u32) as u16,
    );
    u16(
        &mut row,
        if track.bit_depth == 0 {
            16
        } else {
            track.bit_depth
        },
    );
    u16(&mut row, track.duration_secs.min(u16::MAX as u32) as u16);
    u16(&mut row, 0x0029); // u5
    row.push(0); // color_id
    row.push(track.rating.min(5)); // rating
    u16(&mut row, track.file_type as u16);
    u16(&mut row, 0x0003); // u7
    debug_assert_eq!(row.len(), 0x5E);

    // 21 DeviceSQL string offsets + inline string data starting at 0x88.
    const STRING_DATA_START: usize = 0x5E + 21 * 2;
    let mut string_data = Vec::new();
    let mut offsets = [0u16; 21];
    let mut add = |index: usize, bytes: &[u8]| {
        offsets[index] = (STRING_DATA_START + string_data.len()) as u16;
        string_data.extend_from_slice(bytes);
    };
    let filename = track
        .device_audio_path
        .rsplit('/')
        .next()
        .unwrap_or(&track.device_audio_path);
    let date_added = track.date_added.as_deref().unwrap_or("");

    add(0, &[0x03]); // isrc
    add(1, &[0x03]); // lyricist
    add(2, &encode("3")); // unknown2
    add(3, &[0x05, 0x01]); // unknown3 flag
    add(4, &[0x03]);
    add(5, &[0x03]); // message
    add(6, &[0x03]); // publish info
    add(7, &encode("ON")); // autoload_hotcues
    add(8, &[0x03]);
    add(9, &[0x03]);
    add(10, &encode(date_added)); // date_added
    add(11, &[0x03]); // release_date
    add(12, &[0x03]); // mix_name
    add(13, &[0x03]);
    add(14, &encode(&track.device_anlz_path)); // analyze_path
    add(15, &encode(analyze_date)); // analyze_date
    add(16, &[0x03]); // comment
    add(17, &encode(&track.title)); // title
    add(18, &[0x03]);
    add(19, &encode(filename)); // filename
    add(20, &encode(&track.device_audio_path)); // file_path

    for offset in offsets {
        row.extend_from_slice(&offset.to_le_bytes());
    }
    debug_assert_eq!(row.len(), STRING_DATA_START);
    row.extend_from_slice(&string_data);
    row
}

// ---------------------------------------------------------------------
// File header
// ---------------------------------------------------------------------

struct TablePointers {
    tracks_last: u32,
    tracks_empty: u32,
    genres_last: u32,
    genres_empty: u32,
    artists_last: u32,
    artists_empty: u32,
    albums_last: u32,
    albums_empty: u32,
    tree_last: u32,
    tree_empty: u32,
    entries_last: u32,
    entries_empty: u32,
    artwork_last: u32,
    artwork_empty: u32,
}

fn write_file_header(
    file: &mut FileBuf,
    entities: &Entities,
    tracks: &[PioneerTrack],
    playlists: &[PioneerPlaylist],
    num_pages: usize,
    p: &TablePointers,
) {
    let next_unused = (num_pages as u32).max(OVERFLOW_START);
    let sequence = if tracks.len() <= 1 {
        14
    } else {
        let total = tracks.len()
            + entities.artists.len()
            + entities.albums.len()
            + entities.genres.len()
            + playlists.len();
        14 + total as u32 * 3
    };

    let page = file.page_mut(0);
    page[0x00..0x04].copy_from_slice(&[0; 4]); // magic
    page[0x04..0x08].copy_from_slice(&(PAGE_SIZE as u32).to_le_bytes());
    page[0x08..0x0C].copy_from_slice(&NUM_TABLES.to_le_bytes());
    page[0x0C..0x10].copy_from_slice(&next_unused.to_le_bytes());
    page[0x10..0x14].copy_from_slice(&5u32.to_le_bytes());
    page[0x14..0x18].copy_from_slice(&sequence.to_le_bytes());
    page[0x18..0x1C].copy_from_slice(&[0; 4]);

    // 20 table pointers: (type, empty_candidate, first_page, last_page).
    let pointers: [(u32, u32, u32, u32); 20] = [
        (T_TRACKS, p.tracks_empty, 1, p.tracks_last),
        (T_GENRES, p.genres_empty, 3, p.genres_last),
        (T_ARTISTS, p.artists_empty, 5, p.artists_last),
        (T_ALBUMS, p.albums_empty, 7, p.albums_last),
        (T_LABELS, 10, 9, 9),
        (T_KEYS, 50, 11, 12),
        (T_COLORS, 42, 13, 14),
        (T_PLAYLIST_TREE, p.tree_empty, 15, p.tree_last),
        (T_PLAYLIST_ENTRIES, p.entries_empty, 17, p.entries_last),
        (0x09, 20, 19, 19),
        (0x0A, 22, 21, 21),
        (0x0B, 24, 23, 23),
        (0x0C, 26, 25, 25),
        (T_ARTWORK, p.artwork_empty, 27, p.artwork_last),
        (0x0E, 30, 29, 29),
        (0x0F, 32, 31, 31),
        (T_COLUMNS, 43, 33, 34),
        (T_HISTORY_PLAYLISTS, 44, 35, 36),
        (T_HISTORY_ENTRIES, 45, 37, 38),
        (T_HISTORY, 41, 39, 40),
    ];
    let mut w = 0x1C;
    for (table_type, empty, first, last) in pointers {
        page[w..w + 4].copy_from_slice(&table_type.to_le_bytes());
        page[w + 4..w + 8].copy_from_slice(&empty.to_le_bytes());
        page[w + 8..w + 12].copy_from_slice(&first.to_le_bytes());
        page[w + 12..w + 16].copy_from_slice(&last.to_le_bytes());
        w += 16;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::PioneerFileType;

    fn track(title: &str, artist: &str, album: &str, n: u32) -> PioneerTrack {
        PioneerTrack {
            title: title.into(),
            artist: artist.into(),
            album: album.into(),
            genre: Some("House".into()),
            bpm: Some(128.0),
            key: Some(sustain_domain::MusicalKey::AMinor),
            duration_secs: 200,
            file_size: 5_000_000,
            track_number: Some(n),
            year: Some(2020),
            rating: 4,
            bitrate_kbps: Some(320),
            sample_rate_hz: 44_100,
            bit_depth: 16,
            file_type: PioneerFileType::Mp3,
            artwork_id: 0,
            date_added: Some("2026-01-01".into()),
            device_audio_path: format!("/Contents/{artist}/{album}/{n:02} {title}.mp3"),
            device_anlz_path: format!("/PIONEER/USBANLZ/P000/0000000{n}/ANLZ0000.DAT"),
        }
    }

    #[test]
    fn standard_export_is_41_pages() {
        let tracks = vec![track("One", "A", "Alpha", 1), track("Two", "B", "Beta", 2)];
        let playlists = vec![PioneerPlaylist {
            name: "Set".into(),
            entries: vec![0, 1],
        }];
        let bytes = build(&tracks, &playlists, &[], "2026-01-01").expect("build pdb");
        assert_eq!(bytes.len(), STANDARD_PAGES * PAGE_SIZE);
        // File header self-describes page size and table count.
        assert_eq!(
            u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            PAGE_SIZE as u32
        );
        assert_eq!(
            u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            NUM_TABLES
        );
    }

    #[test]
    fn reference_tables_are_embedded() {
        let tracks = vec![track("One", "A", "Alpha", 1)];
        let playlists = vec![PioneerPlaylist {
            name: "Set".into(),
            entries: vec![0],
        }];
        let bytes = build(&tracks, &playlists, &[], "2026-01-01").expect("build pdb");
        // Columns at page 34, history at page 40.
        assert_eq!(
            &bytes[34 * PAGE_SIZE..35 * PAGE_SIZE],
            REF_COLUMNS.as_slice()
        );
        // History page sequence patched at 0x10.
        let hist = &bytes[40 * PAGE_SIZE..41 * PAGE_SIZE];
        assert_eq!(
            u32::from_le_bytes([hist[0x10], hist[0x11], hist[0x12], hist[0x13]]),
            10
        );
    }

    #[test]
    fn track_row_header_is_well_formed() {
        let entities = Entities::build(&[track("T", "Ar", "Al", 1)]);
        let mut t = track("T", "Ar", "Al", 1);
        t.artwork_id = 7;
        let row = track_row(&t, &entities, 0, "2026-01-01");
        assert_eq!(&row[0..2], &0x0024u16.to_le_bytes());
        // artwork_id at 0x1C
        assert_eq!(
            u32::from_le_bytes([row[0x1C], row[0x1D], row[0x1E], row[0x1F]]),
            7
        );
        // track id at 0x48
        assert_eq!(
            u32::from_le_bytes([row[0x48], row[0x49], row[0x4A], row[0x4B]]),
            1
        );
    }

    #[test]
    fn artwork_table_is_populated_and_grows_file() {
        let tracks = vec![track("One", "A", "Alpha", 1)];
        let playlists = vec![PioneerPlaylist {
            name: "Set".into(),
            entries: vec![0],
        }];
        let artworks = vec![PioneerArtwork {
            id: 1,
            path: "/PIONEER/Artwork/00001/a1.jpg".into(),
        }];
        let bytes = build(&tracks, &playlists, &artworks, "2026-01-01").expect("build pdb");

        // The artwork empty-candidate (page 53) must be a real page, so
        // the file grows from the standard 41 pages to 54.
        assert_eq!(bytes.len(), 54 * PAGE_SIZE);

        // Artwork is the 14th table pointer (index 13) at 0x1C, 16 bytes
        // each: (type, empty, first, last).
        let ptr = 0x1C + 13 * 16;
        let read = |off: usize| {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        assert_eq!(read(ptr), T_ARTWORK);
        assert_eq!(read(ptr + 4), OVERFLOW_START); // empty candidate = 53
        assert_eq!(read(ptr + 8), 27); // header/first page
        assert_eq!(read(ptr + 12), 28); // last (data) page

        // The header at page 27 chains into the data page 28.
        let header = &bytes[27 * PAGE_SIZE..28 * PAGE_SIZE];
        assert_eq!(
            u32::from_le_bytes([header[0x0C], header[0x0D], header[0x0E], header[0x0F]]),
            28
        );

        // Page 28 is the artwork data page: self-index 28, table 0x0D,
        // one row, chaining to the empty candidate.
        let data = &bytes[28 * PAGE_SIZE..29 * PAGE_SIZE];
        assert_eq!(
            u32::from_le_bytes([data[0x04], data[0x05], data[0x06], data[0x07]]),
            28
        );
        assert_eq!(
            u32::from_le_bytes([data[0x08], data[0x09], data[0x0A], data[0x0B]]),
            T_ARTWORK
        );
        assert_eq!(
            u32::from_le_bytes([data[0x0C], data[0x0D], data[0x0E], data[0x0F]]),
            OVERFLOW_START
        );
        assert_eq!(data[0x18], 1); // num_rows_small
        // The row begins at the heap start with artwork id 1.
        assert_eq!(
            u32::from_le_bytes([
                data[HEAP_START],
                data[HEAP_START + 1],
                data[HEAP_START + 2],
                data[HEAP_START + 3],
            ]),
            1
        );
    }

    #[test]
    fn artwork_overflows_into_extra_pages() {
        // Enough covers to exceed one page of 36-byte artwork rows,
        // forcing a second data page drawn from the overflow zone.
        let tracks = vec![track("One", "A", "Alpha", 1)];
        let playlists = vec![PioneerPlaylist {
            name: "Set".into(),
            entries: vec![0],
        }];
        let artworks: Vec<PioneerArtwork> = (1..=200)
            .map(|id| PioneerArtwork {
                id,
                path: format!("/PIONEER/Artwork/00001/a{id}.jpg"),
            })
            .collect();
        let bytes = build(&tracks, &playlists, &artworks, "2026-01-01").expect("build pdb");

        let self_index = |idx: usize| {
            let page = &bytes[idx * PAGE_SIZE..(idx + 1) * PAGE_SIZE];
            u32::from_le_bytes([page[4], page[5], page[6], page[7]])
        };
        let next_of = |idx: usize| {
            let page = &bytes[idx * PAGE_SIZE..(idx + 1) * PAGE_SIZE];
            u32::from_le_bytes([page[0x0C], page[0x0D], page[0x0E], page[0x0F]])
        };

        // First artwork data page is the fixed 28; its overflow page is
        // the first free page in the overflow zone (53).
        assert_eq!(self_index(28), 28);
        assert_eq!(next_of(28), OVERFLOW_START); // 28 -> 53
        assert_eq!(self_index(OVERFLOW_START as usize), OVERFLOW_START);
        assert_eq!(next_of(OVERFLOW_START as usize), OVERFLOW_START + 1); // 53 -> empty(54)

        // The artwork table pointer (index 13) reports first=27, last=53.
        let ptr = 0x1C + 13 * 16;
        let read = |off: usize| {
            u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
        };
        assert_eq!(read(ptr), T_ARTWORK);
        assert_eq!(read(ptr + 8), 27); // first (header) page
        assert_eq!(read(ptr + 12), OVERFLOW_START); // last data page = 53
    }

    #[test]
    fn large_playlist_overflows_into_extra_pages() {
        // 1000 entries far exceed one page of playlist-entry rows.
        let tracks: Vec<PioneerTrack> = (1..=1000)
            .map(|i| track(&format!("T{i}"), "Artist", "Album", i))
            .collect();
        let entries: Vec<usize> = (0..1000).collect();
        let playlists = vec![PioneerPlaylist {
            name: "Big".into(),
            entries,
        }];
        let bytes = build(&tracks, &playlists, &[], "2026-01-01").expect("build pdb");
        // Must have grown beyond the standard 41 pages, with overflow
        // drawn from page 53 onward.
        assert!(bytes.len() > STANDARD_PAGES * PAGE_SIZE);
        let self_index = |idx: usize| {
            let page = &bytes[idx * PAGE_SIZE..(idx + 1) * PAGE_SIZE];
            u32::from_le_bytes([page[4], page[5], page[6], page[7]])
        };
        // Fixed data pages and the first overflow page are real,
        // self-describing pages.
        assert_eq!(self_index(2), 2); // tracks
        assert_eq!(self_index(18), 18); // playlist entries first page
        assert_eq!(self_index(OVERFLOW_START as usize), OVERFLOW_START); // first overflow page

        // The playlist-entries header (page 17) chains into page 18.
        let header = &bytes[17 * PAGE_SIZE..18 * PAGE_SIZE];
        assert_eq!(
            u32::from_le_bytes([header[0x0C], header[0x0D], header[0x0E], header[0x0F]]),
            18
        );
    }
}
