// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Shared PO/POT helpers: reduce a template to its comparable message set so
//! that extraction and the currency check agree on what "the same strings"
//! means, independent of source locations, line wrapping, and the volatile
//! header.

use std::path::Path;

use crate::tools;

/// Normalize a POT to its message set: sorted by msgid, source locations
/// dropped, wrapping removed, header stripped. Two POTs with the same set of
/// (context, singular, plural, flags, translator-comment) messages normalize to
/// the same string regardless of where the strings appear in source or when the
/// template was generated.
pub fn normalized_messages(root: &Path, pot: &Path, tag: &str) -> Result<String, String> {
    let normalized = root
        .join("target/i18n/normalize")
        .join(format!("{tag}.norm.pot"));
    if let Some(parent) = normalized.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|err| format!("cannot create {}: {err}", parent.display()))?;
    }
    tools::finish(
        tools::cmd("msgcat")
            .arg("--no-location")
            .arg("--sort-output")
            .arg("--no-wrap")
            .arg("--to-code=UTF-8")
            .arg("-o")
            .arg(&normalized)
            .arg(pot),
    )?;
    let text = std::fs::read_to_string(&normalized)
        .map_err(|err| format!("cannot read {}: {err}", normalized.display()))?;
    Ok(strip_po_header(&text))
}

/// Drop a PO/POT file's leading comment block and header entry, returning only
/// the message catalog. The header is the first paragraph, terminated by the
/// first blank line; PO escapes real newlines inside strings, so a blank line
/// unambiguously ends the header.
fn strip_po_header(po: &str) -> String {
    match po.split_once("\n\n") {
        Some((_header, body)) => body.to_owned(),
        None => String::new(),
    }
}
