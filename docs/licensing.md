# Licensing and third-party attribution

Sustain is licensed **GPL-3.0-or-later** (`[workspace.package].license` in
`Cargo.toml`; full text in [`LICENSE`](../LICENSE)). This document records how
Sustain meets the attribution and notice obligations of the third-party code
it ships, and why each component is handled the way it is.

## What ships, and what carries an obligation

A Sustain release is distributed as a Debian `.deb` and a Flatpak bundle. The
only third-party code compiled *into* the `sustain` binary is the Rust crate
graph, which is **statically linked**. Permissive licenses in that graph (MIT,
BSD, Apache-2.0, ISC, …) require their copyright notices and license text to
travel with the binary. Everything else Sustain depends on is either linked
dynamically or is Sustain's own first-party content.

### Statically linked Rust crates — `THIRD-PARTY-LICENSES.md`

The complete attribution inventory for the crate graph lives in
[`THIRD-PARTY-LICENSES.md`](../THIRD-PARTY-LICENSES.md) at the repository
root. It is **generated, not hand-maintained**, by
[`cargo about`](https://github.com/EmbarkStudios/cargo-about) from the locked
`Cargo.lock`, reading each crate's actual `LICENSE`/`NOTICE` files rather than
trusting its SPDX metadata string.

- Configuration: [`about.toml`](../about.toml) — pins the two Linux release
  targets (amd64, arm64), excludes the first-party `sustain-*` crates and
  test-only dependencies, and accepts exactly the license set that
  [`deny.toml`](../deny.toml) allows inbound. The two configs enforce the same
  policy from different angles: `deny.toml` gates the SPDX *expression*,
  `cargo about` gates the presence of real, reproducible license *text*.
- Template: [`about.hbs`](../about.hbs) — renders the Markdown.
- Regenerate:

  ```sh
  cargo about generate --locked --fail about.hbs -o THIRD-PARTY-LICENSES.md
  ```

  `--fail` rejects any crate whose license text cannot be resolved from local
  sources; the harvest needs no network beyond fetching the crates themselves.

The file is generated with **cargo-about 0.9.0** (pinned in CI). Output is
byte-for-byte reproducible for a given `Cargo.lock`, so CI's `cargo-about` job
regenerates it and fails on any drift — a dependency change that alters the
attribution set must land with its regenerated inventory in the same change.

#### Where it is installed

| Artifact | Path |
| --- | --- |
| Debian `.deb` | `/usr/share/doc/sustain/THIRD-PARTY-LICENSES.md` (alongside the cargo-deb-generated `copyright`) |
| Flatpak | `/app/share/licenses/io.github.open_sustain.sustain/THIRD-PARTY-LICENSES.md` |

The release workflow's install-layout check asserts the Debian path exists, so
a packaging change that drops the file fails the release.

## Non-Cargo components, and why they are not in the inventory

The `cargo about` inventory deliberately covers only the statically linked
crate graph. The remaining components Sustain ships or links are accounted for
here:

- **Dynamically linked platform stack — GTK 4, GStreamer, GLib, Cairo, Pango,
  and the rest of the GNOME runtime.** Sustain does **not** redistribute these.
  The Debian package declares them as dependencies and the distribution ships
  its own copies with their own copyright files; the Flatpak pulls them from
  the `org.gnome.Platform` runtime. Their notices therefore ship with those
  components, not with Sustain.
- **GStreamer plugin packages** (`gstreamer1.0-plugins-good`,
  `gstreamer1.0-plugins-bad`) are likewise runtime Debian dependencies, not
  bundled content.
- **Embedded Pioneer database pages** —
  `crates/pioneer/src/reference/{columns,history,history_entries,history_playlists}.bin`.
  These are four 4096-byte structural pages required for CDJ/XDJ hardware to
  accept a drive. They are format fixtures (content-free table scaffolding),
  not third-party copyrighted creative works, so they carry no attribution
  obligation. Their factual origin and the interoperability basis for the
  exporter are covered separately by the Pioneer-export provenance work; see
  the footer disclaimer on the website and the in-code documentation in
  `crates/pioneer/src/pdb.rs` and `crates/pioneer/src/path_hash.rs`.
- **Sustain's own assets** — application icons, the `.desktop` entry, the
  AppStream metainfo, and `crates/ui_gtk/src/app.css` — are first-party content
  under Sustain's GPL-3.0-or-later license.

## Adding or updating a dependency

1. Make the dependency change and update `Cargo.lock` as usual.
2. Regenerate the inventory with the command above and commit the result.
3. If the new crate's license is not already in `about.toml`'s `accepted` set
   (and `deny.toml`'s `allow` set), confirm it is compatible with
   GPL-3.0-or-later before adding it to **both** lists.

CI enforces steps 2 and 3: the `cargo-about` job fails on a stale inventory,
and `cargo-deny` fails on a disallowed inbound license.
