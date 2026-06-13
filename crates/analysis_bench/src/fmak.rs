// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adapt the FMAK / FMAKv2 key annotations into a benchmark manifest.
//!
//! FMAK (Wong & Hernandez, ISMIR'23 LBD) is an expert song-level key/mode
//! dataset for the Free Music Archive: 5,489 tracks across 17 genres, 24
//! major/minor keys. FMAKv2 (Zenodo `12759100`, the evaluation set for
//! Deezer's STONE, ISMIR'24) is the refined release. Both ship a single CSV
//! of annotations keyed by FMA `track_id` — and no audio. The audio is the
//! Free Music Archive itself, downloaded separately as one of the `fma_*`
//! archives (<https://github.com/mdeff/fma>) and laid out as
//! `<root>/<id6[..3]>/<id6>.mp3` (`id6` = the zero-padded six-digit track id;
//! e.g. track `10` → `000/000010.mp3`).
//!
//! This adapter reads a local FMAK CSV plus a local FMA audio root and emits a
//! [`crate::manifest`] TOML with ground-truth `key`. It never downloads and
//! never vendors audio or annotations into Sustain.
//!
//! Two facts shape the design and set it apart from the GiantSteps adapter:
//!
//! * **Header-driven CSV.** FMAKv2's CSV is `index,key_and_mode,track_id,
//!   spotify_uri`; the original `fma_keys` `keys.csv` is a two-header-row
//!   `track_id,spotify_uri,key_and_mode`. The column *order* differs, so the
//!   parser locates `track_id` and `key_and_mode` by name (the first row that
//!   names both is the header) rather than by position — one adapter reads
//!   either file.
//! * **Plain, unquoted CSV — assumed and enforced, not pulled in via a CSV
//!   crate.** The parser splits on `,`; this is sound because every FMAK field
//!   is structurally comma/quote/newline-free: an integer index, the closed
//!   24-value key vocabulary (`C Major` … `G# minor`), an integer `track_id`,
//!   and a base62 `spotify:track:…` URI. Verified across the full pinned files
//!   (fmakv2.csv md5 `3b2d16784ffbda850c8ddf0519478bfd` and the original
//!   keys.csv): zero `"` characters and a uniform field count throughout. The
//!   assumption is also *enforced*, so heavy reliance is safe: a row whose
//!   field count differs from the header, or whose `track_id` column is not an
//!   integer, is a hard [`AdaptError::MalformedRow`] — any quoted or
//!   embedded-comma field shifts the columns and trips exactly that check,
//!   so a non-simple file fails loudly rather than mis-parsing silently. If a
//!   future variant ever needs RFC-4180 quoting, swap this for a real CSV
//!   reader; today it would only ever error, never lie.
//! * **No per-track checksum.** FMA's integrity model is a per-archive SHA1
//!   verified at download (`sha1sum -c fma_large.zip`), not per-track digests
//!   like GiantSteps' `md5/`. There is nothing upstream to re-verify a single
//!   extracted file against, so this adapter does not fabricate a per-track
//!   md5 step; archive integrity is the maintainer's download-time check,
//!   recorded in `docs/analysis-benchmark-corpora.md`.
//!
//! Coverage depends on which FMA archive the audio root holds. `fma_large`
//! contains all 106,574 tracks (30 s clips) and so covers every FMAK id;
//! `fma_medium`/`fma_small` are genre-biased subsets that cover only part of
//! FMAK. An annotated track with no audio under the given root is therefore a
//! *counted, reported exclusion*, not a hard error — the summary states how
//! many were excluded so partial coverage is never silent. (The clips are
//! 30 s where FMAK keys are song-level; key is global, so a 30 s excerpt is
//! standard practice, but it is a documented caveat.)
//!
//! See `docs/analysis-benchmark-corpora.md` for the sources, licensing, and
//! the full acquisition → adaptation → run sequence.

use std::path::PathBuf;

use serde::Serialize;

use crate::manifest::Task;

/// Stable corpus id, used as the manifest `corpus_id`.
const CORPUS_ID: &str = "fmak_key";

/// Inputs to [`adapt`].
#[derive(Clone, Debug)]
pub struct AdaptOptions {
    /// Path to a local FMAK CSV (`fmakv2.csv` or the original `keys.csv`).
    pub annotations: PathBuf,
    /// Path to the root of an extracted FMA audio archive (`fma_large`,
    /// `fma_medium`, …), under which audio is laid out `<id6[..3]>/<id6>.mp3`.
    pub audio_dir: PathBuf,
}

/// Summary of an adaptation, for the CLI to report. The counts partition the
/// data rows: `annotated == track_count + skipped + missing_audio`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdaptReport {
    /// The corpus id written into the manifest.
    pub corpus_id: String,
    /// The serialized manifest TOML.
    pub manifest_toml: String,
    /// Annotation rows read from the CSV (excludes the header and blanks).
    pub annotated: usize,
    /// Tracks emitted into the manifest (annotated, with audio present).
    pub track_count: usize,
    /// Rows with an empty key label (no ground truth).
    pub skipped: usize,
    /// Annotated tracks excluded because no audio for their id exists under
    /// the audio root — i.e. not present in this FMA archive subset.
    pub missing_audio: usize,
    /// Emitted key labels the harness scorer cannot parse (so they will not
    /// be scored).
    pub key_unparseable: usize,
}

/// Adaptation failures. Anything that would silently corrupt or misrepresent a
/// baseline — a malformed row, a header that does not name the columns, an
/// audio root that matches no annotated track — is a hard error. A *partial*
/// audio subset is not a failure: missing-audio rows are counted and reported.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    /// The annotations CSV does not exist.
    #[error("annotations file not found: {0}")]
    AnnotationsMissing(PathBuf),
    /// A filesystem read failed.
    #[error("reading {path}: {source}")]
    Read {
        /// Path being read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// No CSV row named both `track_id` and `key_and_mode`, so the file is not
    /// a recognizable FMAK annotations table.
    #[error(
        "no header row naming both `track_id` and `key_and_mode` in {0} \
         (is this an FMAK CSV — fmakv2.csv or keys.csv?)"
    )]
    HeaderNotFound(PathBuf),
    /// A data row had the wrong field count, or a `track_id` that is not a
    /// non-negative integer. Surfaced rather than silently skipped, since it
    /// signals the CSV schema drifted from what the header promised.
    #[error("{count} malformed CSV row(s) in {file}; first at line {first_line}: {first:?}")]
    MalformedRow {
        /// How many rows were malformed.
        count: usize,
        /// The CSV being read.
        file: PathBuf,
        /// 1-based line number of the first offender.
        first_line: usize,
        /// The first offending line, verbatim.
        first: String,
    },
    /// The audio root matched no annotated track at all — almost always a
    /// wrong `--audio` path or an unextracted archive, not a real corpus.
    #[error(
        "no audio found under {audio_dir} for any of the {annotated} annotated track(s) \
         (is --audio the root of an extracted fma_* archive?)"
    )]
    NoTracksWithAudio {
        /// The audio root searched.
        audio_dir: PathBuf,
        /// How many annotated tracks were looked up.
        annotated: usize,
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

/// The serializable manifest document this adapter emits. A purpose-built
/// mirror of [`crate::manifest`] (key-only) so we never have to make the
/// *loader* types `Serialize`; the round-trip self-check below guarantees the
/// two agree.
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
    tasks: Vec<Task>,
}

#[derive(Serialize)]
struct TrackDoc {
    id: String,
    path: String,
    key: String,
}

/// Where `track_id` and `key_and_mode` sit in a recognized FMAK header, plus
/// the field count every data row must match.
struct Header {
    track_id: usize,
    key_and_mode: usize,
    field_count: usize,
}

/// Locate the FMAK header: the first line whose comma-separated fields name
/// both `track_id` and `key_and_mode`. Returns the column indices and the line
/// number (1-based) so the caller knows where the data begins. This tolerates
/// the leading pandas index column and the original file's doubled header.
fn find_header(lines: &[&str]) -> Option<(usize, Header)> {
    for (idx, line) in lines.iter().enumerate() {
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let track_id = fields.iter().position(|f| *f == "track_id");
        let key_and_mode = fields.iter().position(|f| *f == "key_and_mode");
        if let (Some(track_id), Some(key_and_mode)) = (track_id, key_and_mode) {
            return Some((
                idx,
                Header {
                    track_id,
                    key_and_mode,
                    field_count: fields.len(),
                },
            ));
        }
    }
    None
}

/// The on-disk relative path of an FMA track: `<id6[..3]>/<id6>.mp3`, where
/// `id6` is the zero-padded six-digit id (FMA's `utils.get_audio_path`).
fn fma_relative_path(track_id: u32) -> PathBuf {
    let id6 = format!("{track_id:06}");
    PathBuf::from(&id6[..3]).join(format!("{id6}.mp3"))
}

/// Build a benchmark manifest from a local FMAK CSV + an FMA audio root.
pub fn adapt(options: &AdaptOptions) -> Result<AdaptReport, AdaptError> {
    if !options.annotations.is_file() {
        return Err(AdaptError::AnnotationsMissing(options.annotations.clone()));
    }
    let raw = std::fs::read_to_string(&options.annotations).map_err(|source| AdaptError::Read {
        path: options.annotations.clone(),
        source,
    })?;
    let lines: Vec<&str> = raw.lines().collect();
    let (header_idx, header) = find_header(&lines)
        .ok_or_else(|| AdaptError::HeaderNotFound(options.annotations.clone()))?;

    // Parse the data rows into (track_id, key label), validating structure as
    // we go. Blank lines are ignored; a wrong field count or non-integer
    // track_id is a hard error (schema drift), collected so the first offender
    // and the total are both reported.
    let mut rows: Vec<(u32, String)> = Vec::new();
    let mut malformed: Vec<(usize, String)> = Vec::new();
    for (offset, line) in lines.iter().enumerate().skip(header_idx + 1) {
        if line.trim().is_empty() {
            continue;
        }
        // Plain split on `,` — sound only for unquoted CSV, which FMAK is (see
        // the module docs). The two checks below enforce that: a quoted or
        // embedded-comma field changes the field count or pushes a non-integer
        // into the `track_id` column, so it errors rather than mis-parsing.
        let fields: Vec<&str> = line.split(',').collect();
        if fields.len() != header.field_count {
            malformed.push((offset + 1, (*line).to_string()));
            continue;
        }
        let Ok(track_id) = fields[header.track_id].trim().parse::<u32>() else {
            malformed.push((offset + 1, (*line).to_string()));
            continue;
        };
        let label = fields[header.key_and_mode].trim().to_string();
        rows.push((track_id, label));
    }
    if let Some((first_line, first)) = malformed.first() {
        return Err(AdaptError::MalformedRow {
            count: malformed.len(),
            file: options.annotations.clone(),
            first_line: *first_line,
            first: first.clone(),
        });
    }

    let annotated = rows.len();
    // Deterministic manifest order regardless of CSV row order.
    rows.sort_by_key(|(track_id, _)| *track_id);

    let mut tracks: Vec<TrackDoc> = Vec::new();
    let mut skipped = 0_usize;
    let mut missing_audio = 0_usize;
    let mut key_unparseable = 0_usize;

    for (track_id, label) in &rows {
        if label.is_empty() {
            skipped += 1;
            continue;
        }
        let audio_path = options.audio_dir.join(fma_relative_path(*track_id));
        if !audio_path.is_file() {
            // Not present in this FMA archive subset — counted, not fatal.
            missing_audio += 1;
            continue;
        }
        if crate::metrics::parse_key(label).is_none() {
            key_unparseable += 1;
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
            id: track_id.to_string(),
            path,
            key: label.clone(),
        });
    }

    if tracks.is_empty() {
        return Err(AdaptError::NoTracksWithAudio {
            audio_dir: options.audio_dir.clone(),
            annotated,
        });
    }

    let track_count = tracks.len();
    let doc = ManifestDoc {
        meta: MetaDoc {
            corpus_id: CORPUS_ID.to_string(),
            description: format!(
                "FMAK / FMAKv2 (expert key ground truth), generated by \
                 `analysis-bench adapt-fmak` from {}. Gitignored — do not edit; \
                 regenerate from the FMAK CSV + an FMA audio root. Audio and \
                 annotations are external and uncommitted; see \
                 docs/analysis-benchmark-corpora.md.",
                options.annotations.display()
            ),
        },
        options: OptionsDoc {
            tasks: vec![Task::Key],
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
        annotated,
        track_count,
        skipped,
        missing_audio,
        key_unparseable,
    })
}

#[cfg(test)]
mod tests {
    use super::{AdaptError, AdaptOptions, adapt, find_header, fma_relative_path};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tempfile::TempDir;

    /// The FMAKv2 header layout: a leading index column, then key/id/uri.
    const FMAKV2_HEADER: &str = ",key_and_mode,track_id,spotify_uri";
    /// The original `keys.csv` layout: id/uri/key, no index column.
    const KEYS_CSV_HEADER: &str = "track_id,spotify_uri,key_and_mode";

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
        fn csv(&self) -> PathBuf {
            self.root.path().join("fmakv2.csv")
        }
        fn audio(&self) -> PathBuf {
            self.root.path().join("fma_large")
        }
        fn write_csv(&self, contents: &str) {
            write(&self.csv(), contents);
        }
        /// Write a dummy audio file at the FMA path for `track_id`.
        fn audio_file(&self, track_id: u32) {
            write(&self.audio().join(super::fma_relative_path(track_id)), "x");
        }
        fn options(&self) -> AdaptOptions {
            AdaptOptions {
                annotations: self.csv(),
                audio_dir: self.audio(),
            }
        }
    }

    #[test]
    fn fma_path_layout_zero_pads_and_buckets() {
        assert_eq!(fma_relative_path(10), PathBuf::from("000/000010.mp3"));
        assert_eq!(fma_relative_path(1000), PathBuf::from("001/001000.mp3"));
        assert_eq!(fma_relative_path(139340), PathBuf::from("139/139340.mp3"));
    }

    #[test]
    fn header_located_by_name_in_both_layouts() {
        // FMAKv2: leading index column, track_id at field 2.
        let v2 = vec![FMAKV2_HEADER];
        let (idx, h) = find_header(&v2).expect("fmakv2 header");
        assert_eq!(idx, 0);
        assert_eq!((h.track_id, h.key_and_mode, h.field_count), (2, 1, 4));

        // keys.csv: a pandas level-0 row first, the real names on line 2.
        let keys = vec![",spotify,key_and_mode", KEYS_CSV_HEADER];
        let (idx, h) = find_header(&keys).expect("keys.csv header");
        assert_eq!(idx, 1);
        assert_eq!((h.track_id, h.key_and_mode, h.field_count), (0, 2, 3));
    }

    #[test]
    fn fmakv2_layout_adapts_and_sorts_by_id() {
        let fx = Fixture::new();
        // Deliberately out of id order; the manifest must come out sorted.
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             1,A minor,331,spotify:track:bbb\n\
             0,F# Major,10,spotify:track:aaa\n"
        ));
        fx.audio_file(331);
        fx.audio_file(10);

        let report = adapt(&fx.options()).expect("adapt fmakv2");
        assert_eq!(report.annotated, 2);
        assert_eq!(report.track_count, 2);
        assert_eq!(report.skipped, 0);
        assert_eq!(report.missing_audio, 0);
        assert_eq!(report.key_unparseable, 0);
        assert!(report.manifest_toml.contains("corpus_id = \"fmak_key\""));
        assert!(report.manifest_toml.contains("tasks = [\"key\"]"));
        // Sorted: id 10 (F# Major) before id 331 (A minor).
        let pos_10 = report.manifest_toml.find("id = \"10\"").expect("id 10");
        let pos_331 = report.manifest_toml.find("id = \"331\"").expect("id 331");
        assert!(pos_10 < pos_331, "tracks must be sorted by numeric id");
        assert!(report.manifest_toml.contains("key = \"F# Major\""));
        assert!(report.manifest_toml.contains("key = \"A minor\""));
    }

    #[test]
    fn original_keys_csv_layout_also_adapts() {
        let fx = Fixture::new();
        // The two-header-row keys.csv layout, different column order.
        fx.write_csv(
            ",spotify,key_and_mode\n\
             track_id,spotify_uri,key_and_mode\n\
             10,spotify:track:aaa,Bb Major\n",
        );
        fx.audio_file(10);

        let report = adapt(&fx.options()).expect("adapt keys.csv");
        assert_eq!(report.track_count, 1);
        assert!(report.manifest_toml.contains("key = \"Bb Major\""));
    }

    #[test]
    fn missing_audio_is_counted_not_fatal() {
        let fx = Fixture::new();
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,F# Major,10,spotify:track:aaa\n\
             1,A minor,331,spotify:track:bbb\n"
        ));
        // Only one of the two tracks has audio in this subset.
        fx.audio_file(10);

        let report = adapt(&fx.options()).expect("adapt with partial coverage");
        assert_eq!(report.annotated, 2);
        assert_eq!(report.track_count, 1);
        assert_eq!(report.missing_audio, 1);
        // Invariant: annotated == track_count + skipped + missing_audio.
        assert_eq!(
            report.annotated,
            report.track_count + report.skipped + report.missing_audio
        );
        assert!(report.manifest_toml.contains("id = \"10\""));
        assert!(!report.manifest_toml.contains("id = \"331\""));
    }

    #[test]
    fn empty_key_label_is_skipped() {
        let fx = Fixture::new();
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,,10,spotify:track:aaa\n\
             1,A minor,331,spotify:track:bbb\n"
        ));
        fx.audio_file(10);
        fx.audio_file(331);

        let report = adapt(&fx.options()).expect("adapt with an empty label");
        assert_eq!(report.annotated, 2);
        assert_eq!(report.skipped, 1);
        assert_eq!(report.track_count, 1);
        assert!(!report.manifest_toml.contains("id = \"10\""));
    }

    #[test]
    fn unparseable_key_label_is_counted_but_emitted() {
        let fx = Fixture::new();
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,Nonsense,10,spotify:track:aaa\n"
        ));
        fx.audio_file(10);

        let report = adapt(&fx.options()).expect("adapt with unparseable label");
        assert_eq!(report.track_count, 1);
        assert_eq!(report.key_unparseable, 1);
        assert!(report.manifest_toml.contains("key = \"Nonsense\""));
    }

    #[test]
    fn malformed_row_is_a_hard_error() {
        let fx = Fixture::new();
        // Second data row has too few fields for the 4-column header.
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,F# Major,10,spotify:track:aaa\n\
             1,A minor\n"
        ));
        fx.audio_file(10);

        let err = adapt(&fx.options()).expect_err("malformed row errors");
        assert!(matches!(err, AdaptError::MalformedRow { count: 1, .. }));
    }

    #[test]
    fn non_integer_track_id_is_a_hard_error() {
        let fx = Fixture::new();
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,F# Major,not-an-id,spotify:track:aaa\n"
        ));

        let err = adapt(&fx.options()).expect_err("bad track_id errors");
        assert!(matches!(err, AdaptError::MalformedRow { count: 1, .. }));
    }

    #[test]
    fn unrecognized_header_is_an_error() {
        let fx = Fixture::new();
        fx.write_csv("col_a,col_b\n1,2\n");
        let err = adapt(&fx.options()).expect_err("no FMAK header errors");
        assert!(matches!(err, AdaptError::HeaderNotFound(_)));
    }

    #[test]
    fn missing_csv_is_an_error() {
        let fx = Fixture::new();
        // No CSV written.
        let err = adapt(&fx.options()).expect_err("missing csv errors");
        assert!(matches!(err, AdaptError::AnnotationsMissing(_)));
    }

    #[test]
    fn no_audio_for_any_track_is_an_error() {
        let fx = Fixture::new();
        fx.write_csv(&format!(
            "{FMAKV2_HEADER}\n\
             0,F# Major,10,spotify:track:aaa\n"
        ));
        // Audio root exists but holds nothing (wrong path / unextracted).
        fs::create_dir_all(fx.audio()).expect("create empty audio root");
        let err = adapt(&fx.options()).expect_err("no matching audio errors");
        assert!(matches!(
            err,
            AdaptError::NoTracksWithAudio { annotated: 1, .. }
        ));
    }
}
