// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Sustain workspace automation, invoked as `cargo xtask <command>` via the
//! alias in `.cargo/config.toml`.
//!
//! Commands:
//!
//! - `i18n-extract` — regenerate `po/sustain.pot` and resync the catalogs.
//! - `i18n-check` — gate the localization catalogs and template (drift,
//!   named-placeholder rule, fuzzy/untranslated/obsolete entries, catalog
//!   validity, LINGUAS consistency).
//! - `i18n-compile` — build the installable `.mo`, desktop, and AppStream
//!   artifacts under `target/i18n/dist/`.

mod check;
mod compile;
mod extract;
mod lint;
mod po;
mod tools;
mod workspace;

use std::process::ExitCode;

/// Usage text for bad invocations and `--help`.
const USAGE: &str = "\
Usage: cargo xtask <command>

Commands:
  i18n-extract   Regenerate po/sustain.pot and resync catalogs
  i18n-check     Validate the localization catalogs and template
  i18n-compile   Build .mo, desktop, and AppStream artifacts under target/i18n/dist/";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(command) = args.next() else {
        eprintln!("xtask: a command is required\n\n{USAGE}");
        return ExitCode::FAILURE;
    };
    if args.next().is_some() {
        eprintln!("xtask: `{command}` takes no arguments\n\n{USAGE}");
        return ExitCode::FAILURE;
    }

    let result = match command.as_str() {
        "i18n-extract" => extract::run(),
        "i18n-check" => check::run(),
        "i18n-compile" => compile::run(),
        "-h" | "--help" | "help" => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        other => Err(format!("unknown command `{other}`\n\n{USAGE}")),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(message) => {
            eprintln!("xtask: {message}");
            ExitCode::FAILURE
        }
    }
}
