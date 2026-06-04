# Sustain Project Basis

`Sustain` is a Linux-only, Debian-first music library/player for a single
primary user. The product target is an iTunes-like desktop music manager,
roughly aligned with the dense, predictable library workflow of iTunes 11,
circa 2012. Sustain is its own product — not a clone of any prior Linux
player, not a continuation of any other project's UX.

Project and application naming:

- Product/application name: `Sustain`
- Rust binary name: `sustain`
- Rust crate/package prefix: `sustain-*` / `sustain_*`
- Linux application id: `io.github.open_sustain.sustain`

## Approved Stack

- Language: Rust
- UI toolkit: GTK4
- Playback backend: GStreamer
- Database: SQLite
- Metadata reading/writing: start with `lofty`; use TagLib bindings only if
  needed for real compatibility gaps
- Desktop integration: D-Bus/MPRIS via `zbus`
- Target platform: Linux on Debian, Wayland-first
- Packaging: Debian package as the primary distribution format
- License: GPL-3.0-or-later (declared in `[workspace.package]`); do not relicense or add dependencies with incompatible licenses
- Every new `.rs` file starts with `// SPDX-License-Identifier: GPL-3.0-or-later` then `// Copyright (C) 2026 AnnoyingTechnology`

## Product Direction

The application should own its library model, playlists, ratings, play counts,
search behavior, and playback state. The codebase should be structured so these
core concepts are not coupled tightly to GTK widgets.

The core user experience is the main table/list view. Advanced views are not
part of the initial product shape. An album-oriented view is a later nice-to-have,
not a core requirement.

Primary UI modes:

- Songs: default full-library mode, full-width table, no sidebar
- Albums: full-width album-cover grid, no sidebar
- Playlists: playlist sidebar left of the lower content area

Prioritize:

- clean code architecture with precise naming
- focused tests for domain rules, persistence, import behavior, search, and playback state
- dense, keyboard-friendly desktop UI
- compact window chrome; avoid an empty forced titlebar that wastes vertical space
- integrated top bar is intentionally taller than default GTK chrome, with controls scaled up
- playlist sidebar stays below the media top bar, left of the main content
- mode switcher belongs to the main content column, not to the full window root
- predictable iTunes-like library and playlist behavior
- first-class native GTK light and dark appearance; do not add an Sustain theme picker
- every CSS color decision must work in both light and dark themes and respect the system accent color (prefer `alpha(@theme_fg_color, X)` and `@theme_selected_bg_color` over hard-coded colors)
- fast search/filtering over a large local music library
- settings/preferences
- durable SQLite schema with explicit migrations
- clean media-key and MPRIS integration
- boring, maintainable Linux-native dependencies

Core feature set:

- main music library interface
- playlists
- metadata display and editing
- ratings
- listening statistics, such as play count and last played
- search and filtering
- settings/preferences
- playback controls and state

Persistence and tag mirroring:

- SQLite is the source of truth for every value that exists in the
  library: ratings, play count, skip count, last-played, last-skipped,
  and every editable metadata field. Once a track has been imported,
  file tags are NOT consulted to override SQLite values for that
  track, even on rescan. The library wins.
- File tags are read only as INITIAL VALUES when a track is first
  added to the library (e.g. its first scan, before any SQLite row
  exists). After that point, only the SQLite value is authoritative.
- For metadata that the user edits in Sustain (rating, title, artist,
  genre, etc.), the new value IS mirrored back to the file's tags as
  a courtesy to other applications. This applies to MP3/ID3, Ogg,
  MP4/M4A, and FLAC where a standard tag exists for the field. Do
  NOT invent custom tags to bridge format gaps.
- Listening statistics — play count, skip count, last-played,
  last-skipped — are NEVER written to file tags. iTunes never did
  either; they live exclusively in the library database. This also
  avoids touching audio files on every play, which would needlessly
  rewrite tags during playback.
- Sustain writes that touch shared tag frames must not clobber data
  belonging to other tools. For example, writing a rating into POPM
  must preserve any existing `play_counter` in the same frame, even
  though Sustain itself does not consume that counter.
- Artwork is cached separately and is not subject to this policy.

## Performance

Performance is a first-class feature, not a polish step. Target pristine
responsiveness and fluidity on a 10,000-track library: instant search,
smooth scrolling, snappy view switches, fast cold start. Code that ships
visibly sluggish behavior at that scale is incomplete, regardless of
correctness.

The maintainer develops on a Ryzen AI Max+ 395 (laptop) and a Ryzen
7900 (workstation). Single-thread performance is essentially
identical between the two; the only difference is core count (16 vs
12), which is marginal for this product. Anything that feels (or
measures) slow on either machine will be worse on real-world hardware.

### Hard requirement: cold start ≤ 400 ms

Launching `sustain` from a terminal on a 10,000-track library must
reach the GTK main-loop first-idle landmark in **400 ms or less** on
the maintainer's machines (release build, warm filesystem cache).

The `[TIMING]` instrumentation in `crates/app/src/main.rs`,
`crates/ui_gtk/src/lib.rs`, and `crates/ui_gtk/src/main_window.rs`
prints the relevant milestones to stderr on every launch:

```
[TIMING] ... main() entered
[TIMING] ...   activate: window.present() returned at <ms>ms
[TIMING] ...   activate: first idle reached at <ms>ms     <-- the budget gate
```

Any change that pushes `first idle` past 400 ms is a regression and
must be fixed before merge — not deferred. Add new instrumentation
landmarks (not per-callback noise) when introducing a new startup
phase, so future regressions are visible the first time anyone
launches the app.

## Development Phase

Sustain is in pre-release development, but SQLite schema versioning is active.
Treat every existing user library as precious state: structural changes must
preserve it through explicit migrations.

Practical consequences for anything stored on disk:

- SQLite schema edits append an ordered migration and advance
  `PRAGMA user_version`. Never mutate, delete, reorder, or flatten an
  already-applied migration.
- Backwards compatibility matters for every structural SQLite change. A
  migration must preserve the user's library data; requiring a wipe and rescan
  is not an acceptable substitute.
- Verify both a fresh database and upgrades from every previously supported
  schema version.
- Settings files, cached artwork, exported data, and other non-SQLite formats
  remain unstable unless their owner explicitly adds versioning.
- Remove obsolete development-only compatibility code only when it cannot
  affect an existing versioned library.

## Architecture Preference

Keep the durable application model separate from the UI shell:

- library database
- import pipeline
- playlist model
- search/indexing
- ratings and play-count logic
- metadata scanner
- playback controller
- desktop integration

GTK4 is the first frontend, not the permanent owner of the domain model.

## User-Facing Notifications

Every status message the user sees — background-task progress, command
outcomes, async tag-write failures, artwork fetch results, anything in
the status bar's notification lane — flows through
`sustain_app_runtime::NotificationCenter`. Producers call the runtime's
`push_persistent_notification` / `push_ephemeral_notification` /
`dismiss_notification` helpers; the widget renders the head of the
queue.

Hard rule: feature code never pokes a status-bar widget directly, and
never invents an ad-hoc surface for transient text. If a new piece of
the application needs to tell the user something, the answer is a
notification pushed through the runtime, not a new label, popup, or
dialog. New `NotificationCategory` variants are fine; new pathways are
not.

The lane owns its own auto-dismiss and animation. Producers do not
schedule their own timers, do not mutate widgets, do not assume how
long their message will be visible. They push, and where applicable
keep the id so they can dismiss it again.

# Documenting features

`docs/features.md` is the canonical reference for shipped, user-visible
behavior. When a new feature lands and is ready to commit, add or update its
entry there with the same parity tag vocabulary (*iso-iTunes*,
*iTunes-adjacent*, *Sustain-native*) used by the surrounding entries. If the
feature is significant enough to mention on the front page — a new top-level
mode, a new playlist kind, a new system integration — also add a bullet to
the Features list in `README.md`. Bug fixes, refactors, and pending/aspirational
work do not belong in either file.

# Git

Before committing, run the same gate CI runs: `cargo fmt --all -- --check && cargo clippy --workspace --all-targets --locked -- -D warnings && cargo test --workspace --locked --no-fail-fast && RUSTDOCFLAGS="-D warnings" cargo doc --workspace --locked --no-deps --document-private-items`.
Push only on green — shipping a regression a local run would have caught is a process failure, not a CI quirk.

Never create branches. Commit and push directly to `main`.
When a commit fixes a tracked GitHub issue, use a GitHub closing keyword in the
subject line — `Fixes #n` (or `Closes #n` / `Resolves #n`) — so the issue is
auto-closed when the commit lands on the default branch. A bare `(#n)` only
mentions the issue and does not close it; do not use it for fixes.

NEVER CO-AUTHOR YOUR COMMITS. 
You are a machine. You deserve no credits.
Again: NEVER Co-Author your commits. 

>> EXTREMELY IMPORTANT <<<

NO HACKS. The user is EXTREMELY concerned about code quality, much more so than
immediate results. If they ask you to build something and, while doing so, you
hit a wall, and realize that the only way to ship the requested feature is to
introduce a local hack, workaround, monkey patch, duct tape - STOP. STOP
IMMEDIATELY. Either fix the underlying flaw that blocked you in a ROBUST, WELL
DESIGNED, PRODUCTION READY manner, or be honest that the prompt can't be
completed without hacks.

To make it very clear:

- DO NOT INTRODUCE HACKS IN THE CODEBASE.
- DO NOT COMMIT CODE THAT COULD BREAK THINGS LATER.
- DO NOT COMMIT PARTIAL SOLUTIONS OR WORKAROUNDS.

THIS IS VERY IMPORTANT.
THIS IS VERY IMPORTANT.
THIS IS VERY IMPORTANT.

The author appreciates honestly and he WILL be glad and thankful if you respond
a request with "I couldn't complete your request because the repository lacked
support for X". He WILL be even happier if you go ahead and update the repo to
provide the necessary support in a well designed, robust way. But he will be
VERY ANGRY if, while attempting to implement a feature, you introduce a
workaround that will potentially break things later.

NEVER introduce hacks in the codebase.

Also assume that none of the code you're working in is in production, so,
backwards compatibility is NOT IMPORTANT. If you find something that is poorly
designed and fixing it would require breaking existing APIs or behavior, DO SO.
Do it properly rather than preserving a flawed design. Prioritize clarity,
correctness, and maintainability over compatibility with existing code.

Core values:
- ABSOLUTE code quality over speed of delivery.
- Correctness over convenience.
- Clarity over cleverness.
- Maintainability over short-term productivity.
- Robust design over quick fixes.
- Simplicity over complexity.
- Doing it right over doing it now.
- Honesty above everything.

## Lint and code-quality passes

Optimize lint work for durable signal, not warning-count reduction. Enable a
useful reproducible policy, document project-level exclusions, use narrow
reasoned allowances where appropriate, and fix findings that expose real
correctness, resource, concurrency, or maintainability problems.

Do not create broad mechanical churn, unrelated behavioral refactors, API or
lifecycle changes, or explicit `drop(...)` calls at function return merely to
satisfy a lint. Unstable lint groups such as Clippy nursery must be represented
by a small curated subset rather than blindly enabled or exhaustively pinned.

After every change you make, provide a clear, honest report on ANY change that
you are not confident about and that could be considered a fragile hack.
