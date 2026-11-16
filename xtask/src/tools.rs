// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Thin wrappers over the installed GNU gettext command-line tools, plus the
//! preflight that fails early with an actionable message when the toolchain
//! cannot satisfy the localization contract. The AppStream ITS rules are
//! vendored in-repo (see `build-aux/gettext/`), so they are not discovered here.

use std::process::{Command, Output};

/// The gettext tools the catalog-maintenance commands (`i18n-extract`,
/// `i18n-check`) rely on. Their absence is a setup error, not a runtime
/// condition, so the preflight checks them up front.
const MAINTENANCE_TOOLS: &[&str] = &[
    "xgettext",
    "msgcat",
    "msgmerge",
    "msgattrib",
    "msgfmt",
    "msgcmp",
];

/// Minimum `xgettext` version. 0.24 is the first release with the `rust-format`
/// flag the extraction contract relies on; Debian trixie ships 0.23.1, which
/// rejects it, so the catalog-maintenance commands require a newer gettext.
const MIN_XGETTEXT: (u32, u32) = (0, 24);

/// Verify the gettext toolchain can maintain the catalogs: every tool present,
/// and `xgettext` new enough to extract Rust sources.
pub fn preflight_maintenance() -> Result<(), String> {
    for tool in MAINTENANCE_TOOLS {
        ensure_present(tool, "GNU gettext >= 0.24")?;
    }
    ensure_xgettext_version()
}

/// Verify the toolchain can compile the committed catalogs into installable
/// artifacts. That needs `msgfmt` alone, and no particular release of it:
/// compiling reads `.po` files, so it does not depend on the Rust support
/// extraction needs. Packaging builds therefore run on the distribution's own
/// gettext (Debian trixie, the GNOME SDK) without a newer toolchain.
pub fn preflight_compile() -> Result<(), String> {
    ensure_present("msgfmt", "GNU gettext")
}

/// A `Command` for `program` with a forced `C` locale so its diagnostics and
/// statistics are stable, parseable English regardless of the caller's locale.
pub fn cmd(program: &str) -> Command {
    let mut command = Command::new(program);
    command.env("LC_ALL", "C");
    command
}

/// Run `command` inheriting stdio and fail on a non-zero exit.
pub fn finish(command: &mut Command) -> Result<(), String> {
    let name = program_name(command);
    let status = command
        .status()
        .map_err(|err| format!("cannot run {name}: {err}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{name} failed ({status})"))
    }
}

/// Run `command` and capture its output without failing on a non-zero exit, so
/// the caller can inspect both the status and the captured text.
pub fn capture(command: &mut Command) -> Result<Output, String> {
    let name = program_name(command);
    command
        .output()
        .map_err(|err| format!("cannot run {name}: {err}"))
}

/// Confirm `tool` can be spawned at all; `package` names what provides it.
fn ensure_present(tool: &str, package: &str) -> Result<(), String> {
    cmd(tool)
        .arg("--version")
        .output()
        .map(|_| ())
        .map_err(|err| {
            format!(
                "required tool `{tool}` is unavailable ({err}); \
                 install {package} (Debian: `apt-get install gettext`)"
            )
        })
}

/// Confirm `xgettext` is at least [`MIN_XGETTEXT`].
fn ensure_xgettext_version() -> Result<(), String> {
    let output = cmd("xgettext")
        .arg("--version")
        .output()
        .map_err(|err| format!("cannot run xgettext --version: {err}"))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let first_line = stdout.lines().next().unwrap_or_default();
    let version_token = first_line.rsplit(' ').next().unwrap_or_default();
    let version = parse_version(version_token)?;
    if version < MIN_XGETTEXT {
        return Err(format!(
            "xgettext {}.{} is too old; need >= {}.{} (reported: {first_line:?})",
            version.0, version.1, MIN_XGETTEXT.0, MIN_XGETTEXT.1,
        ));
    }
    Ok(())
}

/// Parse the leading `MAJOR.MINOR` from a version string such as `0.26` or
/// `0.26.1`.
fn parse_version(version: &str) -> Result<(u32, u32), String> {
    let mut parts = version.split('.');
    let major = parts.next().and_then(|part| part.parse().ok());
    let minor = parts.next().and_then(|part| part.parse().ok());
    match (major, minor) {
        (Some(major), Some(minor)) => Ok((major, minor)),
        _ => Err(format!("cannot parse xgettext version from {version:?}")),
    }
}

/// The program name of a `Command`, for diagnostics.
fn program_name(command: &Command) -> String {
    command.get_program().to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_two_and_three_component_versions() {
        assert_eq!(parse_version("0.26"), Ok((0, 26)));
        assert_eq!(parse_version("0.26.1"), Ok((0, 26)));
        assert_eq!(parse_version("1.0"), Ok((1, 0)));
    }

    #[test]
    fn rejects_unparseable_versions() {
        assert!(parse_version("").is_err());
        assert!(parse_version("garbage").is_err());
    }
}
