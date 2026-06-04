<!--
SPDX-License-Identifier: GPL-3.0-or-later
Copyright (C) 2026 AnnoyingTechnology
-->

# Sustain translation glossary

This glossary keeps Sustain's catalogs consistent with each other and stable
across regenerations. When translating `po/<lang>.po`, follow these decisions
before improvising. It is normative for tone, capitalization, and the handful
of product terms that have a fixed treatment.

## Tone and register

- Address the user the way a native desktop application of this kind does in
  the target language. Match the conventions of the platform's own music and
  file applications, not a literal rendering of the English.
- Prefer concise, button-and-menu register over full sentences for controls;
  use complete sentences for notifications and descriptions.
- Sentence case for menu items, buttons, column headers, and labels, unless the
  target language's desktop convention is title case (e.g. it is not, in
  French, German, Spanish, Portuguese, Italian, Dutch, or Korean — use sentence
  case).

## Names that are never translated

These are identifiers or proper nouns. Keep them verbatim (a catalog entry for
one of them has a `msgstr` identical to its `msgid`):

- **Sustain** — the application name.
- **AnnoyingTechnology** — the developer name.
- **MusicBrainz**, **Cover Art Archive**, **AcoustID**, **LRClib** — external
  services.
- **MPRIS**, **D-Bus**, **GTK**, **GStreamer** — technologies.
- **BPM** — keep as the established acronym unless the target language has an
  equally standard one; do not expand it inline.
- File-format names: **MP3**, **FLAC**, **Ogg**, **Opus**, **AAC**, **MP4**,
  **ID3**, **Vorbis**.

## Product terms (translate consistently)

Translate each of these the same way everywhere it appears. Pick the term a
native music app uses; the English is the source of truth for meaning.

| English | Meaning / note |
| --- | --- |
| Songs | The default full-library mode and its view. The library's tracks as a flat list. |
| Albums | The album-cover grid mode. |
| Playlists | The playlists mode, and the user's playlists collectively. |
| Library | The user's whole music collection. |
| Playlist | A single ordered list of tracks. |
| Smart Playlist | A rule-based, auto-updating playlist. Translate as the local equivalent of "smart/automatic playlist". |
| Playlist Folder | A folder grouping playlists in the sidebar. |
| Track | One song/audio file. May share a translation with "Song" if the language does not distinguish; keep usage consistent. |
| Up Next | The play queue. Use the local equivalent of "queue / coming up". |
| Play Next | Queue an item to play immediately after the current track. |
| Add to Queue | Append an item to the end of Up Next. |
| Get Info | Open the metadata editor for the selection (the iTunes-style command). |
| Rating | The five-star rating. |
| Play count | How many times a track has been played. |
| Skip count | How many times a track has been skipped. |
| Last played | Timestamp of the most recent play. |
| Last skipped | Timestamp of the most recent skip. |
| Genre, Artist, Album Artist, Composer, Year, Title | Standard metadata fields; use the field names the platform's music apps use. |
| Music Key | The musical key of a track (harmonic metadata). Translate as "key" in the musical sense, not a keyboard/keymap key. |
| Library mode | Whether Sustain leaves files in place or organizes them on disk. |

## Verbs vs. nouns (disambiguation)

Several English words are both a verb (a control) and a noun (a label). The
catalogs disambiguate these with gettext message contexts (`pgettext`), so the
same English word can carry two different translations:

- **Play** — the playback control (verb) vs. a column/label (noun). Translate
  each per its context.
- **Shuffle**, **Repeat** — control (verb/state) vs. label.
- **Search** — the action vs. the field placeholder.

Always respect the message context when present; never collapse two contexts
into one translation because the English happens to match.

## Plurals

Count messages use gettext plural forms (`ngettext`/`npgettext`); supply every
plural form your language's rules require. Korean has a single plural form —
fill `msgstr[0]` and let the form stand for all counts; do not omit it.

## Regional notes

- **`pt` is European Portuguese (pt_PT).** Translate for Portugal. Brazilian
  Portuguese, if ever added, is a separate `pt_BR` catalog; do not blend the
  two or hedge between them.
