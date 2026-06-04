<!-- SPDX-License-Identifier: GPL-3.0-or-later -->

# Vendored AppStream ITS rules

`its/metainfo.its` and `its/metainfo.loc` are GNU gettext's
internationalization-tag-set rules for AppStream `*.metainfo.xml` files. They
tell `xgettext` (extraction) and `msgfmt --xml` (compilation) which elements of
`data/io.github.open_sustain.sustain.metainfo.xml` are translatable.

They are **vendored deliberately**. The rules are otherwise read from the
system, but their content varies by environment — the `appstream` package ships
a richer copy (which also marks `<keyword>` and the developer name
translatable) while GNU `gettext` ships a leaner one. Depending on the system
copy made the extracted `po/sustain.pot` non-reproducible across machines and
CI. Vendoring one canonical copy makes extraction and compilation byte-identical
everywhere.

- **Source:** GNU gettext, Debian package `gettext` version `0.26-1`
  (`/usr/share/gettext-0.26/its/`).
- **License:** GPL-3.0-or-later (© Free Software Foundation, Inc.), as stated in
  the files' own headers — compatible with Sustain's GPL-3.0-or-later.
- **Surface:** translatable = `<name>`, `<summary>`, `<description>`, screenshot
  `<caption>`. Not translatable = `<keyword>`, developer name. This is the
  intended localization surface; keywords stay in their canonical form.

The xtask uses these via `cargo xtask i18n-extract`
(`xgettext --its=build-aux/gettext/its/metainfo.its`) and
`cargo xtask i18n-compile` (`msgfmt --xml` with `GETTEXTDATADIR` pointed here).
To refresh from a newer gettext, copy both files and re-run
`cargo xtask i18n-extract`.
