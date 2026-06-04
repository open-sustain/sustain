// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Named-placeholder validation: the project uses `{name}` placeholders only,
//! because translated format strings are fed to `formatx!`, which errors on a
//! positional `{}` or `{0}` — and that error is `.expect()`ed, so a positional
//! placeholder slipping into a catalog would be a runtime panic. `xgettext` and
//! `msgfmt` accept positional placeholders, so the rule is enforced here.

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

    #[test]
    fn unterminated_placeholder_is_reported() {
        assert_eq!(placeholder_problems("Missing {brace").len(), 1);
    }
}
