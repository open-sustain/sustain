// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! `analysis-bench` — the benchmark/validation CLI for Sustain's audio
//! analysis pipeline.
//!
//! ```text
//! analysis-bench run     --manifest <toml> [--out <json>] [--fixtures <dir>] [--reproducible]
//! analysis-bench gen     --manifest <toml> --out <dir>
//! analysis-bench compare --baseline <json> --candidate <json>
//! ```
//!
//! * `run` analyzes every track in a manifest and writes a JSON report
//!   (outputs, timings, and — where the manifest carries ground truth —
//!   scores). `--reproducible` drops the machine-specific environment
//!   capture and all timings so the file is byte-stable; that is the form
//!   committed as a synthetic baseline.
//! * `gen` only generates the manifest's synthetic fixtures to a directory
//!   (for inspection or manual decoding).
//! * `compare` diffs two reports by track id and prints accuracy/timing
//!   deltas — the before/after view for a DSP or decoder change.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use sustain_analysis_bench::manifest::Manifest;
use sustain_analysis_bench::run::{Report, TrackResult, run_manifest};

const USAGE: &str = "\
usage:
  analysis-bench run     --manifest <toml> [--out <json>] [--fixtures <dir>] [--reproducible]
  analysis-bench gen     --manifest <toml> --out <dir>
  analysis-bench compare --baseline <json> --candidate <json>";

/// A CLI failure: either bad invocation (exit 2) or a runtime error (exit 1).
enum CliError {
    Usage(String),
    Failure(String),
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match dispatch(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(CliError::Usage(message)) => {
            eprintln!("{message}\n\n{USAGE}");
            ExitCode::from(2)
        }
        Err(CliError::Failure(message)) => {
            eprintln!("error: {message}");
            ExitCode::FAILURE
        }
    }
}

fn dispatch(args: &[String]) -> Result<(), CliError> {
    let (command, rest) = args
        .split_first()
        .ok_or_else(|| CliError::Usage("no command given".to_string()))?;
    match command.as_str() {
        "run" => cmd_run(rest),
        "gen" => cmd_gen(rest),
        "compare" => cmd_compare(rest),
        other => Err(CliError::Usage(format!("unknown command {other:?}"))),
    }
}

/// Parse `--key value` pairs and bare `--flag` switches.
struct Args {
    values: BTreeMap<String, String>,
    flags: Vec<String>,
}

impl Args {
    fn parse(args: &[String]) -> Result<Self, CliError> {
        let mut values = BTreeMap::new();
        let mut flags = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            let Some(key) = arg.strip_prefix("--") else {
                return Err(CliError::Usage(format!("unexpected argument {arg:?}")));
            };
            // `--reproducible` is the only valueless switch.
            if key == "reproducible" {
                flags.push(key.to_string());
                continue;
            }
            let value = iter
                .next()
                .ok_or_else(|| CliError::Usage(format!("--{key} needs a value")))?;
            values.insert(key.to_string(), value.clone());
        }
        Ok(Self { values, flags })
    }

    fn required(&self, key: &str) -> Result<&str, CliError> {
        self.values
            .get(key)
            .map(String::as_str)
            .ok_or_else(|| CliError::Usage(format!("--{key} is required")))
    }

    fn optional(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    fn has_flag(&self, key: &str) -> bool {
        self.flags.iter().any(|f| f == key)
    }
}

fn cmd_run(args: &[String]) -> Result<(), CliError> {
    let args = Args::parse(args)?;
    let manifest_path = PathBuf::from(args.required("manifest")?);
    let manifest =
        Manifest::load(&manifest_path).map_err(|err| CliError::Failure(format!("{err}")))?;
    let reproducible = args.has_flag("reproducible");

    // Fixtures land in an explicit directory or a fresh temp dir we keep
    // alive for the duration of the run.
    let (fixtures_dir, _guard) = match args.optional("fixtures") {
        Some(dir) => {
            let path = PathBuf::from(dir);
            std::fs::create_dir_all(&path)
                .map_err(|err| CliError::Failure(format!("create fixtures dir: {err}")))?;
            (path, None)
        }
        None => {
            let temp = tempdir().map_err(CliError::Failure)?;
            (temp.clone(), Some(TempDirGuard(temp)))
        }
    };

    let report = run_manifest(&manifest, &fixtures_dir, reproducible);
    let json = serde_json::to_string_pretty(&report)
        .map_err(|err| CliError::Failure(format!("serialize report: {err}")))?;
    match args.optional("out") {
        Some(out) => {
            if let Some(parent) = Path::new(out)
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
            {
                std::fs::create_dir_all(parent).map_err(|err| {
                    CliError::Failure(format!("create {}: {err}", parent.display()))
                })?;
            }
            std::fs::write(out, format!("{json}\n"))
                .map_err(|err| CliError::Failure(format!("write report: {err}")))?;
            eprintln!("wrote {out}");
        }
        None => println!("{json}"),
    }
    print_run_summary(&report);
    Ok(())
}

fn cmd_gen(args: &[String]) -> Result<(), CliError> {
    let args = Args::parse(args)?;
    let manifest = Manifest::load(&PathBuf::from(args.required("manifest")?))
        .map_err(|err| CliError::Failure(format!("{err}")))?;
    let out = PathBuf::from(args.required("out")?);
    std::fs::create_dir_all(&out)
        .map_err(|err| CliError::Failure(format!("create out dir: {err}")))?;
    let mut written = 0;
    for track in &manifest.tracks {
        if let Some(synthetic) = &track.synthetic {
            let path = out.join(format!("{}.wav", sanitize(&track.id)));
            synthetic
                .write_wav(&path)
                .map_err(|err| CliError::Failure(format!("write {}: {err}", path.display())))?;
            written += 1;
        }
    }
    eprintln!(
        "generated {written} synthetic fixture(s) in {}",
        out.display()
    );
    Ok(())
}

fn cmd_compare(args: &[String]) -> Result<(), CliError> {
    let args = Args::parse(args)?;
    let baseline = load_report(args.required("baseline")?)?;
    let candidate = load_report(args.required("candidate")?)?;
    print_comparison(&baseline, &candidate);
    Ok(())
}

fn load_report(path: &str) -> Result<Report, CliError> {
    let text = std::fs::read_to_string(path)
        .map_err(|err| CliError::Failure(format!("read {path}: {err}")))?;
    serde_json::from_str(&text).map_err(|err| CliError::Failure(format!("parse {path}: {err}")))
}

fn print_run_summary(report: &Report) {
    eprintln!(
        "corpus {:?}: {} track(s), {} error(s), BPM range {}-{}",
        report.corpus_id,
        report.summary.track_count,
        report.summary.error_count,
        report.bpm_range[0],
        report.bpm_range[1],
    );
    if let Some(bpm) = &report.summary.bpm {
        eprintln!(
            "  BPM: {:.1}% within ±2 over {} scored, MAE {:.2}",
            bpm.within_tolerance_pct, bpm.scored, bpm.mean_abs_error
        );
    }
    if let Some(key) = &report.summary.key {
        eprintln!(
            "  KEY: {:.1}% MIREX-weighted over {} scored",
            key.weighted_pct, key.scored
        );
    }
    if let Some(timing) = &report.summary.timing {
        eprintln!("  TIME: {:.1} ms total", timing.total_ms);
    }
}

fn print_comparison(baseline: &Report, candidate: &Report) {
    println!("== accuracy ==");
    let base_bpm = baseline
        .summary
        .bpm
        .as_ref()
        .map(|b| b.within_tolerance_pct);
    let cand_bpm = candidate
        .summary
        .bpm
        .as_ref()
        .map(|b| b.within_tolerance_pct);
    if let (Some(b), Some(c)) = (base_bpm, cand_bpm) {
        println!("BPM within ±2:  {b:.1}% -> {c:.1}%  ({:+.1})", c - b);
    }
    let base_key = baseline.summary.key.as_ref().map(|k| k.weighted_pct);
    let cand_key = candidate.summary.key.as_ref().map(|k| k.weighted_pct);
    if let (Some(b), Some(c)) = (base_key, cand_key) {
        println!("KEY MIREX:      {b:.1}% -> {c:.1}%  ({:+.1})", c - b);
    }

    println!("\n== timing (total ms) ==");
    let base_t = baseline.summary.timing.as_ref().map(|t| t.total_ms);
    let cand_t = candidate.summary.timing.as_ref().map(|t| t.total_ms);
    match (base_t, cand_t) {
        (Some(b), Some(c)) => {
            let delta_pct = if b > 0.0 { (c - b) / b * 100.0 } else { 0.0 };
            println!("total:          {b:.1} -> {c:.1}  ({delta_pct:+.1}%)");
        }
        _ => println!("(one or both reports omit timing — run without --reproducible)"),
    }

    // Per-track output divergences: BPM/key/waveform-hash changes.
    let base_by_id: BTreeMap<&str, &TrackResult> =
        baseline.tracks.iter().map(|t| (t.id.as_str(), t)).collect();
    let mut diverged = Vec::new();
    for cand in &candidate.tracks {
        if let Some(base) = base_by_id.get(cand.id.as_str()) {
            if base.bpm != cand.bpm
                || base.key != cand.key
                || waveform_hash(base) != waveform_hash(cand)
            {
                diverged.push(cand.id.as_str());
            }
        }
    }
    if diverged.is_empty() {
        println!(
            "\noutputs identical across {} matched track(s)",
            candidate.tracks.len()
        );
    } else {
        println!("\n{} track(s) with changed output:", diverged.len());
        for id in diverged {
            println!("  {id}");
        }
    }
}

fn waveform_hash(track: &TrackResult) -> Option<(&str, &str)> {
    track
        .waveform
        .as_ref()
        .map(|w| (w.preview_hash.as_str(), w.detail_hash.as_str()))
}

fn sanitize(id: &str) -> String {
    id.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Create a uniquely-named scratch directory under the system temp dir,
/// without pulling in a temp-dir crate for a build tool.
fn tempdir() -> Result<PathBuf, String> {
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let dir = std::env::temp_dir().join(format!("sustain-analysis-bench-{pid}-{nanos}"));
    std::fs::create_dir_all(&dir).map_err(|err| format!("create temp dir: {err}"))?;
    Ok(dir)
}

/// Removes its directory on drop, so a `run` without `--fixtures` leaves
/// no scratch behind.
struct TempDirGuard(PathBuf);

impl Drop for TempDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
