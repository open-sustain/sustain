// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    ops::Range,
    path::{Path, PathBuf},
};

use crate::{TrackMetadata, TrackRelativePath};

const DEFAULT_MAX_COMPONENT_BYTES: usize = 120;
const UNKNOWN_ARTIST: &str = "Unknown Artist";
const UNKNOWN_ALBUM: &str = "Unknown Album";
const UNTITLED: &str = "Untitled";
/// How many paths [`ManagedTrackPathPlanner::candidates`] offers a track:
/// its canonical path and the variants numbered 2 to this.
const CANDIDATE_COUNT: u32 = 9_999;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTrackPathInput<'a> {
    pub metadata: &'a TrackMetadata,
    pub source_path: &'a Path,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTrackPathPlan {
    pub relative_path: TrackRelativePath,
    pub artist_component: String,
    pub album_component: String,
    pub file_name: String,
    pub collision_suffix: Option<u32>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedTrackPathError {
    MissingFileExtension,
    ComponentLimitTooSmall,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedTrackPathPlanner {
    max_component_bytes: usize,
}

impl Default for ManagedTrackPathPlanner {
    fn default() -> Self {
        Self {
            max_component_bytes: DEFAULT_MAX_COMPONENT_BYTES,
        }
    }
}

impl ManagedTrackPathPlanner {
    pub const fn new(max_component_bytes: usize) -> Self {
        Self {
            max_component_bytes,
        }
    }

    /// The paths a track described by `input` may take in a managed
    /// library: its canonical path first, then the numbered variants
    /// (`" 2"`, `" 3"`, …) in order. A caller takes the first candidate
    /// that is free wherever it has to be free — among the library's
    /// tracks, on disk — so the planner never needs the whole library.
    pub fn candidates(
        &self,
        input: ManagedTrackPathInput<'_>,
    ) -> Result<ManagedTrackPathCandidates, ManagedTrackPathError> {
        let extension = input
            .source_path
            .extension()
            .and_then(|extension| extension.to_str())
            .filter(|extension| !extension.trim().is_empty())
            .ok_or(ManagedTrackPathError::MissingFileExtension)?
            .to_owned();

        let artist_component = sanitize_component(
            first_present([
                input.metadata.album_artist.as_deref(),
                input.metadata.artist.as_deref(),
                input.metadata.composer.as_deref(),
            ]),
            UNKNOWN_ARTIST,
            self.max_component_bytes,
        );
        let album_component = sanitize_component(
            input.metadata.album.as_deref(),
            UNKNOWN_ALBUM,
            self.max_component_bytes,
        );
        // The title fallback intentionally does NOT read the source
        // file's name. The planner is called repeatedly across runs
        // (auto-resume on every launch + retargets after edits), and
        // each successful move changes what `source_path.file_stem()`
        // returns. Folding that into the destination produced
        // accumulating track-number prefixes on every launch — the
        // very loop this fallback was meant to avoid. Callers populate
        // `metadata.title` from the source filename at first scan /
        // import time (see `TrackMetadata::ensure_title_from_filename`),
        // so by the time the planner runs the database already holds
        // a stable title for files with no Title tag.
        let title = sanitize_component(
            input.metadata.title.as_deref(),
            UNTITLED,
            self.max_component_bytes,
        );
        let track_artist =
            sanitize_optional_component(input.metadata.artist.as_deref(), self.max_component_bytes);
        let file_stem = file_stem(input.metadata, &title, track_artist.as_deref());

        Ok(ManagedTrackPathCandidates {
            max_component_bytes: self.max_component_bytes,
            artist_component,
            album_component,
            file_stem,
            extension,
            numbers: 1..CANDIDATE_COUNT + 1,
        })
    }
}

/// The candidate paths of one track, from
/// [`ManagedTrackPathPlanner::candidates`].
pub struct ManagedTrackPathCandidates {
    max_component_bytes: usize,
    artist_component: String,
    album_component: String,
    file_stem: String,
    extension: String,
    /// The candidates still to give: 1 is the canonical path, and any
    /// other number `n` its variant `" n"`.
    numbers: Range<u32>,
}

impl Iterator for ManagedTrackPathCandidates {
    type Item = Result<ManagedTrackPathPlan, ManagedTrackPathError>;

    fn next(&mut self) -> Option<Self::Item> {
        let number = self.numbers.next()?;
        let collision_suffix = (number > 1).then_some(number);
        Some(self.candidate(collision_suffix))
    }
}

impl ManagedTrackPathCandidates {
    fn candidate(
        &self,
        collision_suffix: Option<u32>,
    ) -> Result<ManagedTrackPathPlan, ManagedTrackPathError> {
        let file_name = self.file_name_with_suffix(collision_suffix)?;
        let relative_path = TrackRelativePath::new(
            PathBuf::from(&self.artist_component)
                .join(&self.album_component)
                .join(&file_name),
        )
        .ok_or(ManagedTrackPathError::ComponentLimitTooSmall)?;
        Ok(ManagedTrackPathPlan {
            relative_path,
            artist_component: self.artist_component.clone(),
            album_component: self.album_component.clone(),
            file_name,
            collision_suffix,
        })
    }

    fn file_name_with_suffix(
        &self,
        collision_suffix: Option<u32>,
    ) -> Result<String, ManagedTrackPathError> {
        let suffix = collision_suffix
            .map(|suffix| format!(" {suffix}"))
            .unwrap_or_default();
        let extension = &self.extension;
        let reserved_bytes = suffix.len() + 1 + extension.len();
        let available_stem_bytes = self
            .max_component_bytes
            .checked_sub(reserved_bytes)
            .filter(|available| *available > 0)
            .ok_or(ManagedTrackPathError::ComponentLimitTooSmall)?;
        let stem = truncate_utf8_to_max_bytes(&self.file_stem, available_stem_bytes);
        Ok(format!("{stem}{suffix}.{extension}"))
    }
}

fn first_present<'a>(values: impl IntoIterator<Item = Option<&'a str>>) -> Option<&'a str> {
    values
        .into_iter()
        .flatten()
        .find(|value| !value.trim().is_empty())
}

fn file_stem(metadata: &TrackMetadata, title: &str, track_artist: Option<&str>) -> String {
    let mut stem = String::new();
    if let Some(track_number) = metadata.track_number {
        if metadata.disc_total.unwrap_or(1) > 1
            && let Some(disc_number) = metadata.disc_number
        {
            stem.push_str(&format!("{disc_number}-"));
        }
        stem.push_str(&format!("{track_number:02} "));
    }
    if metadata.compilation.unwrap_or(false)
        && let Some(artist) = track_artist
    {
        stem.push_str(artist);
        stem.push_str(" - ");
    }
    stem.push_str(title);
    stem
}

fn sanitize_optional_component(value: Option<&str>, max_bytes: usize) -> Option<String> {
    let sanitized = sanitize_component(value, "", max_bytes);
    (!sanitized.is_empty()).then_some(sanitized)
}

fn sanitize_component(value: Option<&str>, fallback: &str, max_bytes: usize) -> String {
    let raw = value.unwrap_or(fallback);
    let mut sanitized = String::new();
    let mut previous_was_space = false;

    for character in raw.trim().chars() {
        let replacement = if is_forbidden_component_character(character) {
            ' '
        } else {
            character
        };

        if replacement.is_whitespace() {
            if !previous_was_space {
                sanitized.push(' ');
                previous_was_space = true;
            }
        } else {
            sanitized.push(replacement);
            previous_was_space = false;
        }
    }

    let sanitized = sanitized.trim_matches([' ', '.']).to_owned();
    let sanitized = if sanitized.is_empty() || sanitized == "." || sanitized == ".." {
        fallback.to_owned()
    } else {
        sanitized
    };

    truncate_utf8_to_max_bytes(&sanitized, max_bytes)
        .trim_matches([' ', '.'])
        .to_owned()
}

fn is_forbidden_component_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        )
}

fn truncate_utf8_to_max_bytes(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }

    let mut last_valid = 0;
    for (index, character) in value.char_indices() {
        let next = index + character.len_utf8();
        if next > max_bytes {
            break;
        }
        last_valid = next;
    }

    value[..last_valid].trim_end().to_owned()
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeSet, path::Path};

    use crate::{TrackMetadata, TrackRelativePath};

    use super::{ManagedTrackPathError, ManagedTrackPathInput, ManagedTrackPathPlanner};

    #[test]
    fn planner_prefers_album_artist_for_artist_component() {
        let metadata = TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Track Artist".to_owned()),
            album_artist: Some("Album Artist".to_owned()),
            album: Some("Album".to_owned()),
            track_number: Some(7),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "source.flac", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.artist_component, "Album Artist");
        assert_eq!(plan.album_component, "Album");
        assert_eq!(plan.file_name, "07 Song.flac");
        assert_eq!(
            plan.relative_path.to_path_buf(),
            Path::new("Album Artist").join("Album").join("07 Song.flac")
        );
    }

    #[test]
    fn planner_uses_untitled_when_metadata_title_is_missing() {
        // The planner deliberately does NOT consult the source
        // filename — that would feed its own previous output back
        // into the next plan and accumulate prefixes on every run.
        // Callers populate `metadata.title` from the filename at
        // first scan (see `TrackMetadata::ensure_title_from_filename`),
        // so by the time the planner runs the database already holds
        // a stable title; if it doesn't, the file is genuinely
        // nameless and "Untitled" is the right answer.
        let metadata = TrackMetadata::default();

        let plan = plan(&metadata, "Original File.MP3", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.artist_component, "Unknown Artist");
        assert_eq!(plan.album_component, "Unknown Album");
        assert_eq!(plan.file_name, "Untitled.MP3");
    }

    #[test]
    fn planner_uses_untitled_when_title_is_unusable() {
        let metadata = TrackMetadata {
            title: Some("///".to_owned()),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "anything.flac", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.file_name, "Untitled.flac");
    }

    #[test]
    fn planner_is_idempotent_under_track_number_prefix_after_move() {
        // Regression: with the old file-stem fallback, a track whose
        // tag-title was missing got the source filename as a stand-in
        // title, then a track-number prefix in the planned name. On
        // the next run the file was already named "01 X.mp3", so the
        // fallback yielded "01 X" and the planner produced
        // "01 01 X.mp3" — moving the same track again, forever. With
        // the fallback removed, the planner uses "Untitled" and the
        // destination is stable regardless of what `source_path`
        // says.
        let metadata = TrackMetadata {
            track_number: Some(1),
            ..TrackMetadata::default()
        };
        let first_plan =
            plan(&metadata, "Original Filename.mp3", &BTreeSet::new()).expect("first plan");
        assert_eq!(first_plan.file_name, "01 Untitled.mp3");

        // Simulate the file having been moved to the planner's
        // destination on a previous run; pass the new filename as the
        // source.
        let second_plan =
            plan(&metadata, first_plan.file_name.as_str(), &BTreeSet::new()).expect("second plan");

        assert_eq!(
            second_plan.relative_path, first_plan.relative_path,
            "planner output must converge between runs"
        );
    }

    #[test]
    fn planner_adds_disc_prefix_for_multi_disc_tracks() {
        let metadata = TrackMetadata {
            title: Some("Finale".to_owned()),
            album: Some("Live".to_owned()),
            artist: Some("Artist".to_owned()),
            disc_number: Some(2),
            disc_total: Some(3),
            track_number: Some(4),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "source.ogg", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.file_name, "2-04 Finale.ogg");
    }

    #[test]
    fn planner_includes_track_artist_for_compilations() {
        let metadata = TrackMetadata {
            title: Some("Single".to_owned()),
            artist: Some("Singer".to_owned()),
            album_artist: Some("Various Artists".to_owned()),
            album: Some("Compilation".to_owned()),
            compilation: Some(true),
            track_number: Some(3),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "source.m4a", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.file_name, "03 Singer - Single.m4a");
    }

    #[test]
    fn planner_sanitizes_path_separators_and_control_characters() {
        let metadata = TrackMetadata {
            title: Some("Bad/Title:\nMix".to_owned()),
            artist: Some("Artist\\Name".to_owned()),
            album: Some("Album?Name".to_owned()),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "source.flac", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.artist_component, "Artist Name");
        assert_eq!(plan.album_component, "Album Name");
        assert_eq!(plan.file_name, "Bad Title Mix.flac");
        assert!(
            TrackRelativePath::new(plan.relative_path.to_path_buf()).is_some(),
            "planner must produce a safe relative path"
        );
    }

    #[test]
    fn planner_preserves_unicode() {
        let metadata = TrackMetadata {
            title: Some("Déjà vu 東京".to_owned()),
            artist: Some("Björk".to_owned()),
            album: Some("Álbum".to_owned()),
            ..TrackMetadata::default()
        };

        let plan = plan(&metadata, "source.flac", &BTreeSet::new()).expect("planned");

        assert_eq!(plan.artist_component, "Björk");
        assert_eq!(plan.album_component, "Álbum");
        assert_eq!(plan.file_name, "Déjà vu 東京.flac");
    }

    #[test]
    fn planner_resolves_collisions_deterministically() {
        let metadata = TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            track_number: Some(1),
            ..TrackMetadata::default()
        };
        let mut occupied = BTreeSet::new();
        occupied.insert(relative_path("Artist/Album/01 Song.flac"));
        occupied.insert(relative_path("Artist/Album/01 Song 2.flac"));

        let plan = plan(&metadata, "source.flac", &occupied).expect("planned");

        assert_eq!(plan.file_name, "01 Song 3.flac");
        assert_eq!(plan.collision_suffix, Some(3));
    }

    #[test]
    fn candidates_offer_the_canonical_path_then_its_numbered_variants() {
        let metadata = TrackMetadata {
            title: Some("Song".to_owned()),
            artist: Some("Artist".to_owned()),
            album: Some("Album".to_owned()),
            ..TrackMetadata::default()
        };

        let file_names = ManagedTrackPathPlanner::default()
            .candidates(ManagedTrackPathInput {
                metadata: &metadata,
                source_path: Path::new("source.flac"),
            })
            .expect("candidates")
            .map(|candidate| candidate.expect("candidate").file_name)
            .collect::<Vec<_>>();

        assert_eq!(file_names.len(), 9_999);
        assert_eq!(file_names[..3], ["Song.flac", "Song 2.flac", "Song 3.flac"]);
        assert_eq!(
            file_names.last().map(String::as_str),
            Some("Song 9999.flac")
        );
    }

    #[test]
    fn planner_truncates_long_components_without_splitting_utf8() {
        let planner = ManagedTrackPathPlanner::new(16);
        let metadata = TrackMetadata {
            title: Some("Très très très long title".to_owned()),
            artist: Some("Très très très long artist".to_owned()),
            album: Some("Très très très long album".to_owned()),
            ..TrackMetadata::default()
        };

        let plan = planner
            .candidates(ManagedTrackPathInput {
                metadata: &metadata,
                source_path: Path::new("source.flac"),
            })
            .expect("candidates")
            .next()
            .expect("a candidate")
            .expect("planned");

        assert!(plan.artist_component.len() <= 16);
        assert!(plan.album_component.len() <= 16);
        assert!(plan.file_name.len() <= 16);
        assert!(std::str::from_utf8(plan.file_name.as_bytes()).is_ok());
    }

    #[test]
    fn planner_rejects_sources_without_extensions() {
        let error = plan(&TrackMetadata::default(), "source", &BTreeSet::new())
            .expect_err("extension is required");

        assert_eq!(error, ManagedTrackPathError::MissingFileExtension);
    }

    /// The first candidate not in `occupied_paths`.
    fn plan(
        metadata: &TrackMetadata,
        source_path: &str,
        occupied_paths: &BTreeSet<TrackRelativePath>,
    ) -> Result<super::ManagedTrackPathPlan, ManagedTrackPathError> {
        ManagedTrackPathPlanner::default()
            .candidates(ManagedTrackPathInput {
                metadata,
                source_path: Path::new(source_path),
            })?
            .find(|candidate| {
                candidate
                    .as_ref()
                    .map_or(true, |plan| !occupied_paths.contains(&plan.relative_path))
            })
            .expect("a free candidate")
    }

    fn relative_path(path: &str) -> TrackRelativePath {
        TrackRelativePath::new(path).expect("test path is relative")
    }
}
