# Migrating from Rhythmbox

Source-specific spec for backfilling a Sustain library from Rhythmbox. Read
[`migrating-to-sustain.md`](migrating-to-sustain.md) first for the
contract every importer must respect (schema, what the scan does/doesn't
recover, how to match source tracks to Sustain rows, history merge rules).

Two files under `~/.local/share/rhythmbox/`:

- `rhythmdb.xml` — track metadata + play history
- `playlists.xml` — static and automatic playlists

## rhythmdb.xml

XML, streamable. Only `<entry type="song">` is relevant; skip `iradio`,
`podcast-feed`, `podcast-post`, and `ignore` entries. Per-entry child element
→ field mapping:

| Element             | Type / encoding                              | Sustain column           |
|---------------------|----------------------------------------------|--------------------------|
| `<location>`        | `file://` URI, percent-encoded UTF-8         | `relative_path` (after stripping the library root) |
| `<title>`           | string                                       | `title`                  |
| `<artist>`          | string                                       | `artist`                 |
| `<album>`           | string                                       | `album`                  |
| `<album-artist>`    | string                                       | `album_artist`           |
| `<composer>`        | string                                       | `composer`               |
| `<genre>`           | string                                       | `genre`                  |
| `<track-number>`    | int                                          | `track_number`           |
| `<track-total>`     | int                                          | `track_total`            |
| `<disc-number>`     | int                                          | `disc_number`            |
| `<disc-total>`      | int                                          | `disc_total`             |
| `<duration>`        | int (seconds)                                | `duration_seconds`       |
| `<file-size>`       | int (bytes)                                  | `file_size_bytes`        |
| `<bitrate>`         | int (kbps)                                   | `bitrate_kbps`           |
| `<beats-per-minute>`| int                                          | `bpm_hundredths` (×100)  |
| `<comment>`         | string                                       | `comments`               |
| `<rating>`          | float **in system locale** (0..=5)           | `rating` (0..=5 int)     |
| `<play-count>`      | int                                          | `play_count`             |
| `<last-played>`     | int (Unix epoch seconds)                     | `last_played_at_unix`    |
| `<first-seen>`      | int (Unix epoch seconds)                     | `date_added_at_unix`     |
| `<date>`            | int (GDate Julian day — days since 0001-01-01) | `year` (extract year) |

Quirks worth hard-coding:

- Rating is a locale-formatted float: French installs emit `4,000000`, US
  installs `4.000000`. Replace `,` with `.`, parse as f64, round to nearest
  integer, clamp to 0..=5. Treat `0` and parse failures as "no rating".
- `<date>` is a `GDate` Julian day, not a Unix timestamp. Convert via the
  standard "days since 1 Jan 0001" formula (`chrono::NaiveDate::from_num_days_from_ce_opt`
  in Rust). `0` means unset.
- Rhythmbox does **not** track skip counts. Leave `skip_count` and
  `last_skipped_at_unix` alone.
- Rhythmbox does **not** write ratings to file tags — only to its own DB.
  Backfill `tracks.rating` directly; a Sustain rescan keeps it, because the
  library row, not the file, is the durable store. There is no need to
  embed ratings into the audio files first.
- `<location>` is a percent-encoded URI: decode it to the path's raw bytes
  and bind `relative_path` as a BLOB (see the main guide).

## playlists.xml

- `<playlist type="static">` carries explicit `<location>` children. Resolve
  each location to a Sustain `tracks.id` via the matcher, then write rows to
  `playlists` + `playlist_entries`.
- `<playlist type="automatic">` is a rule tree (`<conjunction>`,
  `<disjunction>`, `<subquery>`, leaf `<is>`/`<like>`/`<greater>`/`<less>`/…).
  The leaf rules name Rhythmbox internal properties (`Artist`, `Title`,
  `Album`, `Rating`, `PlayCount`, `Year`, `Duration`, …). Map to Sustain
  `smart_playlists` + `smart_playlist_rules` where the semantics align;
  report anything you can't translate rather than silently dropping it.
