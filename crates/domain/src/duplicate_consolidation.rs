// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};

use crate::{
    AcousticFeatures, Playlist, PlaylistEntry, RecordingIdentity, Track, TrackId, TrackMetadata,
};

const STRICT_DURATION_TOLERANCE: Duration = Duration::from_secs(2);
const LOOSE_DURATION_RATIO_LIMIT: f64 = 1.30;

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum DuplicateMatchMode {
    #[default]
    Loose,
    Strict,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationRequest {
    pub track_ids: Vec<TrackId>,
    pub audio_track_id: TrackId,
    pub metadata: DuplicateMetadataSelection,
    pub artwork_track_id: TrackId,
    pub rating_track_id: TrackId,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum DuplicateMetadataField {
    Title,
    Artist,
    Album,
    AlbumArtist,
    Composer,
    Grouping,
    Genre,
    TrackNumber,
    TrackTotal,
    DiscNumber,
    DiscTotal,
    Year,
    Compilation,
    Bpm,
    Key,
    Comments,
    Lyrics,
}

impl DuplicateMetadataField {
    pub const ALL: [Self; 17] = [
        Self::Title,
        Self::Artist,
        Self::Album,
        Self::AlbumArtist,
        Self::Composer,
        Self::Grouping,
        Self::Genre,
        Self::TrackNumber,
        Self::TrackTotal,
        Self::DiscNumber,
        Self::DiscTotal,
        Self::Year,
        Self::Compilation,
        Self::Bpm,
        Self::Key,
        Self::Comments,
        Self::Lyrics,
    ];
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DuplicateMetadataFieldSelection {
    pub field: DuplicateMetadataField,
    pub track_id: TrackId,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateMetadataSelection {
    pub fields: Vec<DuplicateMetadataFieldSelection>,
}

impl DuplicateMetadataSelection {
    pub fn from_track(track_id: TrackId) -> Self {
        Self {
            fields: DuplicateMetadataField::ALL
                .into_iter()
                .map(|field| DuplicateMetadataFieldSelection { field, track_id })
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DuplicateAudioQuality {
    pub bitrate_kbps: Option<u32>,
    pub lossless: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateConsolidationPlan {
    pub survivor: Track,
    pub removed_track_ids: Vec<TrackId>,
    pub rewritten_playlists: Vec<Playlist>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DuplicateConsolidationError {
    NeedsMultipleTracks,
    RepeatedTrack,
    TrackNotFound,
    TrackUnavailable,
    ReferenceNotSelected,
    InvalidMetadataSelection,
    CountOverflow,
}

pub fn default_duplicate_metadata_selection(
    tracks: &[Track],
) -> Option<DuplicateMetadataSelection> {
    let fallback = tracks.first()?.id;
    Some(DuplicateMetadataSelection {
        fields: DuplicateMetadataField::ALL
            .into_iter()
            .map(|field| DuplicateMetadataFieldSelection {
                field,
                track_id: default_metadata_track_id(tracks, field).unwrap_or(fallback),
            })
            .collect(),
    })
}

fn default_metadata_track_id(tracks: &[Track], field: DuplicateMetadataField) -> Option<TrackId> {
    if field == DuplicateMetadataField::Year {
        return tracks
            .iter()
            .filter_map(|track| track.metadata.year.map(|year| (year, track.id)))
            .min_by_key(|(year, _)| *year)
            .map(|(_, track_id)| track_id);
    }
    if field == DuplicateMetadataField::Compilation {
        return tracks
            .iter()
            .find(|track| track.metadata.compilation == Some(false))
            .or_else(|| {
                tracks
                    .iter()
                    .find(|track| track.metadata.compilation.is_none())
            })
            .or_else(|| {
                tracks
                    .iter()
                    .find(|track| track.metadata.compilation == Some(true))
            })
            .map(|track| track.id);
    }
    tracks
        .iter()
        .find(|track| metadata_field_is_populated(&track.metadata, field))
        .map(|track| track.id)
}

pub fn duplicate_audio_quality(track: &Track) -> DuplicateAudioQuality {
    DuplicateAudioQuality {
        bitrate_kbps: track.metadata.bitrate_kbps,
        lossless: is_lossless_extension(track),
    }
}

pub fn highest_quality_duplicate_audio_track_ids(tracks: &[Track]) -> Vec<TrackId> {
    let Some(highest) = tracks.iter().map(duplicate_audio_quality).max() else {
        return Vec::new();
    };
    tracks
        .iter()
        .filter(|track| duplicate_audio_quality(track) == highest)
        .map(|track| track.id)
        .collect()
}

/// The track whose rating a merge keeps when nobody picks: the highest
/// rating, the earliest of `tracks` on a tie.
pub fn highest_rated_duplicate_track_id(tracks: &[Track]) -> Option<TrackId> {
    tracks
        .iter()
        .rev()
        .max_by_key(|track| track.rating)
        .map(|track| track.id)
}

/// What a merge keeps when nobody picks, apart from the artwork, which needs
/// the images themselves: see [`highest_quality_duplicate_artwork`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateMergeChoices {
    /// The best supported audio, the earliest of the tracks on a tie.
    pub audio_track_id: TrackId,
    /// Per field, as [`default_duplicate_metadata_selection`] picks.
    pub metadata: DuplicateMetadataSelection,
    /// As [`highest_rated_duplicate_track_id`] picks.
    pub rating_track_id: TrackId,
}

/// The default choices for merging `tracks`; `None` for no tracks.
pub fn default_duplicate_merge_choices(tracks: &[Track]) -> Option<DuplicateMergeChoices> {
    Some(DuplicateMergeChoices {
        audio_track_id: *highest_quality_duplicate_audio_track_ids(tracks).first()?,
        metadata: default_duplicate_metadata_selection(tracks)?,
        rating_track_id: highest_rated_duplicate_track_id(tracks)?,
    })
}

/// How good one track's embedded artwork is: more pixels, then more encoded
/// bytes for the same pixels.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DuplicateArtworkQuality {
    pub pixels: u64,
    pub encoded_bytes: usize,
}

/// The index of the best artwork among `qualities`, one per track with
/// `None` for a track without artwork; the earliest on a tie, `None` when no
/// track has artwork.
pub fn highest_quality_duplicate_artwork(
    qualities: &[Option<DuplicateArtworkQuality>],
) -> Option<usize> {
    let mut best: Option<(usize, DuplicateArtworkQuality)> = None;
    for (index, quality) in qualities.iter().enumerate() {
        if let Some(quality) = *quality
            && best.is_none_or(|(_, best_quality)| quality > best_quality)
        {
            best = Some((index, quality));
        }
    }
    best.map(|(index, _)| index)
}

/// One group the audio verified as copies of a single recording, as the
/// user previewed it: the tracks as they were then, and the choices the
/// merge will make. The merge runs only while every track still matches
/// its snapshot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VerifiedDuplicateMerge {
    pub tracks: Vec<Track>,
    pub choices: DuplicateMergeChoices,
}

impl VerifiedDuplicateMerge {
    /// The merge of `tracks` with the default choices; `None` for fewer than
    /// two tracks.
    pub fn new(tracks: Vec<Track>) -> Option<Self> {
        if tracks.len() < 2 {
            return None;
        }
        let choices = default_duplicate_merge_choices(&tracks)?;
        Some(Self { tracks, choices })
    }

    /// Whether `current` still holds everything the preview showed and the
    /// choices were made from. Listening statistics are summed whatever they
    /// are, so a play since the preview does not count as a change.
    pub fn still_matches(&self, current: &[Track]) -> bool {
        self.tracks.len() == current.len()
            && self
                .tracks
                .iter()
                .zip(current)
                .all(|(previewed, current)| same_merge_inputs(previewed, current))
    }

    /// The full request, keeping the artwork of `artwork_track_id`.
    pub fn request(&self, artwork_track_id: TrackId) -> DuplicateConsolidationRequest {
        DuplicateConsolidationRequest {
            track_ids: self.tracks.iter().map(|track| track.id).collect(),
            audio_track_id: self.choices.audio_track_id,
            metadata: self.choices.metadata.clone(),
            artwork_track_id,
            rating_track_id: self.choices.rating_track_id,
        }
    }
}

fn same_merge_inputs(previewed: &Track, current: &Track) -> bool {
    // Destructured so a new track field must be classified here.
    let Track {
        id,
        location,
        metadata,
        rating,
        statistics: _,
        file_size_bytes,
        has_embedded_artwork,
        file_modified_at,
    } = previewed;
    *id == current.id
        && *location == current.location
        && *metadata == current.metadata
        && *rating == current.rating
        && *file_size_bytes == current.file_size_bytes
        && *has_embedded_artwork == current.has_embedded_artwork
        && *file_modified_at == current.file_modified_at
}

/// What the audio says about whether two tracks hold the same recording.
/// Duplicate grouping asks only about pairs whose tags already match, so
/// an implementation may do real work per question.
pub trait RecordingEvidence {
    fn identity(&mut self, left: TrackId, right: TrackId) -> RecordingIdentity;
}

/// Evidence for a caller with no audio to compare: nothing is established.
pub struct NoRecordingEvidence;

impl RecordingEvidence for NoRecordingEvidence {
    fn identity(&mut self, _left: TrackId, _right: TrackId) -> RecordingIdentity {
        RecordingIdentity::NotEstablished
    }
}

/// One suggested set of duplicate tracks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DuplicateGroup {
    /// The members, by ascending id.
    pub track_ids: Vec<TrackId>,
    /// Whether the audio established every pair of members as the same
    /// recording, rather than merely not ruling it out.
    pub same_recording: bool,
}

/// Group `tracks` into suggested duplicates: same normalised artist and
/// title, compatible durations and live markers, no clearly different
/// acoustics, and no pair the audio shows to be a different recording.
/// Every pair inside a group satisfies all of that; a match never extends
/// to a third track by transitivity.
pub fn duplicate_groups(
    tracks: &[Track],
    mode: DuplicateMatchMode,
    acoustics: &BTreeMap<TrackId, AcousticFeatures>,
    evidence: &mut dyn RecordingEvidence,
) -> Vec<DuplicateGroup> {
    let mut loose_groups: BTreeMap<(String, String), Vec<&Track>> = BTreeMap::new();
    for track in tracks {
        let title = normalized_duplicate_text(track.metadata.title.as_deref().unwrap_or_default());
        if title.is_empty() {
            continue;
        }
        let artist =
            normalized_duplicate_text(track.metadata.artist.as_deref().unwrap_or_default());
        loose_groups.entry((artist, title)).or_default().push(track);
    }

    let mut identities = PairIdentities {
        evidence,
        known: BTreeMap::new(),
    };
    let mut groups = Vec::new();
    for tracks in loose_groups.into_values().filter(|tracks| tracks.len() > 1) {
        append_candidate_groups(&mut groups, tracks, mode, acoustics, &mut identities);
    }
    groups
}

/// Recording evidence asked at most once per pair.
struct PairIdentities<'evidence> {
    evidence: &'evidence mut dyn RecordingEvidence,
    known: BTreeMap<(TrackId, TrackId), RecordingIdentity>,
}

impl PairIdentities<'_> {
    fn get(&mut self, left: TrackId, right: TrackId) -> RecordingIdentity {
        let key = (left.min(right), left.max(right));
        *self
            .known
            .entry(key)
            .or_insert_with(|| self.evidence.identity(key.0, key.1))
    }
}

pub fn plan_duplicate_consolidation(
    tracks: &[Track],
    playlists: &[Playlist],
    request: &DuplicateConsolidationRequest,
    resulting_file_size_bytes: u64,
    resulting_has_embedded_artwork: bool,
) -> Result<DuplicateConsolidationPlan, DuplicateConsolidationError> {
    let selected_ids = request.track_ids.iter().copied().collect::<BTreeSet<_>>();
    if selected_ids.len() < 2 {
        return Err(DuplicateConsolidationError::NeedsMultipleTracks);
    }
    if selected_ids.len() != request.track_ids.len() {
        return Err(DuplicateConsolidationError::RepeatedTrack);
    }
    for reference in [
        request.audio_track_id,
        request.artwork_track_id,
        request.rating_track_id,
    ] {
        if !selected_ids.contains(&reference) {
            return Err(DuplicateConsolidationError::ReferenceNotSelected);
        }
    }

    let selected_tracks = selected_ids
        .iter()
        .map(|id| {
            tracks
                .iter()
                .find(|track| track.id == *id)
                .ok_or(DuplicateConsolidationError::TrackNotFound)
        })
        .collect::<Result<Vec<_>, _>>()?;
    if selected_tracks
        .iter()
        .any(|track| track.location.is_missing())
    {
        return Err(DuplicateConsolidationError::TrackUnavailable);
    }
    // The audio survivor is whichever file the user picked. Highest quality is
    // preselected in the UI (see `highest_quality_duplicate_audio_track_ids`),
    // but the choice is deliberately overridable — e.g. keeping a 16-bit FLAC
    // over a larger 24-bit one — so the planner does not enforce it.
    let audio_track = selected_tracks
        .iter()
        .find(|track| track.id == request.audio_track_id)
        .ok_or(DuplicateConsolidationError::TrackNotFound)?;

    let mut survivor = (*audio_track).clone();
    copy_selected_editable_metadata(&mut survivor.metadata, &selected_tracks, &request.metadata)?;
    // The rating, like each metadata field, is whichever track the user picked
    // (highest is preselected in the UI but stays overridable). Listening
    // statistics below are not choices: counts cumulate, dates take the extreme.
    survivor.rating = selected_tracks
        .iter()
        .find(|track| track.id == request.rating_track_id)
        .ok_or(DuplicateConsolidationError::TrackNotFound)?
        .rating;
    survivor.statistics.play_count = selected_tracks
        .iter()
        .try_fold(0_u64, |total, track| {
            total.checked_add(track.statistics.play_count)
        })
        .ok_or(DuplicateConsolidationError::CountOverflow)?;
    survivor.statistics.skip_count = selected_tracks
        .iter()
        .try_fold(0_u64, |total, track| {
            total.checked_add(track.statistics.skip_count)
        })
        .ok_or(DuplicateConsolidationError::CountOverflow)?;
    survivor.statistics.last_played_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.last_played_at)
        .max();
    survivor.statistics.last_skipped_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.last_skipped_at)
        .max();
    survivor.statistics.date_added_at = selected_tracks
        .iter()
        .filter_map(|track| track.statistics.date_added_at)
        .min();
    survivor.file_size_bytes = Some(resulting_file_size_bytes);
    survivor.has_embedded_artwork = Some(resulting_has_embedded_artwork);
    survivor.file_modified_at = None;

    let removed_track_ids = selected_ids
        .iter()
        .copied()
        .filter(|track_id| *track_id != survivor.id)
        .collect::<Vec<_>>();
    let rewritten_playlists = playlists
        .iter()
        .filter_map(|playlist| {
            rewrite_playlist(playlist, &removed_track_ids)
                .then_some(())
                .map(|()| rewrite_playlist_entries(playlist, survivor.id, &removed_track_ids))
        })
        .collect();

    Ok(DuplicateConsolidationPlan {
        survivor,
        removed_track_ids,
        rewritten_playlists,
    })
}

fn append_candidate_groups(
    groups: &mut Vec<DuplicateGroup>,
    mut tracks: Vec<&Track>,
    mode: DuplicateMatchMode,
    acoustics: &BTreeMap<TrackId, AcousticFeatures>,
    identities: &mut PairIdentities<'_>,
) {
    tracks.sort_by_key(|track| {
        (
            track
                .metadata
                .duration
                .map(|duration| duration.as_millis())
                .unwrap_or_default(),
            track.id,
        )
    });

    let mut candidate_groups: Vec<Vec<&Track>> = Vec::new();
    for track in tracks {
        let mut joined = false;
        for group in &mut candidate_groups {
            let mut matches_every_member = true;
            for reference in group.iter() {
                if !duplicate_candidates_match(reference, track, mode, acoustics)
                    || identities.get(reference.id, track.id) == RecordingIdentity::Different
                {
                    matches_every_member = false;
                    break;
                }
            }
            if matches_every_member {
                group.push(track);
                joined = true;
                break;
            }
        }
        if !joined {
            candidate_groups.push(vec![track]);
        }
    }
    for group in candidate_groups.into_iter().filter(|group| group.len() > 1) {
        let mut same_recording = true;
        for (index, left) in group.iter().enumerate() {
            for right in &group[index + 1..] {
                same_recording &= identities.get(left.id, right.id) == RecordingIdentity::Same;
            }
        }
        groups.push(DuplicateGroup {
            track_ids: sorted_ids(group),
            same_recording,
        });
    }
}

/// Whether the tags allow `left` and `right` to be the same track. The
/// audio is consulted separately, and only when this holds.
fn duplicate_candidates_match(
    left: &Track,
    right: &Track,
    mode: DuplicateMatchMode,
    acoustics: &BTreeMap<TrackId, AcousticFeatures>,
) -> bool {
    if !loose_durations_match(left.metadata.duration, right.metadata.duration)
        || live_marker_mismatch(&left.metadata, &right.metadata)
        || acoustics_are_clearly_different(acoustics.get(&left.id), acoustics.get(&right.id))
    {
        return false;
    }

    if mode == DuplicateMatchMode::Strict {
        normalized_duplicate_text(left.metadata.album.as_deref().unwrap_or_default())
            == normalized_duplicate_text(right.metadata.album.as_deref().unwrap_or_default())
            && durations_match(left.metadata.duration, right.metadata.duration)
    } else {
        true
    }
}

fn sorted_ids(mut tracks: Vec<&Track>) -> Vec<TrackId> {
    tracks.sort_by_key(|track| track.id);
    tracks.into_iter().map(|track| track.id).collect()
}

fn durations_match(left: Option<Duration>, right: Option<Duration>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => left.abs_diff(right) <= STRICT_DURATION_TOLERANCE,
        (None, None) => true,
        _ => false,
    }
}

fn loose_durations_match(left: Option<Duration>, right: Option<Duration>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            let left = left.as_secs_f64();
            let right = right.as_secs_f64();
            let min = left.min(right);
            let max = left.max(right);
            if min == 0.0 {
                max == 0.0
            } else {
                max / min <= LOOSE_DURATION_RATIO_LIMIT
            }
        }
        _ => true,
    }
}

fn live_marker_mismatch(left: &TrackMetadata, right: &TrackMetadata) -> bool {
    has_live_marker(left) != has_live_marker(right)
}

fn has_live_marker(metadata: &TrackMetadata) -> bool {
    let mut text = String::new();
    if let Some(title) = &metadata.title {
        text.push_str(title);
        text.push(' ');
    }
    if let Some(album) = &metadata.album {
        text.push_str(album);
    }
    text.nfkd()
        .flat_map(char::to_lowercase)
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                ' '
            }
        })
        .collect::<String>()
        .split_whitespace()
        .any(|token| token == "live")
}

fn acoustics_are_clearly_different(
    left: Option<&AcousticFeatures>,
    right: Option<&AcousticFeatures>,
) -> bool {
    let (Some(left), Some(right)) = (left, right) else {
        return false;
    };

    let mut differences = 0;
    if (left.integrated_lufs - right.integrated_lufs).abs() > 8.0
        || (left.short_term_lufs_max - right.short_term_lufs_max).abs() > 8.0
    {
        differences += 1;
    }
    if (left.loudness_range_lu - right.loudness_range_lu).abs() > 10.0 {
        differences += 1;
    }
    let onset_min = left.onset_rate_hz.min(right.onset_rate_hz);
    let onset_max = left.onset_rate_hz.max(right.onset_rate_hz);
    if onset_max - onset_min > 1.5 && (onset_min <= 0.0 || onset_max / onset_min > 2.5) {
        differences += 1;
    }
    let band_distance = left
        .band_ratios()
        .into_iter()
        .zip(right.band_ratios())
        .map(|(left, right)| (left - right).abs())
        .sum::<f32>();
    if band_distance > 0.65 {
        differences += 1;
    }
    if (left.low_band_variation - right.low_band_variation).abs() > 1.0 {
        differences += 1;
    }
    if (left.tonalness - right.tonalness).abs() > 0.55 {
        differences += 1;
    }

    differences >= 2
}

fn normalized_duplicate_text(value: &str) -> String {
    value
        .nfkd()
        .filter(|character| !is_combining_mark(*character))
        .flat_map(char::to_lowercase)
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn metadata_field_is_populated(metadata: &TrackMetadata, field: DuplicateMetadataField) -> bool {
    match field {
        DuplicateMetadataField::Title => populated_text(&metadata.title),
        DuplicateMetadataField::Artist => populated_text(&metadata.artist),
        DuplicateMetadataField::Album => populated_text(&metadata.album),
        DuplicateMetadataField::AlbumArtist => populated_text(&metadata.album_artist),
        DuplicateMetadataField::Composer => populated_text(&metadata.composer),
        DuplicateMetadataField::Grouping => populated_text(&metadata.grouping),
        DuplicateMetadataField::Genre => populated_text(&metadata.genre),
        DuplicateMetadataField::TrackNumber => metadata.track_number.is_some(),
        DuplicateMetadataField::TrackTotal => metadata.track_total.is_some(),
        DuplicateMetadataField::DiscNumber => metadata.disc_number.is_some(),
        DuplicateMetadataField::DiscTotal => metadata.disc_total.is_some(),
        DuplicateMetadataField::Year => metadata.year.is_some(),
        DuplicateMetadataField::Compilation => metadata.compilation.is_some(),
        DuplicateMetadataField::Bpm => metadata.bpm.is_some(),
        DuplicateMetadataField::Key => metadata.key.is_some(),
        DuplicateMetadataField::Comments => populated_text(&metadata.comments),
        DuplicateMetadataField::Lyrics => populated_text(&metadata.lyrics),
    }
}

fn populated_text(value: &Option<String>) -> bool {
    value
        .as_deref()
        .is_some_and(|value| !value.trim().is_empty())
}

fn copy_selected_editable_metadata(
    target: &mut TrackMetadata,
    tracks: &[&Track],
    selection: &DuplicateMetadataSelection,
) -> Result<(), DuplicateConsolidationError> {
    let mut seen = BTreeSet::new();
    for selection in &selection.fields {
        if !seen.insert(selection.field) {
            return Err(DuplicateConsolidationError::InvalidMetadataSelection);
        }
        let source = tracks
            .iter()
            .find(|track| track.id == selection.track_id)
            .ok_or(DuplicateConsolidationError::ReferenceNotSelected)?;
        copy_metadata_field(target, &source.metadata, selection.field);
    }
    if seen.len() != DuplicateMetadataField::ALL.len() {
        return Err(DuplicateConsolidationError::InvalidMetadataSelection);
    }
    Ok(())
}

fn copy_metadata_field(
    target: &mut TrackMetadata,
    source: &TrackMetadata,
    field: DuplicateMetadataField,
) {
    // A name and the sort value it files under are one choice: taking a
    // copy's title with the survivor's sort title would file "An Apple"
    // under "Zebra (The)".
    match field {
        DuplicateMetadataField::Title => {
            target.title.clone_from(&source.title);
            target.title_sort.clone_from(&source.title_sort);
        }
        DuplicateMetadataField::Artist => {
            target.artist.clone_from(&source.artist);
            target.artist_sort.clone_from(&source.artist_sort);
        }
        DuplicateMetadataField::Album => {
            target.album.clone_from(&source.album);
            target.album_sort.clone_from(&source.album_sort);
        }
        DuplicateMetadataField::AlbumArtist => {
            target.album_artist.clone_from(&source.album_artist);
            target
                .album_artist_sort
                .clone_from(&source.album_artist_sort);
        }
        DuplicateMetadataField::Composer => {
            target.composer.clone_from(&source.composer);
            target.composer_sort.clone_from(&source.composer_sort);
        }
        DuplicateMetadataField::Grouping => target.grouping.clone_from(&source.grouping),
        DuplicateMetadataField::Genre => target.genre.clone_from(&source.genre),
        DuplicateMetadataField::TrackNumber => target.track_number = source.track_number,
        DuplicateMetadataField::TrackTotal => target.track_total = source.track_total,
        DuplicateMetadataField::DiscNumber => target.disc_number = source.disc_number,
        DuplicateMetadataField::DiscTotal => target.disc_total = source.disc_total,
        DuplicateMetadataField::Year => target.year = source.year,
        DuplicateMetadataField::Compilation => target.compilation = source.compilation,
        DuplicateMetadataField::Bpm => {
            target.bpm = source.bpm;
            target.bpm_set_by_user = source.bpm_set_by_user;
        }
        DuplicateMetadataField::Key => {
            target.key = source.key;
            target.key_origin = source.key_origin;
        }
        DuplicateMetadataField::Comments => target.comments.clone_from(&source.comments),
        DuplicateMetadataField::Lyrics => target.lyrics.clone_from(&source.lyrics),
    }
}

fn is_lossless_extension(track: &Track) -> bool {
    track
        .location
        .path()
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(
                extension.to_ascii_lowercase().as_str(),
                "aif" | "aiff" | "alac" | "flac" | "wav" | "wave"
            )
        })
}

fn rewrite_playlist(playlist: &Playlist, removed_ids: &[TrackId]) -> bool {
    playlist
        .entries
        .iter()
        .any(|entry| removed_ids.contains(&entry.track_id))
}

fn rewrite_playlist_entries(
    playlist: &Playlist,
    survivor_id: TrackId,
    removed_ids: &[TrackId],
) -> Playlist {
    let mut entries = playlist.entries.clone();
    entries.sort_by_key(|entry| entry.position);
    // A playlist holds each track once: the survivor keeps the first of
    // the places it and the removed copies held, wherever they stood.
    let mut rewritten = Vec::with_capacity(entries.len());
    let mut survivor_placed = false;
    for entry in entries {
        let track_id = if removed_ids.contains(&entry.track_id) {
            survivor_id
        } else {
            entry.track_id
        };
        if track_id == survivor_id {
            if survivor_placed {
                continue;
            }
            survivor_placed = true;
        }
        rewritten.push(PlaylistEntry {
            playlist_id: playlist.id,
            track_id,
            position: rewritten.len() as u32,
        });
    }
    Playlist {
        entries: rewritten,
        ..playlist.clone()
    }
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, UNIX_EPOCH};

    use crate::{Bpm, PlayStatistics, PlaylistId, Rating, TrackLocation, TrackRelativePath};

    use super::*;

    #[test]
    fn default_choices_keep_the_best_audio_and_the_highest_rating() {
        let mut first = track(1, "Artist", "Title", "Album", 200);
        first.metadata.bitrate_kbps = Some(192);
        first.rating = Rating::new(3).expect("rating");
        let mut best_audio = track(2, "Artist", "Title", "Album", 200);
        best_audio.metadata.bitrate_kbps = Some(320);
        let mut best_rated = track(3, "Artist", "Title", "Album", 200);
        best_rated.metadata.bitrate_kbps = Some(320);
        best_rated.rating = Rating::new(5).expect("rating");
        let tracks = [first, best_audio, best_rated];

        let choices = default_duplicate_merge_choices(&tracks).expect("choices");

        assert_eq!(choices.audio_track_id, track_id(2));
        assert_eq!(choices.rating_track_id, track_id(3));
        assert_eq!(
            Some(choices.metadata),
            default_duplicate_metadata_selection(&tracks)
        );
        assert_eq!(default_duplicate_merge_choices(&[]), None);
    }

    #[test]
    fn a_verified_merge_holds_while_only_listening_statistics_change() {
        let tracks = vec![
            track(1, "Artist", "Title", "Album", 200),
            track(2, "Artist", "Title", "Album", 200),
        ];
        let merge = VerifiedDuplicateMerge::new(tracks.clone()).expect("merge");

        let mut played = tracks.clone();
        played[0].statistics.play_count = 7;
        assert!(merge.still_matches(&played));

        let mut retagged = tracks.clone();
        retagged[1].metadata.album = Some("Other".to_owned());
        assert!(!merge.still_matches(&retagged));

        let mut rerated = tracks.clone();
        rerated[0].rating = Rating::new(4).expect("rating");
        assert!(!merge.still_matches(&rerated));

        let mut replaced = tracks.clone();
        replaced[1].file_size_bytes = Some(1);
        assert!(!merge.still_matches(&replaced));

        assert!(!merge.still_matches(&tracks[..1]));
        assert_eq!(VerifiedDuplicateMerge::new(tracks[..1].to_vec()), None);
    }

    #[test]
    fn a_verified_merge_requests_its_choices_with_the_given_artwork() {
        let tracks = vec![
            track(1, "Artist", "Title", "Album", 200),
            track(2, "Artist", "Title", "Album", 200),
        ];
        let merge = VerifiedDuplicateMerge::new(tracks).expect("merge");

        let request = merge.request(track_id(2));

        assert_eq!(request.track_ids, vec![track_id(1), track_id(2)]);
        assert_eq!(request.audio_track_id, merge.choices.audio_track_id);
        assert_eq!(request.metadata, merge.choices.metadata);
        assert_eq!(request.rating_track_id, merge.choices.rating_track_id);
        assert_eq!(request.artwork_track_id, track_id(2));
    }

    #[test]
    fn artwork_preset_prefers_more_pixels_then_more_encoded_bytes() {
        assert_eq!(
            highest_quality_duplicate_artwork(&[
                Some(DuplicateArtworkQuality {
                    pixels: 1_000,
                    encoded_bytes: 5_000,
                }),
                Some(DuplicateArtworkQuality {
                    pixels: 2_000,
                    encoded_bytes: 1_000,
                }),
                Some(DuplicateArtworkQuality {
                    pixels: 2_000,
                    encoded_bytes: 2_000,
                }),
            ]),
            Some(2)
        );
    }

    #[test]
    fn artwork_preset_ignores_tracks_without_artwork() {
        assert_eq!(
            highest_quality_duplicate_artwork(&[
                None,
                Some(DuplicateArtworkQuality {
                    pixels: 1_000,
                    encoded_bytes: 5_000,
                }),
            ]),
            Some(1)
        );
        assert_eq!(highest_quality_duplicate_artwork(&[None, None]), None);
    }

    #[test]
    fn loose_groups_fold_case_diacritics_and_whitespace() {
        let tracks = vec![
            track(1, "Beyoncé", " Déjà   Vu ", "Album", 200),
            track(2, "BEYONCE", "deja vu", "Other", 240),
            track(3, "Beyoncé", "Halo", "Album", 200),
        ];

        assert_eq!(
            group_ids(&tracks, DuplicateMatchMode::Loose),
            vec![vec![track_id(1), track_id(2)]]
        );
    }

    #[test]
    fn strict_groups_require_album_and_near_duration() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "artist", "song", "album", 202),
            track(3, "artist", "song", "album", 204),
            track(4, "artist", "song", "other", 201),
        ];

        assert_eq!(
            group_ids(&tracks, DuplicateMatchMode::Strict),
            vec![vec![track_id(1), track_id(2)]]
        );
    }

    #[test]
    fn loose_groups_reject_widely_different_durations() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Album", 260),
            track(3, "Artist", "Song", "Album", 500),
        ];

        assert_eq!(
            group_ids(&tracks, DuplicateMatchMode::Loose),
            vec![vec![track_id(1), track_id(2)]]
        );
    }

    #[test]
    fn loose_groups_reject_live_marker_mismatches() {
        let tracks = vec![
            track(1, "Artist", "Song", "Studio Album", 200),
            track(2, "Artist", "Song (Live)", "Live Album", 202),
        ];

        assert!(group_ids(&tracks, DuplicateMatchMode::Loose).is_empty());
    }

    #[test]
    fn analyzed_tracks_with_clearly_different_acoustics_do_not_group() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Album", 201),
        ];
        let acoustics = BTreeMap::from([
            (track_id(1), acoustics(-14.0, -9.0, 0.8, [0.50, 0.35, 0.15])),
            (track_id(2), acoustics(-5.0, -1.0, 3.4, [0.10, 0.30, 0.60])),
        ]);

        assert!(
            duplicate_groups(
                &tracks,
                DuplicateMatchMode::Loose,
                &acoustics,
                &mut NoRecordingEvidence
            )
            .is_empty()
        );
    }

    /// Recording evidence from a fixed table of pair verdicts, counting the
    /// questions it was asked.
    struct KnownRecordings {
        verdicts: BTreeMap<(TrackId, TrackId), RecordingIdentity>,
        questions: Vec<(TrackId, TrackId)>,
    }

    impl KnownRecordings {
        fn new(verdicts: &[(i64, i64, RecordingIdentity)]) -> Self {
            Self {
                verdicts: verdicts
                    .iter()
                    .map(|(left, right, identity)| ((track_id(*left), track_id(*right)), *identity))
                    .collect(),
                questions: Vec::new(),
            }
        }
    }

    impl RecordingEvidence for KnownRecordings {
        fn identity(&mut self, left: TrackId, right: TrackId) -> RecordingIdentity {
            self.questions.push((left, right));
            self.verdicts
                .get(&(left, right))
                .copied()
                .unwrap_or(RecordingIdentity::NotEstablished)
        }
    }

    fn groups_with(tracks: &[Track], evidence: &mut KnownRecordings) -> Vec<DuplicateGroup> {
        duplicate_groups(
            tracks,
            DuplicateMatchMode::Loose,
            &BTreeMap::new(),
            evidence,
        )
    }

    #[test]
    fn a_pair_the_audio_shows_to_differ_is_not_suggested() {
        // Two live recordings: the tags and live markers agree, the audio
        // does not (#302).
        let tracks = vec![
            track(
                1,
                "Adele",
                "Hometown Glory",
                "Live at the Royal Albert Hall",
                257,
            ),
            track(2, "Adele", "Hometown Glory", "Live From SoHo", 237),
        ];
        let mut evidence = KnownRecordings::new(&[(1, 2, RecordingIdentity::Different)]);

        assert!(groups_with(&tracks, &mut evidence).is_empty());
    }

    #[test]
    fn missing_audio_evidence_keeps_the_suggestion_unverified() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Other", 201),
        ];
        let mut evidence = KnownRecordings::new(&[]);

        assert_eq!(
            groups_with(&tracks, &mut evidence),
            vec![DuplicateGroup {
                track_ids: vec![track_id(1), track_id(2)],
                same_recording: false,
            }]
        );
    }

    #[test]
    fn a_group_is_the_same_recording_only_when_every_pair_is() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Other", 201),
            track(3, "Artist", "Song", "Third", 202),
        ];
        let mut all_same = KnownRecordings::new(&[
            (1, 2, RecordingIdentity::Same),
            (1, 3, RecordingIdentity::Same),
            (2, 3, RecordingIdentity::Same),
        ]);
        let mut one_unknown = KnownRecordings::new(&[
            (1, 2, RecordingIdentity::Same),
            (1, 3, RecordingIdentity::Same),
        ]);

        assert!(groups_with(&tracks, &mut all_same)[0].same_recording);
        let groups = groups_with(&tracks, &mut one_unknown);
        assert_eq!(groups[0].track_ids.len(), 3);
        assert!(!groups[0].same_recording);
    }

    #[test]
    fn a_genuine_pair_does_not_pull_in_a_different_version() {
        // 1 and 2 are one recording; 3 is another version that 2 alone
        // cannot rule out, but 1 can.
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Other", 201),
            track(3, "Artist", "Song", "Remix Album", 210),
        ];
        let mut evidence = KnownRecordings::new(&[
            (1, 2, RecordingIdentity::Same),
            (1, 3, RecordingIdentity::Different),
        ]);

        assert_eq!(
            groups_with(&tracks, &mut evidence),
            vec![DuplicateGroup {
                track_ids: vec![track_id(1), track_id(2)],
                same_recording: true,
            }]
        );
    }

    #[test]
    fn the_audio_is_consulted_only_for_pairs_the_tags_allow_and_only_once() {
        let tracks = vec![
            track(1, "Artist", "Song", "Album", 200),
            track(2, "Artist", "Song", "Other", 201),
            track(3, "Artist", "Song", "Long", 600),
            track(4, "Artist", "Other Song", "Album", 200),
        ];
        let mut evidence = KnownRecordings::new(&[(1, 2, RecordingIdentity::Same)]);

        groups_with(&tracks, &mut evidence);

        assert_eq!(evidence.questions, vec![(track_id(1), track_id(2))]);
    }

    fn group_ids(tracks: &[Track], mode: DuplicateMatchMode) -> Vec<Vec<TrackId>> {
        duplicate_groups(tracks, mode, &BTreeMap::new(), &mut NoRecordingEvidence)
            .into_iter()
            .map(|group| group.track_ids)
            .collect()
    }

    #[test]
    fn plan_keeps_audio_identity_takes_metadata_and_aggregates_library_values() {
        let mut first = track(1, "Artist", "First", "Album", 200);
        first.rating = Rating::new(3).expect("rating");
        first.statistics = statistics(2, 1, 200, 100);
        let mut second = track(2, "Artist", "Chosen", "Album", 220);
        second.metadata.bpm = Bpm::from_whole(126);
        second.metadata.bpm_set_by_user = true;
        second.rating = Rating::new(5).expect("rating");
        second.statistics = statistics(7, 4, 300, 150);
        let first_id = first.id;
        let playlist_id = PlaylistId::new(1).expect("playlist");
        let playlists = vec![Playlist {
            id: playlist_id,
            name: "Mix".to_owned(),
            parent_folder_id: None,
            position: 0,
            // The copies are apart: the merged track must still appear once.
            entries: vec![
                entry(playlist_id, second.id, 0),
                entry(playlist_id, track_id(3), 1),
                entry(playlist_id, first.id, 2),
            ],
        }];

        let plan = plan_duplicate_consolidation(
            &[first, second],
            &playlists,
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata: DuplicateMetadataSelection::from_track(track_id(2)),
                artwork_track_id: track_id(2),
                // Deliberately pick the lower-rated track to prove the rating
                // follows the selection rather than the highest available.
                rating_track_id: first_id,
            },
            1234,
            true,
        )
        .expect("plan");

        assert_eq!(plan.survivor.id, track_id(1));
        assert_eq!(plan.survivor.metadata.title.as_deref(), Some("Chosen"));
        // A BPM the user typed stays theirs on the survivor.
        assert_eq!(plan.survivor.metadata.bpm, Bpm::from_whole(126));
        assert!(plan.survivor.metadata.bpm_set_by_user);
        assert_eq!(
            plan.survivor.metadata.duration,
            Some(Duration::from_secs(200))
        );
        assert_eq!(plan.survivor.rating, Rating::new(3).expect("rating"));
        assert_eq!(plan.survivor.statistics.play_count, 9);
        assert_eq!(plan.survivor.statistics.skip_count, 5);
        assert_eq!(
            plan.survivor.statistics.last_played_at,
            Some(UNIX_EPOCH + Duration::from_secs(300))
        );
        assert_eq!(
            plan.survivor.statistics.date_added_at,
            Some(UNIX_EPOCH + Duration::from_secs(100))
        );
        assert_eq!(plan.removed_track_ids, vec![track_id(2)]);
        assert_eq!(
            plan.rewritten_playlists[0]
                .entries
                .iter()
                .map(|entry| entry.track_id)
                .collect::<Vec<_>>(),
            vec![track_id(1), track_id(3)]
        );
    }

    #[test]
    fn a_chosen_name_brings_the_sort_value_it_files_under() {
        let mut kept_audio = track(1, "The Kinks", "The Zebra", "The Album", 200);
        kept_audio.metadata.fill_missing_generated_sort_fields();
        let mut chosen = track(2, "Ray Davies", "An Apple", "Arthur", 200);
        chosen.metadata.composer = Some("The Composer".to_owned());
        chosen.metadata.album_artist = Some("A Band".to_owned());
        chosen.metadata.fill_missing_generated_sort_fields();
        chosen.metadata.artist_sort = Some("Davies, Ray".to_owned());
        let expected = chosen.metadata.clone();

        let plan = plan_duplicate_consolidation(
            &[kept_audio, chosen],
            &[],
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata: DuplicateMetadataSelection::from_track(track_id(2)),
                artwork_track_id: track_id(1),
                rating_track_id: track_id(1),
            },
            1234,
            false,
        )
        .expect("plan");

        let merged = &plan.survivor.metadata;
        assert_eq!(
            merged.title_sort, expected.title_sort,
            "An Apple files under Apple"
        );
        assert_eq!(merged.artist_sort.as_deref(), Some("Davies, Ray"));
        assert_eq!(merged.album_sort, expected.album_sort);
        assert_eq!(merged.album_artist_sort, expected.album_artist_sort);
        assert_eq!(merged.composer_sort, expected.composer_sort);
    }

    #[test]
    fn default_metadata_selection_cherry_picks_populated_fields() {
        let mut sparse = track(1, "Artist", "Song", "Album", 200);
        sparse.metadata.year = None;
        sparse.metadata.genre = Some(" ".to_owned());
        let mut enriched = track(2, "", "Song", "Album", 200);
        enriched.metadata.year = Some(1998);
        enriched.metadata.genre = Some("Trip Hop".to_owned());
        let mut oldest = track(3, "", "Song", "Album", 200);
        oldest.metadata.year = Some(1993);

        let selection =
            default_duplicate_metadata_selection(&[sparse, enriched, oldest]).expect("selection");
        let source_for = |field| {
            selection
                .fields
                .iter()
                .find(|selection| selection.field == field)
                .map(|selection| selection.track_id)
        };

        assert_eq!(
            source_for(DuplicateMetadataField::Artist),
            Some(track_id(1))
        );
        assert_eq!(source_for(DuplicateMetadataField::Year), Some(track_id(3)));
        assert_eq!(source_for(DuplicateMetadataField::Genre), Some(track_id(2)));
    }

    #[test]
    fn default_metadata_selection_prefers_non_compilation_values() {
        let mut compilation = track(1, "Artist", "Song", "Compilation", 200);
        compilation.metadata.compilation = Some(true);
        let mut original_release = track(2, "Artist", "Song", "Album", 200);
        original_release.metadata.compilation = None;

        let selection = default_duplicate_metadata_selection(&[compilation, original_release])
            .expect("selection");

        assert_eq!(
            selection
                .fields
                .iter()
                .find(|selection| selection.field == DuplicateMetadataField::Compilation)
                .map(|selection| selection.track_id),
            Some(track_id(2))
        );
    }

    #[test]
    fn highest_quality_audio_prefers_bitrate_then_lossless_on_tie() {
        let mut lower_lossless = track(1, "Artist", "Song", "Album", 200);
        lower_lossless.metadata.bitrate_kbps = Some(256);
        let mut higher_lossy = track(2, "Artist", "Song", "Album", 200);
        higher_lossy.location =
            TrackLocation::available(TrackRelativePath::new("2.mp3").expect("path"));
        higher_lossy.metadata.bitrate_kbps = Some(320);

        assert_eq!(
            highest_quality_duplicate_audio_track_ids(&[lower_lossless.clone(), higher_lossy]),
            vec![track_id(2)],
            "bitrate wins before the codec tiebreaker"
        );

        let mut tied_lossy = track(2, "Artist", "Song", "Album", 200);
        tied_lossy.location =
            TrackLocation::available(TrackRelativePath::new("2.mp3").expect("path"));
        tied_lossy.metadata.bitrate_kbps = Some(256);
        assert_eq!(
            highest_quality_duplicate_audio_track_ids(&[lower_lossless, tied_lossy]),
            vec![track_id(1)],
            "a lossless file wins when bitrate ties"
        );
    }

    #[test]
    fn planner_accepts_a_user_overridden_lower_quality_audio_survivor() {
        let mut lower = track(1, "Artist", "Song", "Album", 200);
        lower.metadata.bitrate_kbps = Some(128);
        let mut higher = track(2, "Artist", "Song", "Album", 200);
        higher.metadata.bitrate_kbps = Some(320);

        // The highest-quality file is only a preselection, not a hard rule, so
        // deliberately keeping the lower-bitrate file is allowed.
        let plan = plan_duplicate_consolidation(
            &[lower, higher],
            &[],
            &DuplicateConsolidationRequest {
                track_ids: vec![track_id(1), track_id(2)],
                audio_track_id: track_id(1),
                metadata: DuplicateMetadataSelection::from_track(track_id(1)),
                artwork_track_id: track_id(1),
                rating_track_id: track_id(1),
            },
            100,
            false,
        )
        .expect("lower-quality audio survivor is permitted");
        assert_eq!(plan.survivor.id, track_id(1));
        assert_eq!(plan.removed_track_ids, vec![track_id(2)]);
    }

    fn track(id: i64, artist: &str, title: &str, album: &str, duration: u64) -> Track {
        Track {
            id: track_id(id),
            location: TrackLocation::available(
                TrackRelativePath::new(format!("{id}.flac")).expect("path"),
            ),
            metadata: TrackMetadata {
                artist: Some(artist.to_owned()),
                title: Some(title.to_owned()),
                album: Some(album.to_owned()),
                duration: Some(Duration::from_secs(duration)),
                ..TrackMetadata::default()
            },
            rating: Rating::unrated(),
            statistics: PlayStatistics::default(),
            file_size_bytes: Some(100),
            has_embedded_artwork: Some(false),
            file_modified_at: None,
        }
    }

    fn statistics(
        play_count: u64,
        skip_count: u64,
        last_played: u64,
        date_added: u64,
    ) -> PlayStatistics {
        PlayStatistics {
            play_count,
            skip_count,
            last_played_at: Some(UNIX_EPOCH + Duration::from_secs(last_played)),
            last_skipped_at: None,
            date_added_at: Some(UNIX_EPOCH + Duration::from_secs(date_added)),
        }
    }

    fn acoustics(
        integrated_lufs: f32,
        short_term_lufs_max: f32,
        onset_rate_hz: f32,
        band_ratios: [f32; 3],
    ) -> AcousticFeatures {
        AcousticFeatures {
            integrated_lufs,
            short_term_lufs_max,
            loudness_range_lu: 4.0,
            onset_rate_hz,
            low_band_ratio: band_ratios[0],
            mid_band_ratio: band_ratios[1],
            high_band_ratio: band_ratios[2],
            low_band_variation: 0.4,
            tonalness: 0.5,
        }
    }

    fn entry(playlist_id: PlaylistId, track_id: TrackId, position: u32) -> PlaylistEntry {
        PlaylistEntry {
            playlist_id,
            track_id,
            position,
        }
    }

    fn track_id(value: i64) -> TrackId {
        TrackId::new(value).expect("track")
    }
}
