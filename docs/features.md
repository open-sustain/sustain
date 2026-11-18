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
mis-reported as missing; so does a scan that could not read part of the
folder. Unchanged files on a rescan are detected cheaply via
`mtime + size` rather than re-parsed. A file whose size or mtime changed is
parsed again for its technical properties (duration, bitrate, size,
embedded-artwork presence); its library fields stay as SQLite holds them.
In **Keep my library organized** mode, a scan also repairs MP3 tags it
recognizes as malformed (an invalid timestamp or undecodable text) so other
applications can read them; in **Don't touch my files** mode it never
writes to a file.

### Drag-and-drop import — *iso-iTunes*
Dragging files or folders from Files (or any GNOME-compatible source) onto
the Music view imports them into the library. Folders are walked
recursively for supported audio formats. The drop zone shows an active
state while a drag is hovered.

Behavior follows the active library mode:

- **Keep my library organized** — files are copied into the managed
  `Artist/Album/NN Title.ext` layout, skipping any file byte-identical to
  one already in the library, so re-dropping an unchanged file doesn't
  ingest it twice. The comparison covers the whole file, tags included: a
  copy whose tags differ (for example, after editing the managed copy's
  metadata) is imported again.
- **Don't touch my files** — files are indexed in place. A file that
  lives outside the configured library folder is left out, since
  reference mode never moves or copies.

Drops while another library task (scan, import, or organize) is running
are rejected by the runtime rather than racing. Managed copies report
completed/total progress in the status bar. Cancelling keeps files that have
already finished copying and records those completed tracks in the library.

An import skips a file it cannot bring in (unreadable, tags it cannot read,
a copy that fails, outside the library folder in reference mode) and carries
on with the rest; only a failure of the library itself, such as its database
refusing a write, stops it. At the end, the status bar counts the files left
out, and a window lists each one with its folder and the reason, so they can
be found and fixed. *Sustain-native*

### Hard-link move primitive — *Sustain-native*
Managed-mode organization uses a same-filesystem metadata move: retain an
open source descriptor, resolve destination directories through retained
no-symlink directory handles, hard-link that exact file to the destination,
then unlink the source through a verified handoff. It refuses to
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
moving files, with hidden hard-link recovery handles for the exact files being
moved. On the next startup Sustain reconciles the journal so a
crash mid-batch can't desync SQLite from the filesystem. Reconciliation
runs unconditionally during library-service initialization, before any
tracks reach the UI, so before the first frame is drawn every move of an
interrupted batch is either completed or undone, according to how far it
got, and SQLite agrees with the files on disk.

### Supported audio formats — *iso-iTunes*
Scans and imports recognize MP3, FLAC, Ogg Vorbis (`.ogg`, `.oga`),
Opus, the MP4 family (`.m4a`, `.m4b`, `.mp4`), WAV (`.wav`), and AIFF
(`.aif`, `.aiff`, and AIFF-C `.aifc`). Files with other extensions are
skipped silently during library walks.

### Duplicate detection on managed import — *iTunes-adjacent*
When adding external files in managed mode, Sustain skips files whose
bytes are identical to a file already in the library (a whole-file hash,
with file size as a cheap pre-filter); nothing about the hash is stored.
A track's identity in the library is its relative path, not its content.
Plain in-place scans do **not** hash file contents.

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

Track identity is keyed to a file's path inside the library, so moving or
renaming a file outside Sustain — in a file manager, or with a tagger that
rewrites filenames — makes the next scan treat the new path as a new track
while the original row stays visible as missing, retaining its rating,
playlist membership, and listening statistics. To re-attach it, first remove
the extra row the scan added for the new path with **Remove from Library**,
which leaves the file on disk. Then use **Locate Missing Track** on the
missing row and pick the moved file. A file can belong to only one row, so
locating it while the extra row still holds it is refused. Anything recorded
on the extra row in the meantime goes with that row.

### Replace low-bitrate audio from YouTube — *Sustain-native*
For a present track at or below 192 kbps, the row context menu exposes
**Retrieve… → Audio from YouTube**. Paste one YouTube video URL and Sustain
asks the separately-installed `yt-dlp` and FFmpeg tools for the best available
audio. Before publishing anything, Sustain rejects unsupported output,
material duration mismatches, unknown or junk-level bitrate, and downloads
worse than the current file. A successful swap preserves the track's SQLite
identity, editable metadata, rating, listening statistics, and playlist
membership; the previous file moves to the system trash only after the new
file and SQLite row are durable.

This optional integration is available in the Debian package when `yt-dlp`
and FFmpeg are installed. It is not available inside the current Flatpak
sandbox, which cannot execute host-installed tools.

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
Tiles group by album title and album artist (the track artist when no
album artist is set; compilations without one share a bucket), so two
releases with the same title and artist share a tile, which shows the
first year any of its tracks carries. Clicking a tile opens its detail
panel below the row, whose Play and Shuffle buttons play the album in
isolation; a track there plays from that track. The grid intentionally
searches album-level fields only (title, artist, year), not individual
track titles.

### Duplicates — *iTunes-adjacent*
The Duplicates entry under LIBRARY runs a deferred, off-thread scan and
shows only candidate groups. Loose matching folds case, diacritics, and
whitespace for artist + title, and still keeps apart two tracks whose
durations differ by more than 30%, or where only one is marked live (the
word "live" in its title or album). A **Strict matching** toggle also
requires the same album and a duration within two seconds; two tracks
that both lack a duration pass that check. Groups stay adjacent and
visually banded.

When both tracks of a candidate pair carry an analysed waveform, the scan
also compares their audio — *Sustain-native*. It aligns the two
loudness and band envelopes while allowing for encoder padding, trailing
silence and a slightly different clock. A pair whose audio is clearly
another performance, or another edit of the same recording (an extended
mix, a radio edit), is no longer suggested, even though artist and title
match. Pairs without waveforms (audio analysis off, tracks over fifteen
minutes) keep their tag-only suggestion. Verdicts are remembered across
scans until either track is analysed again.

Selecting two or more rows in Songs or Duplicates exposes **Consolidate
to single track**. Double-clicking a Duplicates row plays it normally,
so candidate versions can be compared by ear first. The dialog lays the
duplicates out as one column per track, headed by that track's identity
and path. Every survivor decision below is a self-labelling box — the
field name in bold over its value — that draws an accent border when it
holds the value that will be kept, so the window shows at a glance what
is preserved from what. The audio file (codec, bitrate, duration, size),
artwork, rating, and each editable metadata field are all selectable
this way. The audio file defaults to the highest quality (highest
bitrate, lossless breaking a tie) but stays overridable, so a 16-bit
FLAC can be kept over a larger 24-bit one; metadata fields default to a
cherry-pick of populated values, with the oldest populated year
preferred and the compilation flag taken from an original release when
one is in the group (a "not a compilation" value first, then an unset
flag, and "compilation" only when no copy says otherwise); the rating defaults to the highest available; and a per-column
"Track N" button takes every tag and the rating from one track at once.
Embedded artwork is previewed asynchronously and
presets the highest-resolution available image. Play count, skips, and
date added are not choices — counts are summed and the oldest date is
kept — so they appear as plain non-selectable cells; last-played and
last-skipped keep their most recent values. Playlist membership is
rewritten in place while preserving order and collapsing consecutive
repetitions introduced by the merge.

If any selected file is missing from disk — even one whose row was never
flagged because it was never played — the dialog refuses to open and
explains which track to restore or remove first, rather than stranding
the merge in a half-written state. Before deleting any duplicate
pathname, Sustain writes and verifies a staged survivor, keeps
hard-linked recovery copies, commits SQLite and playlist rewrites
atomically, and crosses the durable database barrier. An interrupted
merge is recovered before library hydration on the next launch. The view
is transient: it is never restored or scanned at startup, so it does not
affect cold-start time.

**Merge Verified Duplicates…** in the Duplicates header — *Sustain-native*
— offers the groups whose audio shows every pair of copies to be the
same recording. Groups that only the tags suggest are left for manual
review. A preview lists each group with the file it keeps (codec,
bitrate, duration, size and path), how many copies it removes, and the
value it keeps for every editable metadata field the copies disagree on;
the rating and artwork follow the stated rule (highest rating,
highest-resolution artwork) and are not itemised. Any group can be
unchecked, and one confirmation starts the batch. Each group merges
exactly like **Consolidate to single track** with its preset choices:
best audio, highest rating, highest-resolution artwork, cherry-picked
metadata, summed counts. Before each merge Sustain re-reads the tracks
and compares their current waveforms again. A group whose tracks were
edited, re-rated or replaced since the preview, or whose audio no longer
verifies, is left alone; a play in the meantime does not count as a
change. The batch runs in the background, one journaled merge at a time;
**Cancel** stops it before the next merge. The final notification
reports how many groups merged and how many tracks they removed, then
counts the groups left alone by reason (changed since the preview, no
longer verified, failed, cancelled).

### Statistics — *Sustain-native*
The last entry under LIBRARY opens a single scrollable page of
whole-library diagnostic charts coloured from the system accent colour.
A row of headline figures opens it — tracks, total library duration,
size on disk, and total plays — its tiles reflowing onto fewer per line in a
narrow window. Large figures are shown whole with their thousands
grouped; the small fractional weights of multi-genre tracks keep a
tenth, so a real contribution never reads as zero:

- **Genre distribution** — a donut of the share of tracks per genre, each
  slice its own hue, rotated from the accent around the colour wheel (the largest dozen
  genres individually, the long tail folded into one *Other* bar;
  untagged tracks counted as *Unknown*). Slash-delimited tags such as
  `House/Deep` are split into equal fractional contributions so each
  component genre is represented without double-counting the track.
  Spellings that differ only in case (`Deep House`, `Deep house`) are one
  genre, shown under the spelling most tracks use.
- **Quality distribution** — a donut of the share of tracks per bitrate
  range (≤ 128, 129–255, 256–320, > 320 kbps), a hue per range, the best
  range in the accent's own hue.
- **Most played genres** — ranked bars of the top five by total play
  count, a hue per genre, using the same fractional genre split as the
  distribution chart.
- **Most liked genres** — ranked bars of the top five by total rating
  points across rated tracks, a hue per genre, so broad library support carries weight instead
  of letting marginal genres dominate; slash-delimited genres split rating
  points without lowering the per-genre average, and zero-star tracks are
  excluded per the rating-as-exclusion convention.
- **Release years** — track counts per release decade; a year before
  recorded sound (1860) is a mistyped tag and counts as *Unknown*.
- **Year added** — track counts per calendar year a track entered the
  library.

Both histograms stack each column by genre in the genre donut's own
colours — largest genre at the foot, the folded *Other* tail on top — so
how the library's genres shift across decades and years reads straight
off the chart; hovering a column lists its genres with their counts and
shares.

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
for the LIBRARY entries — bring the sidebar back to change view (actions
such as **Show Album** still switch views). Dragging the divider resizes
the sidebar, and the width is kept across launches.

### Collapsible playlists section — *iTunes-adjacent*
The PLAYLISTS header is a disclosure row: it carries a caret (▾ open,
▸ folded) and the whole row is the toggle. Clicking it folds the
section, hiding its rows. With the header focused, **Left** folds it and
**Right** unfolds it. The fold state is persisted across launches. The
LIBRARY heading is static. Preferences can hide the Duplicates and
Statistics entries.

### Selection persistence — *Sustain-native*
The sidebar's active row (Music, Albums, Statistics, or a specific playlist), its
collapsed state, and the fold state of the PLAYLISTS section are restored
on next launch. Transient views such as Duplicates
and connected devices are deliberately not restored.

### Window size — *iso-iTunes*
The main window reopens at the size it had when it was closed, and maximized
if it was maximized; un-maximizing it returns to the last un-maximized size.

---

## Playlists

### Regular playlists — *iso-iTunes*
Named, ordered, user-curated track lists. Duplicates are allowed. Tracks
can be reordered within a playlist by drag, individually removed via
context menu, and added from any view by drag or context-menu.
Deleting a playlist does not remove its tracks from the library.

### Playlist folders — *iso-iTunes*
Playlists and folders can be nested inside folders. Sidebar drag-and-drop
moves entries between folders. Every sidebar playlist, smart playlist, and
folder has a right-click **Rename** (inline) and **Delete**; deleting a
folder asks for confirmation and removes every playlist, smart playlist,
and folder inside it, while the tracks stay in the library.

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

Date rules `is before` / `is after` name a calendar day. A track's date
is read in the local time zone, the same way the library displays its
dates, and the rule compares the day it falls on: "Date Added is before
2024-01-01" matches tracks the library shows as added on December 31 or
earlier, and "is after 2024-01-01" those it shows as added on January 2
or later.

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
section, and OK/Cancel. Opening a smart playlist and saving it keeps
every rule and limit the user did not change exactly as stored, even a
value outside the range its control can show.

### Playlist header — *iTunes-adjacent*
The playlist view draws a header strip above the track table, the
same height as the integrated top bar. The strip shows the selected
playlist's name in bold next to Play and Shuffle buttons that match the
album-detail header's behaviour, with a muted second line summarising
the visible set as `N songs, X hours/minutes/days`. Search filtering
updates the summary so the count always matches what's drawn below.
An empty playlist, or one whose tracks the search filters out entirely,
keeps its header with a zero summary; a folder selection, which is not
itself a playable track set, shows no header.

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
exactly — an individual Analyze capability greys out while its
background toggle is on (Analyze → All stays available), and Retrieve
greys out only while a run is in flight.

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
different rate so the two never crawl in lockstep. When the track
carries lyrics, a small rounded vintage-LCD **Lyrics** chip sits inline
just after the title; while it is shown the title holds still and fades
out at its right edge instead of scrolling, so the chip stays pinned to
the title rather than chasing a moving target. Clicking on the artwork
opens a centered, round-cornered artwork overlay; clicking the
**Lyrics** chip opens that same overlay directly on a read-only lyrics
face. Inside the overlay the artwork and lyrics faces cross-fade into
each other when both are available, and the lyrics are set large,
light, and centered. Both overlay colours come from the artwork: the
dominant tone tints the surface and the artwork's secondary accent
colours the lyrics, so they read as part of the cover instead of flat
black-on-dark. The overlay has no close button:
clicking anywhere outside it (or pressing **Escape**) dismisses it,
while clicks inside flip between the artwork and lyrics faces. Optional
synced lyrics enrich the displayed text, while plain lyrics remain the
source of truth for whether the lyrics surface appears.

### Seek bar — *iso-iTunes*
Click or drag on the progress bar to seek. The clickable hit area
extends above the visual bar so the target isn't a one-pixel hairline.

### Now Playing waveform — *Sustain-native*
An opt-in View preference ("Show the waveform in Now Playing") replaces
the progress bar with the playing track's waveform once audio analysis
has measured it: small vertical bars, rounded on top, the played ones
in the cover's most colourful noticeable colour, its dominant one when the
cover is all greys (lightened or darkened just enough to show on the top
bar in light and dark themes; the theme's foreground for a track without
artwork) and the rest a neutral gray. It
seeks exactly like the progress bar, appears as soon as the track's
analysis lands, and falls back to the plain bar for tracks without one.
The bars follow loudness in decibels, every track's peak at full height,
spread over the track's own dynamic range: on a brickwalled master the
few decibels it still varies by fill the bar height, so its contour shows
instead of a near-flat line, and a dynamic one keeps its quiet passages
visible.

### Shuffle and repeat — *iso-iTunes*
Shuffle and repeat toggle buttons sit in the now-playing tile.
Shuffle cycles through Off → Pure → Smart (see *Smart Shuffle*
below) and the chosen mode is persisted across restarts. Repeat
cycles through Off → Repeat All → Repeat One, Repeat One drawn with
the single-song glyph; repeat state is session-only and not persisted.
Repeat One replays a track that ends by itself, while Next and Previous
still step through the list.

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

Smart Shuffle applies to the whole library and to regular playlists,
the stable sets a listener returns to. Playing an album, a smart
playlist, search results, the Duplicates groups, or an audio CD uses
Pure shuffle even when Smart is the persisted setting, because those
are narrow or ad-hoc sets the user has chosen for a specific listen.

### Play order follows the list — *iso-iTunes*
Double-clicking a track plays the list it sits in — Music, a playlist, a
smart playlist, search results, the Duplicates groups — from that track
on, in the order the list is sorted; the Play button starts the visible
list from its first track the same way. Re-sorting the list during
playback re-sorts what plays next. With shuffle on, the rolled order
stays put; once shuffle is turned off, playback follows the list's sort
again. Tracks that join the library or the playing playlist meanwhile
are queued after the rest.

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
MPRIS-aware lock screen all drive Sustain transparently. The service
uses the fixed bus name `org.mpris.MediaPlayer2.sustain`.

### Pause when the audio output goes away — *iTunes-adjacent*
When the device Sustain plays to disappears — Bluetooth headphones
switched off or out of range — or another application's priority stream
claims it, the sound server asks players to pause, and Sustain does,
instead of carrying on through the laptop speakers. A notification says
why the music stopped. Playback does not restart on its own when the
device returns; press Play.

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
into a `[min, max]` band (default 76–155 BPM). The band is adjustable in
the Analysis preferences tab — two BPM fields let a user widen it for
very slow or very fast material or narrow it to bias the octave fold —
and it applies to every analysis run, background sweeps and manual
one-off runs alike. Detection reads a **centered** window of the track —
the middle two minutes — rather than the opening seconds, where intros
(especially in electronic music) are too sparse and tame to read a
steady tempo from. That estimate settles the tempo to the beat; the beat
tracker checks it against the kick, which catches a dotted or triplet
figure mistaken for the beat (house reported at two thirds of its tempo),
then measures it to the hundredth over the whole track (see *Beat grid*
below). The measured tempo becomes the track's BPM, replacing
one imported from a file tag or measured before — so the BPM always
matches the beat grid. A BPM the user typed in Get Info or the BPM cell
is theirs: analysis never replaces it, until they clear it. The
measured BPM stays in the library and is not written to the file. The BPM
column ships visible by default and feeds the `BPM` smart-playlist field
for tempo-aware rules.

Tempos are kept to the hundredth of a beat (`118.86`), the precision
Pioneer players beat-match with, and display without needless digits
(`128`, `124.5`). Tags, the Get Info dialog, and the BPM cell accept a
whole or decimal tempo up to 655 BPM; a zero or out-of-range tag value is
ignored at import, leaving the field for analysis. Edits mirror to the
file as precisely as its format allows: Vorbis comments (FLAC, Ogg) keep
the decimal, while ID3 `TBPM` (MP3, WAV, AIFF) and MP4 `tmpo` hold whole beats
and receive the nearest one — the library keeps the exact value either
way. Smart-playlist BPM rules compare whole beats.

Validation of the version 10 tempo estimator (June 2026, before the
beat tracker described below measured tempos to the hundredth), with the
default 76–155 BPM band and a +/-2 BPM tolerance: every private-reference
track whose labelled tempo falls inside the band landed within tolerance
(22/22). The all-core private set scored 84.6% overall because four
deliberately-kept labels sit at or below the 76 BPM floor; GiantSteps
Tempo scored 61.3%, largely because much of its fast 160–185 BPM material
is outside the shipped range and folds to half tempo. With the current
beat tracker, against Rekordbox's own analysis of 3,902 dance tracks,
88.1% of tempos match to within 0.05 BPM and 95.1% to the whole beat.

### Beat grid — *Sustain-native*
Tempo analysis also places a constant beat grid: every beat of the track
at the measured tempo, numbered 1–4 within its bar. The tracker combs the
track's onsets — the kick band and the full band — for the tempo and phase
that fit every beat of the track at once, so the grid does not drift over
a long track. The kick and the snare-and-voice band then tell the beat
from the off-beat, so a song whose kick is syncopated onto the off-beats
still gets its grid on the backbeat. A model learned from human and
rekordbox downbeats decides which beat starts the bar, from where the
timbre changes, where the longer view shifts, and where the music starts.
Grids are
stored in the library and exported to Pioneer drives. Long tracks (DJ
mixes, classical movements, podcasts) get no grid, since no single tempo
fits them. Tempo analysis sets the track's BPM to the grid's tempo
unless the user typed the BPM. If a typed BPM disagrees with the grid by
more than half a beat per minute, the grid is not
exported: an export shows no grid rather than one that contradicts the
track's tempo. Against Rekordbox on the same 3,902 dance tracks, 67% of
grids land within 20 ms of Rekordbox's beats across the whole track. The
rest mostly lock on the off-beat or a quarter beat away. Where the beats
agree, the bar starts on the same beat as Rekordbox's on 85% of 659
library tracks, against 75% before, scored with weights learned without
them.

### Musical key detection — *Sustain-native*
Estimates the song's tonal centre via chroma analysis over the whole
track (the centered two minutes of a track over 15 minutes), and stores
one of the 24 major/minor labels as the track's key when it has none. Sustain remembers where each key came
from: a key it measured is measured again when the detector improves,
while a key imported from the file's tags or typed in Get Info or a Key
cell is kept, until the user clears it. Unlike the BPM, a tagged key is
not replaced: a person or a dedicated tool set it, and may know better.
The measured key stays in the library and is not written to the file; a
key the user typed is mirrored to it. The Music Key column ships hidden
by default; surface it through the column selector. The same field is
exposed to smart-playlist rules for harmony-aware sets.

Keys read from file tags or typed into the Get Info dialog or a Key cell
are accepted in any common notation — `Am`, `Amin`, `A minor`, `G#m`,
`A♭m`, Camelot (`8A`) or Open Key (`1m`) — and stored as one canonical
spelling (`Am`, `F#m`, `Eb`, …), so the same key is never split across
spellings. A key tag that is not a key at all (free text, a URL) is
ignored at import, leaving the field empty for analysis to fill. Typed
text that is not a key is refused.

The product target is harmonic compatibility for Pioneer/Rekordbox-style
filtering rather than exact tonic+mode alone. The validation headline is
therefore **strict-compatible** key rate: exact key, a fifth away, or the
relative major/minor. The detector weighs the share each pitch class
takes of the track's chroma with key templates learned, as a logistic
regression over the 24 keys, from what Sustain's own front end measures in
3,486 human-labelled tracks. Scored on each public corpus with weights
learned from the others, the strict-compatible rate is 81.0% on 604
GiantSteps Key tracks, 74.7% on 1,159 GiantSteps MTG Key tracks and 76.8%
on 1,723 FMAK `fma_medium` clips. rekordbox is a reference rather than
ground truth, and the bar is to be at least as accurate as it is. On the
377 tracks of the maintainer's library whose files carry a key set by
another tool, Sustain's key is strict-compatible with the tag on 89.4% of
them and rekordbox's on 85.7%; Sustain also agrees with rekordbox
strictly on 85.8% of 2,500 library tracks.

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
instead of decoding a multi-hour file into memory. Both waveforms feed
Pioneer exports. They live in a waveform cache beside the artwork cache,
out of the library database, and a track whose waveform the cache lost
is simply analysed again. The acoustic features are stored per track
and cached into the Smart Shuffle index. Because they all come from one
decode, a single toggle governs the whole pass.

### Background analysis scheduler — *Sustain-native*
The Analysis tab in Preferences exposes three toggles — BPM / Key /
Audio — that gate which capabilities the background sweep requests.
Because the Audio pass yields BPM and key off the same decode, turning
it on forces the BPM and Key toggles on and locks them. Turning Audio
off stops all three background capabilities together, so cancelling the
heavy pass cannot accidentally start BPM and Key queues. With any toggle
on, a paced multi-worker pool walks `tracks_needing_analysis` and runs
only the capabilities due per track: an enabled capability is due until
the track has an attempt recorded at the current analyzer version, and
the Audio pass is also due again when the waveform cache has lost the
track's waveform. A value read from a file tag does not make a
capability less due. What a run may replace is the field's own rule:
the measured BPM replaces an imported one but never a typed one, and a
measured key fills an empty key or replaces an earlier measurement but
leaves an imported or typed key alone. Worker count and CPU/IO priority follow the Background resource
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
The Genre field autocompletes against the genres already in the library
(type to filter, Down/Up to highlight, Enter or click to accept) so a
track is tagged with an existing spelling rather than a near-duplicate
that would splinter listening stats and smart-playlist membership.
A genre the library already spells several ways is offered once, under
the spelling most tracks use.
The File tab shows the file kind, duration, size on disk, bit rate,
sample rate, channels, modification date, and location.
The Artwork tab shows the embedded cover (or a missing-art placeholder)
with add, paste, and remove actions. Paste (also `Ctrl+V` on that tab)
takes an image copied from another application, or an image file copied
in a file manager. The Lyrics tab edits the plain lyrics text: changing
or clearing it saves like any other field and is mirrored to the file's
lyrics tag. (Now Playing's lyrics face only displays them.)

Opening Get Info on a track is `Ctrl+I` or the row context menu.
Previous/Next buttons walk the displayed track order without closing the
dialog, committing edits before navigation. `Ctrl+[` and `Ctrl+]` trigger
the same actions.

With multiple tracks selected, the same entry points open a conservative
batch editor. Every editable metadata field is opt-in: unchecked fields
preserve each track's existing value, while a checked blank deliberately
clears that field across the selection. Per-track artwork, lyrics, ratings,
and listening statistics remain outside the batch editor. Its Genre field
offers the library's genres the same way.

### Inline rating — *iso-iTunes*
The Rating column in the table accepts clicks directly: click a star to
set 1–5, click the current rating to clear.

### Inline cell editing — *iso-iTunes*
In the Songs table and in the Playlists view (both regular and smart
playlists), a single click on an editable cell of the
**already-selected** row opens an inline editor seeded with that field's
current value. A click on an unselected row just selects it (a second,
deliberate click then edits), and a double-click still plays — the edit
never starts on the first press of a double-click. Editable columns are
Track Name, Artist, Album, Genre, Year, BPM, Key, and Track #; other
columns (Bitrate, Type, Duration, Plays, dates, …) ignore the click, and
Rating keeps its own star widget. **Enter** commits, **Escape** cancels,
and **Tab** / **Shift+Tab** commit and move to the next / previous
editable cell in the row; clicking away commits. Committing with Enter
or cancelling with Escape leaves keyboard focus on the row, so the arrow
keys carry on through the table. The Genre editor offers the library's
genres as you type, as in Get Info: Down/Up highlight one, and Enter
accepts it, then a second Enter commits. Edits travel the same
`UpdateMetadata` write path as the Get Info dialog, so SQLite stays
authoritative and the file tag is mirrored. When an edit in a smart
playlist changes whether the track still matches the playlist's rules,
the view reflows to add or drop it; the drag-to-reorder handle on a
regular playlist's leading column is independent of the edit gesture.

### Tag mirroring — *iso-iTunes*
When the user edits metadata in Sustain, the change is written to the
file's native tag format as a courtesy to other tools:

- MP3 / WAV / AIFF — ID3 (including POPM for ratings)
- Ogg / FLAC — Vorbis comments
- MP4 / M4A — MP4 atoms

Listening statistics (play count, skip count, last played, last skipped)
are **never** written to file tags. They live exclusively in SQLite.
Shared frames like ID3 POPM are written carefully so existing
`play_counter` data belonging to other applications is preserved. Other
tags are left exactly as they were: an MP3's ID3v1 and APE tags, a
WAV's RIFF INFO list, and an AIFF's native text chunks.

### Background metadata retrieval — *iTunes-adjacent*
The Online tab in Preferences exposes three independent toggles —
Artwork / Tags / Lyrics — that gate which capabilities a paced
background worker requests for tracks missing the matching data.
Providers:

- **Artwork** — MusicBrainz + Cover Art Archive lookup, matched on
  the track's tags. When Cover Art Archive has
  no cover, or no match is found, the album is looked up on Deezer,
  then on the iTunes Search API, by album artist and album title;
  a cover is taken only from a result whose artist and title are the
  album's, so a same-named album by someone else is never embedded.
- **Tags** — MusicBrainz fills missing title, artist, album, year,
  genre, and (for a track with an album) track number, track total, and
  disc number from a matched release. Album artist is not filled.
- **Lyrics** — LRClib lookup, preferring synced LRC when available
  and falling back to plain text.

The worker is intentionally conservative: capabilities are
missing-only (a track that already has artwork, a populated field,
or both plain and synced lyrics is not contacted), every completed
attempt — a match, no match, or a failure — is stamped so the next
sweep does not re-fetch the same track, and per-host rate limits hold
network use polite even on a fresh library. A track that already has
plain lyrics can still be looked up for its timed lines. A provider's
"too many requests" answer is the exception: it stops the run for the
provider's cool-down and leaves the interrupted capability unstamped,
so the track is tried again once the cool-down has passed.

Each capability also has a manual entry point. Now Playing offers an
immediate artwork lookup for a track without a cover, and the per-playlist and
per-track **Retrieve** submenus run the chosen capability against a
target set independent of the global toggles. Unlike the background
sweep, a manual retrieval ignores the per-track attempt stamp, so it
re-contacts tracks a previous pass left empty — but it still respects
the "missing-only" rule (a track that already has the data is skipped)
and never overwrites an existing value.

### Artwork cache — *Sustain-native*
Embedded artwork is cached in SQLite, one image per display size and
source fingerprint, each generated the first time a surface asks for that
size. Now Playing, the Albums grid, and Get Info all draw from the same
cache; editing or clearing artwork invalidates it so every surface
refreshes.

---

## Ratings and listening statistics

All four counters below survive restarts, never depend on file tags
existing, and feed both the table and the smart-playlist rule engine.

### 5-star rating — *iso-iTunes*
Editable inline in the table and in Get Info. Persists to SQLite *and*
to the file's native rating frame. In Get Info the stars also take the
keyboard: Left/Right, Up/Down or `+`/`-` step the rating, `0`–`5` set
it, End gives five stars, and Home, Backspace or Delete clear it. Screen
readers present the stars as a slider valued from unrated to five stars.

### Play count — *iTunes-adjacent*
Incremented once per playback when the track has actually been heard
for half its duration, or for 10 minutes on a track longer than 20
minutes (paused time does not count; this threshold is Sustain's own).
Reset button exists in Get Info. SQLite-only.

### Skip count — *iTunes-adjacent*
Incremented when the user presses Next before the track has reached
the play threshold; reaching the end, Previous, or picking another
track does not count as a skip. Column is hidden by default; surface it through the column
selector. SQLite-only.

### Last played — *iso-iTunes*
Timestamp of the most recent threshold-crossing playback. SQLite-only.

### Last skipped — *iso-iTunes*
Timestamp of the most recent pre-threshold skip (iTunes' Skip Date,
which the iTunes import carries over). Column hidden by default.
SQLite-only; smart-playlist rules can use it.

---

## Search, sort, and columns

### Search bar — *iso-iTunes*
Top-bar search filters the active view in real time across title,
artist, album, album artist, composer, genre, and file path. Every word
typed must be found, each in any of those fields, so `massive mezzanine`
finds an artist's album. Search is case- and accent-insensitive and
whitespace-normalized; a stroked letter or a ligature matches its plain
spelling too, so `royksopp` finds `Røyksopp` and `coeur` finds `Cœur`. The current search string is persisted across
restarts.

### Column sorting — *iso-iTunes*
Click a column header in the Music or playlist track table to sort by it;
click again to reverse direction. The chosen sort column and direction are
remembered across restarts, per view: the Music library and each smart
playlist keep their own, while regular playlists default to their manual
play order. Albums view is grid-based and does not sort by columns.
Text sorts without regard to case, and an accented letter files with its
base letter: `Édith Piaf` sits under E, beside `Edith`, and `Røyksopp`
under Ro, in the track table and in the Albums grid alike.

### Sort-order metadata — *iso-iTunes*
Sustain reads title, artist, album, album artist, and composer sort-order
tags at first import and stores them in SQLite so display names can remain
unchanged while sorting by their intended keys. When a file lacks a sort
field, Sustain generates leading-English-article values such as
`The Beatles` -> `Beatles (The)`. The Library preference for honoring sort
tags is enabled by default; disabling it makes text columns sort by the
display fields only, and the lists re-file the moment the switch flips. The Albums grid files each album the same way, by
artist and then title, so `The Beatles` sits under B there too. Full metadata mirrors preserve stored sort fields where
the active metadata backend can round-trip the standard file tag.

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
The bottom bar keeps the sidebar toggle and Settings entry on the left,
a centred summary in the middle, and a single notification lane on the
right. The summary counts the tracks, play duration, and size on disk of
the set being shown: the library under the current search in Songs and
Albums, the selected playlist (as filtered) in Playlists, and the tracks of
the candidate groups in Duplicates. Every background-task update, command outcome, and async
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

### Interface language — *iso-iTunes*
Sustain speaks the desktop's language. The interface, notifications,
dialogs, the desktop entry, and the AppStream description are translated
into French, German, Spanish, European Portuguese, Italian, Dutch, and
Korean, and fall back to English for any other locale. As with the
theme, there is no in-app language picker: the standard locale
environment (`LANGUAGE`, `LC_ALL`, `LC_MESSAGES`, `LANG`) decides.
Counts follow each language's plural rules, and large numbers use its
thousands separator.

---

## Track context menu — *iso-iTunes*

Right-clicking a track (or selection) in the Music view or a playlist
view exposes the following actions, separated into visually distinct
groups. Rows backed by keyboard shortcuts show the registered shortcut
at the right edge of the menu. The menu follows iTunes; its Analyze and
Retrieve submenus are *Sustain-native* (see *Per-track analysis & online
retrieval*):

- **Add to Playlist** — submenu listing every regular playlist, each
  labelled with its folder path (`Folder / Playlist`)
- **Play Next** — insert at head of the Up Next queue
- **Add to Queue** — append to the tail of the Up Next queue
- **Get Info** — open the multi-tab editor (`Ctrl+I`)
- **Show Album** — switch to Albums view, reveal the album
- **Copy** — copy the audio file itself
- **Show in folder** — open the system file manager at the file's
  location (`Ctrl+R`)
- **Analyze** — submenu (BPM / Key / Audio / All) running the
  chosen analysis pass on the selected tracks; per-capability items
  are insensitive when the matching global toggle is on, All stays
  available
- **Retrieve** — submenu (Lyrics / Tags / Artwork / All, plus Audio from
  YouTube for a low-bitrate track) running the chosen online retrieval
  on the selected tracks; insensitive only while a retrieval run is in
  flight, whatever the background toggles
- **Remove from Playlist** — in a regular playlist's view; removes
  from that playlist only, leaves the track in the library
- **Remove from library** — delete the library record only, leave the
  file on disk
- **Move to Trash** — delete the library record and send the file to
  the system trash. On Linux, Sustain publishes regular audio files directly
  into the Freedesktop desktop trash through retained directory and file
  handles before deleting the library record.

---

## Preferences

Open Preferences from the **Settings** entry in the status bar's
bottom-left, just to the right of the sidebar collapse toggle (flat
chrome — no button outline until hovered or pressed), or with `Ctrl+,`.

The Preferences window currently exposes:

- Library folder picker (with validation)
- Managed-mode tickbox
- Manual library scan trigger
- Analysis tab: BPM / Key / Audio background toggles
- Online tab: Artwork / Tags / Lyrics background toggles
- Views tab: show/hide the sidebar's Duplicates and Statistics rows
  (both on by default), applied live

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

On the maintainer's Ryzen AI Max+ 395 (16 cores), Aggressive analyses
about **4 tracks per second** (300 tracks in 75 s), so a 10,000-track
library is analysed in roughly 42 minutes.

### Library-backup guidance — *Sustain-native*

The About tab names the folder that holds Sustain's SQLite library
database — the source of truth for ratings, play counts, playlists, and
every metadata edit that never reached the audio files — and tells the
user to back it up alongside their music. The path shown is the one
resolved at runtime, so it stays correct under the `--database` /
`--local-scope` developer flags, and the label is selectable for easy
copying.

---

## Command-line maintenance

### Force tag backfill — *Sustain-native*

`sustain --force-backfill` rewrites every present library track's
editable metadata fields and rating from SQLite to the audio file's
native tags, streams per-track progress to stderr, and exits without
launching the UI. It is intended for advanced maintenance cases such as
manually editing Sustain's database track tags or bulk-importing
external library history into SQLite. Listening statistics remain
SQLite-only and are never written to file tags.

Two exceptions follow the per-edit mirror rules. BPM is written only
when `bpm_set_by_user` is set and the key only when its `key_origin` is
typed (`1`); imported and measured values stay library-only, so a database
edit meant to reach the files must also record that provenance. A track
whose row is marked missing is skipped, even if its file has returned
since the last scan; rescan first to clear the mark.

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

A **DEVICES** section in the sidebar lists connected USB sticks, SD cards, and
Android phones (over MTP). Selecting one opens its sync panel in the main content
column: tick-lists of playlists, smart playlists, and artists to send to the
device, the on-drive format, and a Sync button. An artist the library spells
several ways (`Daft Punk`, `DAFT PUNK`) is listed once and sends the tracks of
every spelling. Mounted USB devices also get a
sidebar eject control that delegates to the desktop's normal eject/unmount
machinery. The same GUI-driven workflow iTunes used for iPod sync, generalised
to any removable drive or phone.

Sustain owns the sync state. A device is recognised across sessions by a
generated `.sustain-device-id` marker written at its root (with the filesystem
volume id as a fallback if the marker is deleted), so the panel reopens with the
playlists and artists you ticked for that device pre-filled. The marker is only
written on first sync — Sustain never touches a device until you ask it to.
**Forget device** in the panel discards that saved configuration — ticked
playlists and artists, layout, and the sync history — without touching
the files on the device.

Re-syncing is **incremental**. Per device, Sustain keeps a manifest of what it
wrote there (track → on-device path + content fingerprint), resolves the ticked
playlists and artists to a track set (smart playlists and artist selections are
re-evaluated every sync), diffs it
against the manifest and what is actually on the drive, and copies only what
changed. The panel shows the plan — to copy / update / unchanged / to remove —
before you sync; removing tracks no longer selected requires ticking an explicit
confirmation, and a folder that removal leaves empty goes with its last file,
so the device stops listing an artist or a playlist with nothing in it. The same
confirmation removes the `.m3u8` of a playlist that was unticked or renamed:
Sustain records each playlist file it writes and only ever removes one it
recorded, never a playlist file it did not write.
Progress and the outcome flow through the status-bar notification
lane, and while the sync runs the device's sidebar row carries the same turning
sync badge as an Audio CD being ripped.

A long sync **resumes where it stopped**. Sustain records each file in the
device's manifest as soon as it is on the drive, so a sync that ends early,
whether cancelled, cut short by a full or unplugged drive, interrupted by quitting
Sustain, or killed by a crash, does not copy those files again the next time.
After a power cut, only the files copied in its last moments may be sent again.

A disk-occupation meter in the panel's footer shows how much of the drive the
selection will occupy, **stacked by genre**: the largest genres each get a colour
derived from the system accent, everything past the top eight folds into a single
grey "other" segment, and hovering a segment pops up its genre and size. The meter
turns red when the selection would not fit.

Three on-drive formats, chosen per device:

- **Playlists as `.m3u8`** — one deduplicated `Music/Artist/Album/NN Title.ext`
  tree plus one UTF-8 `.m3u8` per selected playlist, smart playlist, or artist.
  For phones and players that read playlists. On an Android target each `.m3u8`
  is written inside `Music/` with its entries relative to that folder, where the
  common Android players resolve them.
- **One folder per playlist** — a folder per selected playlist, smart playlist,
  or artist with real audio copies (a track in three selected sources is copied
  three times), no `.m3u`. Names are capped at 32 characters and per-track
  positions stay stable across syncs so adding tracks does not reshuffle the
  folder. An optional per-folder file cap (64 / 128 / 256 / 512, off by default)
  splits oversized playlists into numbered subfolders. For folder-navigating car
  stereos.
- **Pioneer (Rekordbox / XDJ)** — a full Rekordbox export that Pioneer CDJ/XDJ
  hardware (and Rekordbox itself) reads directly off the USB, not just copied
  audio. Sustain writes Pioneer's on-device database
  (`PIONEER/rekordbox/export.pdb`) plus per-track ANLZ analysis files
  (`ANLZ0000.DAT` / `.EXT`) under `PIONEER/USBANLZ/`. Tracks whose paths
  hash to the same analysis directory get their own numbered pair
  (`ANLZ0001`, `ANLZ0002`, …) as Rekordbox does, so no track shows another
  track's waveform or grid. A track's pair is rewritten whenever its analysis
  changes, even when the audio file itself is unchanged, and left untouched
  otherwise; removing a track deletes only its own pair. Each track carries its
  BPM to the hundredth of a beat, musical key, its beat grid, both monochrome and colour
  waveforms (the small browse preview and the full detailed overview), star
  rating, genre, year, disc number, composer and comment — all from Sustain's
  own analysis pipeline and library, not re-read from the files. A track
  with no key, genre, artist or album is exported with that field unset,
  never with a placeholder value. The panel shows how many tracks in the
  selection are missing BPM / key / waveform analysis or will reach the player
  without a beat grid, and offers to run analysis before export. A grid is
  left off a track whose library BPM no longer agrees with it (a BPM typed
  to another metrical level), so the player never shows a grid that
  contradicts the track's tempo. Each track's embedded cover art is rendered to the 80×80 and
  240×240 JPEG thumbnails the XDJ shows in its browse and now-playing screens,
  written under `PIONEER/Artwork/00001/` and linked from the database;
  identical covers (an album's shared art) are stored once. A drive that
  carries no player display settings gets rekordbox's defaults with the
  waveform colour set to RGB, so players show the colour waveforms rather
  than their BLUE monochrome default; settings already on the drive —
  changed on a player or written by rekordbox — are never overwritten. This is the full
  set of analysis data Pioneer gear normally only receives from Rekordbox
  itself — to the maintainer's knowledge, no other Linux application produces
  it. Grids and detailed waveforms are placed on the player's timeline: for
  MP3s, Pioneer players also play the encoder delay that other decoders
  drop. Sustain reads that delay from the file's LAME/Xing header and moves
  the beats and waveform by it, so the grid sits on the beats the player
  plays. Only formats Pioneer players can play are exported: MP3, AAC/ALAC
  (M4A), FLAC, WAV and AIFF. Tracks in any other format (Opus, Ogg Vorbis…)
  are left off the drive, and the sync summary says how many. (Hot cues and
  memory cues are not written.) Sustain only ever writes over an export it
  wrote itself: if the drive holds a database Sustain did not write — one
  rekordbox exported, or Sustain's own after a player recorded history in it —
  the Sync button is greyed out with a note explaining why, and nothing on
  the drive is touched until the user removes its `PIONEER` folder; the
  panel checks again when the user returns to Sustain's window. Artwork
  files Sustain did not write are likewise never deleted. A sync cut short —
  even by a crash — still leaves its own export recognised as Sustain's.

Android phones are reached over **MTP** through the desktop's GVfs backend — the
same path the file manager uses, so no special drivers or root access are needed.
The Debian package recommends `gvfs-backends`, which provides that backend; the
Flatpak talks to the host's GVfs.
A connected phone appears in **DEVICES** alongside removable drives and syncs into
its primary storage (`/sdcard`), defaulting to the `Playlists as .m3u8` format so
audio lands under `/sdcard/Music/Artist/Album/` and each playlist resolves where
Android players look for it. Because MTP cannot rename, files are published in
place; an interrupted copy is caught by the size/fingerprint diff and re-sent on
the next sync. Reconnects are recognised by the same `.sustain-device-id` marker,
with the phone's MTP serial as the fallback. Android's media index can take a
moment to notice freshly copied files, so a player may need a manual library
rescan to see a new sync.

---

## CD import — *iso-iTunes*

Insert an audio CD and it appears as a transient **Audio CD** row in the sidebar's
**DEVICES** section (only while a readable audio disc is present — empty optical
drives are not shown). Selecting it opens a dedicated CD-import page: the release
artwork on the left, the release identity in the middle, and **Encoding Settings**,
**Eject**, and **Import CD** buttons on the right, above a full track table built
exactly like the main Songs table — proper column headers and the same striped
rows, continuing into the empty area below the last track. Each track carries an
import tick (every track ticked by default; there is no select-all), the track
number, an editable title and artist, and its duration. Editing is the same
interaction as the library table — click a cell of the already-selected row to
rename it inline — so a disc MusicBrainz could not identify can be named by hand,
and the typed value wins over the looked-up one. A leading status column shows
each track's rip progress: empty while pending, an animated spinner on the track
currently being ripped, and a green tick once it is imported. While the rip runs,
the sidebar's Audio CD row carries a turning sync badge on green, so the import
stays visible from any view. The same "Import CD" workflow iTunes used, driven
from Sustain's own library model.

The disc is identified by a **MusicBrainz Disc ID** computed from its table of
contents (via libdiscid) — a single disc-id lookup, not per-track title guessing.
When exactly one release matches it is used automatically; when several pressings
match, a release selector appears with the most likely one preselected (a regular
album over a compilation, then the earliest pressing), which the user can change
before importing; when
nothing matches, or there is no network, a valid `Audio CD` / `Unknown Artist` /
`Track NN` fallback is rendered so an offline import still works. The chosen
release's front cover is fetched once from the Cover Art Archive, shown in the
header, and embedded in every imported track. Discovery and the lookup run off
the UI thread and never block the main loop; the first probe happens only after
startup so cold start is unaffected.

Encoding is configured in the **Encoding** Preferences tab. The **CD import
format** offers three profiles — **FLAC** (lossless, the default), **MP3 256
kbps**, and **MP3 320 kbps**. A second **CD read accuracy** control trades speed
against error correction: **Fast** (raw read at full drive speed), **Balanced**
(`cdparanoia` jitter correction — fragment + overlap — at full speed, the
default), and **Paranoid** (adds scratch detection / repair with the drive capped
to 1× for scratched or archival discs). Both are captured when an import starts, so changing them affects
only later imports. Extraction uses GStreamer's `cdparanoiasrc` at the chosen
accuracy; each track is encoded to an unpublished staging file beneath the
library, tagged and given its cover through Sustain's normal tag writer, read back
to capture its real duration / bitrate / sample rate, then published with the
no-overwrite managed-library move and committed as a durable library row. CD
imports always create owned files inside the library folder, even when the library
is otherwise in "reference files in place" mode, because an optical track has no
durable path to reference.

Completed tracks survive cancellation, ejection, a later-track failure, and a
crash: each track's publish/row transition is journaled, so a restart
deterministically finishes or rolls back an interrupted one, and an existing
library file is never overwritten. Per-track progress is shown inline in the
track table's status column as the rip advances; the overall progress,
cancellation, and the success / partial / failure outcome all additionally flow
through the status-bar notification lane; library playback continues
uninterrupted during a rip.

### Audio CD playback — *iso-iTunes*

Double-click a track on the CD-import page to play the disc from it, straight
from the drive, without importing it. The whole disc is queued in track order,
so it plays on through the transport, Now Playing, the queue, MPRIS and the
media keys exactly like a library list; shuffle and repeat apply as usual, with
Smart Shuffle playing a disc as pure shuffle. The playing track shows the
speaker in the page's status column. Disc tracks carry the names the page
shows — the chosen MusicBrainz release, the fallback `Track NN`, or the user's
inline renames, updated live while the disc plays — and the duration read from
the disc. A disc track is not a library track: it keeps no play count, skip
count or rating, and Now Playing shows no artwork for it. The drive serves one
reader at a time: importing the disc stops its playback, a disc being imported
cannot be played, and ejecting the disc (or the disc leaving the drive) stops
it and takes its tracks out of the queue.

---

## Single-instance enforcement — *iso-iTunes*

A second Sustain process targeting the same library database is
refused on startup. The first instance's window is raised and focused
instead. The lock is held on a sidecar `.lock` file next to
`library.sqlite`, so the lock follows the resolved database path: a
build pointed at another database gets its own lock. The GTK
application ID (`io.github.open_sustain.sustain`) and the MPRIS bus
name are fixed, so two builds running against different databases at
once still share those desktop names.

---

## Key locations

By default Sustain follows the XDG base directories:

- Config: `$XDG_CONFIG_HOME/sustain/settings.toml` (`~/.config/sustain/settings.toml`)
- Database: `$XDG_DATA_HOME/sustain/library.sqlite` (`~/.local/share/sustain/library.sqlite`)
- Lock file: the database path with `.lock` appended
- Cache (artwork, waveforms): `$XDG_CACHE_HOME/sustain` (`~/.cache/sustain`)

Developer flags isolate an instance from those locations (`sustain --help`):
`--config <path>` and `--database <path>` pin one file each, and
`--local-scope` (alias `--dev`) keeps `sustain.toml`, `sustain.sqlite`,
and `sustain.cache/` in the working directory. As soon as any of them is
given, Sustain never falls back to the XDG locations: unpinned slots
sit beside the named file, or in the working directory under
`--local-scope`. `--profile` prints startup performance landmarks to
stderr.

---

## Features to come

Checkout the [issues backlog on github](https://github.com/open-sustain/sustain/issues)
