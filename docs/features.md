# Sustain — Features

This document is the canonical reference for what Sustain currently does. It
covers shipped, user-visible behavior only — pending work, open product
questions, and known gaps are tracked in GitHub issues and are **not** listed
here.

Each feature is annotated with a parity tag:

- **iso-iTunes** — direct port of an iTunes 8–12 behavior, intended to feel
  identical to a returning iTunes user waking up from a 15 years coma.
- **iTunes-adjacent** — same idea as iTunes, refined or trimmed for Sustain.
- **Sustain-native** — no direct iTunes analogue; either Linux/GNOME-specific
  integration or a deliberate addition.

Sustain is the source of truth for everything it indexes. SQLite is canonical
once a track is imported; file tags are read only on first import and written
back as a courtesy when the user edits a field. Listening statistics (play
count, skip count, last played, last skipped) live in SQLite only and are
never written to file tags. See `AGENTS.md` for the full persistence policy.

---

## Library management

### Two library modes — *iso-iTunes*
A single tickbox in Preferences chooses between:

- **Don't touch my files** *(default)* — Sustain indexes the configured
  library folder in place. Files are never moved, renamed, or copied. A
  readable folder is sufficient for indexing and playback, including
  read-only or hard-link-incompatible roots.
- **Keep my library organized** — Sustain owns the layout. New files are
  copied into `Artist/Album/NN Title.ext`; existing files are reorganized
  in the background. Before enabling the mode and before each managed
  mutation, Sustain behaviorally probes the selected root for the durable
  hard-link and recovery-journal primitives it needs.

Toggling the mode starts or cancels the background organization task. There
is no separate "Consolidate Library" button — turning the tickbox on *is*
the consolidate action.

SQLite remains authoritative in both modes. Courtesy tag mirroring is
published safely per audio file and retained for automatic retry when a
particular target filesystem or permission policy refuses the replacement.
Sustain never falls back to an in-place tag rewrite.

### Library folder picker — *iso-iTunes*
Preferences exposes a folder chooser plus a manual "Scan Library" trigger.
The path is the only library root; tracks outside it are never indexed as
canonical locations under managed mode.

### Background scan — *iTunes-adjacent*
Scan runs off the GTK main thread. SQLite writes are batched and the
database runs in WAL mode. The status bar surfaces the spinner, a Cancel
button, and the final summary. Cancellation is cooperative and skips the
missing-files sweep so the un-walked portion of the library isn't
mis-reported as missing. Unchanged files on a rescan are detected cheaply
via `mtime + size` rather than re-decoded.

### Drag-and-drop import — *iso-iTunes*
Dragging files or folders from Files (or any GNOME-compatible source) onto
the Music view imports them into the library. Folders are walked
recursively for supported audio formats. The drop zone shows an active
state while a drag is hovered.

Behavior follows the active library mode:

- **Keep my library organized** — files are copied into the managed
  `Artist/Album/NN Title.ext` layout, deduplicated by content hash so a
  re-drop of the same audio isn't ingested twice.
- **Don't touch my files** — files are indexed in place. The drop is
  refused for files that live outside the configured library folder,
  since reference mode never moves or copies.

Drops while another library task (scan, import, or organize) is running
are rejected by the runtime rather than racing.

### Hard-link move primitive — *Sustain-native*
Managed-mode organization uses a same-filesystem metadata move:
hard-link source → destination, then unlink the source. It refuses to
overwrite an existing destination and fails (rather than copy/deleting)
on cross-device moves. This is safe on ext4, XFS, Btrfs, and ZFS; it
fails clean on SMB/FUSE/exFAT and other filesystems that don't support
hard links. Sustain probes the selected root before managed mutations and
keeps the user's organized-mode preference intact if a later remount stops
satisfying the contract. The probe reduces avoidable failures but cannot
prove crash durability, so organized libraries should use normal local
Linux filesystems.

### Recovery journal — *Sustain-native*
Managed reorganization writes a small journal at the library root before
moving files. On the next startup Sustain reconciles the journal so a
crash mid-batch can't desync SQLite from the filesystem. Reconciliation
runs unconditionally during library-service initialization, before any
tracks reach the UI, so an interrupted batch is rolled forward before
the first frame is drawn.

### Supported audio formats — *iso-iTunes*
Scans and imports recognize MP3, FLAC, Ogg Vorbis (`.ogg`, `.oga`),
Opus, the MP4 family (`.m4a`, `.m4b`, `.mp4`), and WAV (`.wav`). Files
with other extensions are skipped silently during library walks.

### Duplicate detection on managed import — *iTunes-adjacent*
When adding external files in managed mode, Sustain skips files that are
already present in the library by content hash, with file-size as a
cheap pre-filter. Plain in-place scans do **not** hash file contents.

### Path-affecting metadata edits — *iso-iTunes*
Editing artist, album artist, composer, album, title, track number, disc
number, disc total, or compilation status while managed mode is active
re-plans the managed path and moves the file accordingly. iTunes did the
same on its managed library. After an organized-mode move or deletion,
Sustain removes empty ancestor folders below the library root. It never
removes the root itself, follows symlinks out of the library, or removes a
folder that still contains sidecars, artwork, or hidden files.

### Missing files stay visible — *iso-iTunes*
When a file recorded in the library disappears from disk, the row stays
in the table with a warning marker. The row is not silently dropped on
the next rescan. Activating a missing row opens a **Locate Missing Track**
chooser. Picking a replacement preserves the existing library identity,
including playlist membership, rating, metadata, and listening statistics.
Reference mode accepts replacements inside the configured library folder;
managed mode copies external replacements into its canonical owned layout.

---

## Views

Sustain has a single navigation surface: a sidebar to the left of the
main content. The sidebar's LIBRARY section lists **Music**, **Albums**,
**Duplicates**, and **Statistics**; the PLAYLISTS section lists every playlist, smart
playlist, and folder. Clicking any entry swaps the right-hand content.
There is no separate horizontal mode switcher.

### Music — *iso-iTunes*
The default entry under LIBRARY, and the landing page for a fresh
session. A dense, full-width track table with multi-select, inline
rating editing, column sorting, customizable columns, and a row
context menu.

### Albums — *iTunes-adjacent*
The Albums entry under LIBRARY opens a full-width album-cover grid.
Tiles group by album (title + album artist + year). Clicking the
cover button on a tile plays the album in isolation. The grid
intentionally searches album-level fields only (title, artist, year),
not individual track titles.

### Duplicates — *iTunes-adjacent*
The Duplicates entry under LIBRARY runs a deferred, off-thread scan and
shows only candidate groups. Loose matching folds case, diacritics, and
whitespace for artist + title. A **Strict matching** toggle also requires
the same album and a duration within two seconds. Groups stay adjacent
and visually banded.

Selecting two or more rows in Songs or Duplicates exposes **Consolidate
to single track**. Double-clicking a Duplicates row plays it normally,
so candidate versions can be compared by ear first. The dialog forces
the highest-bitrate audio file to survive, preferring a lossless format
when bitrate ties; only strictly equal audio-quality candidates remain
user-selectable. Metadata defaults to a field-by-field cherry-pick of
populated values, with controls to override individual fields or take
every editable field from one track. Embedded artwork is previewed
asynchronously and presets the highest-resolution available image.
Consolidation sums listening counts, keeps the most recent last-played
and last-skipped dates, the oldest date-added value, and the highest
rating. Playlist membership is rewritten in place while preserving
order and collapsing consecutive repetitions introduced by the merge.

Before deleting any duplicate pathname, Sustain writes and verifies a
staged survivor, keeps hard-linked recovery copies, commits SQLite and
playlist rewrites atomically, and crosses the durable database barrier.
An interrupted merge is recovered before library hydration on the next
launch. The view is transient: it is never restored or scanned at
startup, so it does not affect cold-start time.

### Statistics — *Sustain-native*
The third entry under LIBRARY opens a single scrollable page of
whole-library diagnostic charts, rendered as aligned proportion bars in
the system accent colour:

- **Genre distribution** — share of tracks per genre (the largest dozen
  genres individually, the long tail folded into one *Other* bar;
  untagged tracks counted as *Unknown*).
- **Quality distribution** — share of tracks per bitrate range
  (≤ 128, 129–255, 256–320, > 320 kbps).
- **Most played genres** — the top five by total play count.
- **Most liked genres** — the top five by average rating, over genres
  with at least five rated tracks; zero-star tracks are excluded, per the
  rating-as-exclusion convention.
- **Release years** — track counts per release decade.
- **Year added** — track counts per calendar year a track entered the
  library.

Every figure comes from SQLite (the authoritative library copy); no file
tags are re-read at view time. The page is computed lazily — only when it
is shown, and refreshed when it is on screen and the library changes — so
it never affects cold start.

### Playlists — *iso-iTunes*
Selecting any row under the PLAYLISTS section opens that playlist's
track table to the right of the sidebar, with a header strip
summarising the playlist.

### Sidebar collapse toggle — *Sustain-native*
A floating button in the bottom-left corner of the content area
slides the sidebar in and out. Collapsed, the Music and Albums views
occupy the full window width; the button stays in place so the
sidebar can be brought back. The collapsed state is persisted across
launches. While the sidebar is collapsed there is no in-app switcher
for the LIBRARY entries — bring the sidebar back to change view.

### Collapsible sections — *iTunes-adjacent*
The LIBRARY and PLAYLISTS headers are disclosure rows: each carries a
caret (▾ open, ▸ folded) and the whole row is the toggle. Clicking a
header folds that section, hiding its rows; folding LIBRARY lets the
PLAYLISTS list rise to fill the freed space. The two sections fold
independently. With a header focused, **Left** folds it and **Right**
unfolds it. Each section's fold state is persisted across launches.

### Selection persistence — *Sustain-native*
The sidebar's active row (Music, Albums, Statistics, or a specific playlist), its
collapsed state, and the fold state of the LIBRARY and PLAYLISTS
sections are restored on next launch. Transient views such as Duplicates
and connected devices are deliberately not restored.

---

## Playlists

### Regular playlists — *iso-iTunes*
Named, ordered, user-curated track lists. Duplicates are allowed. Tracks
can be reordered within a playlist by drag, individually removed via
context menu, and added from any view by drag or context-menu.
Deleting a playlist does not remove its tracks from the library.

### Playlist folders — *iso-iTunes*
Playlists and folders can be nested inside folders. Sidebar drag-and-drop
moves entries between folders.

### Smart playlists — *iso-iTunes*
Rule-based saved queries with iTunes-style operators: `is`,
`contains`, `starts with`, `is in the last N days`, numeric comparisons,
rating comparisons, `is empty` / `is present` for text and numeric
tag fields, and a `File` field whose `is missing` / `is present`
operators match on whether the track's file was last found on disk.
Fields cover the usual tag/metadata set plus BPM
and Music Key for tempo- and harmony-aware rules. The boolean Lyrics field
matches tracks with non-blank plain lyrics; optional synced lines enrich
display but do not independently make a track match. Match mode is
`Match all` / `Match any`. An optional limit picks the top N by
`Most Often Played`, `Random`, etc. Smart playlists are re-evaluated
live on every query.

### Default smart playlists — *iso-iTunes* / *Sustain-native*
A freshly created library is seeded with five iTunes-style starter
smart playlists — **Recently Added**, **Recently Played**,
**Top 25 Most Played**, **4+ Stars**, **Unplayed** — plus two
Sustain-native additions, **Missing Tags** and **Missing Files**.
Missing Tags is a match-any list of tracks lacking any of album, artist,
genre, or year, or that are still unrated, so it doubles as the working
queue for library backfill and consolidation. Missing Files lists the
tracks whose file the last access could not find on disk — the working
set for relocating or pruning broken links. All seven are seeded once at
library creation and not re-seeded afterwards, so the user is free to
delete or edit them.

### Smart playlist editor — *iso-iTunes*
A dedicated editor dialog mirrors the iTunes 11 layout: match mode at
the top, one row per rule with field/operator/value widgets, a limit
section, and OK/Cancel.

### Playlist header — *iTunes-adjacent*
The playlist view draws a header strip above the track table, the
same height as the integrated top bar. The strip shows the selected
playlist's name in bold next to Play and Shuffle buttons that match the
album-detail header's behaviour, with a muted second line summarising
the visible set as `N songs, X hours/minutes/days`. Search filtering
updates the summary so the count always matches what's drawn below.
The header hides for folder selections and for empty states.

### Per-playlist analysis & online retrieval — *Sustain-native*
Right-clicking a playlist or smart playlist exposes two submenus —
**Analyze** (BPM / Key / Audio / All) and **Retrieve** (Lyrics /
Tags / Artwork / All) — that run the chosen capability against that
playlist's track set without waiting for the background sweep to
reach them. Useful for dedicated mix-set playlists destined for
Pioneer PDB export (the audio pass costs a lot of decode time on long
mixes, so most users keep the global audio-analysis toggle off and
trigger it per playlist) or for one-shot "fetch lyrics on this 'Sing me'
playlist" runs.

The two submenus gate their entries differently. **Analyze** entries
go insensitive when the matching global toggle is on — the background
sweep already covers those tracks and re-running a finished,
deterministic analysis is pointless. **Retrieve** is instead a *force*
path: its entries stay available regardless of the toggle and grey out
only while a retrieval run is actually in flight, so a manual retrieval
can re-contact tracks a previous pass left empty (a provider that had
nothing months ago may have it now). The scheduler's missing-only guard
still skips tracks that already carry the data, so a forced run only
touches the gaps and never overwrites. The **All** entry submits the
full mask; per-scheduler inflight dedup keeps a bundled request from
doing duplicate work. Folders don't expose the submenus: they don't
carry tracks of their own.

### Per-track analysis & online retrieval — *Sustain-native*
The same **Analyze** and **Retrieve** submenus appear on the track
context menu (Music view and the playlist track table), so the user can
target the currently-selected tracks instead of a whole playlist.
Naming, menu shape, and gating semantics match the per-playlist version
exactly — Analyze greys out when globally covered, Retrieve greys out
only while a run is in flight.

**Analyze** filters the input through the library store before
dispatching, so a re-run on tracks that already have the requested
analysis is a no-op; the user sees a distinct "All N tracks already
have X — nothing to queue." notification instead of "Queued N tracks",
so a no-op click is never silent. **Retrieve** skips that pre-filter on
purpose — it forces a fresh attempt and leans on the scheduler's
missing-only guard to skip tracks that already have the data — so it
always reports the queued run.

---

## Playback

### Transport controls — *iso-iTunes*
Previous, Play/Pause, and Next buttons live in the integrated top bar.
Spacebar toggles play/pause (focus-aware: it does not intercept while a
text entry has focus). `Ctrl+Left` and `Ctrl+Right` provide the matching
previous / next keyboard controls and likewise yield to text editing.

### Volume slider — *iso-iTunes*
A volume slider in the top bar persists its value to `settings.toml`,
debounced so a drag doesn't thrash the disk. The slider "magnetizes" to
100% above the 90% threshold so you cannot accidentally have rounding 
errors that would bother for audiophiles.

### Now Playing display — *iso-iTunes*
The center of the top bar shows the current track's artwork, title,
artist/album, elapsed and remaining time, and a seekable progress bar.
The artwork's dominant color tints the tile background. Overflowing
title and artist/album lines marquee-scroll, each at a slightly
different rate so the two never crawl in lockstep. Clicking on the
artwork opens a centered artwork overlay. Tracks with non-blank plain
lyrics carry an **L** badge on the artwork; clicking it opens the same
overlay directly on a read-only lyrics face. Clicking enlarged artwork
flips between the artwork and lyrics faces when both are available.
Optional synced lyrics enrich the displayed text, while plain lyrics
remain the source of truth for whether the lyrics surface appears.

### Seek bar — *iso-iTunes*
Click or drag on the progress bar to seek. The clickable hit area
extends above the visual bar so the target isn't a one-pixel hairline.

### Shuffle and repeat — *iso-iTunes*
Shuffle and repeat toggle buttons sit in the now-playing tile.
Shuffle cycles through Off → Pure → Smart (see *Smart Shuffle*
below) and the chosen mode is persisted across restarts. Repeat
cycles through Off → Repeat-One → Repeat-All; repeat state is
session-only and not persisted.

### Smart Shuffle — *Sustain-native*
A "Smart" third state on the transport's shuffle button chooses each
next track as a *continuation* of the one playing now, rather than
jumping at random. It is a sequencer, not a recommender, and it does
no learning: in a hand-curated library every track is already liked,
so the only question worth answering is whether one track *follows*
another well — a largely objective, perceptual judgement.

Each candidate is scored against the currently-playing track by a
fixed, transparent perceptual metric — a masked weighted sum of
per-feature similarities. Metadata terms: genre (IDF-weighted, so a
shared rare genre counts for more than a shared ubiquitous one), tempo
(log-scaled with octave folding, so 90 and 180 BPM match), musical key
(circle-of-fifths proximity plus mode), release year (era of
creation), date added (era of discovery), grouping, composer,
duration, and artist identity. When the track has been through audio
analysis, six **acoustic continuity** terms join the sum: loudness
(integrated LUFS, the heaviest-weighted feature), onset density
(rhythmic busyness, separating a sparse 120-BPM ambient piece from a
busy 120-BPM drum-and-bass track), spectral brightness (dark↔bright
band-energy shape), tonalness (pitched↔noisy), low-band variation (the
"kick-drum check"), and dynamic range. A feature missing on either
side — including the acoustic ones on a not-yet-analysed track — is
masked out rather than guessed, and a coverage term keeps a
two-feature match from outscoring a ten-feature one.

Loudness is also a hard, **asymmetric guard**: a candidate whose
short-term loudness peak sits far above the playing track's is excluded
outright (going from a quiet master into a brickwalled one startles),
while the quieter direction — a natural breakdown — is tolerated much
further. The guard prunes before the candidate pool is formed, so no
Exploration setting can rescue a catastrophic loudness jump. Small
candidate-side nudges (a gentle rating prior) and penalties
(recency/fatigue, a same-artist-streak guard, an anti-album-walk-back)
shape the final score. The surviving candidates form a bounded pool
and one is softmax-sampled; both the pool width and the temperature
are driven by the user-facing **Exploration** preset (Focused /
Balanced / Adventurous). Picks are deterministic given identical
inputs, so the `SUSTAIN_LOG_SMART_SHUFFLE=1` debug trace — a full
per-feature decomposition of every pick — is reproducible after the
fact.

The genre IDF weights, the cached per-track acoustic features, and the
library-derived robust normalization ranges the acoustic terms need are
prepared in a small **index** recomputed on a background worker thread —
milliseconds of work, but genuinely library-dependent. The rebuild is
event-driven: it runs on the events that actually change the index — a
library scan completing, audio-analysis coverage growing, and app launch
— and the worker coalesces overlapping requests, so there is no cadence
to schedule and nothing to press. The Shuffle preferences tab reports
the index state and the analysis coverage the acoustic terms have to
work with; the index is persisted alongside the library database so it
survives restarts. There is no cold-start gate: Smart Shuffle works from
the first track, degrading gracefully on tracks with sparse metadata or
no audio analysis.

Smart Shuffle only applies to library-wide queues. Single-album or
single-playlist playback uses Pure shuffle even when Smart is the
persisted setting, because "what follows this well from the whole
library" is not a meaningful question inside a closed set the user has
already chosen.

### Up Next queue — *iTunes-adjacent*
A `Play Next` action inserts selected tracks at the head of the curated
Up Next region (play immediately after the current track). A separate
`Add to Queue` action appends to the bottom of that region. Curated
tracks always play before the source continuation, so adding a song
while playing the full library never buries it behind thousands of
implicit successors.

### Queue popover — *Sustain-native*
Right-clicking the transport **Next** button opens an autohiding arrow
popover anchored to it. The list shows every curated Up Next track,
followed by a read-only peek at the next ten tracks from the source
continuation; the full library playthrough remains internal rather than
becoming a thousands-row browser. Each row is a two-line cell — title
over artist — with artwork or the neutral missing-artwork tile on the
left and striped backgrounds matching the library tables. Double-clicking
any row starts that track within the existing queue. Curated rows reorder
by drag-and-drop and evict via a cross that fades in on hover; both edits
update the live queue immediately. The list scrolls inside the popover once
it runs past about a dozen tracks, and click-outside or **Escape** closes
it. Under Smart Shuffle the list shows curated tracks and any successors
already chosen on demand; when both are empty, the popover explains that
Smart Shuffle will choose the next track. Sustain opts for this
Next-anchored popover over a sidebar "Queue" item so the queue sits next
to the playback it controls without growing the LIBRARY section.

### Album-scoped play — *iTunes-adjacent*
Triggering Play or Shuffle Play from an album (header button, expanded
track double-click) scopes playback to that album only. Normal album Play
disables shuffle; Shuffle Play explicitly enables Pure shuffle.

### MPRIS / media keys — *Sustain-native*
A D-Bus MPRIS2 service exposes playback controls to the desktop:
play/pause/next/prev media keys, the GNOME Now Playing widget, and any
MPRIS-aware lock screen all drive Sustain transparently. The bus name
is derived from the resolved database path so dev and installed builds
don't collide.

---

## Audio analysis

Sustain derives several signals from each track's audio content — BPM,
musical key, and a heavier full-decode "audio analysis" pass that
yields the waveforms and the perceptual acoustic features Smart Shuffle
uses — and stores them alongside the rest of the track's data in
SQLite. Analysis is paced and runs out of band of playback; freshly
imported tracks are picked up on the next sweep without any user
prompt.

### BPM detection — *Sustain-native*
A tempogram estimator over the track's beat envelope, octave-normalized
into the configured `[min_bpm, max_bpm]` band (default 70–170 BPM). It
reads a **centered** window of the track — the middle two minutes —
rather than the opening seconds, where intros (especially in electronic
music) are too sparse and tame to read a steady tempo from. The estimate
fills `tracks.bpm` only when SQLite has no value for that field —
analysis supplies missing data, it never overrides a value imported from
a file tag or set by the user. The BPM column ships visible by default
and feeds the `BPM` smart-playlist field for tempo-aware rules.

### Musical key detection — *Sustain-native*
Estimates the song's tonal centre via chroma analysis over the same
centered window as BPM, and stores one of the 24 major/minor labels in
`tracks.musical_key`, again only when SQLite has no value for that
field. The Music Key column ships hidden by default; surface it through
the column selector. The same field is exposed to smart-playlist rules
for harmony-aware sets.

### Audio analysis — *Sustain-native*
A single heavy decode pass produces, as byproducts of the one decode,
both a coarse preview waveform and a detailed colour waveform, plus the
perceptual acoustic features (integrated and short-term loudness,
loudness range, onset density, low/mid/high band-energy ratios, low-band
variation, and tonalness) that Smart Shuffle's continuity terms and
loudness guard consume — and, off the same decode, the track's BPM and
key. The pass scales to track length: a normal-length track is measured
whole, while a **long** track (over 15 minutes — classical movements, DJ
mixes, podcasts) has its acoustics measured over a centered 8-minute
sample and skips the waveform entirely, keeping the working set bounded
instead of decoding a multi-hour file into memory. The preview backs the
playback seek bar; the detail waveform is held in SQLite for future
DJ-export targets; the acoustic features are stored per track and cached
into the Smart Shuffle index. Because they all come from one decode, a
single toggle governs the whole pass.

### Background analysis scheduler — *Sustain-native*
The Analysis tab in Preferences exposes three toggles — BPM / Key /
Audio — that gate which capabilities the background sweep requests.
Because the Audio pass yields BPM and key off the same decode, turning
it on forces the BPM and Key toggles on and locks them. Turning Audio
off stops all three background capabilities together, so cancelling the
heavy pass cannot accidentally start BPM and Key queues. With any toggle
on, a paced multi-worker pool walks `tracks_needing_analysis` and runs
only the missing capabilities per track; tracks whose value is already
populated (whether from prior analysis or from a file tag at import) are
skipped. Worker count and CPU/IO priority follow the Background resource
usage slider in the same tab; on Intel hybrid CPUs the polite presets
(Innocuous, Balanced) additionally pin their workers to the efficiency
cores, keeping background analysis off the performance cores playback
and the UI want — a no-op on non-hybrid machines.

The same pool also drains an explicit queue populated by the
per-playlist and per-track **Analyze** submenus, so a one-off
capability can run on a chosen set even with the global toggle off.
Progress and final outcome surface through the status-bar
notification lane.

---

## Track metadata

### Get Info dialog — *iso-iTunes*
A multi-tab editor (Details, Artwork, Lyrics, File). The Details tab
edits title, artist, album, album artist, composer, grouping, genre,
year, track number/total, disc number/total, compilation flag, BPM,
key, and comments, plus the 5-star rating and a play-count reset button.
The File tab shows path, duration, bitrate, sample rate, and channels.
The Artwork tab shows the embedded cover (or a missing-art placeholder)
with add and remove actions. The Lyrics tab shows the raw lyrics text.

Opening Get Info on a track is `Ctrl+I` or the row context menu.
Previous/Next buttons walk the displayed track order without closing the
dialog, committing edits before navigation. `Ctrl+[` and `Ctrl+]` trigger
the same actions.

With multiple tracks selected, the same entry points open a conservative
batch editor. Every editable metadata field is opt-in: unchecked fields
preserve each track's existing value, while a checked blank deliberately
clears that field across the selection. Per-track artwork, lyrics, ratings,
and listening statistics remain outside the batch editor.

### Inline rating — *iso-iTunes*
The Rating column in the table accepts clicks directly: click a star to
set 1–5, click the current rating to clear.

### Inline cell editing — *iso-iTunes*
In the Songs table, a single click on an editable cell of the
**already-selected** row opens an inline editor seeded with that field's
current value. A click on an unselected row just selects it (a second,
deliberate click then edits), and a double-click still plays — the edit
never starts on the first press of a double-click. Editable columns are
Track Name, Artist, Album, Genre, Year, BPM, Key, and Track #; other
columns (Bitrate, Type, Duration, Plays, dates, …) ignore the click, and
Rating keeps its own star widget. **Enter** commits, **Escape** cancels,
and **Tab** / **Shift+Tab** commit and move to the next / previous
editable cell in the row; clicking away commits. Edits travel the same
`UpdateMetadata` write path as the Get Info dialog, so SQLite stays
authoritative and the file tag is mirrored.

### Tag mirroring — *iso-iTunes*
When the user edits metadata in Sustain, the change is written to the
file's native tag format as a courtesy to other tools:

- MP3 / WAV — ID3 (including POPM for ratings)
- Ogg / FLAC — Vorbis comments
- MP4 / M4A — MP4 atoms

Listening statistics (play count, skip count, last played, last skipped)
are **never** written to file tags. They live exclusively in SQLite.
Shared frames like ID3 POPM are written carefully so existing
`play_counter` data belonging to other applications is preserved.

### Background metadata retrieval — *iTunes-adjacent*
The Online tab in Preferences exposes three independent toggles —
Artwork / Tags / Lyrics — that gate which capabilities a paced
background worker requests for tracks missing the matching data.
Providers:

- **Artwork** — MusicBrainz + Cover Art Archive lookup, falling back
  to AcoustID acoustic fingerprinting when the embedded tag set is
  too sparse for a confident text match.
- **Tags** — MusicBrainz fills missing fields (title, artist, album,
  album artist, year, track number, genre…) from a matched release.
- **Lyrics** — LRClib lookup, preferring synced LRC when available
  and falling back to plain text.

The worker is intentionally conservative: capabilities are
missing-only (a track that already has artwork, a populated field,
or stored lyrics is not contacted), every attempt is stamped so
the next sweep does not re-fetch the same track, and per-host rate
limits hold network use polite even on a fresh library.

Each capability also has a manual entry point. The Get Info Artwork
tab can trigger an immediate lookup, and the per-playlist and
per-track **Retrieve** submenus run the chosen capability against a
target set independent of the global toggles. Unlike the background
sweep, a manual retrieval ignores the per-track attempt stamp, so it
re-contacts tracks a previous pass left empty — but it still respects
the "missing-only" rule (a track that already has the data is skipped)
and never overwrites an existing value.

### Artwork cache — *Sustain-native*
Embedded artwork is decoded once and cached in SQLite. Now Playing,
the Albums grid, and Get Info all draw from the same cache; editing or
clearing artwork invalidates the cache so every surface refreshes.

---

## Ratings and listening statistics

All four counters below survive restarts, never depend on file tags
existing, and feed both the table and the smart-playlist rule engine.

### 5-star rating — *iso-iTunes*
Editable inline in the table and in Get Info. Persists to SQLite *and*
to the file's native rating frame.

### Play count — *iso-iTunes*
Incremented when a track plays past a completion threshold (not on
every start). Reset button exists in Get Info. SQLite-only.

### Skip count — *iso-iTunes*
Incremented when the user skips a track before the completion
threshold. Column is hidden by default; surface it through the column
selector. SQLite-only.

### Last played — *iso-iTunes*
Timestamp of the most recent threshold-crossing playback. SQLite-only.

### Last skipped — *iTunes-adjacent*
Timestamp of the most recent pre-threshold skip. Column hidden by
default. SQLite-only. iTunes tracked the count but not the timestamp;
Sustain stores both so smart-playlist rules can use it.

---

## Search, sort, and columns

### Search bar — *iso-iTunes*
Top-bar search filters the active view in real time across title,
artist, album, album artist, composer, genre, and file path. Search is
case- and accent-insensitive and whitespace-normalized. The current search string
is persisted across restarts.

### Column sorting — *iso-iTunes*
Click a column header in the Music or playlist track table to sort by it;
click again to reverse direction. Albums view is grid-based and does
not sort by columns.

### Column customization — *iso-iTunes*
Column visibility, order, and width are user-customizable via the
column header menu and the resize handles. Layout is persisted in
SQLite. Skips, Last Skipped, Music Key, and Lyrics ship hidden by default.
The sortable Lyrics column displays **Yes** or **No** according to whether
the track has non-blank plain lyrics.

### Context-sensitive search scope — *iso-iTunes*
The search bar filters whatever is currently visible: full library in
Music, the active playlist in a playlist view, album-level fields in
Albums.

---

## Window, chrome, and theming

### Integrated top bar — *iTunes-adjacent*
Sustain replaces the standard GTK title bar with a single top strip
that holds the transport buttons, volume slider, now-playing tile, and
search. The bar is intentionally taller than default GTK chrome so the
controls are large enough to use without zooming.

### Custom window frame — *Sustain-native*
Because the title bar is replaced, Sustain also paints its own window
chrome: a soft drop shadow when the window is floating (removed when
maximized or fullscreen) and explicit resize handles on every edge and
corner. The Preferences window uses the same frame style.

### Status bar with notifications lane — *iTunes-adjacent*
The bottom bar shows total track count, total play duration, and total
library size on disk on the left, and a single notification lane on the
right. Every background-task update, command outcome, and async
tag-write result flows through the same lane via `NotificationCenter`.
The lane owns its own auto-dismiss and animation; producers never poke
a status widget directly.

### Background task cancellation — *Sustain-native*
While any background task (scan, import, organize) is running, the
status bar shows a Cancel button next to its spinner. Cancellation is
cooperative — the current file finishes and the worker exits cleanly.

### Native light/dark theme — *Sustain-native*
Sustain follows the system color scheme. There is no in-app theme
picker by design; light and dark are first-class and identical in
quality.

### System accent color — *Sustain-native*
GNOME's accent color is honored for selection highlights, buttons, and
focus rings. Changing the system accent updates Sustain immediately.

---

## Track context menu — *iso-iTunes*

Right-clicking a track (or selection) in the Music view or a playlist
view exposes the following actions, separated into visually distinct
groups. Rows backed by keyboard shortcuts show the registered shortcut
at the right edge of the menu:

- **Add to Playlist** — submenu showing all playlists, nested by folder
- **Play Next** — insert at head of the Up Next queue
- **Add to Queue** — append to the tail of the Up Next queue
- **Get Info** — open the multi-tab editor (`Ctrl+I`)
- **Show Album** — switch to Albums view, reveal the album
- **Copy** — copy the audio file itself
- **Show in folder** — open the system file manager at the file's
  location (`Ctrl+R`)
- **Analyze** — submenu (BPM / Key / Audio / All) running the
  chosen analysis pass on the selected tracks; per-capability items
  are insensitive when the matching global toggle is on
- **Retrieve** — submenu (Lyrics / Tags / Artwork / All) running the
  chosen online retrieval pass on the selected tracks; same
  insensitive-when-globally-covered policy
- **Remove from playlist** — when invoked from a playlist view; removes
  from that playlist only, leaves the track in the library
- **Remove from library** — delete the library record only, leave the
  file on disk
- **Move to Trash** — delete the library record and send the file to
  the system trash

---

## Preferences

The Preferences window currently exposes:

- Library folder picker (with validation)
- Managed-mode tickbox
- Manual library scan trigger
- Analysis tab: BPM / Key / Audio background toggles
- Online tab: Artwork / Tags / Lyrics background toggles

Settings persist to `~/.config/sustain/settings.toml`.

### Background resource usage slider — *Sustain-native*

A three-stop slider in the Analysis tab — Innocuous / Balanced /
Aggressive — controls how many worker threads the background analysis
pool spawns and at what nice + ionice priority they run. The default
is Balanced (≈ half the available cores, mid-low priority). A caption
beneath the slider previews the worker count for the current
selection on this machine. Moving the slider tears down the running
pool and respawns it under the new preset; in-flight tracks finish
naturally before the swap. Settings live in the
`[background_jobs]` section of `settings.toml`.

### Library-backup guidance — *Sustain-native*

The About tab names the folder that holds Sustain's SQLite library
database — the source of truth for ratings, play counts, playlists, and
every metadata edit that never reached the audio files — and tells the
user to back it up alongside their music. The path shown is the one
resolved at runtime, so it stays correct under the `--database` /
`--local-scope` developer flags, and the label is selectable for easy
copying.

---

## Keyboard shortcuts — *iso-iTunes*

Global shortcuts are wired as GTK application actions and listed in the
in-app shortcuts overlay (`Ctrl+?`). Shortcuts that overlap text editing
yield while a text entry has focus.

| Shortcut         | Action                                |
| ---------------- | ------------------------------------- |
| `Space`          | Play / pause toggle                   |
| `Ctrl+Left`      | Previous track                        |
| `Ctrl+Right`     | Next track                            |
| `Ctrl+L`         | Jump to the currently playing track   |
| `Ctrl+N`         | New playlist                          |
| `Ctrl+Alt+N`     | New smart playlist (opens editor)     |
| `Ctrl+F`         | Focus the search bar (select-all)     |
| `Ctrl+A`         | Select all tracks in the visible list |
| `Ctrl+I`         | Get Info on the current selection     |
| `Ctrl+R`         | Reveal the selected track in Files    |
| `Ctrl+,`         | Preferences                           |
| `Ctrl+?`         | Keyboard shortcuts overlay            |
| `Ctrl+W`         | Close window                          |
| `Ctrl+Q`         | Quit Sustain                          |

Inside Get Info, `Ctrl+[` and `Ctrl+]` navigate to the previous / next
track in the active list while committing pending edits.

System media keys (Play, Pause, Next, Previous) are routed through
MPRIS and work globally without focus.

---

## Device sync — *Sustain-native*

A **DEVICES** section in the sidebar lists connected USB sticks and SD cards.
Selecting one opens its sync panel in the main content column: a tick-list of
playlists and smart playlists to send to the device, the on-drive format, and a
Sync button. The same GUI-driven workflow iTunes used for iPod sync, generalised
to any removable drive.

Sustain owns the sync state. A device is recognised across sessions by a
generated `.sustain-device-id` marker written at its root (with the filesystem
volume id as a fallback if the marker is deleted), so the panel reopens with the
playlists you ticked for that device pre-filled. The marker is only written on
first sync — Sustain never touches a device until you ask it to.

Re-syncing is **incremental**. Per device, Sustain keeps a manifest of what it
last wrote (track → on-device path + content fingerprint), resolves the ticked
playlists to a track set (smart playlists are re-evaluated every sync), diffs it
against the manifest and what is actually on the drive, and copies only what
changed. The panel shows the plan — to copy / update / unchanged / to remove —
before you sync; removing tracks no longer selected requires ticking an explicit
confirmation. Progress and the outcome flow through the status-bar notification
lane.

A disk-occupation meter in the panel's footer shows how much of the drive the
selection will occupy, **stacked by genre**: the largest genres each get a colour
derived from the system accent, everything past the top eight folds into a single
grey "other" segment, and hovering a segment pops up its genre and size. The meter
turns red when the selection would not fit.

Three on-drive formats, chosen per device:

- **Playlists as `.m3u8`** — one deduplicated `Music/Artist/Album/NN Title.ext`
  tree plus one UTF-8 `.m3u8` per playlist. For phones and players that read
  playlists.
- **One folder per playlist** — a folder per playlist with real audio copies
  (a track in three playlists is copied three times), no `.m3u`. Names are
  capped at 32 characters and per-track positions stay stable across syncs so
  adding tracks does not reshuffle the folder. An optional per-folder file cap
  (64 / 128 / 256 / 512, off by default) splits oversized playlists into
  numbered subfolders. For folder-navigating car stereos.
- **Pioneer (Rekordbox / XDJ)** — a full Rekordbox export that Pioneer CDJ/XDJ
  hardware (and Rekordbox itself) reads directly off the USB, not just copied
  audio. Sustain writes Pioneer's on-device database
  (`PIONEER/rekordbox/export.pdb`) plus per-track ANLZ analysis files
  (`ANLZ0000.DAT` / `.EXT`) under `PIONEER/USBANLZ/`. Each track carries its
  BPM, musical key, a constant-tempo beat grid, both monochrome and colour
  waveforms (the small browse preview and the full detailed overview), star
  rating, genre and year — all from Sustain's own analysis pipeline and
  library, not re-read from the files. The panel shows how many tracks in the
  selection are missing BPM / key / waveform analysis and offers to run it
  before export. Each track's embedded cover art is rendered to the 80×80 and
  240×240 JPEG thumbnails the XDJ shows in its browse and now-playing screens,
  written under `PIONEER/Artwork/00001/` and linked from the database;
  identical covers (an album's shared art) are stored once. This is the full
  set of analysis data Pioneer gear normally only receives from Rekordbox
  itself — to the maintainer's knowledge, no other Linux application produces
  it. (Hot cues and memory cues are not written.)

Android (MTP) transport is not yet implemented — only mounted block devices
(USB sticks, SD cards, external SSDs) are synced today.

---

## Single-instance enforcement — *iso-iTunes*

A second Sustain process targeting the same library database is
refused on startup. The first instance's window is raised and focused
instead. The lock is held on a sidecar `.lock` file next to
`library.sqlite`; the GTK application ID is derived from the resolved
database path so dev builds and installed builds don't compete for the
same name.

---

## Key locations

- Config: `~/.config/sustain/settings.toml`
- Database: `~/.local/share/sustain/library.sqlite`
- Lock file: `~/.local/share/sustain/library.sqlite.lock`

---

## Features to come

Checkout the [issues backlog on github](https://github.com/open-sustain/sustain/issues)
