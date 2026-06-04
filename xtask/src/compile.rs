// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-compile`: turn the committed catalogs into the build
//! artifacts packaging installs — a binary `.mo` per locale plus localized
//! desktop-entry and AppStream-metadata files — all under `target/i18n/`. The
//! committed source metadata is never edited in place.

use crate::{tools, workspace};

/// Entry point for `cargo xtask i18n-compile`.
pub fn run() -> Result<(), String> {
    let root = workspace::workspace_root();
    // Validates the toolchain and that the AppStream ITS rules are installed at
    // a standard location; `msgfmt --xml` below auto-detects those rules from
    // the `*.metainfo.xml` template name rather than taking an explicit path.
    tools::preflight()?;
    let out_dir = root.join("target/i18n");
    let po_dir = workspace::po_dir(&root);
    let langs = workspace::linguas(&root)?;

    for lang in &langs {
        let mo = out_dir.join(format!(
            "locale/{lang}/LC_MESSAGES/{domain}.mo",
            domain = workspace::TEXT_DOMAIN
        ));
        if let Some(parent) = mo.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
        }
        tools::finish(
            tools::cmd("msgfmt")
                .arg("--check")
                .arg("-o")
                .arg(&mo)
                .arg(workspace::catalog_path(&root, lang)),
        )?;
    }

    std::fs::create_dir_all(&out_dir)
        .map_err(|err| format!("cannot create {}: {err}", out_dir.display()))?;

    let desktop_out = out_dir.join(format!("{}.desktop", workspace::APP_ID));
    tools::finish(
        tools::cmd("msgfmt")
            .arg("--desktop")
            .arg("--template")
            .arg(workspace::desktop_source(&root))
            .arg("-d")
            .arg(&po_dir)
            .arg("-o")
            .arg(&desktop_out),
    )?;

    let metainfo_out = out_dir.join(format!("{}.metainfo.xml", workspace::APP_ID));
    tools::finish(
        tools::cmd("msgfmt")
            .arg("--xml")
            .arg("--template")
            .arg(workspace::metainfo_source(&root))
            .arg("-d")
            .arg(&po_dir)
            .arg("-o")
            .arg(&metainfo_out),
    )?;

    println!(
        "i18n-compile: built {} catalog(s) plus localized desktop and AppStream metadata under {}",
        langs.len(),
        out_dir.display()
    );
    Ok(())
}
