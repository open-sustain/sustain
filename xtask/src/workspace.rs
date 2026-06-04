// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Workspace layout: the paths, source roots, and small parsers the i18n
//! commands share. Everything is resolved relative to the workspace root so
//! the commands behave identically regardless of the current directory.

use std::path::{Path, PathBuf};

/// gettext text domain. Matches `sustain_i18n`'s domain and the installed
/// `sustain.mo` basename.
pub const TEXT_DOMAIN: &str = "sustain";

/// Reverse-DNS application id; the basename of the desktop and AppStream
/// metadata source files under `data/`.
pub const APP_ID: &str = "io.github.open_sustain.sustain";

/// Workspace-relative roots scanned for translatable Rust strings. Only the
/// presentation-boundary crates own user-visible text; the durable domain,
/// store, playback, and desktop-protocol crates stay string-free, so they are
/// deliberately excluded from extraction.
pub const RUST_SOURCE_ROOTS: &[&str] = &["crates/ui_gtk/src", "crates/app_runtime/src"];

/// The workspace root, derived from this crate's compile-time manifest path.
/// `xtask` lives directly under the workspace root, so the manifest directory's
/// parent is that root.
pub fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("the xtask manifest directory always has a parent: the workspace root")
        .to_path_buf()
}

/// The translation-source directory (`po/`).
pub fn po_dir(root: &Path) -> PathBuf {
    root.join("po")
}

/// The committed extraction template (`po/sustain.pot`).
pub fn pot_path(root: &Path) -> PathBuf {
    po_dir(root).join("sustain.pot")
}

/// The locale-list file (`po/LINGUAS`).
pub fn linguas_path(root: &Path) -> PathBuf {
    po_dir(root).join("LINGUAS")
}

/// The catalog for `lang` (`po/<lang>.po`).
pub fn catalog_path(root: &Path, lang: &str) -> PathBuf {
    po_dir(root).join(format!("{lang}.po"))
}

/// The desktop-entry source (`data/<app-id>.desktop`).
pub fn desktop_source(root: &Path) -> PathBuf {
    root.join("data").join(format!("{APP_ID}.desktop"))
}

/// The AppStream metadata source (`data/<app-id>.metainfo.xml`).
pub fn metainfo_source(root: &Path) -> PathBuf {
    root.join("data").join(format!("{APP_ID}.metainfo.xml"))
}

/// Deterministic, sorted list of `*.rs` files under [`RUST_SOURCE_ROOTS`].
pub fn rust_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    for relative in RUST_SOURCE_ROOTS {
        collect_rs(&root.join(relative), &mut files)?;
    }
    files.sort();
    Ok(files)
}

/// Deterministic, sorted list of every `*.rs` file under `crates/`, used by the
/// call-shape guard to scan the whole workspace, not just the extraction roots.
pub fn all_crate_sources(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut files = Vec::new();
    collect_rs(&root.join("crates"), &mut files)?;
    files.sort();
    Ok(files)
}

/// Locale codes listed in `po/LINGUAS`, in file order. Blank lines and lines
/// beginning with `#` are ignored; remaining lines may list several
/// whitespace-separated codes.
pub fn linguas(root: &Path) -> Result<Vec<String>, String> {
    let path = linguas_path(root);
    let text = std::fs::read_to_string(&path)
        .map_err(|err| format!("cannot read {}: {err}", path.display()))?;
    let mut langs = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        langs.extend(line.split_whitespace().map(str::to_owned));
    }
    Ok(langs)
}

/// Locale codes for which a `po/<lang>.po` catalog exists on disk, sorted.
pub fn catalog_langs(root: &Path) -> Result<Vec<String>, String> {
    let dir = po_dir(root);
    let entries =
        std::fs::read_dir(&dir).map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    let mut langs = Vec::new();
    for entry in entries {
        let path = entry
            .map_err(|err| format!("cannot read an entry in {}: {err}", dir.display()))?
            .path();
        if path.extension().is_some_and(|ext| ext == "po") {
            if let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) {
                langs.push(stem.to_owned());
            }
        }
    }
    langs.sort();
    Ok(langs)
}

/// Append every `*.rs` file under `dir` (recursively) to `out`.
fn collect_rs(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
    let entries =
        std::fs::read_dir(dir).map_err(|err| format!("cannot read {}: {err}", dir.display()))?;
    for entry in entries {
        let entry =
            entry.map_err(|err| format!("cannot read an entry in {}: {err}", dir.display()))?;
        let path = entry.path();
        let file_type = entry
            .file_type()
            .map_err(|err| format!("cannot stat {}: {err}", path.display()))?;
        if file_type.is_dir() {
            collect_rs(&path, out)?;
        } else if file_type.is_file() && path.extension().is_some_and(|ext| ext == "rs") {
            out.push(path);
        }
    }
    Ok(())
}
