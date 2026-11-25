# Provenance — `sustain-ipod`

This crate is original Sustain code, licensed GPL-3.0-or-later like the rest
of the workspace. No third-party source file was copied or translated into
it. It does, however, reproduce facts about a file format that other people
reverse-engineered, and this file records where each of them comes from.

## Format reference: libgpod

| | |
| --- | --- |
| Project | libgpod — https://github.com/gtkpod/libgpod |
| Revision consulted | `7982c5554f78dde47fd006afbeff659201d6db3d` (0.8.3) |
| Licence | LGPL-2.1-or-later; `src/itdb_hash58.c` is BSD-3-Clause (Christophe Fergeau, after William Whistler's 2007 proof of concept) |

Used as a reference, not as a source:

- **`iTunesDB` layout** (`src/itdb_itunesdb.c`: `mk_mhbd`, `mk_mhit`,
  `mk_mhod`, `write_playlist`, `mk_long_mhod_id_playlist`, `mk_mhia`,
  `mk_mhii`, `prepare_itdb_for_write`): record tags, header sizes, field
  offsets, the dataset order, the per-format defaults of the track record,
  the playlist column-preferences record, the browse-index and jump-table
  layouts and their tie-breaking order. `src/itunesdb.rs` writes each record
  at libgpod's size with each field at libgpod's offset.
- **Model table** (`src/itdb_device.c`, `ipod_info_table`): which model
  number belongs to which generation, and which generations need hash58 or a
  media type. `src/profile.rs` lists the in-scope rows, with only the family.
- **Artwork** (`src/db-artwork-writer.c`, `src/ithumb-writer.c`,
  `src/db-itunes-parser.h`, and in `src/itdb_device.c` the cover-format
  tables and `itdb_device_supports_sparse_artwork`): the `ArtworkDB` record
  layout and header sizes, the `F{format}_{n}.ithmb` naming, image ids from
  100, which families share images between tracks, and the track record's
  artwork fields (`src/itdb_track.c`). `src/artwork.rs` follows them, with
  two departures: the classic's F1061 format is 55 × 55 in 112-byte rows
  (see below), not libgpod's 56 × 56; and each `.ithmb` file stops at
  32 MiB rather than 256 MB, because Sustain renders a whole file in memory.
- **hash58** (`src/itdb_hash58.c`): the procedure — the GUID's pairwise least
  common multiples, the S-box mixing, the 18-byte seed, the HMAC-SHA1 over
  the normalised file. `src/hash58.rs` implements it from that description;
  the S-boxes are generated from FIPS 197, not copied from libgpod's tables.

## Known-answer fixture

`tests/fixtures/libgpod-empty-classic.itunesdb` (3,364 bytes) is a catalogue
written by libgpod itself, so the signature test compares against an
independent implementation rather than against Sustain's own output. It holds
no personal data. It was produced with Debian's `libgpod4t64` 0.8.3-24 by a
small C program that:

1. creates a mount directory holding `iPod_Control/Device/SysInfo` with
   `ModelNumStr: xB562` (an iPod classic 120 GB) and
   `FirewireGuid: 0x000A27001C0FFEE5` (a made-up GUID with zero bytes, to
   exercise the least-common-multiple convention);
2. calls `itdb_new`, `itdb_set_mountpoint` on that directory, adds a master
   playlist named `Oracle` (`itdb_playlist_new`, `itdb_playlist_set_mpl`,
   `itdb_playlist_add`), and `itdb_write_file`.

libgpod signs the file because the model is a hash58 family. The test
re-signs it with Sustain's implementation and requires identical bytes.

## Structural cross-check (developer tool, not CI)

During development, catalogues from `src/itunesdb.rs` (0, 1, 4 and 500 tracks
with a playlist) were parsed by libgpod's `itdb_parse_file`, which reported
the expected track and playlist counts, and written back with
`itdb_write_file`. libgpod's rewrite has the same size, and its track records
are byte-identical. The remaining differences are the random ids libgpod gives
albums and artists, the hash-table order it writes them in, and the creation
time it stamps on playlists. libgpod does not check signatures, so this
checks structure only.

## Artwork cross-check (developer tool, not CI)

During development, a classic 120 GB and an iPod with video library, each
with three covers (one shared by two tracks, one wider than tall), were
written with `src/artwork.rs` and read by libgpod's own `itdb_parse` and
`itdb_track_get_thumbnail`. libgpod found every cover in every format, at
the stated size, with the expected colours, and the decoded images were
neither skewed nor shifted, which checks the 112-byte F1061 rows. On the
iPod with video, where each track has its own image, the two images of
the shared cover pointed at one copy of the pixels.

## Other references

- iOpenPod's firmware research, revision `5e48bd7`
  (https://github.com/TheRealSavi/iOpenPod, `docs/research/`):
  `f1061-native-geometry.md` for F1061's 55 × 55 pixels in 112-byte rows
  on classic and nano 3G firmware, and
  `classic201-artwork-semantics-2026-10-28.md` for the classic looking a
  track's cover up by the image id in its record. Facts only; no code.
- The cover formats an iTunes-written classic catalogue uses (F1055, F1060,
  F1061), from Omatune's scrubbed classic 120 GB fixture
  (https://github.com/jyooi/omatune, `fixtures/device/CLASSIC_2`), as
  surveyed on #391.

- The iPodLinux *iTunesDB* documentation, via
  https://github.com/raleighlittles/iTunesDB-Parser (`itunesdb_docs/`), for
  field meanings.
- FIPS 197 (AES), §5.1.1 and §5.3.2, for the S-boxes.
- The prior-work survey on
  [#391](https://github.com/open-sustain/sustain/issues/391).
