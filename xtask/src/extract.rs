// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-extract`: regenerate `po/sustain.pot` from the marked Rust
//! strings, the desktop entry, and the AppStream metadata, then resync the
//! committed catalogs against the new template.

use std::path::{Path, PathBuf};

use crate::{po, tools, tools::Tools, workspace};

/// POT header fields for the template. Passed on every `xgettext` pass because
/// under `--join-existing` the pass that writes last also writes the header;
/// keeping them uniform yields one clean, non-FSF-boilerplate header.
fn header_args() -> [&'static str; 4] {
    [
        "--package-name=Sustain",
        "--copyright-holder=AnnoyingTechnology",
        "--msgid-bugs-address=https://github.com/open-sustain/sustain/issues",
        "--foreign-user",
    ]
}

/// Entry point for `cargo xtask i18n-extract`.
pub fn run() -> Result<(), String> {
    let root = workspace::workspace_root();
    let tools = tools::preflight()?;
    let pot = workspace::pot_path(&root);

    // Generate into scratch first, then replace the committed template only if
    // its message set actually changed. This keeps `po/sustain.pot` byte-stable
    // across no-op extractions — the volatile POT-Creation-Date and source
    // locations refresh only when the strings themselves change.
    let fresh = root.join("target/i18n/tmp/sustain.fresh.pot");
    generate_pot(&root, &tools, &fresh)?;
    let updated = replace_if_changed(&root, &fresh, &pot)?;

    let langs = workspace::linguas(&root)?;
    for lang in &langs {
        let catalog = workspace::catalog_path(&root, lang);
        tools::finish(
            tools::cmd("msgmerge")
                .arg("--quiet")
                .arg("--update")
                .arg("--backup=none")
                .arg("--previous")
                .arg(&catalog)
                .arg(&pot),
        )?;
    }

    println!(
        "i18n-extract: {} ({}), resynced {} catalog(s)",
        pot.display(),
        if updated {
            "updated"
        } else {
            "already current"
        },
        langs.len()
    );
    Ok(())
}

/// Replace `committed` with `fresh` only when their message sets differ.
/// Returns whether the committed template was rewritten.
fn replace_if_changed(root: &Path, fresh: &Path, committed: &Path) -> Result<bool, String> {
    if committed.is_file()
        && po::normalized_messages(root, committed, "committed")?
            == po::normalized_messages(root, fresh, "fresh")?
    {
        return Ok(false);
    }
    std::fs::copy(fresh, committed).map_err(|err| {
        format!(
            "cannot write {} from {}: {err}",
            committed.display(),
            fresh.display()
        )
    })?;
    Ok(true)
}

/// Build a deterministic POT at `out`. Shared by extraction and the currency
/// check.
///
/// Each source language needs its own `xgettext` pass (Rust, Desktop, ITS-based
/// AppStream), so the Rust pass creates a single accumulator with the template
/// header and the other two append to it with `--join-existing`. A final
/// `msgcat --sort-output` orders entries by msgid, so the committed template
/// changes only when the message set changes, not when source lines move.
pub fn generate_pot(root: &Path, tools: &Tools, out: &Path) -> Result<(), String> {
    let scratch = root.join("target/i18n/tmp");
    std::fs::create_dir_all(&scratch)
        .map_err(|err| format!("cannot create {}: {err}", scratch.display()))?;
    let accumulator = scratch.join("accumulator.pot");

    extract_rust(root, &accumulator)?;
    extract_desktop(root, &accumulator)?;
    extract_appdata(root, tools, &accumulator)?;

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    tools::finish(
        tools::cmd("msgcat")
            .arg("--sort-output")
            .arg("--to-code=UTF-8")
            .arg("-o")
            .arg(out)
            .arg(&accumulator),
    )
}

/// Create the accumulator from the marked Rust call sites. Runs with the
/// workspace root as the working directory and passes root-relative paths so
/// the recorded `#:` source locations are portable.
fn extract_rust(root: &Path, accumulator: &Path) -> Result<(), String> {
    let sources = workspace::rust_sources(root)?;
    if sources.is_empty() {
        return Err(format!(
            "no Rust sources found under {:?}",
            workspace::RUST_SOURCE_ROOTS
        ));
    }
    let relative = relative_to(root, &sources);
    tools::finish(
        tools::cmd("xgettext")
            .current_dir(root)
            .arg("--language=Rust")
            .arg("--from-code=UTF-8")
            .arg("--force-po")
            // Disable the default keyword set, then declare exactly the
            // sustain_i18n surface (with msgctxt/plural argument positions).
            .arg("--keyword")
            .arg("--keyword=gettext")
            .arg("--keyword=ngettext:1,2")
            .arg("--keyword=pgettext:1c,2")
            .arg("--keyword=npgettext:1c,2,3")
            // Mark every msgid argument as rust-format so `msgfmt --check-format`
            // can reject a translation whose `{named}` placeholders drift.
            .arg("--flag=gettext:1:rust-format")
            .arg("--flag=ngettext:1:rust-format")
            .arg("--flag=ngettext:2:rust-format")
            .arg("--flag=pgettext:2:rust-format")
            .arg("--flag=npgettext:2:rust-format")
            .arg("--flag=npgettext:3:rust-format")
            .arg("--add-comments=Translators:")
            .args(header_args())
            .arg("-o")
            .arg(accumulator)
            .args(&relative),
    )
}

/// Append the desktop entry's translatable keys (Name, Comment, Keywords, …)
/// to the accumulator.
fn extract_desktop(root: &Path, accumulator: &Path) -> Result<(), String> {
    let source = relative_to_one(root, &workspace::desktop_source(root));
    tools::finish(
        tools::cmd("xgettext")
            .current_dir(root)
            .arg("--language=Desktop")
            .arg("--from-code=UTF-8")
            .arg("--join-existing")
            .args(header_args())
            .arg("-o")
            .arg(accumulator)
            .arg(source),
    )
}

/// Append the translatable AppStream metadata (selected by GNU gettext's ITS
/// rules) to the accumulator.
fn extract_appdata(root: &Path, tools: &Tools, accumulator: &Path) -> Result<(), String> {
    let source = relative_to_one(root, &workspace::metainfo_source(root));
    tools::finish(
        tools::cmd("xgettext")
            .current_dir(root)
            .arg(format!("--its={}", tools.metainfo_its.display()))
            .arg("--from-code=UTF-8")
            .arg("--join-existing")
            .args(header_args())
            .arg("-o")
            .arg(accumulator)
            .arg(source),
    )
}

/// Map absolute workspace paths to paths relative to `root`, for stable `#:`
/// source locations.
fn relative_to(root: &Path, paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| relative_to_one(root, path))
        .collect()
}

/// Relativize a single path against `root`, falling back to the original path
/// when it is not under `root`.
fn relative_to_one(root: &Path, path: &Path) -> PathBuf {
    path.strip_prefix(root).unwrap_or(path).to_path_buf()
}
