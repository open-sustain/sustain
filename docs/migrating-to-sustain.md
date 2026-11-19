# Migrating to Sustain

Sustain does not bundle importers for iTunes, Apple Music, Rhythmbox, or any
other player. The expectation is that users migrate their own library by
pointing Sustain at the audio files on disk and, where necessary, running a
one-off backfill script (hand-written or LLM-generated against the user's
specific export) for the fields the scan cannot recover.

This document is the contract that script must respect.

## What the scan does

When Sustain scans the library folder and finds a file it has no row for, it
reads the file with `lofty` and populates a new SQLite `tracks` row from the
file's embedded tags and audio properties. Tags are only initial values:
once a row exists, a rescan refreshes technical properties (duration,
bitrate, size, artwork presence) but never replaces the row's metadata,
rating, or history with what the file's tags say. SQLite is the durable
store.

Audio formats supported: MP3, WAV, AIFF (ID3v2), FLAC, Ogg Vorbis, Opus
(Vorbis comments), MP4 / M4A (MP4 atoms). Other formats are skipped.

Fields populated from file tags:

- `title`, `artist`, `album`, `album_artist`, `composer`, `grouping`, `genre`
- `track_number`, `track_total`, `disc_number`, `disc_total`
- `year`, `compilation`, `bpm_hundredths` (the tempo × 100), `musical_key`,
  `comments`, `lyrics`
- `rating` (0–5 stars). It is read from the first ID3 `POPM` frame, in
  its owner's scale, or from the first Vorbis `RATING` or
  `RATING:<owner>` comment, as a percentage or a fraction of five stars,
  or from an MP4 `rate` atom. A rating byte of 0 means unrated.
  `FMPS_RATING` is not read.
- artwork (embedded `CoverFront` picture, or the first picture if none is
  flagged as cover)

Fields populated from audio properties:

- `duration_seconds`, `bitrate_kbps`, `sample_rate_hz`, `channels`

Fields populated from filesystem / scan context:

- `relative_path` (relative to the configured library folder)
- `file_size_bytes`, `file_modified_at_unix`
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
match by `relative_path` and update the fields below. `relative_path` is a
BLOB holding the path's raw bytes (Linux file names need not be UTF-8), so
bind it as bytes — in Python, `os.fsencode(path)` — never as text: a TEXT
value never equals a BLOB in SQLite, so a text-bound lookup matches no row,
even for a plain ASCII path. A source that does not record the same relative
paths needs a matcher that reports ambiguous candidates instead of taking
the first one (see the source-specific guides).

- `play_count` — total plays from the source
- `skip_count` — total skips from the source (iTunes has this; Rhythmbox does
  not)
- `last_played_at_unix` — Unix epoch seconds, nullable
- `last_skipped_at_unix` — Unix epoch seconds, nullable
- `date_added_at_unix` — Unix epoch seconds; the original add date from the
  source library

Ratings are already picked up by the scan if they live in the file tags. If
your source library stores ratings only in its own database (Rhythmbox does
this; iTunes optionally writes POPM to tags but the library XML is the
canonical store), write them directly to the `tracks.rating` column
(integer 0–5, 0 meaning unrated). A rescan keeps that value: the row, not
the file, is the durable store, so there is no need to rewrite audio files
for a backfill. Ratings the user later changes inside Sustain are mirrored
to the file's tags as a courtesy to other applications.

Tempo and key carry their origin: `bpm_set_by_user = 1` and `key_origin = 1`
mark a value as the user's own, which analysis never replaces and Sustain
mirrors to the file's tags. Leave both at their default for values from a
source library's tags, so analysis may refine them.

## Merging history onto existing rows

When overlaying external history onto a Sustain row, apply per-field rules
rather than blanket-overwriting:

- `date_added_at_unix` — **min** (oldest wins; recovers the original add date
  that the scan stamped to "now").
- `play_count` — two totals cannot tell how far two histories overlap, so
  choose a rule from what you know and record it: `max(sustain, external)`
  when one history contains the other (for example, Sustain's plays were
  all made after it imported the external count); `sustain + external`
  only when the two histories provably do not overlap. When the overlap is
  unknown, keep Sustain's value and decide by hand; no formula on the
  totals alone is right in general.
- `skip_count` — **max**.
- `last_played_at_unix`, `last_skipped_at_unix` — **max** (most recent wins).
- A field that is absent in the external source must leave Sustain's value
  untouched. Do not write `0` or `NULL` over existing data.

## Source-specific guides

Field-by-field specs for individual source libraries:

- [Migrating from Rhythmbox](migrating-from-rhythmbox.md)
- [Migrating from iTunes / Apple Music](migrating-from-itunes.md)

## Schema reference

The schema is built by the ordered migrations in
`crates/library_store/src/migrations.rs`: the `CREATE TABLE` baseline in
`crates/library_store/src/schema.rs` plus every later migration (several
add, rename, or drop `tracks` columns). Inspect the database you are
writing to — `PRAGMA table_info(tracks);` — rather than relying on
documentation or on the baseline alone.

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
