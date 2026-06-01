# Migrating to Sustain

Sustain does not bundle importers for iTunes, Apple Music, Rhythmbox, or any
other player. The expectation is that users migrate their own library by
pointing Sustain at the audio files on disk and, where necessary, running a
one-off backfill script (hand-written or LLM-generated against the user's
specific export) for the fields the scan cannot recover.

This document is the contract that script must respect.

## What the scan does

When Sustain scans the library folder, it reads each audio file with `lofty`
and populates the SQLite `tracks` row from the file's embedded tags and audio
properties.

Audio formats supported: MP3 (ID3), Ogg / FLAC (Vorbis comments), MP4 / M4A
(MP4 atoms). Other formats are skipped.

Fields populated from file tags:

- `title`, `artist`, `album`, `album_artist`, `composer`, `grouping`, `genre`
- `track_number`, `track_total`, `disc_number`, `disc_total`
- `year`, `compilation`, `bpm`, `musical_key`, `comments`, `lyrics`
- `rating` (read from POPM / MP4 `rate` atom / Vorbis `FMPS_RATING` via lofty;
  whatever the file format carries)
- artwork (embedded `CoverFront` picture, or the first picture if none is
  flagged as cover)

Fields populated from audio properties:

- `duration_seconds`, `bitrate_kbps`, `sample_rate_hz`, `channels`

Fields populated from filesystem / scan context:

- `relative_path` (relative to the configured library folder)
- `content_hash`, `file_size_bytes`
- `date_added_at_unix` — **set to the scan time, not the original add date**

## What the scan does not recover

These columns exist in the `tracks` table but cannot be reconstructed from
the audio files alone. They default to `0` or `NULL` on a fresh scan:

- `play_count`
- `skip_count`
- `last_played_at_unix`
- `last_skipped_at_unix`
- `date_added_at_unix` — see note above; the scan stamps this to "now", so
  the original add date from the source library is lost unless backfilled

Sidecar artwork files (`folder.jpg`, `cover.png`, etc.) are **not** read.
Only embedded artwork is picked up. If your source library stores artwork
externally, embed it into the tags before the scan or backfill it
afterwards.

## What a backfill script should do

After the scan completes, write directly to the SQLite database at:

```
$XDG_DATA_HOME/sustain/library.sqlite
```

(falls back to `~/.local/share/sustain/library.sqlite` if `XDG_DATA_HOME`
is unset)

For each track present in both your source library and Sustain's database,
match by `relative_path` (or by `content_hash` if your source provides
matching hashes) and update:

- `play_count` — total plays from the source
- `skip_count` — total skips from the source (iTunes has this; Rhythmbox does
  not)
- `last_played_at_unix` — Unix epoch seconds, nullable
- `last_skipped_at_unix` — Unix epoch seconds, nullable
- `date_added_at_unix` — Unix epoch seconds; the original add date from the
  source library

Ratings are already picked up by the scan if they live in the file tags. If
your source library stores ratings only in its own database (Rhythmbox does
this; iTunes optionally writes POPM to tags but the library xml is the
canonical store), either:

1. write the ratings to the file tags before the scan, then let Sustain
   pick them up, or
2. write them directly to the `tracks.rating` column (integer 0–5) — but
   Sustain treats the file tag as the durable source on rescan, so any
   rating that lives only in SQLite will be overwritten by `NULL` the next
   time the file is scanned if the tag is empty. Option 1 is the correct
   one.

## Merging history onto existing rows

When overlaying external history onto a Sustain row, apply per-field rules
rather than blanket-overwriting:

- `date_added_at_unix` — **min** (oldest wins; recovers the original add date
  that the scan stamped to "now").
- `play_count` — choose a strategy and document it: `max(sustain, external)`
  when the external era is already represented in Sustain; `sustain + external`
  only when the two eras provably do not overlap; or the "smart" rule
  `sustain >= external ? sustain : sustain + external` for the common case
  where one era partially absorbs the other.
- `skip_count` — **max**.
- `last_played_at_unix`, `last_skipped_at_unix` — **max** (most recent wins).
- A field that is absent in the external source must leave Sustain's value
  untouched. Do not write `0` or `NULL` over existing data.

## Source-specific guides

Field-by-field specs for individual source libraries:

- [Migrating from Rhythmbox](migrating-from-rhythmbox.md)
- [Migrating from iTunes / Apple Music](migrating-from-itunes.md)

## Schema reference

The authoritative schema baseline is in
`crates/library_store/src/schema.rs` under the `CREATE TABLE` statements.
Read it directly rather than relying on documentation; the schema is the
single source of truth.

Relevant tables for a migration script:

- `tracks` — one row per audio file
- `playlists`, `playlist_entries`, `playlist_folders` — regular playlists
  and their hierarchy
- `smart_playlists`, `smart_playlist_rules` — smart playlists (rule-based
  saved queries)

Playlist migration is straightforward: insert into `playlists` (or
`playlist_folders` for folders), then insert `playlist_entries` rows with
the `track_id` of the corresponding `tracks` row. Smart playlists from
other players don't have a clean mapping to Sustain's rule format and are
best left to be recreated by the user inside Sustain.

## Pre-release caveat

Sustain is pre-release, but SQLite schema versioning is active. A backfill
script written today against the current schema may need to be adjusted when
the schema changes between development versions. SQLite changes advance
`PRAGMA user_version` through ordered migrations that preserve existing
libraries; applied migrations are never flattened or rewritten.
