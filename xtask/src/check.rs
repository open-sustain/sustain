// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-check`: gate the localization contract. Fails on template
//! message drift, header tampering, non-named placeholders, catalog/LINGUAS
//! inconsistency, fuzzy/untranslated/obsolete entries, placeholder/header
//! errors, msgid-set drift between a catalog and the template, and gettext call
//! shapes the extractor would miss.
//!
//! The message-set comparison ignores source locations and the volatile
//! creation date so unrelated code movement never falses; the separate header
//! comparison catches license/header tampering that the message comparison
//! ignores. The check does not claim to find every unmarked English literal;
//! that remains a source-audit concern.

use std::path::Path;

use crate::{extract, lint, po, tools, workspace};

/// Entry point for `cargo xtask i18n-check`.
pub fn run() -> Result<(), String> {
    run_in(&workspace::workspace_root())
}

/// Check against an explicit workspace root (the real one, or a fixture in
/// tests).
pub fn run_in(root: &Path) -> Result<(), String> {
    let tools = tools::preflight()?;
    let mut problems = Vec::new();

    check_pot_current(root, &tools, &mut problems)?;
    check_pot_placeholders(root, &mut problems)?;

    let linguas = workspace::linguas(root)?;
    let catalogs = workspace::catalog_langs(root)?;
    check_linguas_consistency(&linguas, &catalogs, &mut problems);

    let pot = workspace::pot_path(root);
    for lang in &linguas {
        check_catalog(root, lang, &pot, &mut problems)?;
    }

    check_call_shapes(root, &mut problems)?;

    if problems.is_empty() {
        println!("i18n-check: ok ({} catalog(s))", linguas.len());
        Ok(())
    } else {
        Err(format!(
            "i18n-check found {} problem(s):\n  - {}",
            problems.len(),
            problems.join("\n  - "),
        ))
    }
}

/// Fail if `po/sustain.pot` does not match a freshly extracted template — both
/// its message set and its header (modulo the volatile creation date).
fn check_pot_current(
    root: &Path,
    tools: &tools::Tools,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let committed = workspace::pot_path(root);
    if !committed.is_file() {
        problems.push(format!(
            "{} is missing; run `cargo xtask i18n-extract`",
            committed.display()
        ));
        return Ok(());
    }

    let fresh = workspace::work_dir(root).join("check/sustain.pot");
    extract::generate_pot(root, tools, &fresh)?;

    if po::message_body(root, &committed, "check-committed")?
        != po::message_body(root, &fresh, "check-fresh")?
    {
        problems.push(
            "po/sustain.pot is out of date with the marked source strings; \
             run `cargo xtask i18n-extract` and commit po/sustain.pot"
                .to_owned(),
        );
    }
    if po::header_block(&committed)? != po::header_block(&fresh)? {
        problems.push(
            "po/sustain.pot header differs from a freshly generated template \
             (hand-edited or stale); run `cargo xtask i18n-extract` and commit po/sustain.pot"
                .to_owned(),
        );
    }
    Ok(())
}

/// Fail if any `rust-format` source string in the template uses a placeholder
/// that is not a `{name}` named placeholder.
fn check_pot_placeholders(root: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    let committed = workspace::pot_path(root);
    if !committed.is_file() {
        return Ok(());
    }
    let text = std::fs::read_to_string(&committed)
        .map_err(|err| format!("cannot read {}: {err}", committed.display()))?;
    for source in po::rust_format_source_strings(&text) {
        for problem in lint::placeholder_problems(&source.value) {
            problems.push(format!(
                "po/sustain.pot: in \"{}\": {problem}",
                truncate(&source.reference)
            ));
        }
    }
    Ok(())
}

/// Fail on any mismatch between `po/LINGUAS` and the catalogs on disk.
fn check_linguas_consistency(linguas: &[String], catalogs: &[String], problems: &mut Vec<String>) {
    for lang in linguas {
        if !catalogs.contains(lang) {
            problems.push(format!(
                "LINGUAS lists `{lang}` but po/{lang}.po is missing"
            ));
        }
    }
    for catalog in catalogs {
        if !linguas.contains(catalog) {
            problems.push(format!(
                "po/{catalog}.po exists but `{catalog}` is not listed in po/LINGUAS"
            ));
        }
    }
}

/// Validate one catalog: placeholders, headers, plural rules, the absence of
/// fuzzy/untranslated/obsolete entries, and msgid coverage against the template.
fn check_catalog(
    root: &Path,
    lang: &str,
    pot: &Path,
    problems: &mut Vec<String>,
) -> Result<(), String> {
    let po = workspace::catalog_path(root, lang);
    if !po.is_file() {
        // Already reported by the LINGUAS-consistency check.
        return Ok(());
    }

    let mo = workspace::work_dir(root).join(format!("check/{lang}.mo"));
    if let Some(parent) = mo.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    let formatted = tools::capture(
        tools::cmd("msgfmt")
            .arg("--check")
            .arg("--check-format")
            .arg("--check-header")
            .arg("--statistics")
            .arg("-o")
            .arg(&mo)
            .arg(&po),
    )?;
    let report = String::from_utf8_lossy(&formatted.stderr);
    if formatted.status.success() {
        if report.contains("fuzzy") {
            problems.push(format!(
                "po/{lang}.po has fuzzy entries; resolve them (zero fuzzy required)"
            ));
        }
        if report.contains("untranslated") {
            problems.push(format!(
                "po/{lang}.po has untranslated entries; complete them (zero untranslated required)"
            ));
        }
    } else {
        problems.push(format!(
            "po/{lang}.po failed msgfmt --check:\n      {}",
            indent(report.trim())
        ));
    }

    let text = std::fs::read_to_string(&po)
        .map_err(|err| format!("cannot read {}: {err}", po.display()))?;
    if text.lines().any(|line| line.starts_with("#~")) {
        problems.push(format!(
            "po/{lang}.po has obsolete entries; strip them with `msgattrib --no-obsolete`"
        ));
    }

    let compared = tools::capture(tools::cmd("msgcmp").arg(&po).arg(pot))?;
    if !compared.status.success() {
        problems.push(format!(
            "po/{lang}.po msgid set differs from the template:\n      {}",
            indent(String::from_utf8_lossy(&compared.stderr).trim())
        ));
    }
    Ok(())
}

/// Fail on gettext call shapes the extractor would silently miss, scanning the
/// whole workspace on the token tree (so qualified calls — with any spacing —
/// and unqualified calls outside the extraction roots are both caught).
fn check_call_shapes(root: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    for file in workspace::all_crate_sources(root)? {
        let source = std::fs::read_to_string(&file)
            .map_err(|err| format!("cannot read {}: {err}", file.display()))?;
        let location = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
        let in_root = workspace::is_extraction_root_file(root, &file);
        let allow_gettextrs = workspace::is_i18n_crate_file(root, &file);
        match lint::call_shape_violations(&source, in_root, allow_gettextrs) {
            Ok(violations) => {
                for violation in violations {
                    problems.push(format!(
                        "{location}:{}:{}: {}",
                        violation.line, violation.column, violation.message
                    ));
                }
            }
            Err(err) => problems.push(format!("{location}: {err}")),
        }
    }
    Ok(())
}

/// Shorten a long msgid for a one-line diagnostic.
fn truncate(text: &str) -> String {
    const MAX: usize = 48;
    if text.chars().count() <= MAX {
        text.to_owned()
    } else {
        let head: String = text.chars().take(MAX).collect();
        format!("{head}…")
    }
}

/// Indent a captured multi-line tool message to line up under a problem bullet.
fn indent(text: &str) -> String {
    text.replace('\n', "\n      ")
}
