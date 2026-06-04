// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `cargo xtask i18n-check`: gate the localization contract. Fails on template
//! drift, catalog/LINGUAS inconsistency, fuzzy/untranslated/obsolete entries,
//! placeholder or header errors, msgid-set drift between a catalog and the
//! template, and gettext call shapes the extractor would miss.
//!
//! The check is semantic: the template comparison ignores source locations and
//! the header so unrelated code movement and a refreshed creation date never
//! produce a false failure — only a changed message set does. It does not claim
//! to find every unmarked English literal; that remains a source-audit concern.

use std::path::Path;

use crate::{extract, po, tools, workspace};

/// Qualified call forms that `xgettext` cannot see. Consumers must import the
/// names from `sustain_i18n` and call them unqualified.
const FORBIDDEN_QUALIFIED_CALLS: &[&str] = &[
    "sustain_i18n::gettext(",
    "sustain_i18n::ngettext(",
    "sustain_i18n::pgettext(",
    "sustain_i18n::npgettext(",
];

/// Entry point for `cargo xtask i18n-check`.
pub fn run() -> Result<(), String> {
    let root = workspace::workspace_root();
    let tools = tools::preflight()?;
    let mut problems = Vec::new();

    check_pot_current(&root, &tools, &mut problems)?;

    let linguas = workspace::linguas(&root)?;
    let catalogs = workspace::catalog_langs(&root)?;
    check_linguas_consistency(&linguas, &catalogs, &mut problems);

    let pot = workspace::pot_path(&root);
    for lang in &linguas {
        check_catalog(&root, lang, &pot, &mut problems)?;
    }

    check_call_shapes(&root, &mut problems)?;

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

/// Fail if `po/sustain.pot` does not match a freshly extracted template,
/// comparing message sets only (locations and header stripped).
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

    let fresh = root.join("target/i18n/check/sustain.pot");
    extract::generate_pot(root, tools, &fresh)?;

    let committed_messages = po::normalized_messages(root, &committed, "committed")?;
    let fresh_messages = po::normalized_messages(root, &fresh, "fresh")?;
    if committed_messages != fresh_messages {
        problems.push(
            "po/sustain.pot is out of date with the marked source strings; \
             run `cargo xtask i18n-extract` and commit po/sustain.pot"
                .to_owned(),
        );
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

/// Validate one catalog: placeholders, headers, plural rules, and the absence
/// of fuzzy, untranslated, or obsolete entries, plus msgid coverage against the
/// template.
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

    let mo = root.join("target/i18n/check").join(format!("{lang}.mo"));
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

/// Fail on gettext call shapes the extractor would silently miss.
fn check_call_shapes(root: &Path, problems: &mut Vec<String>) -> Result<(), String> {
    let i18n_crate = root.join("crates").join("i18n");
    for file in workspace::all_crate_sources(root)? {
        // The i18n crate is the one place allowed to name `gettextrs` directly.
        if file.starts_with(&i18n_crate) {
            continue;
        }
        let text = std::fs::read_to_string(&file)
            .map_err(|err| format!("cannot read {}: {err}", file.display()))?;
        let location = file.strip_prefix(root).unwrap_or(&file).to_string_lossy();
        for (index, line) in text.lines().enumerate() {
            let line_number = index + 1;
            for call in FORBIDDEN_QUALIFIED_CALLS {
                if line.contains(call) {
                    problems.push(format!(
                        "{location}:{line_number}: qualified `{}` is invisible to xgettext; \
                         import the name from sustain_i18n and call it unqualified",
                        call.trim_end_matches('(')
                    ));
                }
            }
            if line.contains("gettextrs::") {
                problems.push(format!(
                    "{location}:{line_number}: direct `gettextrs::` use bypasses the sustain_i18n boundary"
                ));
            }
        }
    }
    Ok(())
}

/// Indent a captured multi-line tool message to line up under a problem bullet.
fn indent(text: &str) -> String {
    text.replace('\n', "\n      ")
}
