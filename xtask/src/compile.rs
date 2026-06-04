// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-compile`: turn the committed catalogs into the build
//! artifacts packaging installs — a binary `.mo` per locale plus localized
//! desktop-entry and AppStream-metadata files.
//!
//! Output is built into a clean staging tree and then swapped into the
//! packaging artifact directory (`target/i18n/dist`) by an atomic rename, so a
//! catalog that was removed from `LINGUAS` cannot survive as a stale `.mo` in
//! what packaging consumes. The committed source metadata is never edited in
//! place, and scratch files live outside the artifact tree.

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
    // Validates the toolchain and that the AppStream ITS rules are installed at
    // a standard location; `msgfmt --xml` below auto-detects those rules from
    // the `*.metainfo.xml` template name rather than taking an explicit path.
    tools::preflight()?;

    let po_dir = workspace::po_dir(root);
    let langs = workspace::linguas(root)?;

    // Build into a fresh staging tree so nothing from a previous run survives.
    let staging = workspace::work_dir(root).join("staging");
    reset_dir(&staging)?;

    for lang in &langs {
        let mo = staging.join(format!(
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

    let desktop_out = staging.join(format!("{}.desktop", workspace::APP_ID));
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

    let metainfo_out = staging.join(format!("{}.metainfo.xml", workspace::APP_ID));
    tools::finish(
        tools::cmd("msgfmt")
            .arg("--xml")
            .arg("--template")
            .arg(workspace::metainfo_source(root))
            .arg("-d")
            .arg(&po_dir)
            .arg("-o")
            .arg(&metainfo_out),
    )?;

    // Atomically replace the artifact tree with the freshly staged one.
    let dist = workspace::dist_dir(root);
    if dist.exists() {
        std::fs::remove_dir_all(&dist)
            .map_err(|err| format!("cannot remove {}: {err}", dist.display()))?;
    }
    if let Some(parent) = dist.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    std::fs::rename(&staging, &dist).map_err(|err| {
        format!(
            "cannot move {} into {}: {err}",
            staging.display(),
            dist.display()
        )
    })?;

    println!(
        "i18n-compile: built {} catalog(s) plus localized desktop and AppStream metadata under {}",
        langs.len(),
        dist.display()
    );
    Ok(())
}

/// Remove `dir` if present and recreate it empty.
fn reset_dir(dir: &Path) -> Result<(), String> {
    if dir.exists() {
        std::fs::remove_dir_all(dir)
            .map_err(|err| format!("cannot remove {}: {err}", dir.display()))?;
    }
    std::fs::create_dir_all(dir).map_err(|err| format!("cannot create {}: {err}", dir.display()))
}
