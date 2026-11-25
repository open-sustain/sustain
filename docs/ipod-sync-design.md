# iPod sync — architecture

Status: approved design, 2026-11-25; implementation in progress. Scope,
feasibility and the sourced prior-work survey live on
[#391](https://github.com/open-sustain/sustain/issues/391). This document is
the design the code follows; it was validated against the `device_sync`
engine before implementation began.

## 1. Scope

- **Models:** unsigned (iPod 1G–5G, mini, nano 1G–2G) and hash58 (Classic, all
  revisions; nano 3G–4G). Out of scope: nano 5G, 6G and 7G; iPod touch,
  iPhone and iPad.
- **Content:** music, playlists and album artwork, in one release.
- **Direction:** one-way, to the device. No import from the iPod.
- **Filesystem:** FAT32 only. A Mac-formatted (HFS+) iPod is refused, and the
  message explains why.
- **Formats:** no transcoding. Tracks the iPod cannot play are skipped and
  counted in the sync summary.
- **Ownership:** as with Pioneer exports, Sustain writes only over an iPod
  library it wrote itself, or over one that lists no tracks (a freshly
  restored iPod, see §5). An iPod holding an iTunes or Finder library is
  refused until the user restores it, so **Sustain never deletes a file it
  did not write**.

## 2. Release posture

The feature ships on `main`, in regular releases, labeled **beta**. People
without an iPod are not affected, by construction:

- The iPod layout is offered only for a device of kind `DeviceKind::Ipod`.
  Discovery assigns that kind only to a mount whose root holds an
  `iPod_Control` directory. Every other device keeps exactly the layouts it
  has today.
- Discovery remains lazy: it never runs at startup. The cold-start budget is
  untouched, and no startup phase is added.
- Migration 27 (§6) runs for everyone. It is a data-preserving rename and is
  tested both on a fresh database and on upgrades from every earlier version.

"Beta" is an honest label, not a gate. The layout reads "iPod (beta)". Each
model family in the profile table carries a `validated` flag, shown in the
device panel and in `docs/features.md`. Every in-scope family is offered:
with no gate, an unoffered family could never be validated. The label is
dropped once two different families have passed the hardware checklist (§10).

## 3. Crate `sustain-ipod` (`crates/ipod`)

The crate is pure format code: flat inputs in, bytes out, with no I/O, no
library model and no transport, like `sustain-pioneer`. It declares
`#![forbid(unsafe_code)]`.

| Module | Responsibility |
|---|---|
| `model` | Flat inputs: `IpodTrack`, `IpodPlaylist`, prepared covers. |
| `profile` | The model table and the `SysInfo` parser. The table maps a model number (`ModelNumStr`, e.g. `MB562`) to a family, a checksum scheme (none or hash58), the cover formats and `validated`. Its facts come from libgpod's `itdb_device.c`, limited to in-scope models. |
| `itunesdb` | The `iTunesDB` writer. It emits the root `mhbd`; the track list (`mhlt/mhit/mhod`); playlists (`mhlp/mhyp/mhip`, with the master playlist first); the album list (`mhla/mhia`); the artist list (`mhli/mhii`); and, on the master playlist, the type-52 sort indexes and type-53 jump tables. All numbers are little-endian and strings are UTF-16LE. Versions and record sizes match libgpod's output. |
| `itunesdb::track_count` | A bounded reader of untrusted input that returns only how many tracks a catalogue lists (`mhbd → mhsd 1 → mhlt` child count). Any inconsistency counts as "not empty". |
| `hash58` | Signing, per survey §2. The S-boxes are generated from the AES definition (FIPS 197) rather than transcribed; the 18-byte seed is the only protocol constant. The code is written from the documented algorithm and credited in `crates/ipod/PROVENANCE.md`. |
| `artwork` | The `ArtworkDB` writer and the `.ithmb` cover files, stored as little-endian RGB565 at each format's row stride. F1061 is 55×55 with 112-byte rows, 6,160 bytes per cover (survey §4), not libgpod's 56×56. |
| `sort` | Index collation through `sustain_domain::CollationKey` and the library's existing sort fields, so the iPod orders tracks the way Sustain does. |

New dependencies are `sha1` and `hmac`. Both are RustCrypto crates under
MIT/Apache-2.0, the same family as the `sha2` already in the lockfile. hash58
is HMAC-SHA1, and the dependency tree had no SHA-1. The plan on #391 said "no
dependencies added"; that was wrong.

`SysInfoExtended` is not parsed. Reading it would need an XML plist parser,
and the first tester's device has no such file. The model table covers the
same facts.

## 4. Device identification

- **Discovery** (`identity.rs`, on the GTK thread) makes one cheap check: does
  `iPod_Control` exist at the mount root? If it does, the device kind is
  `DeviceKind::Ipod`. `ensure_device_config` refreshes the kind of an
  existing row, since a device configured before this release is stored as
  `UsbDrive`.
- **Everything else is resolved on the sync worker**, from the opened
  `DeviceRoot`, so it describes the very root being written and is current:
  - The model, from `iPod_Control/Device/SysInfo`.
  - The filesystem type and read-only state, from `fstatfs`/`fstatvfs` on the
    root fd. Mount options parsed at discovery could go stale, for example if
    the kernel remounts a damaged FAT volume read-only.
  - The FireWire GUID (hash58 models only). First choice is `SysInfo`'s
    `FirewireGuid`, which a stock Classic usually lacks. Otherwise it is the
    USB `serial` of the root's own device: from `fstat` `st_dev`, through
    `/sys/dev/block/MAJ:MIN`, up to the nearest USB ancestor. On the
    tester's device the USB serial is the expected source.
- The volume facts are exposed as defaulted `DeviceTransport` methods, so
  engine tests can fake them. `SyncRequest` gains no iPod field.

## 5. Engine integration (`device_sync`)

- **Layouts:** `DeviceLayout::Ipod` (`as_db` 3) and `DeviceKind::Ipod`
  (`as_db` 2). `DeviceLayout::offered_for(kind)` and `default_for(kind)`
  replace the hard-coded `M3u` defaults and the unfiltered `DeviceLayout::ALL`
  in the panel. An iPod is offered iPod, M3U and folder-per-playlist; M3U is
  what Rockbox users want.
- **Base:** the device root, as for Pioneer.
- **Placements** are derived from the track id, so re-syncs diff with the
  existing machinery and never rename:
  `iPod_Control/Music/F{id mod 50:02}/S{id base36}.{ext}`. The directory
  count is one constant: the firmware follows the path stored in the
  catalogue, and a count read from a table would re-copy the whole library
  if the table were ever corrected.
- **Playability** (`layout::plays`) is decided from the extension (`mp3`,
  `m4a`, `wav`, `aif`, `aiff`) plus `sample_rate_hz <= 48_000`, both already
  known at plan time. libgpod also derives the file-type marker from the
  extension, so AAC and ALAC share `M4A `.
- **Track fields:** `SyncInputTrack` gains `album_artist`, `compilation` and
  the sort fields, all already present in `TrackMetadata`. The `mhit`
  timestamp is derived from `date_added`. Sound Check is written as 0
  ("no value"), because the library stores no ReplayGain. The gapless fields
  are 0: tracks play normally, with no gapless claim. Both limits are
  documented.
- **Pre-check:** the ownership check sits with the Pioneer one, before the
  temporary-file cleanup and before the marker write (`engine.rs`). An iPod
  is refused when:
  - the root is HFS+ or read-only;
  - the model is unknown or out of scope;
  - a hash58 model has no readable GUID;
  - its `iTunesDB` or `ArtworkDB` matches no recorded identity and lists
    tracks.
- **Write order**, which fits the engine's copy → finalize → remove sequence:
  1. Audio copies.
  2. `.ithmb` cover files, then `ArtworkDB`.
  3. Delete `iTunes/Play Counts` and `iTunes/OTGPlaylistInfo`. Their rows
     refer to tracks by position in the previous catalogue, so applying them
     to the new one would hit the wrong tracks. Numbered On-The-Go siblings
     (`OTGPlaylistInfo_1`, …) are left to the firmware, which drops them when
     it finds the catalogue changed; libgpod does the same.
  4. Write `iTunesDB`, signed, through the transport's atomic replace.
  5. The existing stale-file removals, behind the existing confirmation.

  Each database identity is recorded in the journal before the database is
  written, as for Pioneer. If a run is interrupted before step 4, the
  previous catalogue is still in place and its audio is still present,
  because removals come last. Between steps 2 and 4, only the covers can be
  wrong; the next sync fixes them. A cancelled run never writes a catalogue.
- **Capacity:** the plan's byte total includes a cover estimate derived from
  the profile (about 244 KB per distinct cover on a Classic). Without it, a
  sync could run out of space after the audio is copied.

## 6. Catalogue record (shared with Pioneer)

`PioneerExportRecord { databases, artwork }` already holds exactly what the
iPod needs:

- `databases`: the size and SHA-256 of every catalogue file Sustain may have
  left on the device, here `iTunesDB` and `ArtworkDB`;
- `artwork`: the cover files Sustain owns, for stale-file removal.

It is renamed to a layout-neutral `CatalogueRecord`. Migration 27 renames its
two tables (`ALTER TABLE … RENAME`), keeping every row, and the version bump
stops older binaries from choking on the new layout and kind values. No
parallel record is threaded through the request, outcome, failure, journal
or store.

## 7. Covers

The engine never reads source files while it writes to the device: that is
the point of `PreparedSyncRequest`. Covers therefore follow
`PreparedPioneerAssets`:

- Preparation reads each track's embedded art, which is where Pioneer covers
  come from too. It decodes each distinct cover once, shrinks it to the
  profile's largest format edge (320 px on a Classic), and keeps it as a
  compact JPEG, roughly 40 KB.
- The engine renders the RGB565 for each format from those prepared covers,
  one `.ithmb` file at a time. Each file is capped well below libgpod's
  256 MB limit, so memory holds at most one file.
- All artwork is rewritten on every sync, as Pioneer does. If hardware timings
  show this hurts, one recorded digest of the cover set can skip it,
  following the `AnlzRecord` precedent.
- Known leak: `.ithmb` files left by iTunes on a restored iPod are not
  Sustain's, so Sustain never removes them.

## 8. Refusals and messages

New `SyncBlocker` variants, rendered by the existing blocker surface, with
text in `sync_blocker_text`:

- `MacFormattedIpod`: the iPod is formatted for Mac, and Linux cannot safely
  write to it. The fix is to restore it with iTunes for Windows.
- `ReadOnlyDevice`: the device is mounted read-only.
- `UnsupportedIpod`: the model is unknown or out of scope.
- `UnidentifiedIpod`: the iPod's serial number, needed to sign its
  library, could not be read.
- `ForeignIpodLibrary`: the iPod holds a library from iTunes or Finder.
  Restoring it erases it, recordings included, so they should be copied off
  first.

Outcomes and failures go through `NotificationCenter`, as today. The layout
labels are passed through gettext, and the strings are translated in every
`po/*.po`.

## 9. Testing

- **hash58:** key derivation and digest are checked against a small
  synthetic catalogue whose GUID and digest come from libgpod, an independent
  implementation. Its provenance is recorded in `PROVENANCE.md`. The two
  public fixtures (Apple's nano 4G and Omatune's Classic) are not committed:
  an ignored developer test takes their paths.
- **Writer:** structural invariants (lengths, counts, master playlist first),
  index permutations and jump-table bounds, `track_count` on Sustain's own
  output and on truncated or adversarial input.
- **Artwork:** row stride per format, and every `mhni` range inside its
  `.ithmb` file.
- **Engine:** every refusal and its order relative to mutation, the write
  order, an interrupted run, and capacity.
- **Store:** migration 27 on a fresh database and on upgrades from every
  version.
- **Oracle (developer tool, not CI):** libgpod's parser over generated
  databases. It does not check signatures (survey §11).

## 10. Hardware checklist (per family)

Run on a freshly restored FAT32 iPod with a library Sustain wrote from
scratch. The iPod must:

- boot without "No music";
- play MP3, AAC and ALAC;
- show Artists, Albums and Songs correctly, letter-jump included;
- show correct covers in lists and on Now Playing;
- pick up a re-sync after a metadata edit;
- remove a playlist dropped from the selection;
- recover through a re-sync after being unplugged mid-sync.

## 11. Milestones

Each milestone leaves `main` green and shippable.

1. `crates/ipod`, not yet wired: the profile table and `SysInfo` parser, the
   `iTunesDB` writer, hash58 and its known-answer test, `track_count`,
   licensing and provenance.
2. `crates/ipod` artwork: `ArtworkDB` and `.ithmb`.
3. The record generalisation and migration 27. A pure refactor.
4. Wiring: `DeviceKind::Ipod`, the transport facts, refusals, placements,
   playability, finalize, `Play Counts` cleanup, the panel and i18n. The
   iPod layout becomes visible here, so this milestone and the next must
   land before a release is cut.
5. Cover wiring: prepared covers, `.ithmb` writes, stale cover-file removal,
   the capacity estimate.
6. The hardware checklist, `docs/features.md`, the README and the
   `validated` flags.
