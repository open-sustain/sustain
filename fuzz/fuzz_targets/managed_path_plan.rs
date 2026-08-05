// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

#![no_main]

use std::{
    collections::BTreeSet,
    ffi::{OsStr, OsString},
    os::unix::ffi::{OsStrExt, OsStringExt},
    path::{Component, Path, PathBuf},
};

use arbitrary::{Arbitrary, Unstructured};
use libfuzzer_sys::fuzz_target;
use sustain_domain::{
    ManagedTrackPathInput, ManagedTrackPathPlan, ManagedTrackPathPlanner, TrackMetadata,
    TrackRelativePath,
};

const MAX_COMPONENT_BYTES: usize = 120;
const MAX_COLLISION_DEPTH: u8 = 64;
const MAX_DECORATED_TEXT_BYTES: usize = 65_536;
const SOURCE_STEM_BYTE_LIMIT: usize = 4_096;
const SUPPORTED_EXTENSIONS: &[&str] = &[
    "mp3", "ogg", "oga", "opus", "flac", "m4a", "m4b", "mp4", "wav",
];
const HOSTILE_DECORATIONS: &[&str] = &[
    "",
    " \t\r\n ",
    "\0",
    "/../\\:*?\"<>|",
    "e\u{301}\u{327}",
    "\u{200f}\u{202e}עברית",
    "東京🧪",
    "...",
];

#[derive(Arbitrary, Debug)]
struct PlannerFuzzInput {
    title: Option<String>,
    artist: Option<String>,
    album: Option<String>,
    album_artist: Option<String>,
    composer: Option<String>,
    track_number: Option<u32>,
    track_total: Option<u32>,
    disc_number: Option<u32>,
    disc_total: Option<u32>,
    compilation: Option<bool>,
    source_stem: Vec<u8>,
    extension_selector: u8,
    uppercase_extension: bool,
    derive_title_from_source: bool,
    decoration_seed: u8,
    collision_depth: u8,
}

fuzz_target!(|data: &[u8]| {
    let mut unstructured = Unstructured::new(data);
    let Ok(input) = PlannerFuzzInput::arbitrary(&mut unstructured) else {
        return;
    };
    exercise_planner(input);
});

fn exercise_planner(input: PlannerFuzzInput) {
    let extension = selected_extension(input.extension_selector, input.uppercase_extension);
    let source_path = source_path(&input.source_stem, &extension);
    let mut metadata = TrackMetadata {
        title: decorated(input.title, input.decoration_seed),
        artist: decorated(input.artist, input.decoration_seed.wrapping_add(1)),
        album: decorated(input.album, input.decoration_seed.wrapping_add(2)),
        album_artist: decorated(input.album_artist, input.decoration_seed.wrapping_add(3)),
        composer: decorated(input.composer, input.decoration_seed.wrapping_add(4)),
        track_number: input.track_number,
        track_total: input.track_total,
        disc_number: input.disc_number,
        disc_total: input.disc_total,
        compilation: input.compilation,
        ..TrackMetadata::default()
    };
    if input.derive_title_from_source {
        metadata.title = None;
        metadata.ensure_title_from_filename(&source_path);
    }

    let planner = ManagedTrackPathPlanner::default();
    let planner_input = ManagedTrackPathInput {
        metadata: &metadata,
        source_path: &source_path,
    };
    let collision_depth = usize::from(input.collision_depth % MAX_COLLISION_DEPTH);
    let mut occupied_paths = BTreeSet::new();

    for _ in 0..collision_depth {
        let occupied = planner
            .plan(planner_input.clone(), &occupied_paths)
            .expect("valid managed-path input must remain plannable");
        assert_plan_invariants(&occupied, &extension, &occupied_paths);
        assert!(occupied_paths.insert(occupied.relative_path));
    }

    let plan = planner
        .plan(planner_input.clone(), &occupied_paths)
        .expect("valid managed-path input must remain plannable");
    assert_plan_invariants(&plan, &extension, &occupied_paths);
    if collision_depth == 0 {
        assert_eq!(plan.collision_suffix, None);
    } else {
        assert!(plan.collision_suffix.is_some_and(|suffix| suffix >= 2));
    }

    let repeated = planner
        .plan(planner_input, &occupied_paths)
        .expect("repeated planning must succeed");
    assert_eq!(plan, repeated, "planning must be deterministic");

    let replanned = planner
        .plan(
            ManagedTrackPathInput {
                metadata: &metadata,
                source_path: plan.relative_path.as_path(),
            },
            &occupied_paths,
        )
        .expect("planning from the prior destination must succeed");
    assert_eq!(
        plan, replanned,
        "planning must converge after a managed move"
    );
}

fn selected_extension(selector: u8, uppercase: bool) -> String {
    let extension = SUPPORTED_EXTENSIONS[usize::from(selector) % SUPPORTED_EXTENSIONS.len()];
    if uppercase {
        extension.to_ascii_uppercase()
    } else {
        extension.to_owned()
    }
}

fn source_path(stem_bytes: &[u8], extension: &str) -> PathBuf {
    let mut bytes = Vec::with_capacity(stem_bytes.len().min(SOURCE_STEM_BYTE_LIMIT) + 6);
    bytes.push(b'x');
    bytes.extend(
        stem_bytes
            .iter()
            .copied()
            .take(SOURCE_STEM_BYTE_LIMIT)
            .map(|byte| match byte {
                0 | b'/' => b'_',
                other => other,
            }),
    );
    bytes.push(b'.');
    bytes.extend_from_slice(extension.as_bytes());
    PathBuf::from(OsString::from_vec(bytes))
}

fn decorated(value: Option<String>, selector: u8) -> Option<String> {
    value.map(|mut value| {
        value.push_str(HOSTILE_DECORATIONS[usize::from(selector) % HOSTILE_DECORATIONS.len()]);
        amplify_text(&mut value, selector);
        value
    })
}

fn amplify_text(value: &mut String, selector: u8) {
    if value.is_empty() {
        return;
    }
    let requested_copies = match selector >> 5 {
        0..=4 => 1,
        5 => 8,
        6 => 64,
        _ => 512,
    };
    let copies = requested_copies.min((MAX_DECORATED_TEXT_BYTES / value.len()).max(1));
    if copies == 1 {
        return;
    }
    let unit = value.clone();
    value.reserve(unit.len() * (copies - 1));
    for _ in 1..copies {
        value.push_str(&unit);
    }
}

fn assert_plan_invariants(
    plan: &ManagedTrackPathPlan,
    extension: &str,
    occupied_paths: &BTreeSet<TrackRelativePath>,
) {
    assert!(!occupied_paths.contains(&plan.relative_path));
    assert_eq!(
        plan.relative_path.as_path().extension(),
        Some(OsStr::new(extension))
    );
    assert_eq!(
        plan.relative_path.as_path().file_name(),
        Some(OsStr::new(&plan.file_name))
    );
    assert!(
        plan.relative_path
            .resolve(Path::new("/tmp/sustain-library"))
            .starts_with("/tmp/sustain-library")
    );

    let components = plan
        .relative_path
        .as_path()
        .components()
        .map(|component| match component {
            Component::Normal(component) => component,
            _ => panic!("managed paths must contain only normal components"),
        })
        .collect::<Vec<_>>();
    assert_eq!(components.len(), 3);
    for component in &components {
        assert!(!component.as_bytes().is_empty());
        assert!(component.as_bytes().len() <= MAX_COMPONENT_BYTES);
    }
    assert_eq!(components[0].as_bytes(), plan.artist_component.as_bytes());
    assert_eq!(components[1].as_bytes(), plan.album_component.as_bytes());
    assert_eq!(components[2].as_bytes(), plan.file_name.as_bytes());
    assert!(
        Path::new(&plan.file_name)
            .file_stem()
            .is_some_and(|stem| !stem.as_bytes().is_empty())
    );
    assert!(
        !plan
            .artist_component
            .chars()
            .any(is_forbidden_component_character)
    );
    assert!(
        !plan
            .album_component
            .chars()
            .any(is_forbidden_component_character)
    );
    assert!(!plan.file_name.chars().any(is_forbidden_component_character));
}

fn is_forbidden_component_character(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|'
        )
}
