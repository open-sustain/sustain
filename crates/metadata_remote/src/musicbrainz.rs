// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! MusicBrainz Web Service v2 client.
//!
//! Two operations matter for Sustain today:
//!
//! 1. **Recording search**: given the local tag values we already have
//!    (artist, title, optional album and duration), find the best
//!    MusicBrainz recording that matches them. The recording carries
//!    the canonical metadata fields and points at the releases it
//!    appears on, which is what we need to look up cover art.
//! 2. **Recording lookup by ID**: confirm an AcoustID match by
//!    resolving its recording MBID into a full record. AcoustID's
//!    lookup returns recording IDs only; the rest of the metadata
//!    lives on MusicBrainz proper.
//!
//! The client deliberately does *not* expose every field MusicBrainz
//! returns. It exposes the small structured view that Sustain's
//! [`crate::service::RemoteMetadataService`] can collapse into either
//! [`crate::service::TrackMatch`] or `FetchedArtwork`. Adding fields
//! later is cheap; surfacing the whole MusicBrainz schema upfront
//! would couple the consumer to provider details it should not care
//! about.

use std::sync::Arc;

use serde::Deserialize;

use crate::client::HttpClient;
use crate::error::RemoteResult;
use crate::http::url_encode;
use crate::mbid::is_well_formed;

const SEARCH_BASE: &str = "https://musicbrainz.org/ws/2/recording/";
const LOOKUP_BASE: &str = "https://musicbrainz.org/ws/2/recording";
const DISC_LOOKUP_BASE: &str = "https://musicbrainz.org/ws/2/discid";
/// Includes for the disc-id lookup. `recordings` brings the per-track
/// titles and recording MBIDs; `artist-credits` the release and per-track
/// artists; `release-groups` the release-group MBID and the
/// `secondary-types` that mark a compilation; `labels` the label name.
const DISC_LOOKUP_INCLUDES: &str = "recordings+artist-credits+release-groups+labels";
const RELEASE_GROUP_LOOKUP_BASE: &str = "https://musicbrainz.org/ws/2/release-group";
const ARTIST_LOOKUP_BASE: &str = "https://musicbrainz.org/ws/2/artist";
/// Includes for the lookup-by-id endpoint. Release-level details
/// (and the release-group MBID) drive Cover Art Archive fallbacks;
/// artist credit comes along for the read-back display and exposes
/// the primary artist MBID for the genre-fallback walk; `genres`
/// carries community-voted genre tags so tag enrichment can surface
/// a primary genre. Search hits don't carry these extra fields, so
/// the composed service is expected to promote any text-search winner
/// to a follow-up lookup before handing the match to callers.
const LOOKUP_INCLUDES: &str = "releases+release-groups+artist-credits+media+genres";
/// Includes for the genre-fallback lookups on release-group and
/// artist endpoints. Sustain only consumes the curated `genres`
/// array; broader user-applied tags are intentionally left alone to
/// avoid pulling moods, years, and country tags into the genre field.
const GENRE_LOOKUP_INCLUDES: &str = "genres";

/// Number of recordings the search asks MusicBrainz for. Five is
/// enough to disambiguate noisy tags (a track with no album, several
/// remasters, etc.) without making MusicBrainz do more index work
/// than necessary.
const SEARCH_LIMIT: u32 = 5;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RecordingSearchTerms {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    /// Track duration in milliseconds, when known. MusicBrainz scores
    /// matches partly by length, so passing this consistently
    /// improves precision on common-title tracks.
    pub duration_ms: Option<u64>,
}

impl RecordingSearchTerms {
    pub fn is_usable(&self) -> bool {
        self.title.as_deref().is_some_and(is_non_blank)
            || self.artist.as_deref().is_some_and(is_non_blank)
    }
}

/// A recording-level view of one search hit. Only the fields Sustain
/// can act on are surfaced; everything else (relations, ISRC, work
/// credits) is dropped at parse time.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingMatch {
    pub recording_mbid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub duration_ms: Option<u64>,
    /// Server-assigned score, 0..=100. Used to drop low-confidence
    /// matches before they reach the UI.
    pub score: u8,
    /// Year extracted from the recording's `first-release-date`. This
    /// is the song's year (when it was first recorded/released), not
    /// any one release's year — compilations, reissues, and remasters
    /// all share the same `first_release_year`. `None` when MB has
    /// no first-release-date for the recording, or when only the
    /// search endpoint was hit (search hits omit this field).
    pub first_release_year: Option<i32>,
    /// Community-voted genre tags, sorted by vote count descending.
    /// Empty for recordings with no curated genre tags or when only
    /// the search endpoint was hit (search hits don't include genres).
    pub genres: Vec<GenreVote>,
    /// Releases the recording appears on, in MusicBrainz order. Empty
    /// for recordings that exist in the database but are not
    /// associated with any release — those are skipped by Cover Art
    /// Archive lookup.
    pub releases: Vec<RecordingRelease>,
    /// MusicBrainz MBID of the recording's primary (first) artist
    /// credit. Used by the service layer as a genre-fallback source —
    /// most MusicBrainz genre tags live at the artist level, not the
    /// recording level. `None` when only the search endpoint was hit
    /// (search hits omit artist IDs) or when the artist-credit was
    /// empty or malformed.
    pub primary_artist_mbid: Option<String>,
}

/// One community-voted genre tag attached to a recording.
/// `vote_count` is MusicBrainz's tally of users who agreed the tag
/// applies; the same tag can appear with different counts on
/// different recordings.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenreVote {
    pub name: String,
    pub vote_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecordingRelease {
    pub release_mbid: String,
    pub release_group_mbid: Option<String>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
}

/// One release whose attached disc matches a probed MusicBrainz Disc ID,
/// together with the ordered audio-track mapping of the matching medium.
///
/// Unlike [`RecordingRelease`] (a recording's view of the releases it
/// appears on) this is a release's view of one *medium* — the unit a
/// physical CD maps onto. The CD import flow renders these as selectable
/// candidates and, once one is chosen, maps its [`DiscTrack`] list onto
/// the physical audio tracks reported by the optical TOC.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscRelease {
    pub release_mbid: String,
    pub release_group_mbid: Option<String>,
    pub title: Option<String>,
    /// Flattened release artist credit (e.g. `"A & B"`).
    pub artist_credit: Option<String>,
    /// Year parsed from the release date, when present.
    pub year: Option<i32>,
    /// Raw release date string as MusicBrainz returned it (ISO-ish).
    pub date: Option<String>,
    pub country: Option<String>,
    /// First label name attached to the release, when present.
    pub label: Option<String>,
    /// Medium format, e.g. `"CD"`.
    pub format: Option<String>,
    /// 1-based position of the matching medium within the release.
    pub disc_number: Option<u32>,
    /// Total number of media in the release when the lookup response
    /// carried more than one; a disc-id lookup commonly returns only the
    /// matching medium, in which case this is `None`.
    pub disc_total: Option<u32>,
    /// Number of audio tracks on the matching medium. Always equal to the
    /// probed TOC's audio-track count — that equality is the compatibility
    /// guard [`MusicBrainzClient::lookup_disc`] enforces.
    pub track_total: u32,
    /// True only when MusicBrainz explicitly types the release-group as a
    /// compilation. Never inferred from artist strings.
    pub is_compilation: bool,
    /// Ordered audio tracks of the matching medium.
    pub tracks: Vec<DiscTrack>,
}

/// One audio track of a [`DiscRelease`]'s matching medium.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiscTrack {
    /// 1-based physical position on the medium.
    pub position: u32,
    pub title: Option<String>,
    /// Flattened per-track artist credit, when the track carries one.
    pub artist_credit: Option<String>,
    pub duration_ms: Option<u64>,
    pub recording_mbid: Option<String>,
}

#[derive(Clone)]
pub struct MusicBrainzClient {
    http: Arc<HttpClient>,
}

impl MusicBrainzClient {
    pub fn new(http: Arc<HttpClient>) -> Self {
        Self { http }
    }

    /// Search MusicBrainz for recordings matching the given terms.
    /// Returns an empty vector if the terms carry no usable text;
    /// MusicBrainz would otherwise reject the query as malformed.
    ///
    /// Terms are normalised before they reach the Lucene query: the
    /// artist string is collapsed to its primary segment (so a tag
    /// like `"A, B, C"` becomes `"A"`), and parenthesised feature
    /// credits are stripped from the title. This is the only path
    /// the service layer uses for text identification, and the
    /// normalisation policy is identical regardless of caller.
    ///
    /// One resilience retry: if the fully-constrained query (with an
    /// album) returns no hits, the album clause is dropped and the
    /// search is reissued. Album titles drift the most across editions
    /// and reissues, and dropping the constraint costs at most one
    /// additional MusicBrainz request when the first attempt failed.
    pub fn search_recordings(
        &self,
        terms: &RecordingSearchTerms,
    ) -> RemoteResult<Vec<RecordingMatch>> {
        if !terms.is_usable() {
            return Ok(Vec::new());
        }
        let normalised = normalize_search_terms(terms);
        if !normalised.is_usable() {
            return Ok(Vec::new());
        }
        let mut hits = self.run_recording_search(&normalised)?;
        if hits.is_empty() && normalised.album.is_some() {
            let no_album = RecordingSearchTerms {
                album: None,
                ..normalised.clone()
            };
            hits = self.run_recording_search(&no_album)?;
        }
        Ok(hits)
    }

    fn run_recording_search(
        &self,
        terms: &RecordingSearchTerms,
    ) -> RemoteResult<Vec<RecordingMatch>> {
        let query = build_search_query(terms);
        // The Lucene grammar is delivered through the percent-encoded query:
        // MusicBrainz decodes the value before parsing, so quoted strings,
        // colons, and bracketed ranges all reach the parser intact.
        let url = format!(
            "{SEARCH_BASE}?query={query}&fmt=json&limit={SEARCH_LIMIT}",
            query = url_encode(&query),
        );
        let payload: SearchPayload = self.http.get_json(&url)?;
        Ok(payload
            .recordings
            .into_iter()
            .filter_map(into_recording_match)
            .collect())
    }

    /// Look up a recording by its MusicBrainz ID. This is the
    /// preferred path once an MBID is known (e.g. after an AcoustID
    /// fingerprint match) because the API returns the canonical
    /// record rather than a ranked search result.
    pub fn lookup_recording(&self, recording_mbid: &str) -> RemoteResult<Option<RecordingMatch>> {
        if !is_well_formed(recording_mbid) {
            return Ok(None);
        }
        let url = format!(
            "{LOOKUP_BASE}/{recording_mbid}?inc={includes}&fmt=json",
            includes = LOOKUP_INCLUDES,
        );
        let recording: RawRecording = match self.http.get_json(&url) {
            Ok(value) => value,
            Err(crate::error::RemoteError::BadStatus(404)) => return Ok(None),
            Err(error) => return Err(error),
        };
        // The lookup endpoint omits the `score` field — it returns
        // exactly one record. Synthesise a maximum score so the
        // caller's confidence checks pass uniformly.
        let recording = RawRecording {
            score: Some(100),
            ..recording
        };
        Ok(into_recording_match(recording))
    }

    /// Fetch the community-voted genre tags attached to a release-
    /// group. Used by the service layer as the first fallback when a
    /// recording itself carries no curated genres. Returns an empty
    /// vector for 404 responses (recordings can reference release-
    /// groups that have since been merged or removed); other errors
    /// propagate so the caller can decide whether to retry.
    pub fn lookup_release_group_genres(
        &self,
        release_group_mbid: &str,
    ) -> RemoteResult<Vec<GenreVote>> {
        self.lookup_entity_genres(RELEASE_GROUP_LOOKUP_BASE, release_group_mbid)
    }

    /// Fetch the community-voted genre tags attached to an artist.
    /// Used as the final fallback when neither the recording nor any
    /// of its release-groups carry curated genres. Same 404 handling
    /// as [`Self::lookup_release_group_genres`].
    pub fn lookup_artist_genres(&self, artist_mbid: &str) -> RemoteResult<Vec<GenreVote>> {
        self.lookup_entity_genres(ARTIST_LOOKUP_BASE, artist_mbid)
    }

    /// Look up the releases attached to a probed MusicBrainz Disc ID.
    ///
    /// This is a single [Disc ID lookup][discid] — not N independent title
    /// searches — so the result is grounded in MusicBrainz's own TOC
    /// attachment rather than fuzzy text matching. `audio_track_count` is
    /// the number of audio tracks the optical TOC reported; releases whose
    /// matching medium has a different track count are rejected, because a
    /// genuine disc-id match (the id is derived from the exact TOC) always
    /// agrees and a disagreement means the mapping cannot be trusted to tag
    /// the rip.
    ///
    /// Returns an empty vector for a malformed disc id, a 404 (the id is
    /// unknown to MusicBrainz), or a response with no compatible release —
    /// all normal "no match" outcomes. Transport and rate-limit errors
    /// propagate so the caller can report them once.
    ///
    /// [discid]: https://musicbrainz.org/doc/MusicBrainz_API#discid
    pub fn lookup_disc(
        &self,
        disc_id: &str,
        audio_track_count: u32,
    ) -> RemoteResult<Vec<DiscRelease>> {
        if !disc_id_looks_valid(disc_id) {
            return Ok(Vec::new());
        }
        let url = format!(
            "{DISC_LOOKUP_BASE}/{id}?inc={DISC_LOOKUP_INCLUDES}&fmt=json",
            id = url_encode(disc_id),
        );
        let payload: DiscPayload = match self.http.get_json(&url) {
            Ok(value) => value,
            Err(crate::error::RemoteError::BadStatus(404)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(parse_disc_releases(payload, disc_id, audio_track_count))
    }

    fn lookup_entity_genres(&self, base_url: &str, mbid: &str) -> RemoteResult<Vec<GenreVote>> {
        if !is_well_formed(mbid) {
            return Ok(Vec::new());
        }
        let url = format!("{base_url}/{mbid}?inc={GENRE_LOOKUP_INCLUDES}&fmt=json");
        let payload: GenrePayload = match self.http.get_json(&url) {
            Ok(value) => value,
            Err(crate::error::RemoteError::BadStatus(404)) => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        Ok(sort_genres(payload.genres.unwrap_or_default()))
    }
}

/// Apply Sustain's normalisation policy to the raw search terms
/// before they reach Lucene. Comma-and-feat-style multi-artist
/// strings collapse to their primary artist; parenthesised feature
/// credits drop out of titles. The album field is preserved as-is —
/// any retry without it is handled by [`MusicBrainzClient::search_recordings`].
fn normalize_search_terms(terms: &RecordingSearchTerms) -> RecordingSearchTerms {
    RecordingSearchTerms {
        artist: terms.artist.as_deref().and_then(primary_artist),
        title: terms
            .title
            .as_deref()
            .map(clean_recording_title)
            .filter(|value| !value.is_empty()),
        album: terms.album.as_deref().and_then(|value| {
            let trimmed = value.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.to_owned())
            }
        }),
        duration_ms: terms.duration_ms,
    }
}

/// Reduce a multi-artist tag string to its primary (first) artist.
///
/// Music files commonly store collaborative credits as a single
/// flat string — `"A, B"`, `"A feat. B"`, `"A & B"`, `"A x B"` —
/// because most tag editors do not support structured multi-artist
/// values. MusicBrainz's search, by contrast, expects a literal
/// artist name; a Lucene phrase like `artist:"A, B"` matches zero
/// MusicBrainz artists because no MB entity has that exact name.
/// Picking the first segment is the closest thing to a "primary
/// artist" we can recover without changing the library's storage
/// model. Returns `None` if the input has no usable text.
pub(crate) fn primary_artist(input: &str) -> Option<String> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }
    let primary = split_primary_artist(trimmed);
    let cleaned = primary.trim();
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned.to_owned())
    }
}

/// Return the slice of `input` up to (but not including) the first
/// multi-artist separator. The padding spaces in the word-shaped
/// separators are deliberate — they prevent matching inside a single
/// word (e.g. the `x` in "Charli xcx" or the `&` in an HTML-escaped
/// string).
fn split_primary_artist(input: &str) -> &str {
    const WORD_SEPS: &[&str] = &[
        " feat. ",
        " feat ",
        " featuring ",
        " ft. ",
        " ft ",
        " vs. ",
        " vs ",
        " versus ",
        " x ",
        " / ",
        " & ",
        " + ",
    ];
    let lower = input.to_ascii_lowercase();
    let comma_pos = lower.find(',');
    let word_pos = WORD_SEPS.iter().filter_map(|sep| lower.find(sep)).min();
    let earliest = match (comma_pos, word_pos) {
        (Some(c), Some(w)) => Some(c.min(w)),
        (Some(c), None) => Some(c),
        (None, Some(w)) => Some(w),
        (None, None) => None,
    };
    match earliest {
        Some(pos) => &input[..pos],
        None => input,
    }
}

/// Strip parenthesised or bracketed feature credits from a recording
/// title. `"Flowerz (feat. Roland Clark)"` becomes `"Flowerz"`, which
/// matches the canonical title MusicBrainz stores. Other parenthetical
/// segments (`"(Live)"`, `"(Remastered 2011)"`, `"(Original Mix)"`)
/// are kept verbatim: they distinguish meaningfully different
/// recordings, and dropping them would hurt precision more than the
/// feat-strip helps.
pub(crate) fn clean_recording_title(input: &str) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut output: Vec<char> = Vec::with_capacity(chars.len());
    let mut i = 0;
    while i < chars.len() {
        let ch = chars[i];
        let close = match ch {
            '(' => Some(')'),
            '[' => Some(']'),
            _ => None,
        };
        if let Some(closer) = close
            && let Some(end_offset) = chars[i + 1..].iter().position(|c| *c == closer)
        {
            let inner: String = chars[i + 1..i + 1 + end_offset].iter().collect();
            if is_featured_credit(&inner) {
                i += end_offset + 2;
                while output.last() == Some(&' ') {
                    output.pop();
                }
                continue;
            }
        }
        output.push(ch);
        i += 1;
    }
    let result: String = output.into_iter().collect();
    result.trim().to_owned()
}

fn is_featured_credit(inner: &str) -> bool {
    let lower = inner.trim().to_ascii_lowercase();
    const PREFIXES: &[&str] = &["feat.", "feat ", "featuring ", "ft.", "ft ", "with "];
    if matches!(lower.as_str(), "feat" | "ft" | "featuring") {
        return true;
    }
    PREFIXES.iter().any(|prefix| lower.starts_with(prefix))
}

/// Construct the Lucene query MusicBrainz expects. Each field is
/// quoted and Lucene-escaped; missing fields are omitted entirely so
/// they don't constrain the match. Duration is expressed as a
/// loosely-bracketed range (±5s) because tag-derived durations
/// frequently drift from MusicBrainz's by a few hundred milliseconds.
fn build_search_query(terms: &RecordingSearchTerms) -> String {
    let mut clauses: Vec<String> = Vec::new();
    if let Some(title) = terms.title.as_deref().filter(|value| is_non_blank(value)) {
        clauses.push(format!("recording:\"{}\"", lucene_escape(title)));
    }
    if let Some(artist) = terms.artist.as_deref().filter(|value| is_non_blank(value)) {
        clauses.push(format!("artist:\"{}\"", lucene_escape(artist)));
    }
    if let Some(album) = terms.album.as_deref().filter(|value| is_non_blank(value)) {
        clauses.push(format!("release:\"{}\"", lucene_escape(album)));
    }
    if let Some(duration_ms) = terms.duration_ms {
        let lower = duration_ms.saturating_sub(5_000);
        let upper = duration_ms.saturating_add(5_000);
        clauses.push(format!("dur:[{lower} TO {upper}]"));
    }
    clauses.join(" AND ")
}

fn lucene_escape(value: &str) -> String {
    // MusicBrainz's Lucene-based query parser treats these characters
    // as syntax; escape them with a backslash so user-supplied strings
    // (which routinely contain `+`, `-`, `&`, `:`, etc.) are treated as
    // text. We do not escape double quotes — the caller wraps the
    // entire value in quotes already, and escaping the quote inside a
    // quoted value would terminate the field early. If the input
    // contains an actual `"`, we drop it: there is no clean way to
    // embed a quote inside a quoted Lucene value without significantly
    // expanding the grammar we accept here.
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '"' => continue,
            '\\' | '+' | '-' | '!' | '(' | ')' | '{' | '}' | '[' | ']' | '^' | '~' | '*' | '?'
            | ':' | '/' | '&' | '|' => {
                escaped.push('\\');
                escaped.push(character);
            }
            _ => escaped.push(character),
        }
    }
    escaped
}

fn into_recording_match(raw: RawRecording) -> Option<RecordingMatch> {
    if raw.id.is_empty() {
        return None;
    }
    let releases = raw
        .releases
        .unwrap_or_default()
        .into_iter()
        .filter_map(into_recording_release)
        .collect();
    let first_release_year = raw.first_release_date.as_deref().and_then(parse_year);
    let genres = sort_genres(raw.genres.unwrap_or_default());
    let primary_artist_mbid = raw
        .artist_credit
        .as_deref()
        .and_then(|credits| credits.first())
        .and_then(|credit| credit.artist.as_ref())
        .and_then(|artist| artist.id.as_deref())
        .filter(|value| !value.trim().is_empty())
        .map(|value| value.to_owned());
    Some(RecordingMatch {
        recording_mbid: raw.id,
        title: raw.title.filter(|value| is_non_blank(value)),
        artist: raw
            .artist_credit
            .as_deref()
            .and_then(format_artist_credit)
            .filter(|value| is_non_blank(value)),
        duration_ms: raw.length,
        score: raw.score.unwrap_or(0).min(100),
        first_release_year,
        genres,
        releases,
        primary_artist_mbid,
    })
}

/// Convert MB's raw genre list into the public, sorted-by-vote
/// representation. Drops entries with blank names and tags with zero
/// votes (MB sometimes returns a tag with `count: 0` when the genre
/// exists on the recording but no one has voted for it — these are
/// indistinguishable from spurious additions and shouldn't drive
/// automatic tag enrichment).
fn sort_genres(raw: Vec<RawGenre>) -> Vec<GenreVote> {
    let mut votes: Vec<GenreVote> = raw
        .into_iter()
        .filter_map(|entry| {
            let name = entry.name.unwrap_or_default();
            let trimmed = name.trim();
            if trimmed.is_empty() {
                return None;
            }
            let vote_count = entry.count.unwrap_or(0);
            if vote_count == 0 {
                return None;
            }
            Some(GenreVote {
                name: trimmed.to_owned(),
                vote_count,
            })
        })
        .collect();
    votes.sort_by_key(|vote| std::cmp::Reverse(vote.vote_count));
    votes
}

/// A MusicBrainz disc id is libdiscid's 28-character base64url-ish string.
/// Validating it before assembling a URL keeps malformed ids off the wire
/// and out of the shared rate-limit budget.
fn disc_id_looks_valid(disc_id: &str) -> bool {
    disc_id.len() == 28
        && disc_id.bytes().all(
            |byte| matches!(byte, b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-'),
        )
}

fn parse_disc_releases(
    payload: DiscPayload,
    disc_id: &str,
    audio_track_count: u32,
) -> Vec<DiscRelease> {
    payload
        .releases
        .unwrap_or_default()
        .into_iter()
        .filter_map(|release| into_disc_release(release, disc_id, audio_track_count))
        .collect()
}

fn into_disc_release(
    release: DiscRawRelease,
    disc_id: &str,
    audio_track_count: u32,
) -> Option<DiscRelease> {
    if release.id.is_empty() {
        return None;
    }
    let media = release.media.unwrap_or_default();
    let medium = select_disc_medium(&media, disc_id)?;
    let raw_tracks = medium.tracks.as_deref().unwrap_or_default();
    // The disc id is computed from the exact TOC, so a real match always has
    // the probed audio-track count. Reject anything else rather than risk a
    // mis-ordered tagging.
    if raw_tracks.len() as u32 != audio_track_count {
        return None;
    }
    let tracks = raw_tracks
        .iter()
        .enumerate()
        .map(|(index, track)| into_disc_track(track, index))
        .collect();
    let track_total = medium.track_count.unwrap_or(audio_track_count);
    let disc_total = (media.len() > 1).then_some(media.len() as u32);
    let release_group_mbid = release
        .release_group
        .as_ref()
        .map(|group| group.id.clone())
        .filter(|value| is_non_blank(value));
    let is_compilation = release
        .release_group
        .as_ref()
        .and_then(|group| group.secondary_types.as_ref())
        .is_some_and(|types| {
            types
                .iter()
                .any(|kind| kind.eq_ignore_ascii_case("Compilation"))
        });
    let label = release
        .label_info
        .unwrap_or_default()
        .into_iter()
        .find_map(|info| info.label.and_then(|label| label.name))
        .filter(|value| is_non_blank(value));
    Some(DiscRelease {
        release_mbid: release.id,
        release_group_mbid,
        title: release.title.filter(|value| is_non_blank(value)),
        artist_credit: release
            .artist_credit
            .as_deref()
            .and_then(format_artist_credit)
            .filter(|value| is_non_blank(value)),
        year: release.date.as_deref().and_then(parse_year),
        date: release.date.filter(|value| is_non_blank(value)),
        country: release.country.filter(|value| is_non_blank(value)),
        label,
        format: medium.format.clone().filter(|value| is_non_blank(value)),
        disc_number: medium.position,
        disc_total,
        track_total,
        is_compilation,
        tracks,
    })
}

/// Choose the medium the disc id is attached to. The disc-id endpoint
/// usually returns only the matching medium, but a release with several
/// media can carry the explicit `discs` array; prefer that exact match and
/// fall back to the sole medium when only one was returned.
fn select_disc_medium<'a>(media: &'a [DiscRawMedium], disc_id: &str) -> Option<&'a DiscRawMedium> {
    if let Some(medium) = media.iter().find(|medium| {
        medium
            .discs
            .as_deref()
            .unwrap_or_default()
            .iter()
            .any(|disc| disc.id.as_deref() == Some(disc_id))
    }) {
        return Some(medium);
    }
    if media.len() == 1 {
        return media.first();
    }
    None
}

fn into_disc_track(track: &DiscRawTrack, index: usize) -> DiscTrack {
    let recording = track.recording.as_ref();
    let title = track
        .title
        .clone()
        .filter(|value| is_non_blank(value))
        .or_else(|| {
            recording
                .and_then(|recording| recording.title.clone())
                .filter(|value| is_non_blank(value))
        });
    let artist_credit = track
        .artist_credit
        .as_deref()
        .and_then(format_artist_credit)
        .or_else(|| {
            recording
                .and_then(|recording| recording.artist_credit.as_deref())
                .and_then(format_artist_credit)
        })
        .filter(|value| is_non_blank(value));
    let duration_ms = track
        .length
        .or_else(|| recording.and_then(|recording| recording.length));
    let recording_mbid = recording
        .map(|recording| recording.id.clone())
        .filter(|value| is_non_blank(value));
    DiscTrack {
        position: track.position.unwrap_or((index + 1) as u32),
        title,
        artist_credit,
        duration_ms,
        recording_mbid,
    }
}

fn into_recording_release(raw: RawRelease) -> Option<RecordingRelease> {
    if raw.id.is_empty() {
        return None;
    }
    let (track_number, track_total, disc_number) = raw
        .media
        .as_deref()
        .map(extract_track_position)
        .unwrap_or((None, None, None));
    Some(RecordingRelease {
        release_mbid: raw.id,
        release_group_mbid: raw
            .release_group
            .as_ref()
            .map(|group| group.id.clone())
            .filter(|value| is_non_blank(value)),
        title: raw.title.filter(|value| is_non_blank(value)),
        year: raw.date.as_deref().and_then(parse_year),
        track_number,
        track_total,
        disc_number,
    })
}

fn format_artist_credit(credits: &[RawArtistCredit]) -> Option<String> {
    if credits.is_empty() {
        return None;
    }
    let mut output = String::new();
    for credit in credits {
        if let Some(name) = credit.name.as_deref() {
            output.push_str(name);
        } else if let Some(artist) = &credit.artist
            && let Some(name) = artist.name.as_deref()
        {
            output.push_str(name);
        }
        if let Some(joinphrase) = credit.joinphrase.as_deref() {
            output.push_str(joinphrase);
        }
    }
    Some(output.trim().to_owned())
}

fn extract_track_position(media: &[RawMedium]) -> (Option<u32>, Option<u32>, Option<u32>) {
    for medium in media {
        let Some(tracks) = medium.tracks.as_deref() else {
            continue;
        };
        if let Some(track) = tracks.first() {
            return (
                track.number.as_deref().and_then(parse_track_position),
                medium.track_count,
                medium.position,
            );
        }
    }
    (None, None, None)
}

fn parse_track_position(value: &str) -> Option<u32> {
    // MusicBrainz exposes the track's printed position, which can be
    // alphanumeric on vinyl ("A1", "B2"). For our purposes we only
    // recover the integer prefix when one exists.
    let digits: String = value
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .collect();
    digits.parse().ok()
}

fn parse_year(value: &str) -> Option<i32> {
    // MusicBrainz date strings are ISO-ish: "1973", "1973-05", "1973-05-12".
    // We only want the year prefix.
    value
        .split('-')
        .next()
        .and_then(|year| year.parse::<i32>().ok())
}

fn is_non_blank(value: &str) -> bool {
    !value.trim().is_empty()
}

#[derive(Deserialize)]
struct SearchPayload {
    #[serde(default)]
    recordings: Vec<RawRecording>,
}

#[derive(Deserialize)]
struct RawRecording {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(default)]
    score: Option<u8>,
    #[serde(rename = "first-release-date", default)]
    first_release_date: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Option<Vec<RawArtistCredit>>,
    #[serde(default)]
    releases: Option<Vec<RawRelease>>,
    #[serde(default)]
    genres: Option<Vec<RawGenre>>,
}

#[derive(Deserialize)]
struct RawGenre {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    count: Option<u32>,
}

#[derive(Deserialize)]
struct RawArtistCredit {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    joinphrase: Option<String>,
    #[serde(default)]
    artist: Option<RawArtist>,
}

#[derive(Deserialize)]
struct RawArtist {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct GenrePayload {
    #[serde(default)]
    genres: Option<Vec<RawGenre>>,
}

#[derive(Deserialize)]
struct RawRelease {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(rename = "release-group", default)]
    release_group: Option<RawReleaseGroup>,
    #[serde(default)]
    media: Option<Vec<RawMedium>>,
}

#[derive(Deserialize)]
struct RawReleaseGroup {
    #[serde(default)]
    id: String,
}

#[derive(Deserialize)]
struct RawMedium {
    #[serde(default)]
    position: Option<u32>,
    #[serde(rename = "track-count", default)]
    track_count: Option<u32>,
    #[serde(default)]
    tracks: Option<Vec<RawTrack>>,
}

#[derive(Deserialize)]
struct RawTrack {
    #[serde(default)]
    number: Option<String>,
}

#[derive(Deserialize)]
struct DiscPayload {
    #[serde(default)]
    releases: Option<Vec<DiscRawRelease>>,
}

#[derive(Deserialize)]
struct DiscRawRelease {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    date: Option<String>,
    #[serde(default)]
    country: Option<String>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Option<Vec<RawArtistCredit>>,
    #[serde(rename = "release-group", default)]
    release_group: Option<DiscRawReleaseGroup>,
    #[serde(rename = "label-info", default)]
    label_info: Option<Vec<RawLabelInfo>>,
    #[serde(default)]
    media: Option<Vec<DiscRawMedium>>,
}

#[derive(Deserialize)]
struct DiscRawReleaseGroup {
    #[serde(default)]
    id: String,
    #[serde(rename = "secondary-types", default)]
    secondary_types: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct RawLabelInfo {
    #[serde(default)]
    label: Option<RawLabel>,
}

#[derive(Deserialize)]
struct RawLabel {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct DiscRawMedium {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    format: Option<String>,
    #[serde(rename = "track-count", default)]
    track_count: Option<u32>,
    #[serde(default)]
    discs: Option<Vec<DiscRawDisc>>,
    #[serde(default)]
    tracks: Option<Vec<DiscRawTrack>>,
}

#[derive(Deserialize)]
struct DiscRawDisc {
    #[serde(default)]
    id: Option<String>,
}

#[derive(Deserialize)]
struct DiscRawTrack {
    #[serde(default)]
    position: Option<u32>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Option<Vec<RawArtistCredit>>,
    #[serde(default)]
    recording: Option<DiscRawRecording>,
}

#[derive(Deserialize)]
struct DiscRawRecording {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    length: Option<u64>,
    #[serde(rename = "artist-credit", default)]
    artist_credit: Option<Vec<RawArtistCredit>>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 28-character disc id from the allowed base64url-ish set, matching
    /// the `id` field of every disc-lookup fixture.
    const TEST_DISC_ID: &str = "abcdefghijklmnopqrstuvwxyz._";

    fn parse_fixture(json: &str, audio_track_count: u32) -> Vec<DiscRelease> {
        let payload: DiscPayload =
            serde_json::from_str(json).expect("fixture parses as a disc payload");
        parse_disc_releases(payload, TEST_DISC_ID, audio_track_count)
    }

    #[test]
    fn disc_id_validation_matches_libdiscid_shape() {
        assert!(disc_id_looks_valid(TEST_DISC_ID));
        assert!(!disc_id_looks_valid("too-short"));
        // A canonical UUID is the wrong shape for a disc id.
        assert!(!disc_id_looks_valid("3b3d130a-87a8-4a47-b9fb-920f2530d134"));
        assert!(!disc_id_looks_valid(&"a".repeat(27)));
        assert!(!disc_id_looks_valid(&"a".repeat(29)));
        // 28 chars but with a disallowed space/`+`.
        assert!(!disc_id_looks_valid("abcdefghijklmnopqrstuvwxyz +"));
    }

    #[test]
    fn single_compatible_release_maps_tracks_and_metadata() {
        let releases = parse_fixture(
            include_str!("../tests/fixtures/discid_single_release.json"),
            2,
        );
        assert_eq!(releases.len(), 1);
        let release = &releases[0];
        assert_eq!(release.release_mbid, "11111111-1111-1111-1111-111111111111");
        assert_eq!(
            release.release_group_mbid.as_deref(),
            Some("22222222-2222-2222-2222-222222222222")
        );
        assert_eq!(release.title.as_deref(), Some("Compatible Album"));
        assert_eq!(release.artist_credit.as_deref(), Some("The Band"));
        assert_eq!(release.year, Some(1997));
        assert_eq!(release.country.as_deref(), Some("GB"));
        assert_eq!(release.label.as_deref(), Some("Test Label"));
        assert_eq!(release.format.as_deref(), Some("CD"));
        assert_eq!(release.disc_number, Some(1));
        assert_eq!(release.disc_total, None);
        assert_eq!(release.track_total, 2);
        assert!(!release.is_compilation);
        assert_eq!(release.tracks.len(), 2);
        assert_eq!(release.tracks[0].position, 1);
        assert_eq!(release.tracks[0].title.as_deref(), Some("Opener"));
        assert_eq!(release.tracks[0].artist_credit.as_deref(), Some("The Band"));
        assert_eq!(release.tracks[0].duration_ms, Some(180_000));
        assert_eq!(
            release.tracks[0].recording_mbid.as_deref(),
            Some("cccc1111-1111-1111-1111-111111111111")
        );
        // The second track has no track-level artist credit, so the
        // recording's credit is used as the fallback.
        assert_eq!(release.tracks[1].title.as_deref(), Some("Closer"));
        assert_eq!(
            release.tracks[1].artist_credit.as_deref(),
            Some("Guest Artist")
        );
    }

    #[test]
    fn multiple_compatible_releases_are_all_returned() {
        let releases = parse_fixture(
            include_str!("../tests/fixtures/discid_multiple_releases.json"),
            2,
        );
        assert_eq!(releases.len(), 2);
        assert_eq!(releases[0].title.as_deref(), Some("Original Pressing"));
        assert!(!releases[0].is_compilation);
        assert_eq!(releases[1].title.as_deref(), Some("Greatest Hits"));
        assert!(
            releases[1].is_compilation,
            "a release-group secondary-type of Compilation must be honored"
        );
    }

    #[test]
    fn incompatible_track_mapping_is_rejected() {
        let releases = parse_fixture(
            include_str!("../tests/fixtures/discid_incompatible_rejected.json"),
            2,
        );
        assert_eq!(
            releases.len(),
            1,
            "only the 2-track release is compatible with a 2-track TOC"
        );
        assert_eq!(
            releases[0].release_mbid,
            "11111111-1111-1111-1111-111111111111"
        );
    }

    #[test]
    fn no_match_yields_no_releases() {
        let releases = parse_fixture(include_str!("../tests/fixtures/discid_no_match.json"), 2);
        assert!(releases.is_empty());
    }

    #[test]
    fn lucene_escape_protects_syntax_characters() {
        assert_eq!(lucene_escape("AC/DC"), "AC\\/DC");
        assert_eq!(lucene_escape("a+b"), "a\\+b");
        assert_eq!(lucene_escape("title: subtitle"), "title\\: subtitle");
    }

    #[test]
    fn lucene_escape_drops_inner_quotes() {
        assert_eq!(lucene_escape("She said \"hello\""), "She said hello");
    }

    #[test]
    fn parse_year_handles_partial_dates() {
        assert_eq!(parse_year("1973"), Some(1973));
        assert_eq!(parse_year("1973-05"), Some(1973));
        assert_eq!(parse_year("1973-05-12"), Some(1973));
        assert_eq!(parse_year("not a year"), None);
    }

    #[test]
    fn parse_track_position_recovers_integer_prefix() {
        assert_eq!(parse_track_position("3"), Some(3));
        assert_eq!(parse_track_position("12"), Some(12));
        assert_eq!(parse_track_position("A1"), None);
        assert_eq!(parse_track_position(""), None);
    }

    #[test]
    fn search_query_skips_blank_fields() {
        let terms = RecordingSearchTerms {
            artist: Some("  ".to_owned()),
            title: Some("Stairway".to_owned()),
            album: None,
            duration_ms: None,
        };
        assert_eq!(build_search_query(&terms), "recording:\"Stairway\"");
    }

    #[test]
    fn search_query_includes_all_fields_when_present() {
        let terms = RecordingSearchTerms {
            artist: Some("Led Zeppelin".to_owned()),
            title: Some("Stairway to Heaven".to_owned()),
            album: Some("Led Zeppelin IV".to_owned()),
            duration_ms: Some(482_000),
        };
        assert_eq!(
            build_search_query(&terms),
            "recording:\"Stairway to Heaven\" AND artist:\"Led Zeppelin\" AND release:\"Led Zeppelin IV\" AND dur:[477000 TO 487000]"
        );
    }

    #[test]
    fn unusable_terms_skip_the_network_call() {
        let http = Arc::new(HttpClient::new(crate::client::HttpClientConfig {
            user_agent: "test".to_owned(),
        }));
        let client = MusicBrainzClient::new(http);
        let empty = RecordingSearchTerms::default();
        let result = client
            .search_recordings(&empty)
            .expect("blank terms must not error");
        assert!(result.is_empty());
    }

    #[test]
    fn sort_genres_drops_zero_votes_and_orders_by_count_descending() {
        let raw = vec![
            RawGenre {
                name: Some("house".to_owned()),
                count: Some(3),
            },
            RawGenre {
                name: Some("electronica".to_owned()),
                count: Some(7),
            },
            RawGenre {
                name: Some("noise".to_owned()),
                count: Some(0),
            },
            RawGenre {
                name: Some("  ".to_owned()),
                count: Some(2),
            },
            RawGenre {
                name: Some("ambient".to_owned()),
                count: Some(3),
            },
        ];
        let sorted = sort_genres(raw);
        let names: Vec<&str> = sorted.iter().map(|vote| vote.name.as_str()).collect();
        assert_eq!(names, vec!["electronica", "house", "ambient"]);
    }

    #[test]
    fn artist_credit_falls_back_to_nested_artist_name() {
        let credits = vec![
            RawArtistCredit {
                name: None,
                joinphrase: Some(" & ".to_owned()),
                artist: Some(RawArtist {
                    id: None,
                    name: Some("Simon".to_owned()),
                }),
            },
            RawArtistCredit {
                name: Some("Garfunkel".to_owned()),
                joinphrase: None,
                artist: None,
            },
        ];
        assert_eq!(
            format_artist_credit(&credits).as_deref(),
            Some("Simon & Garfunkel")
        );
    }

    #[test]
    fn primary_artist_strips_secondary_credits() {
        assert_eq!(
            primary_artist("Armand Van Helden, Roland Clark").as_deref(),
            Some("Armand Van Helden")
        );
        assert_eq!(
            primary_artist("2nd Exit, Alfa Mist, Lester Duval").as_deref(),
            Some("2nd Exit")
        );
        assert_eq!(
            primary_artist("Simon & Garfunkel").as_deref(),
            Some("Simon")
        );
        assert_eq!(
            primary_artist("Daft Punk feat. Pharrell Williams").as_deref(),
            Some("Daft Punk")
        );
        assert_eq!(
            primary_artist("Daft Punk featuring Pharrell Williams").as_deref(),
            Some("Daft Punk")
        );
        assert_eq!(primary_artist("Diplo x Skrillex").as_deref(), Some("Diplo"));
        assert_eq!(
            primary_artist("Run-D.M.C. vs. Jason Nevins").as_deref(),
            Some("Run-D.M.C.")
        );
        assert_eq!(
            primary_artist("Pharrell Williams + Daft Punk").as_deref(),
            Some("Pharrell Williams")
        );
    }

    #[test]
    fn primary_artist_preserves_single_artist_strings() {
        assert_eq!(primary_artist("Queen").as_deref(), Some("Queen"));
        assert_eq!(primary_artist("AC/DC").as_deref(), Some("AC/DC"));
        assert_eq!(primary_artist("Run-D.M.C.").as_deref(), Some("Run-D.M.C."));
        // "Charli xcx" contains 'x' but not a " x " separator.
        assert_eq!(primary_artist("Charli xcx").as_deref(), Some("Charli xcx"));
        // "Earth, Wind & Fire" — band names containing commas exist
        // but are rare; we accept the loss because comma in tag fields
        // overwhelmingly means "multiple artists". The user can edit
        // the tag to use a non-comma separator if MB identification
        // matters for that specific track.
        assert_eq!(
            primary_artist("Earth, Wind & Fire").as_deref(),
            Some("Earth")
        );
    }

    #[test]
    fn primary_artist_handles_blank_input() {
        assert_eq!(primary_artist(""), None);
        assert_eq!(primary_artist("   "), None);
        // Leading separator produces an empty primary segment.
        assert_eq!(primary_artist(", B"), None);
    }

    #[test]
    fn clean_recording_title_strips_feat_parens() {
        assert_eq!(
            clean_recording_title("Flowerz (feat. Roland Clark)"),
            "Flowerz"
        );
        assert_eq!(
            clean_recording_title("Get Lucky (featuring Pharrell Williams)"),
            "Get Lucky"
        );
        assert_eq!(clean_recording_title("Title [feat. X]"), "Title");
        assert_eq!(clean_recording_title("Track (ft. Guest)"), "Track");
        assert_eq!(clean_recording_title("Track (with Guest)"), "Track");
    }

    #[test]
    fn clean_recording_title_preserves_version_parens() {
        assert_eq!(
            clean_recording_title("Stairway to Heaven (Live)"),
            "Stairway to Heaven (Live)"
        );
        assert_eq!(
            clean_recording_title("Money (Remastered 2011)"),
            "Money (Remastered 2011)"
        );
        assert_eq!(
            clean_recording_title("Flowerz (feat. Roland Clark) (Original Mix)"),
            "Flowerz (Original Mix)"
        );
    }

    #[test]
    fn clean_recording_title_handles_titles_without_parens() {
        assert_eq!(
            clean_recording_title("Bohemian Rhapsody"),
            "Bohemian Rhapsody"
        );
        assert_eq!(clean_recording_title(""), "");
    }

    #[test]
    fn normalize_search_terms_applies_both_helpers() {
        let raw = RecordingSearchTerms {
            artist: Some("Armand Van Helden, Roland Clark".to_owned()),
            title: Some("Flowerz (feat. Roland Clark)".to_owned()),
            album: Some("2 Future 4 U".to_owned()),
            duration_ms: Some(200_000),
        };
        let normalised = normalize_search_terms(&raw);
        assert_eq!(normalised.artist.as_deref(), Some("Armand Van Helden"));
        assert_eq!(normalised.title.as_deref(), Some("Flowerz"));
        assert_eq!(normalised.album.as_deref(), Some("2 Future 4 U"));
        assert_eq!(normalised.duration_ms, Some(200_000));
    }

    #[test]
    fn normalize_search_terms_drops_blanks() {
        let raw = RecordingSearchTerms {
            artist: Some("   ".to_owned()),
            title: Some("(feat. Solo)".to_owned()),
            album: Some(" ".to_owned()),
            duration_ms: None,
        };
        let normalised = normalize_search_terms(&raw);
        assert_eq!(normalised.artist, None);
        // Title was nothing but a feat-paren — drops to empty, which
        // the filter then strips.
        assert_eq!(normalised.title, None);
        assert_eq!(normalised.album, None);
    }

    #[test]
    fn into_recording_match_captures_primary_artist_mbid() {
        let raw = RawRecording {
            id: "rec-1".to_owned(),
            title: Some("Song".to_owned()),
            length: Some(180_000),
            score: Some(95),
            first_release_date: Some("1973".to_owned()),
            artist_credit: Some(vec![
                RawArtistCredit {
                    name: None,
                    joinphrase: Some(" feat. ".to_owned()),
                    artist: Some(RawArtist {
                        id: Some("artist-primary".to_owned()),
                        name: Some("Primary".to_owned()),
                    }),
                },
                RawArtistCredit {
                    name: Some("Guest".to_owned()),
                    joinphrase: None,
                    artist: Some(RawArtist {
                        id: Some("artist-guest".to_owned()),
                        name: Some("Guest".to_owned()),
                    }),
                },
            ]),
            releases: None,
            genres: None,
        };
        let matched = into_recording_match(raw).expect("match builds");
        assert_eq!(
            matched.primary_artist_mbid.as_deref(),
            Some("artist-primary")
        );
    }

    #[test]
    fn into_recording_match_handles_missing_artist_id() {
        let raw = RawRecording {
            id: "rec-1".to_owned(),
            title: Some("Song".to_owned()),
            length: None,
            score: Some(80),
            first_release_date: None,
            artist_credit: Some(vec![RawArtistCredit {
                name: Some("Solo".to_owned()),
                joinphrase: None,
                artist: None,
            }]),
            releases: None,
            genres: None,
        };
        let matched = into_recording_match(raw).expect("match builds");
        assert_eq!(matched.primary_artist_mbid, None);
    }
}
