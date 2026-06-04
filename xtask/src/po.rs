// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! PO/POT helpers used by extraction and the check: a canonical message-body
//! form for currency comparison, and a minimal reader that yields the source
//! strings of `rust-format` entries so their placeholders can be validated.
//!
//! Only what these callers need is parsed here; the heavy lifting stays with
//! the GNU gettext tools.

use std::path::Path;

use crate::tools;

/// The message catalog of `pot`, normalized for currency comparison: sorted by
/// msgid, locations dropped, wrapping removed, header stripped. Two POTs with
/// the same set of (context, singular, plural, flags, translator-comment)
/// messages produce the same string regardless of source layout or timestamp.
pub fn message_body(root: &Path, pot: &Path, tag: &str) -> Result<String, String> {
    let normalized = root
        .join("target/i18n/work/normalize")
        .join(format!("{tag}.pot"));
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
    Ok(strip_header(&text))
}

/// The message catalog: everything after the header paragraph. PO escapes real
/// newlines inside strings, so the first blank line unambiguously ends the
/// header (which carries the volatile creation date, excluded from comparison).
fn strip_header(po: &str) -> String {
    po.split_once("\n\n")
        .map_or_else(String::new, |(_, body)| body.to_owned())
}

/// A source string carried by a `rust-format` entry, with a human reference for
/// diagnostics.
pub struct RustFormatString {
    /// The msgid text the string belongs to, for error messages.
    pub reference: String,
    /// The format string itself (msgid or msgid_plural).
    pub value: String,
}

/// Collect the source strings (msgid and msgid_plural) of every `rust-format`
/// entry in `pot`, so their placeholders can be validated. Translations inherit
/// the named-placeholder constraint transitively: `msgfmt --check-format`
/// rejects any catalog whose msgstr placeholders differ from the msgid's.
pub fn rust_format_source_strings(pot: &str) -> Vec<RustFormatString> {
    let mut strings = Vec::new();
    for entry in pot.split("\n\n") {
        if !entry_is_rust_format(entry) {
            continue;
        }
        let mut reference = String::new();
        for value in entry_source_strings(entry) {
            if reference.is_empty() {
                reference = value.clone();
            }
            strings.push(RustFormatString {
                reference: reference.clone(),
                value,
            });
        }
    }
    strings
}

/// Whether a PO entry block carries the `rust-format` flag.
fn entry_is_rust_format(entry: &str) -> bool {
    entry
        .lines()
        .filter(|line| line.starts_with("#,"))
        .any(|line| line.split(',').any(|flag| flag.trim() == "rust-format"))
}

/// The concatenated values of the `msgid` and `msgid_plural` fields of a PO
/// entry block (the source strings; msgstr translations are not validated
/// here).
fn entry_source_strings(entry: &str) -> Vec<String> {
    let mut strings = Vec::new();
    let mut current: Option<String> = None;
    for line in entry.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed
            .strip_prefix("msgid ")
            .or_else(|| trimmed.strip_prefix("msgid_plural "))
        {
            if let Some(done) = current.take() {
                strings.push(done);
            }
            current = Some(quoted_content(rest).unwrap_or_default().to_owned());
        } else if trimmed.starts_with("msgstr") {
            if let Some(done) = current.take() {
                strings.push(done);
            }
        } else if trimmed.starts_with('"') {
            if let Some(buffer) = current.as_mut() {
                if let Some(content) = quoted_content(trimmed) {
                    buffer.push_str(content);
                }
            }
        }
    }
    if let Some(done) = current.take() {
        strings.push(done);
    }
    strings.retain(|value| !value.is_empty());
    strings
}

/// The text between the first and last double quote of a PO line. PO does not
/// escape braces, so this raw slice is sufficient for placeholder scanning.
fn quoted_content(line: &str) -> Option<&str> {
    let start = line.find('"')?;
    let end = line.rfind('"')?;
    if end > start {
        line.get(start + 1..end)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_header_returns_only_the_message_catalog() {
        let pot = "# comment\nmsgid \"\"\nmsgstr \"\"\n\n#: a.rs:1\nmsgid \"Hi\"\nmsgstr \"\"\n";
        assert_eq!(strip_header(pot), "#: a.rs:1\nmsgid \"Hi\"\nmsgstr \"\"\n");
    }

    #[test]
    fn rust_format_source_strings_collects_singular_and_plural() {
        let pot = "\
#: a.rs:1
#, rust-format
msgid \"{count} song\"
msgid_plural \"{count} songs\"
msgstr[0] \"\"
msgstr[1] \"\"

#: b.rs:2
msgid \"Not format flagged {0}\"
msgstr \"\"

#: c.rs:3
#, rust-format
msgid \"Imported {} files\"
msgstr \"\"";
        let collected = rust_format_source_strings(pot);
        let values: Vec<&str> = collected.iter().map(|s| s.value.as_str()).collect();
        assert_eq!(
            values,
            vec!["{count} song", "{count} songs", "Imported {} files"]
        );
    }

    #[test]
    fn multiline_msgid_is_concatenated() {
        let pot = "\
#, rust-format
msgid \"\"
\"first {a} \"
\"second {b}\"
msgstr \"\"";
        let collected = rust_format_source_strings(pot);
        assert_eq!(collected.len(), 1);
        assert_eq!(collected[0].value, "first {a} second {b}");
    }
}
