// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Source-level localization lints that GNU gettext cannot perform itself:
//!
//! - **Call-shape validation** — gettext is extracted only from *unqualified*
//!   calls in the declared extraction roots. A qualified call
//!   (`sustain_i18n::gettext(...)`), or an unqualified call in a crate outside
//!   the roots, would be silently missed by `xgettext`. These are detected on
//!   the token tree (not by substring matching), which is immune to whitespace
//!   and comments and, unlike a `syn` AST walk, descends into macro bodies so a
//!   call nested in `formatx!(...)` is still seen.
//! - **Named-placeholder validation** — the project requires `{name}`
//!   placeholders only; `xgettext`/`msgfmt` accept positional `{}` and `{0}`
//!   too, so the rule is enforced here.

use std::str::FromStr;

use proc_macro2::{Delimiter, TokenStream, TokenTree};

use crate::workspace::RUST_SOURCE_ROOTS;

/// The unqualified function names `xgettext` is configured to extract.
const GETTEXT_FNS: &[&str] = &["gettext", "ngettext", "pgettext", "npgettext"];

/// A gettext call whose shape the extractor would miss.
#[derive(Debug, PartialEq, Eq)]
pub struct CallShapeViolation {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// Inspect Rust `source` for gettext call shapes the extractor cannot see.
///
/// `in_extraction_root` is whether the file is under [`RUST_SOURCE_ROOTS`]
/// (where unqualified calls are extracted and therefore allowed);
/// `allow_gettextrs` is whether the file is the `sustain_i18n` crate itself
/// (the one place permitted to name `gettextrs`).
pub fn call_shape_violations(
    source: &str,
    in_extraction_root: bool,
    allow_gettextrs: bool,
) -> Result<Vec<CallShapeViolation>, String> {
    let stream = TokenStream::from_str(source)
        .map_err(|err| format!("could not tokenize Rust source: {err}"))?;
    let mut violations = Vec::new();
    let config = Config {
        in_extraction_root,
        allow_gettextrs,
    };
    walk(
        &stream.into_iter().collect::<Vec<_>>(),
        &config,
        &mut violations,
    );
    Ok(violations)
}

/// Per-file context for [`walk`].
struct Config {
    in_extraction_root: bool,
    allow_gettextrs: bool,
}

/// Walk a flat token sequence (recursing into every delimited group, including
/// macro bodies) and record call-shape violations.
fn walk(tokens: &[TokenTree], config: &Config, out: &mut Vec<CallShapeViolation>) {
    for (index, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Ident(ident) => {
                let name = ident.to_string();
                let previous = index.checked_sub(1).and_then(|i| tokens.get(i));
                let next = tokens.get(index + 1);
                let preceded_by_path_sep =
                    matches!(previous, Some(TokenTree::Punct(p)) if p.as_char() == ':');
                let preceded_by_dot =
                    matches!(previous, Some(TokenTree::Punct(p)) if p.as_char() == '.');
                let is_call = matches!(
                    next,
                    Some(TokenTree::Group(group)) if group.delimiter() == Delimiter::Parenthesis
                );
                let start = ident.span().start();

                if GETTEXT_FNS.contains(&name.as_str()) && is_call && !preceded_by_dot {
                    if preceded_by_path_sep {
                        out.push(CallShapeViolation {
                            line: start.line,
                            column: start.column,
                            message: format!(
                                "qualified `{name}(...)` call is invisible to xgettext; \
                                 import the name from sustain_i18n and call it unqualified"
                            ),
                        });
                    } else if !config.in_extraction_root {
                        out.push(CallShapeViolation {
                            line: start.line,
                            column: start.column,
                            message: format!(
                                "unqualified `{name}(...)` call outside the extraction roots \
                                 ({RUST_SOURCE_ROOTS:?}); user-visible text belongs only there"
                            ),
                        });
                    }
                }

                if name == "gettextrs"
                    && !config.allow_gettextrs
                    && matches!(next, Some(TokenTree::Punct(p)) if p.as_char() == ':')
                {
                    out.push(CallShapeViolation {
                        line: start.line,
                        column: start.column,
                        message: "direct `gettextrs::` use bypasses the sustain_i18n boundary"
                            .to_owned(),
                    });
                }
            }
            TokenTree::Group(group) => {
                walk(&group.stream().into_iter().collect::<Vec<_>>(), config, out);
            }
            _ => {}
        }
    }
}

/// Describe every placeholder in `format` that is not a `{name}` named
/// placeholder. An empty list means the string is acceptable.
///
/// `{{` / `}}` are literal-brace escapes and are skipped. A placeholder is named
/// when the text before any `:` format spec is a Rust-style identifier; `{}`
/// (positional) and `{0}` (numbered) are rejected.
pub fn placeholder_problems(format: &str) -> Vec<String> {
    let mut problems = Vec::new();
    let mut chars = format.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                if chars.peek() == Some(&'{') {
                    chars.next();
                    continue;
                }
                let mut body = String::new();
                let mut closed = false;
                while let Some(&next) = chars.peek() {
                    chars.next();
                    if next == '}' {
                        closed = true;
                        break;
                    }
                    body.push(next);
                }
                if !closed {
                    problems.push(format!("unterminated placeholder `{{{body}`"));
                    continue;
                }
                let name = body.split(':').next().unwrap_or_default();
                if !is_identifier(name) {
                    problems.push(format!(
                        "non-named placeholder `{{{body}}}`; use `{{name}}` (named placeholders only)"
                    ));
                }
            }
            '}' if chars.peek() == Some(&'}') => {
                chars.next();
            }
            _ => {}
        }
    }
    problems
}

/// Whether `text` is a non-empty Rust-style identifier (so `{name}` is named,
/// while `{}` and `{0}` are not).
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    match chars.next() {
        Some(first) if first.is_alphabetic() || first == '_' => {}
        _ => return false,
    }
    chars.all(|ch| ch.is_alphanumeric() || ch == '_')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn messages(source: &str, in_root: bool, allow_gettextrs: bool) -> Vec<String> {
        call_shape_violations(source, in_root, allow_gettextrs)
            .expect("valid Rust tokenizes")
            .into_iter()
            .map(|violation| violation.message)
            .collect()
    }

    #[test]
    fn unqualified_call_in_root_is_allowed() {
        let messages = messages(r#"fn f() { let _ = gettext("Songs"); }"#, true, false);
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn unqualified_call_outside_roots_is_flagged() {
        let messages = messages(r#"fn f() { let _ = gettext("Songs"); }"#, false, false);
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("outside the extraction roots"));
    }

    #[test]
    fn qualified_call_is_flagged_even_with_whitespace() {
        // The exact case a substring guard misses: spaces around `::` and `(`.
        let messages = messages(
            r#"fn f() { let _ = sustain_i18n :: gettext ("Songs"); }"#,
            true,
            false,
        );
        assert_eq!(messages.len(), 1);
        assert!(messages[0].contains("invisible to xgettext"));
    }

    #[test]
    fn call_nested_in_formatx_macro_is_seen() {
        // syn would treat the macro body as opaque; the token walk descends.
        let qualified = messages(
            r#"fn f(n: u64) { let _ = formatx!(crate::i18n::ngettext("{n} a", "{n} b", n)); }"#,
            true,
            false,
        );
        assert_eq!(qualified.len(), 1, "{qualified:?}");
        assert!(qualified[0].contains("invisible to xgettext"));

        let unqualified_in_root = messages(
            r#"fn f(n: u64) { let _ = formatx!(ngettext("a", "b", n)); }"#,
            true,
            false,
        );
        assert!(unqualified_in_root.is_empty(), "{unqualified_in_root:?}");
    }

    #[test]
    fn gettextrs_use_is_flagged_outside_the_i18n_crate() {
        let flagged = messages("use gettextrs::setlocale;", false, false);
        assert_eq!(flagged.len(), 1);
        assert!(flagged[0].contains("bypasses the sustain_i18n boundary"));

        let allowed = messages("use gettextrs::setlocale;", false, true);
        assert!(allowed.is_empty(), "{allowed:?}");
    }

    #[test]
    fn method_named_gettext_is_not_a_gettext_call() {
        let messages = messages(r#"fn f(x: T) { let _ = x.gettext(); }"#, false, false);
        assert!(messages.is_empty(), "{messages:?}");
    }

    #[test]
    fn named_placeholders_are_accepted() {
        assert!(placeholder_problems("Imported {count} of {total} tracks").is_empty());
        assert!(placeholder_problems("Rating {value:>3} stars").is_empty());
        assert!(placeholder_problems("A literal {{brace}} and {name}").is_empty());
        assert!(placeholder_problems("No placeholders here").is_empty());
    }

    #[test]
    fn positional_placeholders_are_rejected() {
        assert_eq!(placeholder_problems("Imported {} tracks").len(), 1);
        assert_eq!(placeholder_problems("{0} of {1}").len(), 2);
        assert_eq!(placeholder_problems("Bad {1count}").len(), 1);
    }
}
