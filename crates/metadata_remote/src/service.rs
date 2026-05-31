// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! High-level service trait composed from the three underlying clients.
//!
//! This is the surface the rest of the application is expected to
//! depend on. Callers do not import [`crate::musicbrainz`],
//! [`crate::cover_art_archive`], or [`crate::acoustid`] directly —
//! those modules are the building blocks; this trait is the contract.
//!
//! The composed implementation lives below ([`ComposedRemoteMetadataService`])
//! and wires the three clients together with a small set of selection
//! rules:
//!
//! * **Track identification** first asks MusicBrainz to search by
//!   the supplied tag terms. If no confident match is returned and a
//!   precomputed audio fingerprint is provided, the AcoustID client
//!   is used as a fallback. The first usable candidate wins; the
//!   service does not aggregate scores or attempt ML-style ranking.
//! * **Artwork lookup** takes the chosen [`TrackMatch`], walks its
//!   releases in MusicBrainz-provided order, and queries Cover Art
//!   Archive for each release MBID until one returns bytes. If every
//!   release misses, the release-group MBID is tried as a final
//!   fallback.
//!
//! Errors propagate; partial successes (e.g. identification works,
//! artwork lookup fails) are not silently converted into "no match".
//! Mixing those would make the failure surface ambiguous at the UI
//! layer, which is the only useful place to distinguish them.

use std::sync::Arc;

use crate::acoustid::{AcoustIdClient, AudioFingerprint};
use crate::cover_art_archive::CoverArtArchiveClient;
use crate::error::RemoteResult;
use crate::lrclib::LrcLibClient;
use crate::musicbrainz::{MusicBrainzClient, RecordingMatch, RecordingSearchTerms};

/// Minimum MusicBrainz score (0..=100) we accept from a text-based
/// recording search before we'd rather fall back to fingerprint
/// identification (when one is available). Below this threshold, the
/// match is treated as "no confident result" — the search returned
/// *something* but not something we'd risk writing to tags.
///
/// MusicBrainz scores cluster around 100 for clean tag matches, drop
/// to the 80s when one field disagrees (different release of the same
/// recording, slightly off album name, "\[Remastered\]" / "\[Live\]"
/// suffixes), and tail off below 60 once the result is only loosely
/// related. 75 catches the well-tagged cases plus the common
/// real-world drift without crossing into ambiguity — and the
/// non-destructive contract is enforced one layer up (we never
/// overwrite existing artwork or tags), so a wrong match on a track
/// that already has no cover only affects that track.
const MUSICBRAINZ_CONFIDENT_SCORE: u8 = 75;

/// Minimum AcoustID score (0.0..=1.0). The fingerprint scoring
/// behaves differently from text-based scoring: a score above 0.85
/// effectively means "yes, this is the same recording", and anything
/// below 0.5 is statistically indistinguishable from noise. We pick a
/// conservative cutoff because the AcoustID path runs when text
/// matching has already failed and the user has nothing better.
const ACOUSTID_CONFIDENT_SCORE: f32 = 0.7;

/// Sustain's high-level query for "identify this track".
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TrackQuery {
    pub artist: Option<String>,
    pub title: Option<String>,
    pub album: Option<String>,
    pub duration_ms: Option<u64>,
    /// Precomputed audio fingerprint, if available. Optional — the
    /// service falls back to text-only identification when absent and
    /// AcoustID is not configured.
    pub fingerprint: Option<AudioFingerprint>,
}

impl TrackQuery {
    pub fn has_any_text(&self) -> bool {
        let text = [
            self.artist.as_deref(),
            self.title.as_deref(),
            self.album.as_deref(),
        ];
        text.iter()
            .any(|value| value.is_some_and(|inner| !inner.trim().is_empty()))
    }

    fn to_search_terms(&self) -> RecordingSearchTerms {
        RecordingSearchTerms {
            artist: self.artist.clone(),
            title: self.title.clone(),
            album: self.album.clone(),
            duration_ms: self.duration_ms,
        }
    }
}

/// Resolved identification: a MusicBrainz recording the service is
/// confident represents the track. The struct is intentionally
/// projection-shaped (one row's worth of fields) rather than nested —
/// the caller writes selected fields back through the existing tag
/// path and doesn't need a deeper graph.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackMatch {
    pub recording_mbid: String,
    pub title: Option<String>,
    pub artist: Option<String>,
    /// Year of the recording's first release per MusicBrainz. This
    /// is the song's "year" in the iTunes-ish sense — not the year
    /// of any one release, so compilations and reissues do not skew
    /// it. `None` when MB has no first-release-date for the
    /// recording.
    pub first_release_year: Option<i32>,
    /// Community-voted genre tags, sorted by vote count descending.
    /// Empty when MB has no curated genres on the recording. The
    /// caller picks which (if any) to surface — for example,
    /// preferring genres already present in the user's library to
    /// avoid genre sprawl.
    pub genres: Vec<GenreCandidate>,
    /// Releases the recording appears on, ordered as MusicBrainz
    /// returned them. The first entry is the service's best guess
    /// for the "primary" release; artwork lookup walks the list in
    /// order. Empty if no release is associated with the recording.
    pub releases: Vec<TrackMatchRelease>,
    /// Identification provenance. Useful at the UI layer for
    /// explaining *how* a match was made.
    pub source: TrackMatchSource,
}

/// One curated genre tag offered as a candidate for the recording.
/// `vote_count` is MusicBrainz's community tally; higher means more
/// users agreed the tag applies. Callers may use the count to break
/// ties or as a confidence signal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenreCandidate {
    pub name: String,
    pub vote_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TrackMatchRelease {
    pub release_mbid: String,
    pub release_group_mbid: Option<String>,
    pub title: Option<String>,
    pub year: Option<i32>,
    pub track_number: Option<u32>,
    pub track_total: Option<u32>,
    pub disc_number: Option<u32>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TrackMatchSource {
    /// Match came from MusicBrainz's text search.
    MusicBrainzTags,
    /// Match came from AcoustID fingerprint lookup, then verified
    /// against MusicBrainz.
    AcoustIdFingerprint,
}

/// Cover image bytes plus enough provenance to render diagnostics or
/// a status-bar message. The bytes are exactly what Cover Art Archive
/// returned — they are not re-encoded, cropped, or normalised here.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FetchedArtwork {
    pub bytes: Vec<u8>,
    pub release_mbid: String,
}

/// Lyrics payload as returned by a remote provider. Both fields are
/// optional and independent — providers commonly serve plain-only or
/// synced-only entries. `synced_lrc` is the verbatim LRC source text;
/// callers parse it via `sustain_domain::SyncedLyrics::parse_lrc`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct FetchedLyrics {
    pub plain: Option<String>,
    pub synced_lrc: Option<String>,
}

impl FetchedLyrics {
    pub fn is_empty(&self) -> bool {
        self.plain.is_none() && self.synced_lrc.is_none()
    }
}

/// Service contract. Implementations are required to be `Send + Sync`
/// so the runtime can hand them across worker boundaries.
pub trait RemoteMetadataService: Send + Sync {
    /// Identify the track described by `query`. Returns `Ok(None)`
    /// when no confident match exists; that is a normal outcome and
    /// the caller treats it like any other "no result".
    ///
    /// Identification is deliberately cheap: the text-search path runs a
    /// single search request and the resulting match carries the title,
    /// artist, and releases — everything artwork needs — but *not* the
    /// lookup-only fields (`first_release_year`, `genres`). A caller that
    /// needs those promotes the match with [`Self::enrich_match`].
    fn identify_track(&self, query: &TrackQuery) -> RemoteResult<Option<TrackMatch>>;

    /// Promote a match to a full record, filling the lookup-only fields
    /// (`first_release_year`, `genres`) that the cheap text-search path
    /// does not carry. The default returns the match unchanged —
    /// appropriate for any provider without a by-id lookup endpoint;
    /// [`ComposedRemoteMetadataService`] overrides it with a MusicBrainz
    /// `lookup_recording`.
    ///
    /// Callers invoke this only when they actually need those fields and
    /// the match came from the text-search path
    /// ([`TrackMatchSource::MusicBrainzTags`]); AcoustID matches are
    /// already enriched during identification, so the extra request is
    /// paid only when it pays off.
    fn enrich_match(&self, track_match: &TrackMatch) -> RemoteResult<TrackMatch> {
        Ok(track_match.clone())
    }

    /// Fetch artwork for an already-identified match. Returns
    /// `Ok(None)` if Cover Art Archive has no front cover on file
    /// for any of the match's releases.
    fn fetch_artwork_for_match(
        &self,
        track_match: &TrackMatch,
    ) -> RemoteResult<Option<FetchedArtwork>>;

    /// Convenience composition: identify, then fetch artwork. The
    /// individual operations are kept on the trait so callers that
    /// already hold a [`TrackMatch`] (e.g. after a Get Info dialog)
    /// can reuse it without re-identifying.
    fn fetch_artwork(&self, query: &TrackQuery) -> RemoteResult<Option<FetchedArtwork>> {
        let Some(track_match) = self.identify_track(query)? else {
            return Ok(None);
        };
        self.fetch_artwork_for_match(&track_match)
    }

    /// Fetch lyrics for the track described by `query`. Returns
    /// `Ok(None)` when the provider has no entry for the track —
    /// indistinguishable from "we tried, no hit", and the caller
    /// records the attempt regardless so the scheduler does not
    /// keep retrying the same miss.
    fn fetch_lyrics(&self, query: &TrackQuery) -> RemoteResult<Option<FetchedLyrics>>;
}

/// The production implementation. Owns one [`MusicBrainzClient`], one
/// [`CoverArtArchiveClient`], one [`LrcLibClient`], and optionally one
/// [`AcoustIdClient`]. The AcoustID slot is optional because builds
/// without an API key must still work for tag-based identification.
pub struct ComposedRemoteMetadataService {
    musicbrainz: MusicBrainzClient,
    cover_art_archive: CoverArtArchiveClient,
    lrclib: LrcLibClient,
    acoustid: Option<AcoustIdClient>,
}

impl ComposedRemoteMetadataService {
    pub fn new(
        musicbrainz: MusicBrainzClient,
        cover_art_archive: CoverArtArchiveClient,
        lrclib: LrcLibClient,
        acoustid: Option<AcoustIdClient>,
    ) -> Self {
        Self {
            musicbrainz,
            cover_art_archive,
            lrclib,
            acoustid,
        }
    }

    /// Convenience wrapper: build the whole service stack from a
    /// shared [`crate::client::HttpClient`] and an optional AcoustID
    /// key. The HTTP client carries the User-Agent and rate-limit
    /// state shared across all four providers.
    pub fn from_http_client(
        http: Arc<crate::client::HttpClient>,
        acoustid_api_key: Option<&str>,
    ) -> Self {
        let musicbrainz = MusicBrainzClient::new(Arc::clone(&http));
        let cover_art_archive = CoverArtArchiveClient::new(Arc::clone(&http));
        let lrclib = LrcLibClient::new(Arc::clone(&http));
        let acoustid = acoustid_api_key.and_then(|key| AcoustIdClient::new(http, key));
        Self::new(musicbrainz, cover_art_archive, lrclib, acoustid)
    }

    fn identify_via_musicbrainz(&self, query: &TrackQuery) -> RemoteResult<Option<TrackMatch>> {
        if !query.has_any_text() {
            return Ok(None);
        }
        let results = self
            .musicbrainz
            .search_recordings(&query.to_search_terms())?;
        let Some(best) = pick_best_musicbrainz(results, MUSICBRAINZ_CONFIDENT_SCORE) else {
            return Ok(None);
        };
        // Search hits ship a partial record: title, artist, and releases
        // (enough for artwork) but no genres and no first-release-date.
        // We return the match as-is; the lookup-only fields are filled
        // lazily by `enrich_match`, only when tag enrichment actually
        // needs them (issue #44). The eager promote-to-full-lookup that
        // used to live here roughly doubled enrichment wall time per
        // track even when the data was never used.
        Ok(Some(recording_to_match(
            best,
            TrackMatchSource::MusicBrainzTags,
        )))
    }

    fn identify_via_acoustid(&self, query: &TrackQuery) -> RemoteResult<Option<TrackMatch>> {
        let Some(acoustid) = &self.acoustid else {
            return Ok(None);
        };
        let Some(fingerprint) = &query.fingerprint else {
            return Ok(None);
        };
        let matches = acoustid.lookup_fingerprint(fingerprint)?;
        let Some(best) = matches
            .into_iter()
            .filter(|candidate| candidate.score >= ACOUSTID_CONFIDENT_SCORE)
            .max_by(|left, right| {
                left.score
                    .partial_cmp(&right.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        else {
            return Ok(None);
        };

        // AcoustID returns bare recording IDs with no descriptive
        // fields. We resolve each candidate against MusicBrainz's
        // lookup-by-id endpoint so the downstream artwork path has a
        // populated release list to walk; an unresolved MBID would
        // carry no releases and would silently produce no artwork.
        for recording_mbid in &best.recording_mbids {
            if let Some(resolved) = self.musicbrainz.lookup_recording(recording_mbid)? {
                let resolved = self.enrich_genres_with_fallbacks(resolved)?;
                return Ok(Some(recording_to_match(
                    resolved,
                    TrackMatchSource::AcoustIdFingerprint,
                )));
            }
        }
        Ok(None)
    }

    /// Walk the MusicBrainz genre hierarchy for `recording`, filling
    /// the `genres` field from the first non-empty source. The walk
    /// is recording → each release-group (in MB-supplied order) →
    /// primary artist. Most MusicBrainz tracks carry no curated
    /// genres at the recording level, so without this fallback even
    /// perfectly identified famous tracks (Pink Floyd's "Money",
    /// Queen's "Bohemian Rhapsody") would surface with an empty
    /// genre list. The waterfall mirrors how MusicBrainz Picard
    /// resolves genres by default.
    ///
    /// Short-circuits on the first non-empty hit to keep the request
    /// count bounded — a worst-case enrichment is one search, one
    /// recording lookup, one release-group lookup, one artist
    /// lookup, all serialised by the per-host rate limiter.
    fn enrich_genres_with_fallbacks(
        &self,
        recording: RecordingMatch,
    ) -> RemoteResult<RecordingMatch> {
        if !recording.genres.is_empty() {
            return Ok(recording);
        }
        for release in &recording.releases {
            let Some(release_group_mbid) = release.release_group_mbid.as_deref() else {
                continue;
            };
            let genres = self
                .musicbrainz
                .lookup_release_group_genres(release_group_mbid)?;
            if !genres.is_empty() {
                return Ok(RecordingMatch {
                    genres,
                    ..recording
                });
            }
        }
        if let Some(artist_mbid) = recording.primary_artist_mbid.as_deref() {
            let genres = self.musicbrainz.lookup_artist_genres(artist_mbid)?;
            if !genres.is_empty() {
                return Ok(RecordingMatch {
                    genres,
                    ..recording
                });
            }
        }
        Ok(recording)
    }
}

/// Pure policy: pick the first non-empty genre source in the
/// (recording, release-group, artist) waterfall. Extracted so the
/// fallback ordering can be exercised without an HTTP mock; the real
/// `enrich_genres_with_fallbacks` short-circuits on each I/O step
/// rather than fetching all three sources up-front.
#[cfg(test)]
fn pick_genre_source(
    recording: Vec<crate::musicbrainz::GenreVote>,
    release_group: Vec<crate::musicbrainz::GenreVote>,
    artist: Vec<crate::musicbrainz::GenreVote>,
) -> Vec<crate::musicbrainz::GenreVote> {
    if !recording.is_empty() {
        return recording;
    }
    if !release_group.is_empty() {
        return release_group;
    }
    artist
}

impl RemoteMetadataService for ComposedRemoteMetadataService {
    fn identify_track(&self, query: &TrackQuery) -> RemoteResult<Option<TrackMatch>> {
        if let Some(track_match) = self.identify_via_musicbrainz(query)? {
            return Ok(Some(track_match));
        }
        self.identify_via_acoustid(query)
    }

    fn enrich_match(&self, track_match: &TrackMatch) -> RemoteResult<TrackMatch> {
        // The lookup-by-id endpoint returns the canonical record with the
        // fields the text-search winner lacks (first-release-date,
        // curated genres). If it 404s (race against an MB merge/delete)
        // keep the search-derived match rather than dropping the fields
        // the caller already has.
        let Some(detailed) = self
            .musicbrainz
            .lookup_recording(&track_match.recording_mbid)?
        else {
            return Ok(track_match.clone());
        };
        let detailed = self.enrich_genres_with_fallbacks(detailed)?;
        Ok(recording_to_match(detailed, track_match.source))
    }

    fn fetch_artwork_for_match(
        &self,
        track_match: &TrackMatch,
    ) -> RemoteResult<Option<FetchedArtwork>> {
        for release in &track_match.releases {
            if let Some(bytes) = self
                .cover_art_archive
                .fetch_release_front(&release.release_mbid)?
            {
                return Ok(Some(FetchedArtwork {
                    bytes,
                    release_mbid: release.release_mbid.clone(),
                }));
            }
        }
        // Release-level lookups all missed. Try the first
        // release-group, which is normally the broadest available
        // bucket: editions/reissues without their own cover art
        // typically still inherit a group-level front.
        for release in &track_match.releases {
            let Some(release_group_mbid) = release.release_group_mbid.as_deref() else {
                continue;
            };
            if let Some(bytes) = self
                .cover_art_archive
                .fetch_release_group_front(release_group_mbid)?
            {
                return Ok(Some(FetchedArtwork {
                    bytes,
                    release_mbid: release.release_mbid.clone(),
                }));
            }
        }
        Ok(None)
    }

    fn fetch_lyrics(&self, query: &TrackQuery) -> RemoteResult<Option<FetchedLyrics>> {
        self.lrclib.fetch(query)
    }
}

fn pick_best_musicbrainz(
    mut results: Vec<RecordingMatch>,
    min_score: u8,
) -> Option<RecordingMatch> {
    results.sort_by_key(|recording| std::cmp::Reverse(recording.score));
    results
        .into_iter()
        .find(|recording| recording.score >= min_score && !recording.releases.is_empty())
}

fn recording_to_match(recording: RecordingMatch, source: TrackMatchSource) -> TrackMatch {
    let releases = recording
        .releases
        .into_iter()
        .map(|release| TrackMatchRelease {
            release_mbid: release.release_mbid,
            release_group_mbid: release.release_group_mbid,
            title: release.title,
            year: release.year,
            track_number: release.track_number,
            track_total: release.track_total,
            disc_number: release.disc_number,
        })
        .collect();
    let genres = recording
        .genres
        .into_iter()
        .map(|vote| GenreCandidate {
            name: vote.name,
            vote_count: vote.vote_count,
        })
        .collect();
    TrackMatch {
        recording_mbid: recording.recording_mbid,
        title: recording.title,
        artist: recording.artist,
        first_release_year: recording.first_release_year,
        genres,
        releases,
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::musicbrainz::RecordingRelease;

    fn make_recording(score: u8, release_mbid: &str) -> RecordingMatch {
        RecordingMatch {
            recording_mbid: format!("rec-{score}"),
            title: Some("Title".to_owned()),
            artist: Some("Artist".to_owned()),
            duration_ms: Some(200_000),
            score,
            first_release_year: None,
            genres: Vec::new(),
            releases: vec![RecordingRelease {
                release_mbid: release_mbid.to_owned(),
                release_group_mbid: None,
                title: Some("Album".to_owned()),
                year: Some(2000),
                track_number: Some(1),
                track_total: Some(10),
                disc_number: Some(1),
            }],
            primary_artist_mbid: None,
        }
    }

    fn vote(name: &str, count: u32) -> crate::musicbrainz::GenreVote {
        crate::musicbrainz::GenreVote {
            name: name.to_owned(),
            vote_count: count,
        }
    }

    #[test]
    fn pick_genre_source_prefers_recording_when_present() {
        let recording = vec![vote("electronic", 5)];
        let release_group = vec![vote("ambient", 10)];
        let artist = vec![vote("synth-pop", 20)];
        let chosen = pick_genre_source(recording, release_group, artist);
        let names: Vec<_> = chosen.iter().map(|v| v.name.as_str()).collect();
        assert_eq!(names, vec!["electronic"]);
    }

    #[test]
    fn pick_genre_source_falls_back_to_release_group_when_recording_empty() {
        let recording = Vec::new();
        let release_group = vec![vote("progressive rock", 41)];
        let artist = vec![vote("rock", 100)];
        let chosen = pick_genre_source(recording, release_group, artist);
        assert_eq!(chosen[0].name, "progressive rock");
    }

    #[test]
    fn pick_genre_source_falls_back_to_artist_when_recording_and_release_group_empty() {
        let recording = Vec::new();
        let release_group = Vec::new();
        let artist = vec![vote("progressive rock", 55)];
        let chosen = pick_genre_source(recording, release_group, artist);
        assert_eq!(chosen[0].name, "progressive rock");
    }

    #[test]
    fn pick_genre_source_returns_empty_when_no_source_has_data() {
        let chosen = pick_genre_source(Vec::new(), Vec::new(), Vec::new());
        assert!(chosen.is_empty());
    }

    #[test]
    fn pick_best_returns_highest_scoring_match_above_threshold() {
        let results = vec![
            make_recording(70, "release-a"),
            make_recording(95, "release-b"),
            make_recording(85, "release-c"),
        ];
        let best = pick_best_musicbrainz(results, MUSICBRAINZ_CONFIDENT_SCORE);
        assert_eq!(
            best.expect("test seeded a passing match").recording_mbid,
            "rec-95"
        );
    }

    #[test]
    fn pick_best_returns_none_when_no_score_passes_threshold() {
        let results = vec![
            make_recording(60, "release-a"),
            make_recording(70, "release-b"),
        ];
        assert!(pick_best_musicbrainz(results, MUSICBRAINZ_CONFIDENT_SCORE).is_none());
    }

    #[test]
    fn pick_best_skips_releaseless_recordings() {
        let mut releaseless = make_recording(100, "release-a");
        releaseless.releases.clear();
        let results = vec![releaseless, make_recording(91, "release-b")];
        assert_eq!(
            pick_best_musicbrainz(results, MUSICBRAINZ_CONFIDENT_SCORE)
                .expect("releaseless candidates must be skipped, not block the search")
                .recording_mbid,
            "rec-91"
        );
    }

    #[test]
    fn track_query_text_detection_is_blank_safe() {
        assert!(!TrackQuery::default().has_any_text());
        assert!(
            !TrackQuery {
                artist: Some("  ".to_owned()),
                title: Some(String::new()),
                ..Default::default()
            }
            .has_any_text()
        );
        assert!(
            TrackQuery {
                artist: Some("Bowie".to_owned()),
                ..Default::default()
            }
            .has_any_text()
        );
    }

    #[test]
    fn recording_to_match_preserves_release_order() {
        let mut recording = make_recording(95, "release-a");
        recording.releases.push(RecordingRelease {
            release_mbid: "release-b".to_owned(),
            release_group_mbid: Some("group-b".to_owned()),
            title: None,
            year: None,
            track_number: None,
            track_total: None,
            disc_number: None,
        });
        let track_match = recording_to_match(recording, TrackMatchSource::MusicBrainzTags);
        assert_eq!(track_match.releases[0].release_mbid, "release-a");
        assert_eq!(track_match.releases[1].release_mbid, "release-b");
    }
}
