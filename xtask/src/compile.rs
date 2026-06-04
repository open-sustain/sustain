// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-compile`: turn the committed catalogs into the build
//! artifacts packaging installs — a binary `.mo` per locale plus localized
//! desktop-entry and AppStream-metadata files.
//!
//! The artifact directory (`target/i18n/dist`) is wiped and rebuilt each run,
//! so a catalog removed from `LINGUAS` cannot linger as a stale `.mo` in what
//! packaging consumes. The committed source metadata is never edited in place.

use std::path::Path;

use crate::tools;
use crate::workspace;

/// Entry point for `cargo xtask i18n-compile`.
pub fn run() -> Result<(), String> {
    run_in(&workspace::workspace_root())
}

/// Compile against an explicit workspace root (the real one, or a fixture in
/// tests).
pub fn run_in(root: &Path) -> Result<(), String> {
    tools::preflight()?;

    let po_dir = workspace::po_dir(root);
    let langs = workspace::linguas(root)?;

    // Wipe and rebuild the artifact tree so nothing from a previous run (e.g. a
    // catalog since removed from LINGUAS) survives.
    let dist = workspace::dist_dir(root);
    if dist.exists() {
        std::fs::remove_dir_all(&dist)
            .map_err(|err| format!("cannot remove {}: {err}", dist.display()))?;
    }
    std::fs::create_dir_all(&dist)
        .map_err(|err| format!("cannot create {}: {err}", dist.display()))?;

    for lang in &langs {
        let mo = dist.join(format!(
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
                .arg(workspace::catalog_path(root, lang)),
        )?;
    }

    let desktop_out = dist.join(format!("{}.desktop", workspace::APP_ID));
    tools::finish(
        tools::cmd("msgfmt")
            .arg("--desktop")
            .arg("--template")
            .arg(workspace::desktop_source(root))
            .arg("-d")
            .arg(&po_dir)
            .arg("-o")
            .arg(&desktop_out),
    )?;

    // `msgfmt --xml` has no `--its` flag; point GETTEXTDATADIR at the vendored
    // rules so it selects the same translatable elements extraction did, rather
    // than whatever the host's gettext/appstream packages provide.
    let metainfo_out = dist.join(format!("{}.metainfo.xml", workspace::APP_ID));
    tools::finish(
        tools::cmd("msgfmt")
            .env("GETTEXTDATADIR", workspace::vendored_gettext_datadir(root))
            .arg("--xml")
            .arg("--template")
            .arg(workspace::metainfo_source(root))
            .arg("-d")
            .arg(&po_dir)
            .arg("-o")
            .arg(&metainfo_out),
    )?;

    println!(
        "i18n-compile: built {} catalog(s) plus localized desktop and AppStream metadata under {}",
        langs.len(),
        dist.display()
    );
    Ok(())
}
