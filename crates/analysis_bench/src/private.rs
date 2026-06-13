// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adapt the maintainer's private key/BPM reference set into a benchmark
//! manifest.
//!
//! The private corpus is the maintainer's own audio with hand-curated
//! ground-truth key (and BPM where known), kept entirely outside the repo
//! under the gitignored `validation-data/` workspace. It is a *reality
//! check* against real, commercially-mastered pop/rock/rap — material a
//! public corpus like GiantSteps (electronic, ~85% minor) does not cover —
//! not a public benchmark: the audio, the source URLs, the per-track JSON,
//! and the manifest this adapter emits all stay uncommitted. Only aggregate
//! metrics and provenance reach `docs/`.
//!
//! Its canonical form is a rich `reference.toml`:
//!
//! ```toml
//! [[tracks]]
//! file = "some-track.flac"        # relative to the audio root
//! key = "Cm"                       # authoritative ground-truth key
//! bpm = 76.0                       # ground-truth tempo, where known
//! confidence = "goldish"           # provenance tier (see below)
//! duration_seconds = 289           # window-placement hint
//! # … plus artist/title/album/source URLs/notes the harness does not need
//! ```
//!
//! This adapter reads that file plus the audio root and emits a
//! [`crate::manifest`] TOML carrying the fields the harness scores against
//! (`key`, `bpm`, `duration_secs`), with the run pinned to the BPM and task
//! set a key/tempo evaluation needs. It never downloads and never copies
//! audio into the repo.
//!
//! Three facts shape the design:
//!
//! * **Filename-stem ids.** `reference.toml` carries no stable numeric id,
//!   so the manifest `id` is the audio file's stem (`some-track.flac` →
//!   `some-track`). Collisions are a hard [`AdaptError::DuplicateId`] — two
//!   reference rows resolving to one id would silently overwrite each other
//!   in the keyed results. The manifest is gitignored, so the stem (which
//!   names the track) never reaches git.
//! * **Provenance tiers are reported, not embedded.** Each row's
//!   `confidence` (`goldish` = human musical analysis + ≥2 independent DBs
//!   agree; `silver` = ≥3 secondary DBs, largely Spotify-correlated) is
//!   counted in the [`AdaptReport`] so the gold/silver mix is visible at
//!   adaptation time, but it is *not* written into the manifest: the shared
//!   harness scores against ground truth and has no tier concept. The split
//!   is recovered when reading results by joining ids back to this file.
//! * **Numbers tolerate int or float.** `reference.toml` writes `bpm = 76.0`
//!   (float) but `duration_seconds = 289` (int); both are accepted.
//!
//! A reference row whose audio is absent under the root is a *counted,
//! reported exclusion* (the curated set should be complete, but a missing
//! file is surfaced, never silently dropped); only an audio root that
//! matches nothing at all is a hard error.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sustain_analysis::AnalysisOptions;

use crate::manifest::Task;

/// Stable corpus id, used as the manifest `corpus_id`.
const CORPUS_ID: &str = "private_pop_core";

/// The `goldish` provenance tier: human musical analysis plus independent
/// database agreement.
const TIER_GOLDISH: &str = "goldish";
/// The `silver` provenance tier: secondary-database agreement only.
const TIER_SILVER: &str = "silver";

/// Inputs to [`adapt`].
#[derive(Clone, Debug)]
pub struct AdaptOptions {
    /// Path to the private `reference.toml`.
    pub reference: PathBuf,
    /// Path to the audio root the `file` columns are relative to.
    pub audio_dir: PathBuf,
}

/// Summary of an adaptation, for the CLI to report. The counts partition the
/// reference rows: `total == track_count + missing_audio`, and within the
/// emitted tracks `goldish + silver + untiered == track_count`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdaptReport {
    /// The corpus id written into the manifest.
    pub corpus_id: String,
    /// The serialized manifest TOML.
    pub manifest_toml: String,
    /// Reference rows read from `reference.toml`.
    pub total: usize,
    /// Tracks emitted into the manifest (reference rows whose audio exists).
    pub track_count: usize,
    /// Reference rows excluded because their audio is absent under the root.
    pub missing_audio: usize,
    /// Emitted tracks tagged `goldish`.
    pub goldish: usize,
    /// Emitted tracks tagged `silver`.
    pub silver: usize,
    /// Emitted tracks with an absent or unrecognized `confidence` tier.
    pub untiered: usize,
    /// Emitted tracks carrying key ground truth.
    pub with_key: usize,
    /// Emitted tracks carrying BPM ground truth.
    pub with_bpm: usize,
    /// Emitted key labels the harness scorer cannot parse (so they will not
    /// be scored).
    pub key_unparseable: usize,
}

/// Adaptation failures. Anything that would silently corrupt or misrepresent
/// a baseline — an unparseable reference, two rows colliding on one id, an
/// audio root that matches nothing — is a hard error. A *partial* set
/// (some rows missing audio) is not: those rows are counted and reported.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    /// The reference file does not exist.
    #[error("reference file not found: {0}")]
    ReferenceMissing(PathBuf),
    /// A filesystem read failed.
    #[error("reading {path}: {source}")]
    Read {
        /// Path being read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The reference TOML did not parse.
    #[error("parsing reference {path}: {source}")]
    Parse {
        /// The reference path.
        path: PathBuf,
        /// Underlying parse error.
        #[source]
        source: toml::de::Error,
    },
    /// A reference row had an empty `file`, so no audio could be located.
    #[error("reference row {row} has an empty `file`")]
    EmptyFile {
        /// 1-based index of the offending `[[tracks]]` row.
        row: usize,
    },
    /// A `file` had no stem to derive an id from (e.g. `".flac"`).
    #[error("reference row {row} `file` {file:?} has no filename stem for an id")]
    NoStem {
        /// 1-based index of the offending `[[tracks]]` row.
        row: usize,
        /// The offending `file` value.
        file: String,
    },
    /// Two reference rows resolved to the same manifest id.
    #[error("duplicate id {id:?} from files {first:?} and {second:?}")]
    DuplicateId {
        /// The colliding id.
        id: String,
        /// The first `file` that produced it.
        first: String,
        /// The second `file` that produced it.
        second: String,
    },
    /// The audio root matched no reference row at all — almost always a wrong
    /// `--audio` path, not a real corpus.
    #[error(
        "no audio found under {audio_dir} for any of the {total} reference row(s) \
         (is --audio the folder the `file` columns are relative to?)"
    )]
    NoTracksWithAudio {
        /// The audio root searched.
        audio_dir: PathBuf,
        /// How many reference rows were looked up.
        total: usize,
    },
    /// A path could not be represented as UTF-8 (TOML manifests are UTF-8).
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// An audio file path could not be canonicalized to an absolute path, so
    /// the manifest would otherwise be tied to the adapter's working
    /// directory.
    #[error("canonicalizing audio path {path}: {source}")]
    CanonicalizeAudio {
        /// Path that failed to canonicalize.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// The manifest failed to serialize to TOML.
    #[error("serializing manifest: {0}")]
    Serialize(#[source] toml::ser::Error),
    /// The generated manifest did not round-trip through the loader — an
    /// internal invariant violation, surfaced rather than swallowed.
    #[error("generated manifest failed to round-trip through the loader: {0}")]
    RoundTrip(String),
}

/// The private `reference.toml` document. Only the fields the harness needs
/// are modeled; the rich provenance fields (artist, title, source URLs,
/// notes, …) are ignored by serde.
#[derive(Deserialize)]
struct Reference {
    #[serde(default)]
    tracks: Vec<RefTrack>,
}

/// One `[[tracks]]` row of `reference.toml`.
#[derive(Deserialize)]
struct RefTrack {
    /// Audio file, relative to the audio root.
    file: String,
    /// Authoritative ground-truth key, where present.
    #[serde(default)]
    key: Option<String>,
    /// Ground-truth tempo, where known. Written as a float in the reference
    /// but accepted as either int or float.
    #[serde(default, deserialize_with = "de_opt_number")]
    bpm: Option<f64>,
    /// Track duration, used as the analyzer's window-placement hint. Written
    /// as an int in the reference but accepted as either int or float.
    #[serde(default, deserialize_with = "de_opt_number")]
    duration_seconds: Option<f64>,
    /// Provenance tier (`goldish` / `silver`); reported, not embedded.
    #[serde(default)]
    confidence: Option<String>,
}

/// Deserialize an optional number that the reference may write as either a
/// TOML integer or float into `f64`. Absent → `None`; present → `Some`.
fn de_opt_number<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Number {
        Float(f64),
        Int(i64),
    }
    Ok(Some(match Number::deserialize(deserializer)? {
        Number::Float(value) => value,
        Number::Int(value) => value as f64,
    }))
}

/// The serializable manifest document this adapter emits. A purpose-built
/// mirror of [`crate::manifest`] so we never have to make the *loader* types
/// `Serialize`; the round-trip self-check below guarantees the two agree.
#[derive(Serialize)]
struct ManifestDoc {
    meta: MetaDoc,
    options: OptionsDoc,
    #[serde(rename = "track")]
    tracks: Vec<TrackDoc>,
}

#[derive(Serialize)]
struct MetaDoc {
    corpus_id: String,
    description: String,
}

#[derive(Serialize)]
struct OptionsDoc {
    min_bpm: f32,
    max_bpm: f32,
    tasks: Vec<Task>,
}

#[derive(Serialize)]
struct TrackDoc {
    id: String,
    path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    duration_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    bpm: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    key: Option<String>,
}

/// Derive the manifest id for a reference `file`: its filename stem.
fn id_from_file(file: &str) -> Option<String> {
    Path::new(file)
        .file_stem()
        .and_then(|stem| stem.to_str())
        .filter(|stem| !stem.is_empty())
        .map(str::to_string)
}

/// Build a benchmark manifest from a private `reference.toml` + an audio root.
pub fn adapt(options: &AdaptOptions) -> Result<AdaptReport, AdaptError> {
    if !options.reference.is_file() {
        return Err(AdaptError::ReferenceMissing(options.reference.clone()));
    }
    let raw = std::fs::read_to_string(&options.reference).map_err(|source| AdaptError::Read {
        path: options.reference.clone(),
        source,
    })?;
    let reference: Reference = toml::from_str(&raw).map_err(|source| AdaptError::Parse {
        path: options.reference.clone(),
        source,
    })?;

    let total = reference.tracks.len();
    let mut tracks: Vec<TrackDoc> = Vec::new();
    let mut seen_ids: HashMap<String, String> = HashMap::new();
    let mut missing_audio = 0_usize;
    let mut goldish = 0_usize;
    let mut silver = 0_usize;
    let mut untiered = 0_usize;
    let mut with_key = 0_usize;
    let mut with_bpm = 0_usize;
    let mut key_unparseable = 0_usize;

    for (index, row) in reference.tracks.iter().enumerate() {
        let row_number = index + 1;
        if row.file.trim().is_empty() {
            return Err(AdaptError::EmptyFile { row: row_number });
        }
        let id = id_from_file(&row.file).ok_or_else(|| AdaptError::NoStem {
            row: row_number,
            file: row.file.clone(),
        })?;
        if let Some(first) = seen_ids.get(&id) {
            return Err(AdaptError::DuplicateId {
                id,
                first: first.clone(),
                second: row.file.clone(),
            });
        }

        let audio_path = options.audio_dir.join(&row.file);
        if !audio_path.is_file() {
            // Absent under this root — counted and reported, never silent.
            missing_audio += 1;
            continue;
        }
        seen_ids.insert(id.clone(), row.file.clone());

        match row.confidence.as_deref() {
            Some(TIER_GOLDISH) => goldish += 1,
            Some(TIER_SILVER) => silver += 1,
            _ => untiered += 1,
        }
        if let Some(key) = &row.key {
            with_key += 1;
            if crate::metrics::parse_key(key).is_none() {
                key_unparseable += 1;
            }
        }
        if row.bpm.is_some() {
            with_bpm += 1;
        }

        // Canonicalize so the manifest carries an absolute path independent of
        // any later `analysis-bench run` working directory. The file exists
        // (checked above), so failure here is genuinely exceptional.
        let canonical =
            audio_path
                .canonicalize()
                .map_err(|source| AdaptError::CanonicalizeAudio {
                    path: audio_path.clone(),
                    source,
                })?;
        let path = canonical
            .to_str()
            .ok_or_else(|| AdaptError::NonUtf8Path(canonical.clone()))?
            .to_string();
        tracks.push(TrackDoc {
            id,
            path,
            duration_secs: row.duration_seconds,
            bpm: row.bpm,
            key: row.key.clone(),
        });
    }

    if tracks.is_empty() {
        return Err(AdaptError::NoTracksWithAudio {
            audio_dir: options.audio_dir.clone(),
            total,
        });
    }

    // Deterministic manifest order regardless of reference row order.
    tracks.sort_by(|a, b| a.id.cmp(&b.id));

    let track_count = tracks.len();
    // Pin the BPM range to the analyzer's shipped default so the private
    // baseline matches what users get; sourced from the analyzer rather than
    // a literal so it can never drift from the shipped range.
    let defaults = AnalysisOptions::default();
    let doc = ManifestDoc {
        meta: MetaDoc {
            corpus_id: CORPUS_ID.to_string(),
            description: format!(
                "Private key/BPM reality-check corpus (maintainer's own audio, \
                 hand-curated ground truth), generated by `analysis-bench \
                 adapt-private` from {}. Gitignored — do not edit or commit; \
                 regenerate from reference.toml + the audio root. Audio, source \
                 URLs, and per-track results are external and uncommitted; only \
                 aggregate metrics reach docs/analysis-benchmark-results.md.",
                options.reference.display()
            ),
        },
        options: OptionsDoc {
            min_bpm: defaults.min_bpm,
            max_bpm: defaults.max_bpm,
            tasks: vec![Task::Bpm, Task::Key],
        },
        tracks,
    };
    let manifest_toml = toml::to_string(&doc).map_err(AdaptError::Serialize)?;

    // Invariant: whatever we emit, the harness loader must accept. Parsing it
    // back as the real manifest type guards against this adapter's output
    // schema drifting from `crate::manifest`.
    toml::from_str::<crate::manifest::Manifest>(&manifest_toml)
        .map_err(|err| AdaptError::RoundTrip(err.to_string()))?;

    Ok(AdaptReport {
        corpus_id: CORPUS_ID.to_string(),
        manifest_toml,
        total,
        track_count,
        missing_audio,
        goldish,
        silver,
        untiered,
        with_key,
        with_bpm,
        key_unparseable,
    })
}

#[cfg(test)]
mod tests {
    use super::{AdaptError, AdaptOptions, adapt, id_from_file};
    use crate::manifest::{Manifest, Task};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// Parse an emitted manifest back into the real loader type, so tests
    /// assert on typed fields rather than brittle substrings.
    fn parse(manifest_toml: &str) -> Manifest {
        toml::from_str(manifest_toml).expect("emitted manifest parses")
    }

    fn write(path: &Path, contents: &str) {
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("create fixture dirs");
        fs::write(path, contents).expect("write fixture file");
    }

    struct Fixture {
        root: TempDir,
    }

    impl Fixture {
        fn new() -> Self {
            Self {
                root: TempDir::new().expect("create temp dir"),
            }
        }
        fn reference(&self) -> PathBuf {
            self.root.path().join("reference.toml")
        }
        fn audio(&self) -> PathBuf {
            self.root.path().to_path_buf()
        }
        fn write_reference(&self, contents: &str) {
            write(&self.reference(), contents);
        }
        /// Write a dummy audio file under the audio root.
        fn audio_file(&self, name: &str) {
            write(&self.audio().join(name), "x");
        }
        fn options(&self) -> AdaptOptions {
            AdaptOptions {
                reference: self.reference(),
                audio_dir: self.audio(),
            }
        }
    }

    #[test]
    fn id_is_the_filename_stem() {
        assert_eq!(
            id_from_file("adele-skyfall.mp3").as_deref(),
            Some("adele-skyfall")
        );
        assert_eq!(id_from_file("a.b.flac").as_deref(), Some("a.b"));
        // A name that is entirely a leading-dot "extension" has no further
        // stem to strip, so `file_stem` keeps the whole name.
        assert_eq!(id_from_file(".flac").as_deref(), Some(".flac"));
        assert_eq!(id_from_file(""), None);
    }

    #[test]
    fn adapts_key_and_bpm_and_sorts_by_id() {
        let fx = Fixture::new();
        // Deliberately out of id order; the manifest must come out sorted.
        // `bpm` is a float, `duration_seconds` an int — both must parse.
        fx.write_reference(
            "[[tracks]]\n\
             file = \"zztop-la-grange.flac\"\n\
             key = \"A\"\n\
             bpm = 82.0\n\
             confidence = \"silver\"\n\
             duration_seconds = 230\n\
             \n\
             [[tracks]]\n\
             file = \"adele-skyfall.mp3\"\n\
             key = \"Cm\"\n\
             bpm = 76.0\n\
             confidence = \"goldish\"\n\
             duration_seconds = 289\n",
        );
        fx.audio_file("zztop-la-grange.flac");
        fx.audio_file("adele-skyfall.mp3");

        let report = adapt(&fx.options()).expect("adapt private");
        assert_eq!(report.total, 2);
        assert_eq!(report.track_count, 2);
        assert_eq!(report.missing_audio, 0);
        assert_eq!(report.goldish, 1);
        assert_eq!(report.silver, 1);
        assert_eq!(report.untiered, 0);
        assert_eq!(report.with_key, 2);
        assert_eq!(report.with_bpm, 2);
        assert_eq!(report.key_unparseable, 0);
        assert_eq!(report.corpus_id, "private_pop_core");

        let manifest = parse(&report.manifest_toml);
        // Sorted by id: adele- before zztop-, with full ground truth carried.
        let ids: Vec<&str> = manifest.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["adele-skyfall", "zztop-la-grange"]);
        let adele = &manifest.tracks[0];
        assert_eq!(adele.bpm, Some(76.0));
        assert_eq!(adele.duration_secs, Some(289.0));
        assert_eq!(adele.key.as_deref(), Some("Cm"));
        assert_eq!(
            manifest.options.tasks,
            Some(vec![Task::Bpm, Task::Key]),
            "a key/BPM evaluation runs exactly those two tasks"
        );
        // Confidence is reported, never embedded in the manifest.
        assert!(!report.manifest_toml.contains("confidence"));
        assert!(!report.manifest_toml.contains("goldish"));
    }

    #[test]
    fn options_pin_the_shipped_bpm_range() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"a.flac\"\n\
             key = \"Am\"\n",
        );
        fx.audio_file("a.flac");
        let report = adapt(&fx.options()).expect("adapt");
        let defaults = sustain_analysis::AnalysisOptions::default();
        let manifest = parse(&report.manifest_toml);
        assert_eq!(manifest.options.min_bpm, Some(defaults.min_bpm));
        assert_eq!(manifest.options.max_bpm, Some(defaults.max_bpm));
    }

    #[test]
    fn missing_audio_is_counted_not_fatal() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"present.flac\"\n\
             key = \"Am\"\n\
             confidence = \"goldish\"\n\
             \n\
             [[tracks]]\n\
             file = \"absent.flac\"\n\
             key = \"Em\"\n\
             confidence = \"silver\"\n",
        );
        // Only one of the two has audio.
        fx.audio_file("present.flac");

        let report = adapt(&fx.options()).expect("adapt with partial coverage");
        assert_eq!(report.total, 2);
        assert_eq!(report.track_count, 1);
        assert_eq!(report.missing_audio, 1);
        // Invariant: total == track_count + missing_audio.
        assert_eq!(report.total, report.track_count + report.missing_audio);
        assert_eq!(report.goldish, 1);
        assert_eq!(report.silver, 0);
        let manifest = parse(&report.manifest_toml);
        let ids: Vec<&str> = manifest.tracks.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(ids, ["present"]);
    }

    #[test]
    fn key_only_track_emits_without_bpm() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"a.flac\"\n\
             key = \"F#m\"\n\
             confidence = \"goldish\"\n",
        );
        fx.audio_file("a.flac");
        let report = adapt(&fx.options()).expect("adapt key-only");
        assert_eq!(report.with_key, 1);
        assert_eq!(report.with_bpm, 0);
        let manifest = parse(&report.manifest_toml);
        assert_eq!(manifest.tracks[0].key.as_deref(), Some("F#m"));
        assert_eq!(manifest.tracks[0].bpm, None);
    }

    #[test]
    fn unparseable_key_is_counted_but_emitted() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"a.flac\"\n\
             key = \"Nonsense\"\n\
             confidence = \"silver\"\n",
        );
        fx.audio_file("a.flac");
        let report = adapt(&fx.options()).expect("adapt unparseable key");
        assert_eq!(report.with_key, 1);
        assert_eq!(report.key_unparseable, 1);
        let manifest = parse(&report.manifest_toml);
        assert_eq!(manifest.tracks[0].key.as_deref(), Some("Nonsense"));
    }

    #[test]
    fn duplicate_id_is_a_hard_error() {
        let fx = Fixture::new();
        // Two different files (different extensions) collide on stem "track".
        fx.write_reference(
            "[[tracks]]\n\
             file = \"track.flac\"\n\
             key = \"Am\"\n\
             \n\
             [[tracks]]\n\
             file = \"track.mp3\"\n\
             key = \"Em\"\n",
        );
        fx.audio_file("track.flac");
        fx.audio_file("track.mp3");
        let err = adapt(&fx.options()).expect_err("duplicate id errors");
        assert!(matches!(err, AdaptError::DuplicateId { .. }));
    }

    #[test]
    fn empty_file_is_a_hard_error() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"\"\n\
             key = \"Am\"\n",
        );
        let err = adapt(&fx.options()).expect_err("empty file errors");
        assert!(matches!(err, AdaptError::EmptyFile { row: 1 }));
    }

    #[test]
    fn missing_reference_is_an_error() {
        let fx = Fixture::new();
        // No reference written.
        let err = adapt(&fx.options()).expect_err("missing reference errors");
        assert!(matches!(err, AdaptError::ReferenceMissing(_)));
    }

    #[test]
    fn no_audio_for_any_track_is_an_error() {
        let fx = Fixture::new();
        fx.write_reference(
            "[[tracks]]\n\
             file = \"a.flac\"\n\
             key = \"Am\"\n",
        );
        // Reference parses but the audio root holds nothing.
        let err = adapt(&fx.options()).expect_err("no matching audio errors");
        assert!(matches!(
            err,
            AdaptError::NoTracksWithAudio { total: 1, .. }
        ));
    }

    #[test]
    fn malformed_reference_is_an_error() {
        let fx = Fixture::new();
        fx.write_reference("this is not toml = = =\n");
        let err = adapt(&fx.options()).expect_err("bad toml errors");
        assert!(matches!(err, AdaptError::Parse { .. }));
    }
}
