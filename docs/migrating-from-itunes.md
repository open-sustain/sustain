# Migrating from iTunes / Apple Music

Source-specific spec for backfilling a Sustain library from an iTunes /
Apple Music library export. Read
[`migrating-to-sustain.md`](migrating-to-sustain.md) first for the
contract every importer must respect (schema, what the scan does/doesn't
recover, how to match source tracks to Sustain rows, history merge rules).

Single file: "iTunes Music Library.xml" (Apple property list, XML
representation). Parse with a plist library — the root is a dict containing
two interesting sections: `Tracks` (dict keyed by stringified track ID) and
`Playlists` (array of dicts).

## Track dict

| Key                   | Type / encoding                              | Sustain column           |
|-----------------------|----------------------------------------------|--------------------------|
| Track ID (the `Tracks` dict key, repeated as `Track ID`) | int | not stored; map each to the matched `tracks.id` to resolve `Playlist Items` |
| `Persistent ID`       | 16-hex string                                | not stored                |
| `Name`                | string                                       | `title`                  |
| `Artist`              | string                                       | `artist`                 |
| `Album`               | string                                       | `album`                  |
| `Album Artist`        | string                                       | `album_artist`           |
| `Composer`            | string                                       | `composer`               |
| `Genre`               | string                                       | `genre`                  |
| `Size`                | int (bytes)                                  | `file_size_bytes`        |
| `Total Time`          | int (**milliseconds**)                       | `duration_seconds` (÷1000, round) |
| `Track Number` / `Track Count` | int                                 | `track_number` / `track_total` |
| `Disc Number` / `Disc Count`   | int                                 | `disc_number` / `disc_total`  |
| `Year`                | int                                          | `year`                   |
| `Bit Rate`            | int (kbps)                                   | `bitrate_kbps`           |
| `Sample Rate`         | int (Hz)                                     | `sample_rate_hz`         |
| `BPM`                 | int                                          | `bpm_hundredths` (×100)  |
| `Date Added`          | plist date (ISO-8601)                        | `date_added_at_unix`     |
| `Play Date UTC`       | plist date (ISO-8601)                        | `last_played_at_unix`    |
| `Skip Date`           | plist date (ISO-8601)                        | `last_skipped_at_unix`   |
| `Play Count`          | int                                          | `play_count`             |
| `Skip Count`          | int                                          | `skip_count`             |
| `Rating`              | int **0..=100 in steps of 20**               | `rating` (÷20 → 0..=5)   |
| `Rating Computed`     | bool                                         | gate: only use `Rating` as a user rating when this is missing or false |
| `Comments`            | string                                       | `comments`               |
| `Location`            | `file://` URI (HFS+ percent-encoding on macOS exports) | matched against `relative_path`, see below |
| `Disabled`            | bool — track is unticked in iTunes           | optional: skip or import as-is |

Quirks worth hard-coding:

- `Total Time` is milliseconds, not seconds.
- `Rating` is on the 0..=100 scale; rounded to nearest 20 by iTunes. Divide
  by 20 for the Sustain 0..=5 column.
- `Rating Computed: true` means Apple Music auto-rated the track based on
  play history — not a user rating. Do not treat it as authoritative.
- `Location` paths in libraries exported from macOS use HFS percent-encoding
  and reference the user's iTunes Media folder, which usually no longer
  exists on the Linux machine doing the import. Percent-decode the URI,
  strip the iTunes Media folder prefix (`…/iTunes Media/Music/`), and
  compare the remainder with each row's `relative_path`. When the Linux
  library is laid out differently, match on the longest path suffix that
  identifies exactly one row. A file name alone is not enough:
  `Live/01 Intro.mp3` and `Studio/01 Intro.mp3` are different tracks.
  Report every ambiguous or unmatched entry instead of taking the first
  candidate.
- iTunes optionally writes ratings to file tags as POPM (MP3). If Sustain
  has already scanned the file and ingested a POPM rating, that is the
  authoritative value — only fill `tracks.rating` from the XML when Sustain
  has no rating yet.

## Playlist array

Each entry is a dict. Skip:

- `Master: true` — the implicit "Music" library, not a user playlist.
- `Visible: false` — hidden system playlists.
- Any of `Music`, `Movies`, `TV Shows`, `Podcasts`, `Audiobooks`,
  `Purchased Music`, `Party Shuffle` set to `true` — system-managed.

For the rest:

- `Folder: true` → write to `playlist_folders`; no items.
- Otherwise → write to `playlists`, then walk `Playlist Items` (an array of
  `{Track ID: N}` dicts, in playlist order) and emit `playlist_entries` with
  the Sustain `tracks.id` that track ID was matched to.
- Hierarchy: `Parent Persistent ID` on a child references `Playlist
  Persistent ID` on its parent folder.
- Smart playlists carry `Smart Info` and `Smart Criteria` as base64-encoded
  binary blobs in a proprietary, partially-documented format. Translating
  them to Sustain smart-playlist rules is non-trivial; the path of least
  resistance is to skip them and let the user recreate inside Sustain.
