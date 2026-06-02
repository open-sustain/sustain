// SPDX-License-Identifier: GPL-3.0-or-later
// Copyright (C) 2026 AnnoyingTechnology

//! Blocking HTTP client wrapper with per-host rate limiting.
//!
//! The remote providers (MusicBrainz, Cover Art Archive, AcoustID) all
//! publish strict rate-limit policies. MusicBrainz caps a single
//! application at one request per second, and Cover Art Archive — which
//! is a thin redirect layer on top of the MusicBrainz database — shares
//! that ceiling. AcoustID is more permissive but documents a soft
//! three-per-second limit. Sending a burst of requests will quickly
//! return HTTP 503 and, with repeated offence, get the User-Agent
//! blocked. The wrapper enforces a minimum gap per host so concurrent
//! requests across providers cannot collectively violate either policy.
//!
//! Synchronous blocking I/O is intentional. The networked metadata path
//! is gated behind explicit user actions (a click, a settings tickbox);
//! its callers always run on a dedicated worker thread that already owns
//! a `RemoteMetadataService` handle. Bringing in a Tokio runtime just to
//! issue one HTTPS request would be substantially heavier than the
//! request itself.

use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
    thread,
    time::{Duration, Instant},
};

use serde::de::DeserializeOwned;
use ureq::{Agent, http};

use crate::error::{RemoteError, RemoteResult};

/// Minimum gap MusicBrainz enforces for a single client; we match it
/// exactly rather than running close to the edge.
pub const MUSICBRAINZ_MIN_REQUEST_GAP: Duration = Duration::from_millis(1_050);

/// Cover Art Archive piggybacks on MusicBrainz infrastructure, so the
/// same gap applies to it.
pub const COVER_ART_ARCHIVE_MIN_REQUEST_GAP: Duration = MUSICBRAINZ_MIN_REQUEST_GAP;

/// AcoustID documents a soft three-per-second limit for fingerprint
/// lookups; we sit just under it.
pub const ACOUSTID_MIN_REQUEST_GAP: Duration = Duration::from_millis(360);

/// LRClib does not publish a hard rate limit. We pace ourselves at
/// roughly three requests per second to stay polite while still
/// draining a 9000-track library in a sensible amount of time when
/// the user enables background lyrics retrieval.
pub const LRCLIB_MIN_REQUEST_GAP: Duration = Duration::from_millis(350);

/// Fallback cool-down applied when a server returns 429/503 without
/// a usable `Retry-After` header. One minute is comfortably more
/// than any of the providers' published gaps; the caller can fail
/// fast if a smaller value is appropriate.
pub const DEFAULT_RATE_LIMITED_COOL_DOWN: Duration = Duration::from_secs(60);

/// Upper bound applied to whatever the server asked for in
/// `Retry-After`. A misconfigured or hostile server cannot pin our
/// worker for an unbounded duration; if a provider really wants us
/// off for a longer span the user will eventually nudge the
/// scheduler (library scan, settings toggle, restart).
pub const MAX_RATE_LIMITED_COOL_DOWN: Duration = Duration::from_secs(15 * 60);

/// HTTP request timeout. The remote endpoints answer in well under a
/// second on a healthy network; anything past a few seconds is almost
/// certainly a network problem rather than a slow server. Failing fast
/// keeps the UI's spinner from staring at a hung socket.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// Maximum accepted JSON response body. Provider metadata replies are small;
/// one MiB leaves ample room for legitimate search results while preventing a
/// hostile or broken endpoint from allocating without bound before serde runs.
const MAX_JSON_RESPONSE_BYTES: usize = 1024 * 1024;

/// Standard `Accept` header for JSON endpoints across all three
/// providers. MusicBrainz and AcoustID return JSON when asked; Cover
/// Art Archive's `/release/{mbid}` endpoint also returns JSON for the
/// index and binary for the image endpoints.
const ACCEPT_JSON: &str = "application/json";

/// User-supplied configuration for the HTTP client. The User-Agent
/// matters: MusicBrainz documents that requests without a meaningful
/// User-Agent are subject to rate-limit blocks and the contact URL is
/// expected to be reachable for abuse reports.
#[derive(Clone, Debug)]
pub struct HttpClientConfig {
    pub user_agent: String,
}

/// Per-host rate limit policy. Hosts not listed fall through to no
/// limiter — a convenience for tests that hit a localhost mock.
struct RateLimitPolicy {
    minimum_gap: Duration,
}

#[derive(Default)]
struct HostState {
    /// "Intent to fire" time for the most recent request to this
    /// host. New requests wait until this Instant before sending.
    /// Pushed forward by both the per-host minimum gap *and* any
    /// 429/503 cool-down observed by [`HttpClient::record_cool_down`],
    /// so the rate limiter automatically holds the worker back after
    /// the provider has asked us to stop.
    next_request_at: Option<Instant>,
}

pub struct HttpClient {
    agent: Agent,
    user_agent: String,
    rate_limits: HashMap<&'static str, RateLimitPolicy>,
    host_states: Mutex<HashMap<&'static str, HostState>>,
}

impl HttpClient {
    pub fn new(config: HttpClientConfig) -> Self {
        // http_status_as_error(false) so non-2xx responses come back
        // as Ok(response) with their headers intact — we need to read
        // `Retry-After` on 429/503 before the response body is dropped.
        let agent: Agent = Agent::config_builder()
            .timeout_global(Some(REQUEST_TIMEOUT))
            .http_status_as_error(false)
            .build()
            .into();

        let mut rate_limits = HashMap::new();
        rate_limits.insert(
            "musicbrainz.org",
            RateLimitPolicy {
                minimum_gap: MUSICBRAINZ_MIN_REQUEST_GAP,
            },
        );
        rate_limits.insert(
            "coverartarchive.org",
            RateLimitPolicy {
                minimum_gap: COVER_ART_ARCHIVE_MIN_REQUEST_GAP,
            },
        );
        rate_limits.insert(
            "api.acoustid.org",
            RateLimitPolicy {
                minimum_gap: ACOUSTID_MIN_REQUEST_GAP,
            },
        );
        rate_limits.insert(
            "lrclib.net",
            RateLimitPolicy {
                minimum_gap: LRCLIB_MIN_REQUEST_GAP,
            },
        );

        Self {
            agent,
            user_agent: config.user_agent,
            rate_limits,
            host_states: Mutex::new(HashMap::new()),
        }
    }

    pub fn user_agent(&self) -> &str {
        &self.user_agent
    }

    /// Issues a GET that returns deserialised JSON. Caller is
    /// responsible for assembling the full URL (including query
    /// parameters) — the rate limiter and User-Agent are applied here.
    pub fn get_json<T: DeserializeOwned>(&self, url: &str) -> RemoteResult<T> {
        self.respect_rate_limit(url);
        let response = self
            .agent
            .get(url)
            .header("User-Agent", &self.user_agent)
            .header("Accept", ACCEPT_JSON)
            .call()
            .map_err(|error| map_ureq_error(error, url))?;

        let status = response.status().as_u16();
        if let Some(cool_down) = self.handle_rate_limit_status(url, status, response.headers()) {
            return Err(RemoteError::RateLimited { cool_down });
        }
        if !response.status().is_success() {
            return Err(RemoteError::BadStatus(status));
        }

        let read_limit = MAX_JSON_RESPONSE_BYTES
            .checked_add(1)
            .and_then(|limit| u64::try_from(limit).ok())
            .ok_or(RemoteError::PayloadTooLarge)?;
        let bytes = response
            .into_body()
            .into_with_config()
            .limit(read_limit)
            .read_to_vec()
            .map_err(map_json_payload_read_error)?;
        if bytes.len() > MAX_JSON_RESPONSE_BYTES {
            return Err(RemoteError::PayloadTooLarge);
        }
        serde_json::from_slice(&bytes).map_err(|_| RemoteError::InvalidResponse)
    }

    /// Issues a GET that returns a raw byte payload, following
    /// redirects (Cover Art Archive's image endpoints reply with a 307
    /// to the actual image URL on archive.org). Returns `None` for 404,
    /// which the providers use to indicate "no image present" and is
    /// not an error in our model.
    pub fn get_bytes(&self, url: &str, max_bytes: usize) -> RemoteResult<Option<Vec<u8>>> {
        self.respect_rate_limit(url);
        let response = match self
            .agent
            .get(url)
            .header("User-Agent", &self.user_agent)
            .call()
        {
            Ok(response) => response,
            Err(error) => return map_get_bytes_error(error, url),
        };

        let status = response.status().as_u16();
        if status == 404 {
            return Ok(None);
        }
        if let Some(cool_down) = self.handle_rate_limit_status(url, status, response.headers()) {
            return Err(RemoteError::RateLimited { cool_down });
        }
        if !response.status().is_success() {
            return Err(RemoteError::BadStatus(status));
        }

        let read_limit = max_bytes
            .checked_add(1)
            .and_then(|limit| u64::try_from(limit).ok())
            .ok_or(RemoteError::PayloadTooLarge)?;
        let bytes = response
            .into_body()
            .into_with_config()
            .limit(read_limit)
            .read_to_vec()
            .map_err(map_binary_payload_read_error)?;
        if bytes.len() > max_bytes {
            return Err(RemoteError::PayloadTooLarge);
        }
        if bytes.is_empty() {
            return Ok(None);
        }
        Ok(Some(bytes))
    }

    /// Recognise 429 (Too Many Requests) and 503 (Service Unavailable).
    /// On a match, parse `Retry-After`, push the host's cool-down
    /// forward, and return `Some(cool_down)` so the caller can surface
    /// a [`RemoteError::RateLimited`]. Returns `None` for any other
    /// status, leaving normal handling to the caller.
    fn handle_rate_limit_status(
        &self,
        url: &str,
        status: u16,
        headers: &http::HeaderMap,
    ) -> Option<Duration> {
        if status != 429 && status != 503 {
            return None;
        }
        let cool_down = parse_retry_after(headers).unwrap_or(DEFAULT_RATE_LIMITED_COOL_DOWN);
        let cool_down = cool_down.min(MAX_RATE_LIMITED_COOL_DOWN);
        if let Some(host) = host_from_url(url) {
            self.record_cool_down(host, cool_down);
        }
        eprintln!(
            "Sustain: remote {url} returned HTTP {status}; holding the host for {} s before the next request",
            cool_down.as_secs()
        );
        Some(cool_down)
    }

    /// Push the named host's "next request at" forward by `cool_down`
    /// from now. Subsequent calls to [`Self::respect_rate_limit`] for
    /// the same host wait against the new value, so a 429 reply
    /// automatically translates into a real pause for every other
    /// scheduled request to that host.
    fn record_cool_down(&self, host: &'static str, cool_down: Duration) {
        let mut states = match self.host_states.lock() {
            Ok(guard) => guard,
            Err(poisoned) => recover_poisoned(poisoned),
        };
        let entry = states.entry(host).or_default();
        let target = Instant::now() + cool_down;
        entry.next_request_at = Some(match entry.next_request_at {
            Some(existing) => existing.max(target),
            None => target,
        });
    }

    fn respect_rate_limit(&self, url: &str) {
        let Some(host) = host_from_url(url) else {
            return;
        };
        let Some(policy) = self.rate_limits.get(host) else {
            return;
        };

        // We use a single mutex guarding the whole host-state map. The
        // lock is only held long enough to read/update one timestamp;
        // the sleep itself happens outside any lock so other threads
        // targeting different hosts are not blocked behind us.
        let sleep_for = {
            let mut states = match self.host_states.lock() {
                Ok(guard) => guard,
                Err(poisoned) => recover_poisoned(poisoned),
            };
            let entry = states.entry(host).or_default();
            let now = Instant::now();
            let gap_target = entry
                .next_request_at
                .map(|previous| previous + policy.minimum_gap)
                .unwrap_or(now);
            let target = gap_target.max(now);
            let sleep_for = target.saturating_duration_since(now);
            // Record the time we *intend* to fire — not the time we
            // actually fire — so concurrent threads serialise against
            // an honest schedule even if some of them are still sleeping.
            entry.next_request_at = Some(target);
            sleep_for
        };

        if !sleep_for.is_zero() {
            thread::sleep(sleep_for);
        }
    }
}

fn map_binary_payload_read_error(error: ureq::Error) -> RemoteError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => RemoteError::PayloadTooLarge,
        _ => RemoteError::InvalidResponse,
    }
}

fn map_json_payload_read_error(error: ureq::Error) -> RemoteError {
    match error {
        ureq::Error::BodyExceedsLimit(_) => RemoteError::PayloadTooLarge,
        _ => RemoteError::InvalidResponse,
    }
}

/// Parse the value of an HTTP `Retry-After` header into a duration.
/// Per RFC 7231 the header is either a non-negative delta-seconds
/// integer or an HTTP-date. We honour the delta-seconds form (by far
/// the more common reply from MusicBrainz et al.) and treat the
/// HTTP-date form as unparsed — the caller falls back to the default
/// cool-down rather than try to parse arbitrary date formats here.
fn parse_retry_after(headers: &http::HeaderMap) -> Option<Duration> {
    let value = headers.get("retry-after")?.to_str().ok()?;
    let seconds: u64 = value.trim().parse().ok()?;
    Some(Duration::from_secs(seconds))
}

fn map_ureq_error(error: ureq::Error, url: &str) -> RemoteError {
    match error {
        ureq::Error::StatusCode(code) => RemoteError::BadStatus(code),
        ureq::Error::Timeout(_) => RemoteError::Network,
        // Transport-level failures (DNS, TLS, connect refused, etc.).
        // Log the underlying ureq error so diagnosis isn't blocked by
        // the coarse user-facing `RemoteError::Network` value — this
        // is the only place provider-side detail surfaces.
        other => {
            eprintln!("Sustain: remote request to {url} failed: {other:?}");
            RemoteError::Network
        }
    }
}

fn map_get_bytes_error(error: ureq::Error, url: &str) -> RemoteResult<Option<Vec<u8>>> {
    if let ureq::Error::StatusCode(404) = error {
        return Ok(None);
    }
    Err(map_ureq_error(error, url))
}

fn host_from_url(url: &str) -> Option<&'static str> {
    // We don't take a `url` crate dependency just to extract the host;
    // a tiny string scan is enough for our small fixed set of
    // recognised hosts. Returning &'static str lets the host string be
    // a key directly into the rate-limit map.
    const HOSTS: &[&str] = &[
        "musicbrainz.org",
        "coverartarchive.org",
        "api.acoustid.org",
        "lrclib.net",
    ];
    HOSTS
        .iter()
        .copied()
        .find(|host| url_matches_host(url, host))
}

fn url_matches_host(url: &str, host: &str) -> bool {
    let after_scheme = url.split_once("://").map(|(_, rest)| rest).unwrap_or(url);
    let authority = after_scheme.split(['/', '?']).next().unwrap_or("");
    let authority = authority
        .split_once('@')
        .map_or(authority, |(_, rest)| rest);
    let authority = authority
        .split_once(':')
        .map_or(authority, |(host, _)| host);
    authority.eq_ignore_ascii_case(host)
}

fn recover_poisoned<T>(poisoned: std::sync::PoisonError<MutexGuard<'_, T>>) -> MutexGuard<'_, T> {
    // Poisoning here would mean another thread panicked while updating
    // a rate-limit timestamp. The stored state is just a `HashMap` of
    // `Instant`s; nothing about it is corrupted by a panic during an
    // update, so recovering and continuing is safe. We do not propagate
    // the panic — a single network call should not bring down every
    // future fetch.
    poisoned.into_inner()
}

#[cfg(test)]
mod tests {
    use std::{
        io::{Read, Write},
        net::TcpListener,
        thread,
    };

    use serde::Deserialize;

    use super::*;

    #[test]
    fn recognises_known_hosts_case_insensitively() {
        assert_eq!(
            host_from_url("https://MusicBrainz.org/ws/2/recording/"),
            Some("musicbrainz.org")
        );
        assert_eq!(
            host_from_url("https://coverartarchive.org/release/abc/front"),
            Some("coverartarchive.org")
        );
        assert_eq!(
            host_from_url("https://api.acoustid.org/v2/lookup"),
            Some("api.acoustid.org")
        );
    }

    #[test]
    fn ignores_unknown_hosts() {
        assert_eq!(host_from_url("https://example.com/whatever"), None);
        assert_eq!(host_from_url("not a url"), None);
    }

    #[test]
    fn oversized_binary_payload_maps_to_typed_error() {
        assert_eq!(
            map_binary_payload_read_error(ureq::Error::BodyExceedsLimit(123)),
            RemoteError::PayloadTooLarge
        );
    }

    #[derive(Debug, Deserialize, Eq, PartialEq)]
    struct TestPayload {
        value: String,
    }

    #[test]
    fn json_payload_is_deserialized_within_the_acquisition_cap() {
        let url = serve_once(br#"{"value":"ok"}"#.to_vec());
        let client = HttpClient::new(HttpClientConfig {
            user_agent: "Sustain-test/0".to_owned(),
        });

        assert_eq!(
            client.get_json::<TestPayload>(&url),
            Ok(TestPayload {
                value: "ok".to_owned()
            })
        );
    }

    #[test]
    fn oversized_json_payload_maps_to_typed_error() {
        let url = serve_once(vec![b' '; MAX_JSON_RESPONSE_BYTES + 1]);
        let client = HttpClient::new(HttpClientConfig {
            user_agent: "Sustain-test/0".to_owned(),
        });

        assert_eq!(
            client.get_json::<TestPayload>(&url),
            Err(RemoteError::PayloadTooLarge)
        );
    }

    fn serve_once(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind HTTP fixture");
        let address = listener.local_addr().expect("fixture address");
        thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept HTTP request");
            let mut request = [0; 1024];
            let _ = stream.read(&mut request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .expect("write HTTP fixture headers");
            stream.write_all(&body).expect("write HTTP fixture body");
        });
        format!("http://{address}/payload")
    }

    fn headers_with_retry_after(value: &str) -> http::HeaderMap {
        let mut headers = http::HeaderMap::new();
        headers.insert(
            "retry-after",
            http::HeaderValue::from_str(value).expect("valid header"),
        );
        headers
    }

    #[test]
    fn parse_retry_after_accepts_integer_seconds() {
        let headers = headers_with_retry_after("42");
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(42)));
    }

    #[test]
    fn parse_retry_after_trims_whitespace() {
        let headers = headers_with_retry_after("  7  ");
        assert_eq!(parse_retry_after(&headers), Some(Duration::from_secs(7)));
    }

    #[test]
    fn parse_retry_after_rejects_http_date_form() {
        // RFC 7231 also allows an HTTP-date here. We intentionally do
        // not parse it; the caller falls back to the default
        // cool-down, which is comfortably longer than any sane
        // server-supplied wait.
        let headers = headers_with_retry_after("Wed, 21 Oct 2015 07:28:00 GMT");
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn parse_retry_after_returns_none_when_header_is_absent() {
        let headers = http::HeaderMap::new();
        assert_eq!(parse_retry_after(&headers), None);
    }

    #[test]
    fn record_cool_down_pushes_next_request_at_forward() {
        let client = HttpClient::new(HttpClientConfig {
            user_agent: "Sustain-test/0".to_owned(),
        });
        // Seed the host with a recent "next request at" so we can
        // verify the cool-down genuinely moves it forward (not just
        // sets a non-None value).
        {
            let mut states = client.host_states.lock().expect("lock");
            states.entry("musicbrainz.org").or_default().next_request_at = Some(Instant::now());
        }
        let before = client
            .host_states
            .lock()
            .expect("lock")
            .get("musicbrainz.org")
            .and_then(|s| s.next_request_at)
            .expect("seeded");
        client.record_cool_down("musicbrainz.org", Duration::from_secs(30));
        let after = client
            .host_states
            .lock()
            .expect("lock")
            .get("musicbrainz.org")
            .and_then(|s| s.next_request_at)
            .expect("present after cool-down");
        // The new value should be far in the future relative to
        // `before` — at least the cool-down minus a little slack for
        // the function call itself.
        assert!(
            after.saturating_duration_since(before) >= Duration::from_secs(29),
            "cool-down should move next_request_at forward by ~30s, moved {:?}",
            after.saturating_duration_since(before)
        );
    }

    #[test]
    fn record_cool_down_never_pulls_next_request_at_backward() {
        // If we somehow recorded an earlier cool-down after a longer
        // one (e.g. a 30s window already in place, a transient 5s
        // retry-after later), the later call must not shorten the
        // existing window.
        let client = HttpClient::new(HttpClientConfig {
            user_agent: "Sustain-test/0".to_owned(),
        });
        client.record_cool_down("musicbrainz.org", Duration::from_secs(120));
        let long_window = client
            .host_states
            .lock()
            .expect("lock")
            .get("musicbrainz.org")
            .and_then(|s| s.next_request_at)
            .expect("present");
        client.record_cool_down("musicbrainz.org", Duration::from_secs(1));
        let observed = client
            .host_states
            .lock()
            .expect("lock")
            .get("musicbrainz.org")
            .and_then(|s| s.next_request_at)
            .expect("still present");
        assert_eq!(observed, long_window);
    }
}
