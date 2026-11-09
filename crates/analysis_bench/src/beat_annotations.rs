// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Adapt human beat and downbeat annotations into benchmark manifests.
//!
//! Two public corpora have every beat of their clips marked by hand, with
//! its position in the bar. Both are 30 s clips, and neither ships its
//! audio with its annotations:
//!
//! * **Ballroom** (Gouyon et al., ISMIR 2004): 698 clips of ballroom
//!   dance music. Beats and bars annotated by Krebs, Böck & Widmer
//!   (<https://github.com/CPJKU/BallroomAnnotations>), one `<stem>.beats`
//!   per clip at the repository root. The audio is the MTG archive
//!   `data1.tar.gz`, laid out `BallroomData/<style>/<stem>.wav`.
//! * **GTZAN** (Tzanetakis & Cook, 2002): 1,000 clips over ten genres.
//!   Beats and downbeats annotated by Marchand & Peeters
//!   (<https://github.com/TempoBeatDownbeat/gtzan_tempo_beat>), as
//!   `beats/gtzan_<genre>_<nnnnn>.beats`. The audio is the `genres`
//!   archive, laid out `genres/<genre>/<genre>.<nnnnn>.wav` (`.au` in the
//!   original release).
//!
//! An annotation file lists one beat per line: its time in seconds and its
//! position in the bar, separated by whitespace. A few GTZAN files mark
//! only the beats' times, with no bars; their clips are counted and
//! excluded. Any other line — or a file mixing both forms, or beats out of
//! time order — is a hard error: the annotations are not what this adapter
//! expects. The analyzer lays out bars of four beats, so only clips counted
//! in fours (positions running up to 4, no further) are kept; the others —
//! waltzes, songs in 3/4 or 6/8 — are counted and excluded. Audio without
//! an annotation file is counted too.
//!
//! The manifest carries each clip's beats inline, so a run needs nothing
//! but the manifest and the audio. See `docs/analysis-benchmark-corpora.md`
//! for sources, licenses and checksums.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::manifest::{Manifest, Task, annotated_beats_problem};
use crate::metrics::AnnotatedBeat;

/// The beats per bar the analyzer lays out.
const BEATS_PER_BAR: u8 = 4;

/// A corpus this adapter reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BeatCorpus {
    /// Ballroom, with the CPJKU beat and bar annotations.
    Ballroom,
    /// GTZAN, with the Marchand & Peeters beat and downbeat annotations.
    Gtzan,
}

impl BeatCorpus {
    /// The corpus named on the command line.
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "ballroom" => Some(Self::Ballroom),
            "gtzan" => Some(Self::Gtzan),
            _ => None,
        }
    }

    /// The manifest `corpus_id`.
    pub fn corpus_id(self) -> &'static str {
        match self {
            Self::Ballroom => "ballroom_beats",
            Self::Gtzan => "gtzan_beats",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Ballroom => "ballroom",
            Self::Gtzan => "gtzan",
        }
    }

    /// The clip a file of the audio tree holds, or `None` for files that
    /// are not part of the corpus (hidden files, metadata).
    fn clip(self, group: &str, file: &Path, annotations: &Path) -> Option<Clip> {
        let file_name = file.file_name()?.to_str()?;
        if file_name.starts_with('.') {
            return None;
        }
        let (stem, extension) = file_name.rsplit_once('.')?;
        match self {
            Self::Ballroom => (extension == "wav").then(|| Clip {
                id: format!("ballroom/{group}/{stem}"),
                audio: file.to_path_buf(),
                annotation: annotations.join(format!("{stem}.beats")),
            }),
            Self::Gtzan => {
                let (genre, number) = stem.split_once('.')?;
                let number: u32 = number.parse().ok()?;
                (matches!(extension, "wav" | "au") && genre == group).then(|| Clip {
                    id: format!("gtzan/{genre}/{number:05}"),
                    audio: file.to_path_buf(),
                    annotation: annotations
                        .join("beats")
                        .join(format!("gtzan_{genre}_{number:05}.beats")),
                })
            }
        }
    }
}

/// One clip of a corpus: its manifest id, its audio, and where its
/// annotation file would be.
struct Clip {
    id: String,
    audio: PathBuf,
    annotation: PathBuf,
}

/// Inputs to [`adapt`].
#[derive(Clone, Debug)]
pub struct AdaptOptions {
    /// Which corpus.
    pub corpus: BeatCorpus,
    /// The root of a checkout of the corpus's annotation repository.
    pub annotations: PathBuf,
    /// The audio tree: Ballroom's `BallroomData`, GTZAN's `genres`.
    pub audio_dir: PathBuf,
}

/// Summary of an adaptation. The counts partition the clips found:
/// `clips == track_count + unannotated + unpositioned + other_meter`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdaptReport {
    /// The corpus id written into the manifest.
    pub corpus_id: String,
    /// The serialized manifest TOML.
    pub manifest_toml: String,
    /// Audio clips found under the audio tree.
    pub clips: usize,
    /// Clips emitted into the manifest.
    pub track_count: usize,
    /// Clips without an annotation file.
    pub unannotated: usize,
    /// Clips whose annotations mark beats but no bars.
    pub unpositioned: usize,
    /// Annotated clips whose bars are not of four beats.
    pub other_meter: usize,
}

/// Adaptation failures.
#[derive(Debug, thiserror::Error)]
pub enum AdaptError {
    /// A filesystem read failed.
    #[error("reading {path}: {source}")]
    Read {
        /// Path being read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
    /// An annotation line is not a time and a bar position.
    #[error("{file}:{line}: expected `<seconds> <bar position>`, found {text:?}")]
    MalformedLine {
        /// The annotation file.
        file: PathBuf,
        /// 1-based line number.
        line: usize,
        /// The line, verbatim.
        text: String,
    },
    /// An annotation file's beats cannot be scored.
    #[error("{file}: {problem}")]
    UnusableBeats {
        /// The annotation file.
        file: PathBuf,
        /// What is wrong with its beats.
        problem: &'static str,
    },
    /// No clip under the audio tree had annotations in four — almost
    /// always a wrong `--audio` or `--annotations` path.
    #[error(
        "no annotated clip in fours under {audio_dir} ({clips} clip(s) found; \
         are --audio and --annotations the corpus's audio tree and annotation checkout?)"
    )]
    NoTracks {
        /// The audio tree searched.
        audio_dir: PathBuf,
        /// Clips found there.
        clips: usize,
    },
    /// A path could not be represented as UTF-8 (TOML manifests are UTF-8).
    #[error("path is not valid UTF-8: {0}")]
    NonUtf8Path(PathBuf),
    /// The manifest failed to serialize to TOML.
    #[error("serializing manifest: {0}")]
    Serialize(#[source] toml::ser::Error),
    /// The generated manifest did not load back — an internal invariant
    /// violation, surfaced rather than swallowed.
    #[error("generated manifest failed to load back: {0}")]
    RoundTrip(String),
}

/// The serializable manifest this adapter emits: a mirror of
/// [`crate::manifest`] with only the fields it writes. Loading the output
/// back as a [`Manifest`] guarantees the two agree.
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
    beats: Vec<AnnotatedBeat>,
}

/// Build a manifest from a corpus's audio tree and annotation checkout.
pub fn adapt(options: &AdaptOptions) -> Result<AdaptReport, AdaptError> {
    let clips = clips(options)?;
    let mut tracks = Vec::new();
    let mut unannotated = 0;
    let mut unpositioned = 0;
    let mut other_meter = 0;
    for clip in &clips {
        if !clip.annotation.is_file() {
            unannotated += 1;
            continue;
        }
        let Some(beats) = read_beats(&clip.annotation)? else {
            unpositioned += 1;
            continue;
        };
        if beats.iter().map(|beat| beat.position).max() != Some(BEATS_PER_BAR) {
            other_meter += 1;
            continue;
        }
        if let Some(problem) = annotated_beats_problem(&beats, false) {
            return Err(AdaptError::UnusableBeats {
                file: clip.annotation.clone(),
                problem,
            });
        }
        let audio = std::path::absolute(&clip.audio).map_err(|source| AdaptError::Read {
            path: clip.audio.clone(),
            source,
        })?;
        let path = audio
            .to_str()
            .ok_or_else(|| AdaptError::NonUtf8Path(audio.clone()))?
            .to_string();
        tracks.push(TrackDoc {
            id: clip.id.clone(),
            path,
            beats,
        });
    }
    if tracks.is_empty() {
        return Err(AdaptError::NoTracks {
            audio_dir: options.audio_dir.clone(),
            clips: clips.len(),
        });
    }

    let corpus = options.corpus;
    let track_count = tracks.len();
    let doc = ManifestDoc {
        meta: MetaDoc {
            corpus_id: corpus.corpus_id().to_string(),
            description: format!(
                "{} clips with human beat and bar annotations, generated by \
                 `analysis-bench adapt-beat-annotations --corpus {}` from {}. \
                 Gitignored — do not edit; regenerate from the audio and the \
                 annotation checkout. See docs/analysis-benchmark-corpora.md.",
                corpus.name(),
                corpus.name(),
                options.annotations.display()
            ),
        },
        options: OptionsDoc {
            tasks: vec![Task::Bpm],
        },
        tracks,
    };
    let manifest_toml = toml::to_string(&doc).map_err(AdaptError::Serialize)?;
    Manifest::parse(&manifest_toml, "the generated manifest")
        .map_err(|err| AdaptError::RoundTrip(err.to_string()))?;

    Ok(AdaptReport {
        corpus_id: corpus.corpus_id().to_string(),
        manifest_toml,
        clips: clips.len(),
        track_count,
        unannotated,
        unpositioned,
        other_meter,
    })
}

/// Every clip under the audio tree, one directory level down (Ballroom's
/// styles, GTZAN's genres), in a stable order.
fn clips(options: &AdaptOptions) -> Result<Vec<Clip>, AdaptError> {
    let mut clips = Vec::new();
    for group in sorted_entries(&options.audio_dir)? {
        let Some(name) = group.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        if name.starts_with('.') || !group.is_dir() {
            continue;
        }
        for file in sorted_entries(&group)? {
            if let Some(clip) = options.corpus.clip(name, &file, &options.annotations) {
                clips.push(clip);
            }
        }
    }
    Ok(clips)
}

fn sorted_entries(dir: &Path) -> Result<Vec<PathBuf>, AdaptError> {
    let read_error = |source| AdaptError::Read {
        path: dir.to_path_buf(),
        source,
    };
    let mut entries = std::fs::read_dir(dir)
        .map_err(read_error)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(read_error)?;
    entries.sort();
    Ok(entries)
}

/// The beats of one annotation file, or `None` when it marks only the
/// beats' times, without bar positions.
fn read_beats(file: &Path) -> Result<Option<Vec<AnnotatedBeat>>, AdaptError> {
    let text = std::fs::read_to_string(file).map_err(|source| AdaptError::Read {
        path: file.to_path_buf(),
        source,
    })?;
    let mut beats = Vec::new();
    let mut unpositioned = 0;
    for (index, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let malformed = || AdaptError::MalformedLine {
            file: file.to_path_buf(),
            line: index + 1,
            text: line.to_string(),
        };
        let fields: Vec<&str> = line.split_whitespace().collect();
        let secs: f64 = fields[0].parse().map_err(|_| malformed())?;
        match fields[1..] {
            [] => unpositioned += 1,
            [position] => beats.push(AnnotatedBeat {
                secs,
                position: position.parse().map_err(|_| malformed())?,
            }),
            _ => return Err(malformed()),
        }
    }
    match (beats.is_empty(), unpositioned) {
        (_, 0) => Ok(Some(beats)),
        (true, _) => Ok(None),
        (false, _) => Err(AdaptError::UnusableBeats {
            file: file.to_path_buf(),
            problem: "some beats have a bar position and some do not",
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `contents` to `root/relative`, creating its directories.
    fn write(root: &Path, relative: &str, contents: &str) {
        let path = root.join(relative);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("create dirs");
        std::fs::write(path, contents).expect("write file");
    }

    const IN_FOUR: &str = "0.30 3\n0.80 4\n1.30 1\n1.80 2\n";
    const IN_THREE: &str = "0.40 1\n1.00 2\n1.60 3\n2.20 1\n";

    #[test]
    fn ballroom_keeps_annotated_clips_in_four() {
        let audio = tempfile::tempdir().expect("audio dir");
        let annotations = tempfile::tempdir().expect("annotations dir");
        write(audio.path(), "Samba/Albums-B-02.wav", "");
        write(audio.path(), "ChaChaCha/Albums-A-01.wav", "");
        write(audio.path(), "ChaChaCha/._Albums-A-01.wav", "");
        write(audio.path(), "Waltz/Albums-C-03.wav", "");
        write(audio.path(), "Tango/Albums-D-04.wav", "");
        write(annotations.path(), "Albums-A-01.beats", IN_FOUR);
        write(annotations.path(), "Albums-B-02.beats", IN_FOUR);
        write(annotations.path(), "Albums-C-03.beats", IN_THREE);

        let report = adapt(&AdaptOptions {
            corpus: BeatCorpus::Ballroom,
            annotations: annotations.path().to_path_buf(),
            audio_dir: audio.path().to_path_buf(),
        })
        .expect("adapt");
        assert_eq!(
            (
                report.clips,
                report.track_count,
                report.unannotated,
                report.other_meter
            ),
            (4, 2, 1, 1)
        );
        let manifest = Manifest::parse(&report.manifest_toml, "test").expect("load back");
        let ids: Vec<&str> = manifest
            .tracks
            .iter()
            .map(|track| track.id.as_str())
            .collect();
        assert_eq!(
            ids,
            [
                "ballroom/ChaChaCha/Albums-A-01",
                "ballroom/Samba/Albums-B-02"
            ]
        );
        let beats = manifest.tracks[0].beats.as_ref().expect("beats");
        assert_eq!(
            beats[0],
            AnnotatedBeat {
                secs: 0.3,
                position: 3
            }
        );
        assert_eq!(beats.len(), 4);
    }

    #[test]
    fn gtzan_pairs_clips_with_their_numbered_annotations() {
        let audio = tempfile::tempdir().expect("audio dir");
        let annotations = tempfile::tempdir().expect("annotations dir");
        write(audio.path(), "blues/blues.00007.wav", "");
        write(audio.path(), "blues/blues.mf", "");
        write(audio.path(), "jazz/jazz.00012.au", "");
        write(audio.path(), "jazz/jazz.00013.au", "");
        write(annotations.path(), "beats/gtzan_blues_00007.beats", IN_FOUR);
        write(annotations.path(), "beats/gtzan_jazz_00012.beats", IN_FOUR);
        write(
            annotations.path(),
            "beats/gtzan_jazz_00013.beats",
            "0.29\n1.08\n1.85\n",
        );

        let report = adapt(&AdaptOptions {
            corpus: BeatCorpus::Gtzan,
            annotations: annotations.path().to_path_buf(),
            audio_dir: audio.path().to_path_buf(),
        })
        .expect("adapt");
        assert_eq!((report.track_count, report.unpositioned), (2, 1));
        let manifest = Manifest::parse(&report.manifest_toml, "test").expect("load back");
        assert_eq!(manifest.tracks[0].id, "gtzan/blues/00007");
        assert_eq!(manifest.tracks[1].id, "gtzan/jazz/00012");
    }

    #[test]
    fn a_malformed_annotation_is_an_error() {
        let audio = tempfile::tempdir().expect("audio dir");
        let annotations = tempfile::tempdir().expect("annotations dir");
        write(audio.path(), "Jive/Albums-E-05.wav", "");
        write(
            annotations.path(),
            "Albums-E-05.beats",
            "0.30 1\n0.80 two\n",
        );
        let error = adapt(&AdaptOptions {
            corpus: BeatCorpus::Ballroom,
            annotations: annotations.path().to_path_buf(),
            audio_dir: audio.path().to_path_buf(),
        })
        .expect_err("a malformed line");
        assert!(
            matches!(error, AdaptError::MalformedLine { line: 2, .. }),
            "{error}"
        );
    }

    #[test]
    fn nothing_to_adapt_is_an_error() {
        let audio = tempfile::tempdir().expect("audio dir");
        let annotations = tempfile::tempdir().expect("annotations dir");
        write(audio.path(), "Jive/Albums-E-05.wav", "");
        let error = adapt(&AdaptOptions {
            corpus: BeatCorpus::Ballroom,
            annotations: annotations.path().to_path_buf(),
            audio_dir: audio.path().to_path_buf(),
        })
        .expect_err("no annotations");
        assert!(
            matches!(error, AdaptError::NoTracks { clips: 1, .. }),
            "{error}"
        );
    }
}
