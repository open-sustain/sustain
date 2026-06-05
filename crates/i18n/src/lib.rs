// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Sustain's localization boundary.
//!
//! This is the only crate in the workspace that links GNU gettext (via
//! `gettext-rs`) and the only one that depends on `formatx`. Feature code that
//! shows user-visible text — `sustain_ui_gtk` widgets and the
//! `sustain_app_runtime` notification messages — localizes by importing the
//! re-exported `gettext`, `ngettext`, `pgettext`, and `npgettext` functions and
//! the `formatx!` macro from here, and never reaches for `gettext-rs` directly.
//! The durable domain, store, playback, and desktop-protocol crates stay free
//! of localized strings.
//!
//! # Call shape
//!
//! GNU `xgettext` extracts message ids by recognizing the *unqualified* names
//! `gettext`, `ngettext`, `pgettext`, and `npgettext` at the call site. Import
//! them by name and call them unqualified; do not write `sustain_i18n::gettext`
//! or the extractor will silently miss the string. A `gettext`/`ngettext` call
//! nested inside a macro is still extracted, so interpolated messages go through
//! the [`tr_format!`] macro: it formats a `gettext`/`ngettext` template with
//! named placeholders and panics only on a placeholder mismatch that
//! `cargo xtask i18n-check` proves cannot occur in a committed catalog.
//!
//! ```no_run
//! use sustain_i18n::{ngettext, tr_format};
//!
//! let count: u32 = 3;
//! let text = tr_format!(
//!     ngettext("{count} song imported", "{count} songs imported", count),
//!     count = count,
//! );
//! # let _ = text;
//! ```
//!
//! Format strings use named placeholders only (`{count}`, `{path}`, `{title}`),
//! because word order varies across languages. Keep them inside the simple
//! `Display` subset `formatx` supports and pre-format any specialized value
//! before inserting it.
//!
//! # Initialization
//!
//! [`init`] is called exactly once, early in `main`, before GTK initialization
//! or thread creation. It binds the process locale to the environment and
//! routes lookups to Sustain's installed catalogs. Locale state is
//! process-global and is never mutated again.

use std::{ffi::OsString, path::PathBuf};

use gettextrs::LocaleCategory;

// The localization surface every consumer imports from this crate. These are
// re-exported, not wrapped, so that `xgettext` sees ordinary `gettext(...)` /
// `ngettext(...)` / `pgettext(...)` / `npgettext(...)` call sites in consumer
// source and can extract their message ids.
pub use formatx::formatx;
pub use gettextrs::{gettext, ngettext, npgettext, pgettext};

/// Format a localized template, substituting named placeholders, and panic if
/// substitution fails.
///
/// The first argument is a `gettext` / `ngettext` (etc.) call, kept literal so
/// `xgettext` extracts its message id; the remaining `name = value` arguments
/// bind the template's `{name}` placeholders exactly as [`formatx!`] expects.
///
/// `cargo xtask i18n-check` validates that every committed catalog's
/// placeholders match the English template, so the only way the substitution
/// can fail is a programming error in this repository — a binding that does not
/// match the template — which surfaces the first time the line runs in a test or
/// in development, never in a shipped translation. The panic is therefore an
/// invariant assertion, not a runtime error path, which is why this is a thin
/// wrapper over `formatx!(…).expect(…)` rather than a fallible API.
///
/// ```no_run
/// use sustain_i18n::{ngettext, tr_format};
///
/// let count: u32 = 3;
/// let text = tr_format!(
///     ngettext("{count} song imported", "{count} songs imported", count),
///     count = count,
/// );
/// # let _ = text;
/// ```
#[macro_export]
macro_rules! tr_format {
    ($template:expr $(,)?) => {
        $crate::formatx!($template)
            .expect("gettext catalog placeholders are validated by `cargo xtask i18n-check`")
    };
    ($template:expr, $($arg:tt)+) => {
        $crate::formatx!($template, $($arg)+)
            .expect("gettext catalog placeholders are validated by `cargo xtask i18n-check`")
    };
}

/// The gettext text domain. Matches the basename of the installed catalogs at
/// `<locale dir>/<lang>/LC_MESSAGES/sustain.mo`.
const TEXT_DOMAIN: &str = "sustain";

/// Compile-time installed locale directory.
///
/// Debian's `.deb` installs catalogs under `/usr/share/locale`, which is the
/// default. The Flatpak build compiles with `SUSTAIN_INSTALLED_LOCALE_DIR` set
/// to `/app/share/locale` to match its sandbox prefix. This is resolved at
/// build time so the binary carries no runtime prefix probing or filesystem
/// heuristics.
const INSTALLED_LOCALE_DIR: &str = match option_env!("SUSTAIN_INSTALLED_LOCALE_DIR") {
    Some(dir) => dir,
    None => "/usr/share/locale",
};

/// Environment variable that overrides the locale directory for development and
/// tests, where no catalog is installed under the system prefix. Honored ahead
/// of the compile-time default; an empty value is treated as unset.
const LOCALE_DIR_OVERRIDE_ENV: &str = "SUSTAIN_LOCALE_DIR";

/// Bind the process to the user's locale and route gettext lookups to Sustain's
/// catalogs.
///
/// Call this exactly once, as early in `main` as possible — after the first
/// timing landmark but before GTK is initialized or any thread is spawned, so
/// that `setlocale` also lets GTK localize its own stock strings and no other
/// thread observes a half-initialized locale.
///
/// `setlocale(LC_ALL, "")` reads the standard POSIX precedence
/// (`LC_ALL` → `LC_MESSAGES` → `LANG`); `LANG=C` or an unsupported locale leaves
/// the C locale active, under which every lookup returns its English message id
/// unchanged.
pub fn init() {
    let locale_dir = locale_dir();

    // Adopt the environment's locale. An unset/`C` environment leaves the C
    // locale active and lookups fall back to the English message ids.
    gettextrs::setlocale(LocaleCategory::LcAll, "");

    // The domain name and codeset are static, NUL-free byte strings, and a
    // locale directory path cannot contain an interior NUL on Unix; the only
    // documented failure of these calls is an interior NUL in an argument, so
    // a failure here is an unreachable invariant violation rather than a
    // recoverable runtime condition.
    gettextrs::bindtextdomain(TEXT_DOMAIN, locale_dir.as_path())
        .expect("bindtextdomain only fails on an interior NUL in the domain or path");
    gettextrs::bind_textdomain_codeset(TEXT_DOMAIN, "UTF-8")
        .expect("bind_textdomain_codeset only fails on an interior NUL in the domain or codeset");
    gettextrs::textdomain(TEXT_DOMAIN)
        .expect("textdomain only fails on an interior NUL in the domain");
}

/// Resolve the directory passed to `bindtextdomain` from the real environment
/// and the compile-time default.
fn locale_dir() -> PathBuf {
    resolve_locale_dir(
        std::env::var_os(LOCALE_DIR_OVERRIDE_ENV),
        INSTALLED_LOCALE_DIR,
    )
}

/// Deterministic locale-directory selection, factored out of [`locale_dir`] so
/// it can be unit-tested without touching process-global environment or locale
/// state: the development/test override wins when present and non-empty,
/// otherwise the compile-time installed default is used.
fn resolve_locale_dir(override_value: Option<OsString>, installed_default: &str) -> PathBuf {
    match override_value {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => PathBuf::from(installed_default),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_when_present_and_non_empty_takes_precedence() {
        let dir = resolve_locale_dir(
            Some(OsString::from("/opt/sustain/locale")),
            "/usr/share/locale",
        );
        assert_eq!(dir, PathBuf::from("/opt/sustain/locale"));
    }

    #[test]
    fn installed_default_used_when_override_absent() {
        let dir = resolve_locale_dir(None, "/usr/share/locale");
        assert_eq!(dir, PathBuf::from("/usr/share/locale"));
    }

    #[test]
    fn empty_override_falls_back_to_installed_default() {
        let dir = resolve_locale_dir(Some(OsString::new()), "/app/share/locale");
        assert_eq!(dir, PathBuf::from("/app/share/locale"));
    }
}
