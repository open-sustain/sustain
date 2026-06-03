// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Command-line parsing and on-disk path resolution for the Sustain
//! binary.
//!
//! A developer running Sustain out of a checkout must be able to point
//! the running instance at a throwaway database, settings file, and
//! artwork cache so it never collides with — or corrupts — the data of
//! a system-installed instance (issue #7). This module owns every
//! decision about *where* Sustain reads and writes, so the composition
//! root in `main` resolves all three locations once, logs them, and
//! hands concrete paths to the library store, settings store, and UI.
//!
//! Resolution is a pure function of the parsed flags, the process
//! working directory, and the platform XDG defaults, so the precedence
//! matrix is unit-tested below without touching the real environment.

use std::fmt;
use std::path::{Path, PathBuf};

use directories::BaseDirs;

/// File names used inside an override scope. The database and settings
/// land directly under the scope base; derived caches go in a sibling
/// directory, so a developer can wipe caches without touching their dev
/// library.
const SCOPE_DATABASE_NAME: &str = "sustain.sqlite";
const SCOPE_SETTINGS_NAME: &str = "sustain.toml";
const SCOPE_CACHE_DIR_NAME: &str = "sustain.cache";

/// Help text printed for `--help` and on any argument error.
pub(crate) const USAGE: &str = "\
Usage: sustain [OPTIONS]

Developer-isolation options (default: XDG config/data/cache locations):
  --config <path>     Use this TOML settings file instead of the XDG default.
  --database <path>   Use this SQLite database instead of the XDG default.
  --local-scope       Keep config, database, and cache in the working
      --dev           directory (sustain.toml, sustain.sqlite, sustain.cache/).
  -h, --help          Print this help and exit.

Explicit --config/--database win over --local-scope. When any of these
flags is present, Sustain never falls back to the XDG locations: the
instance is fully self-contained in the overridden paths.";

/// Parsed command-line flags.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct Cli {
    pub(crate) config: Option<PathBuf>,
    pub(crate) database: Option<PathBuf>,
    pub(crate) local_scope: bool,
    /// Hidden maintenance command: rewrite every library track's file
    /// tags from the authoritative SQLite values, then exit without
    /// launching the UI (#143). Deliberately absent from [`USAGE`] — it
    /// is only useful after a bulk external import (Rhythmbox / iTunes
    /// XML) left SQLite and the files out of sync; a normal Sustain user
    /// never drifts, so the command stays undocumented.
    pub(crate) force_backfill: bool,
}

/// Parsed Sustain flags plus the deliberately minimal argv forwarded to GTK.
/// GApplication must see `argv[0]`, but developer-isolation flags belong to
/// Sustain and would otherwise be rejected as unknown GTK options.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ParsedProcessArgs {
    pub(crate) cli: Cli,
    pub(crate) gtk_arguments: Vec<String>,
}

/// An argument the parser could not accept.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum CliError {
    /// A value-taking flag (`--config` / `--database`) had no path argument.
    MissingValue(&'static str),
    /// An unrecognised argument was supplied.
    Unknown(String),
}

impl fmt::Display for CliError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingValue(flag) => write!(formatter, "missing path value for {flag}"),
            Self::Unknown(argument) => write!(formatter, "unrecognised argument '{argument}'"),
        }
    }
}

/// Parse already-program-name-stripped arguments into a [`Cli`].
///
/// `Ok(None)` means `--help`/`-h` was requested; the caller should print
/// [`USAGE`] and exit successfully.
pub(crate) fn parse_args<I>(arguments: I) -> Result<Option<Cli>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut cli = Cli::default();
    let mut arguments = arguments.into_iter();
    while let Some(argument) = arguments.next() {
        let (name, inline) = match argument.split_once('=') {
            Some((flag, value)) => (flag.to_owned(), Some(value.to_owned())),
            None => (argument.clone(), None),
        };
        match name.as_str() {
            "--help" | "-h" if inline.is_none() => return Ok(None),
            "--local-scope" | "--dev" if inline.is_none() => cli.local_scope = true,
            "--force-backfill" if inline.is_none() => cli.force_backfill = true,
            "--config" => cli.config = Some(take_value("--config", inline, &mut arguments)?),
            "--database" => cli.database = Some(take_value("--database", inline, &mut arguments)?),
            _ => return Err(CliError::Unknown(argument)),
        }
    }
    Ok(Some(cli))
}

/// Parse a process argument vector including `argv[0]`, retaining only `argv[0]`
/// for GTK after Sustain has consumed its own options.
pub(crate) fn parse_process_args<I>(arguments: I) -> Result<Option<ParsedProcessArgs>, CliError>
where
    I: IntoIterator<Item = String>,
{
    let mut arguments = arguments.into_iter();
    let program_name = arguments.next().unwrap_or_else(|| "sustain".to_owned());
    parse_args(arguments).map(|parsed| {
        parsed.map(|cli| ParsedProcessArgs {
            cli,
            gtk_arguments: vec![program_name],
        })
    })
}

/// Resolve a value-taking flag's path from either its inline `=value`
/// form or the next argument.
fn take_value(
    flag: &'static str,
    inline: Option<String>,
    rest: &mut impl Iterator<Item = String>,
) -> Result<PathBuf, CliError> {
    match inline {
        Some(value) => Ok(PathBuf::from(value)),
        None => rest
            .next()
            .map(PathBuf::from)
            .ok_or(CliError::MissingValue(flag)),
    }
}

/// The platform XDG defaults, resolved once by the caller. A field is
/// `None` when its base directory cannot be derived (e.g. both the
/// relevant `XDG_*` variable and `HOME` are unset); that is only an
/// error when no override flag covers the slot.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct XdgDefaults {
    pub(crate) config: Option<PathBuf>,
    pub(crate) database: Option<PathBuf>,
    pub(crate) cache_dir: Option<PathBuf>,
}

/// The three concrete on-disk locations the running instance will touch.
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ResolvedPaths {
    pub(crate) config: PathBuf,
    pub(crate) database: PathBuf,
    pub(crate) cache_dir: PathBuf,
}

/// Why path resolution failed.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum PathError {
    /// An override scope needs cwd-relative locations, but the process
    /// working directory could not be determined.
    WorkingDirectoryUnavailable,
    /// Default (XDG) mode, but the named base directory is underivable.
    DefaultUnavailable(&'static str),
}

impl fmt::Display for PathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WorkingDirectoryUnavailable => {
                write!(formatter, "the current working directory is unavailable")
            }
            Self::DefaultUnavailable(slot) => write!(
                formatter,
                "the default {slot} location is unavailable (XDG_* and HOME are unset)"
            ),
        }
    }
}

/// The default XDG artwork-cache directory: `<cache_dir>/sustain`.
/// `None` when the platform cache directory cannot be derived.
pub(crate) fn default_cache_dir() -> Option<PathBuf> {
    Some(BaseDirs::new()?.cache_dir().join("sustain"))
}

/// Resolve the config, database, and cache-directory paths from the
/// parsed flags, the process working directory, and the XDG defaults.
///
/// Precedence (issue #7):
/// * No flags → the XDG defaults verbatim.
/// * Otherwise (any override flag present) → never touch XDG. A *scope
///   base directory* anchors every slot the flags don't pin explicitly:
///   the working directory under `--local-scope`, else the parent
///   directory of whichever explicit path was given. Explicit
///   `--config` / `--database` always win for their own slot; the
///   artwork cache always lives in `<base>/sustain.cache`.
pub(crate) fn resolve_paths(
    cli: &Cli,
    working_dir: Option<&Path>,
    defaults: &XdgDefaults,
) -> Result<ResolvedPaths, PathError> {
    let any_override = cli.local_scope || cli.config.is_some() || cli.database.is_some();
    if !any_override {
        return Ok(ResolvedPaths {
            config: defaults
                .config
                .clone()
                .ok_or(PathError::DefaultUnavailable("config"))?,
            database: defaults
                .database
                .clone()
                .ok_or(PathError::DefaultUnavailable("database"))?,
            cache_dir: defaults
                .cache_dir
                .clone()
                .ok_or(PathError::DefaultUnavailable("cache"))?,
        });
    }

    let base = scope_base(cli, working_dir)?;
    Ok(ResolvedPaths {
        database: cli
            .database
            .clone()
            .unwrap_or_else(|| base.join(SCOPE_DATABASE_NAME)),
        config: cli
            .config
            .clone()
            .unwrap_or_else(|| base.join(SCOPE_SETTINGS_NAME)),
        cache_dir: base.join(SCOPE_CACHE_DIR_NAME),
    })
}

/// The directory that anchors any slot an override scope leaves
/// implicit. `--local-scope` anchors on the working directory; an
/// explicit `--database`/`--config` without `--local-scope` anchors on
/// that path's parent directory, so the whole instance stays beside the
/// file the developer named.
fn scope_base(cli: &Cli, working_dir: Option<&Path>) -> Result<PathBuf, PathError> {
    if cli.local_scope {
        return working_dir
            .map(Path::to_path_buf)
            .ok_or(PathError::WorkingDirectoryUnavailable);
    }
    // `any_override` was true and `local_scope` is false, so at least one
    // explicit path is present.
    let anchor = cli
        .database
        .as_deref()
        .or(cli.config.as_deref())
        .expect("an override without --local-scope always names an explicit path");
    parent_or_working_dir(anchor, working_dir)
}

/// The parent directory of `path`, or the working directory when `path`
/// is a bare relative file name (empty parent) so its sibling cache
/// lands beside it.
fn parent_or_working_dir(path: &Path, working_dir: Option<&Path>) -> Result<PathBuf, PathError> {
    match path.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => Ok(parent.to_path_buf()),
        _ => working_dir
            .map(Path::to_path_buf)
            .ok_or(PathError::WorkingDirectoryUnavailable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    fn xdg_defaults() -> XdgDefaults {
        XdgDefaults {
            config: Some(PathBuf::from("/xdg/config/sustain/settings.toml")),
            database: Some(PathBuf::from("/xdg/data/sustain/library.sqlite")),
            cache_dir: Some(PathBuf::from("/xdg/cache/sustain")),
        }
    }

    #[test]
    fn parses_no_arguments_as_empty_cli() {
        assert_eq!(parse_args(args(&[])), Ok(Some(Cli::default())));
    }

    #[test]
    fn process_parser_consumes_sustain_flags_before_gtk_argument_forwarding() {
        let parsed = parse_process_args(args(&[
            "sustain",
            "--config",
            "a.toml",
            "--database=b.sqlite",
            "--local-scope",
        ]))
        .expect("valid")
        .expect("not help");

        assert_eq!(parsed.gtk_arguments, vec!["sustain"]);
        assert_eq!(
            parsed.cli,
            Cli {
                config: Some(PathBuf::from("a.toml")),
                database: Some(PathBuf::from("b.sqlite")),
                local_scope: true,
                force_backfill: false,
            }
        );
    }

    #[test]
    fn parses_value_flags_in_both_forms() {
        let spaced = parse_args(args(&["--config", "a.toml", "--database", "b.sqlite"]))
            .expect("valid")
            .expect("not help");
        let inlined = parse_args(args(&["--config=a.toml", "--database=b.sqlite"]))
            .expect("valid")
            .expect("not help");
        let expected = Cli {
            config: Some(PathBuf::from("a.toml")),
            database: Some(PathBuf::from("b.sqlite")),
            local_scope: false,
            force_backfill: false,
        };
        assert_eq!(spaced, expected);
        assert_eq!(inlined, expected);
    }

    #[test]
    fn parses_local_scope_and_its_dev_alias() {
        for flag in ["--local-scope", "--dev"] {
            let cli = parse_args(args(&[flag])).expect("valid").expect("not help");
            assert!(cli.local_scope);
        }
    }

    #[test]
    fn parses_hidden_force_backfill_flag() {
        let cli = parse_args(args(&["--force-backfill"]))
            .expect("valid")
            .expect("not help");
        assert!(cli.force_backfill);
        // A value appended to the boolean flag is rejected, not accepted.
        assert_eq!(
            parse_args(args(&["--force-backfill=1"])),
            Err(CliError::Unknown("--force-backfill=1".to_owned()))
        );
    }

    #[test]
    fn help_short_circuits() {
        assert_eq!(parse_args(args(&["--help"])), Ok(None));
        assert_eq!(parse_args(args(&["-h"])), Ok(None));
    }

    #[test]
    fn missing_value_is_an_error() {
        assert_eq!(
            parse_args(args(&["--config"])),
            Err(CliError::MissingValue("--config"))
        );
    }

    #[test]
    fn unknown_flag_is_an_error() {
        assert_eq!(
            parse_args(args(&["--nope"])),
            Err(CliError::Unknown("--nope".to_owned()))
        );
        // A value appended to a boolean flag is unrecognised, not silently
        // accepted.
        assert_eq!(
            parse_args(args(&["--dev=1"])),
            Err(CliError::Unknown("--dev=1".to_owned()))
        );
    }

    #[test]
    fn no_flags_resolves_to_xdg_defaults() {
        let resolved = resolve_paths(&Cli::default(), Some(Path::new("/work")), &xdg_defaults())
            .expect("xdg defaults present");
        assert_eq!(
            resolved,
            ResolvedPaths {
                config: PathBuf::from("/xdg/config/sustain/settings.toml"),
                database: PathBuf::from("/xdg/data/sustain/library.sqlite"),
                cache_dir: PathBuf::from("/xdg/cache/sustain"),
            }
        );
    }

    #[test]
    fn missing_xdg_default_is_reported() {
        let mut defaults = xdg_defaults();
        defaults.database = None;
        assert_eq!(
            resolve_paths(&Cli::default(), Some(Path::new("/work")), &defaults),
            Err(PathError::DefaultUnavailable("database"))
        );
    }

    #[test]
    fn local_scope_anchors_every_slot_in_the_working_directory() {
        let cli = Cli {
            local_scope: true,
            ..Cli::default()
        };
        let resolved = resolve_paths(&cli, Some(Path::new("/work")), &xdg_defaults())
            .expect("working dir present");
        assert_eq!(
            resolved,
            ResolvedPaths {
                config: PathBuf::from("/work/sustain.toml"),
                database: PathBuf::from("/work/sustain.sqlite"),
                cache_dir: PathBuf::from("/work/sustain.cache"),
            }
        );
    }

    #[test]
    fn explicit_flags_win_over_local_scope_but_cache_stays_in_scope() {
        let cli = Cli {
            config: Some(PathBuf::from("/etc/custom.toml")),
            database: Some(PathBuf::from("/data/custom.sqlite")),
            local_scope: true,
            force_backfill: false,
        };
        let resolved = resolve_paths(&cli, Some(Path::new("/work")), &xdg_defaults())
            .expect("working dir present");
        assert_eq!(
            resolved,
            ResolvedPaths {
                config: PathBuf::from("/etc/custom.toml"),
                database: PathBuf::from("/data/custom.sqlite"),
                cache_dir: PathBuf::from("/work/sustain.cache"),
            }
        );
    }

    #[test]
    fn explicit_pair_without_local_scope_is_self_contained_beside_the_database() {
        let cli = Cli {
            config: Some(PathBuf::from("/etc/custom.toml")),
            database: Some(PathBuf::from("/data/custom.sqlite")),
            local_scope: false,
            force_backfill: false,
        };
        let resolved = resolve_paths(&cli, Some(Path::new("/work")), &xdg_defaults())
            .expect("anchored on the database directory");
        // Never the XDG cache: the cache is anchored on the database's
        // parent directory.
        assert_eq!(resolved.cache_dir, PathBuf::from("/data/sustain.cache"));
        assert_eq!(resolved.config, PathBuf::from("/etc/custom.toml"));
        assert_eq!(resolved.database, PathBuf::from("/data/custom.sqlite"));
    }

    #[test]
    fn lone_database_override_derives_config_and_cache_beside_it() {
        let cli = Cli {
            database: Some(PathBuf::from("/data/custom.sqlite")),
            ..Cli::default()
        };
        let resolved = resolve_paths(&cli, Some(Path::new("/work")), &xdg_defaults())
            .expect("anchored on the database directory");
        assert_eq!(
            resolved,
            ResolvedPaths {
                config: PathBuf::from("/data/sustain.toml"),
                database: PathBuf::from("/data/custom.sqlite"),
                cache_dir: PathBuf::from("/data/sustain.cache"),
            }
        );
    }

    #[test]
    fn bare_relative_override_anchors_on_the_working_directory() {
        let cli = Cli {
            database: Some(PathBuf::from("custom.sqlite")),
            ..Cli::default()
        };
        let resolved = resolve_paths(&cli, Some(Path::new("/work")), &xdg_defaults())
            .expect("empty parent falls back to the working directory");
        assert_eq!(
            resolved,
            ResolvedPaths {
                config: PathBuf::from("/work/sustain.toml"),
                database: PathBuf::from("custom.sqlite"),
                cache_dir: PathBuf::from("/work/sustain.cache"),
            }
        );
    }

    #[test]
    fn override_without_a_working_directory_is_reported() {
        let cli = Cli {
            local_scope: true,
            ..Cli::default()
        };
        assert_eq!(
            resolve_paths(&cli, None, &xdg_defaults()),
            Err(PathError::WorkingDirectoryUnavailable)
        );
    }
}
