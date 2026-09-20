//! X/Twitter search via the FxTwitter API v2 (no key, stable JSON).
//!
//! `GET {host}/2/search` with `q`, `feed=latest` and `count`, answering
//! `APISearchResults {code, results, cursor}`. The `code` mirrors the HTTP
//! status and is checked even on HTTP 200 — a 2xx transport does not imply a
//! successful answer.
//!
//! The default host is the public `https://api.fxtwitter.com` (1000 req/min
//! per IP). Operators who need more headroom — or who would rather not depend
//! on a third-party host — can point `fxtwitter_base_url` at a self-hosted
//! FxTwitter instance; a loopback/LAN host additionally needs
//! `allow_private_network = true`, enforced at request time by the egress
//! guard like every other destination.

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::{dedupe_and_truncate, SearchEngine};
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{http_url, join_meta, normalize_snippet};

/// Default search endpoint (public FxTwitter host).
const SEARCH_URL: &str = "https://api.fxtwitter.com/2/search";
/// FxTwitter rejects an empty `q` and caps it at 512 characters.
const MAX_QUERY_CHARS: usize = 512;

pub struct FxTwitter {
    base: String,
}

impl Default for FxTwitter {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl FxTwitter {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct SearchResp {
    code: f64,
    results: Vec<Status>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(default)]
    id: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    text: String,
    #[serde(default)]
    created_at: String,
    #[serde(default)]
    likes: f64,
    #[serde(default)]
    reposts: f64,
    #[serde(default)]
    replies: f64,
    #[serde(default)]
    author: Option<Author>,
}

#[derive(Deserialize)]
struct Author {
    #[serde(default)]
    name: String,
    #[serde(default)]
    screen_name: String,
}

impl SearchEngine for FxTwitter {
    fn name(&self) -> &'static str {
        "fxtwitter"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let query = checked_query(query)?;
        let endpoint = self.search_endpoint(opts)?;
        let count = opts.count.clamp(1, crate::cli::MAX_RESULTS).to_string();
        let feed = checked_feed(opts.feed.as_deref())?;
        let params: Vec<(&str, &str)> = vec![("q", query), ("feed", feed), ("count", &count)];
        let url = Url::parse_with_params(&endpoint, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("fxtwitter request failed: {e}")))?;
        // FxTwitter answers a no-match search with HTTP 200 + `results: []`,
        // so an HTTP 404 means the timeline is down, not "no results" — it is
        // an honest error rather than a confident empty answer.
        crate::http::api_status(resp.status().as_u16())?;
        let body = crate::http::response_text(resp)?;
        Ok(dedupe_and_truncate(
            parse_results(&body)?,
            opts.count,
            |r| &r.url,
        ))
    }
}

impl FxTwitter {
    /// Effective search endpoint: the configured self-host wins over the
    /// built-in public host. Only the URL *shape* is validated here
    /// (http/https, no credentials); private-network hosts stay subject to the
    /// request-time egress guard, so loopback self-hosts need
    /// `allow_private_network = true` exactly like any LAN destination.
    fn search_endpoint(&self, opts: &SearchOpts) -> Result<String> {
        let trimmed = opts
            .fxtwitter_base_url
            .as_deref()
            .map(str::trim)
            .filter(|h| !h.is_empty());
        let Some(host) = trimmed else {
            return Ok(self.base.clone());
        };
        let checked = http_url(host).ok_or_else(|| {
            Error::Config(format!(
                "invalid fxtwitter_base_url '{host}': expected an http(s) URL without credentials"
            ))
        })?;
        Ok(format!("{}/2/search", checked.trim_end_matches('/')))
    }
}

/// Reject the queries FxTwitter itself would 400 on, before any request.
fn checked_query(query: &str) -> Result<&str> {
    let query = query.trim();
    if query.is_empty() {
        return Err(Error::Config("fxtwitter query must not be empty".into()));
    }
    if query.chars().count() > MAX_QUERY_CHARS {
        return Err(Error::Config(format!(
            "fxtwitter query exceeds {MAX_QUERY_CHARS} characters"
        )));
    }
    Ok(query)
}

/// Resolve the `feed` ordering: unset/blank means `latest`, anything else
/// must be a value the API accepts. Shared with CLI/recipe validation so a
/// typo fails before any request is sent.
pub(crate) fn checked_feed(feed: Option<&str>) -> Result<&str> {
    match feed.map(str::trim) {
        None | Some("") => Ok("latest"),
        Some(valid @ ("latest" | "top" | "media")) => Ok(valid),
        Some(other) => Err(Error::Config(format!(
            "invalid fxtwitter feed '{other}' (expected latest, top or media)"
        ))),
    }
}

/// Pure parser (unit-tested against fixtures): JSON in, typed results out.
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<SearchResp>(body)
        .map_err(|e| Error::Parse(format!("fxtwitter response is not valid JSON: {e}")))?;
    // The mirrored `code` carries the real verdict, mirroring `api_status`.
    match resp.code as i64 {
        202 | 429 => Err(Error::RateLimited(format!(
            "fxtwitter answered code {} (retry later or lower request rate)",
            resp.code as i64
        ))),
        200..=299 => Ok(map_results(resp.results)),
        other => Err(Error::Http(u16::try_from(other).unwrap_or(500))),
    }
}

fn map_results(items: Vec<Status>) -> Vec<SearchResult> {
    items
        .into_iter()
        .filter_map(|s| {
            let url = http_url(&s.url).or_else(|| fallback_status_url(&s.id))?;
            let meta = join_meta(&[
                &format!("{} likes", (s.likes as i64).max(0)),
                &format!("{} reposts", (s.reposts as i64).max(0)),
                &format!("{} replies", (s.replies as i64).max(0)),
            ]);
            Some(SearchResult {
                title: normalize_snippet(&format!("@{}", author_handle(&s.author))),
                url,
                snippet: normalize_snippet(&join_meta(&[s.text.trim(), &meta])),
                published: crate::time::x_created_at(&s.created_at),
            })
        })
        .collect()
}

fn author_handle(author: &Option<Author>) -> String {
    author
        .as_ref()
        .and_then(|a| {
            let screen = a.screen_name.trim();
            if !screen.is_empty() {
                return Some(screen);
            }
            let name = a.name.trim();
            if !name.is_empty() {
                return Some(name);
            }
            None
        })
        .unwrap_or("unknown")
        .to_string()
}

/// Canonical post URL when the API omits `url`: `id` is an X snowflake.
fn fallback_status_url(id: &str) -> Option<String> {
    let id = id.trim();
    if (2..=20).contains(&id.len()) && id.bytes().all(|b| b.is_ascii_digit()) {
        Some(format!("https://x.com/i/status/{id}"))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"code":200,"results":[
      {"type":"status","id":"1234567890123456789","url":"https://twitter.com/alice/status/1234567890123456789",
       "text":"Tokio 1.0 is out","created_at":"Sun Sep 20 07:57:08 +0000 2026",
       "likes":512,"reposts":44,"replies":12,
       "author":{"name":"Alice","screen_name":"alice"}},
      {"type":"status","id":"9876543210987654321","url":"",
       "text":"media-only post","likes":3,"reposts":0,"replies":1,
       "author":{"name":"","screen_name":""}},
      {"type":"status","id":"not-a-snowflake","url":"ftp://x/y","text":"unlinkable"}
    ],"cursor":{"top":null,"bottom":"abc"}}"#;

    #[test]
    fn parses_search_results_and_falls_back_to_status_url() {
        let r = parse_results(FIXTURE).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "@alice");
        assert_eq!(
            r[0].url,
            "https://twitter.com/alice/status/1234567890123456789"
        );
        assert_eq!(
            r[0].snippet,
            "Tokio 1.0 is out · 512 likes · 44 reposts · 12 replies"
        );
        assert_eq!(r[0].published.as_deref(), Some("2026-09-20T07:57:08+00:00"));
        assert_eq!(r[1].published, None, "missing created_at stays unknown");
        // Missing URL + numeric id -> canonical x.com status link.
        assert_eq!(r[1].url, "https://x.com/i/status/9876543210987654321");
        assert_eq!(r[1].title, "@unknown");
        // The third item has neither a usable URL nor a numeric id: dropped.
    }

    #[test]
    fn mirrored_code_is_checked_even_on_http_200() {
        let ok = parse_results(r#"{"code":200,"results":[]}"#).unwrap();
        assert!(ok.is_empty());
        let err = parse_results(r#"{"code":500,"results":[]}"#).unwrap_err();
        assert_eq!(err.kind(), "http");
        assert!(matches!(err, Error::Http(500)));
        let err = parse_results(r#"{"code":429,"results":[]}"#).unwrap_err();
        assert_eq!(err.kind(), "rate_limited");
    }

    #[test]
    fn malformed_bodies_are_parse_errors() {
        assert!(parse_results("not json").is_err());
        assert!(parse_results(r#"{"code":200}"#).is_err());
        assert!(parse_results(r#"{"results":[]}"#).is_err());
    }

    #[test]
    fn queries_are_validated_before_any_request() {
        assert!(checked_query("rust").is_ok());
        assert!(checked_query("  ").is_err());
        assert!(checked_query(&"x".repeat(513)).is_err());
        assert!(checked_query(&"x".repeat(512)).is_ok());
    }

    #[test]
    fn self_host_override_replaces_the_public_endpoint() {
        let engine = FxTwitter::default();
        let plain = SearchOpts::default();
        assert_eq!(
            engine.search_endpoint(&plain).unwrap(),
            "https://api.fxtwitter.com/2/search"
        );
        let custom = SearchOpts {
            fxtwitter_base_url: Some("https://fx.example.com/".into()),
            ..SearchOpts::default()
        };
        assert_eq!(
            engine.search_endpoint(&custom).unwrap(),
            "https://fx.example.com/2/search"
        );
        for bad in [
            "ftp://fx.example.com",
            "https://u:p@fx.example.com",
            "not a url",
        ] {
            let opts = SearchOpts {
                fxtwitter_base_url: Some(bad.into()),
                ..SearchOpts::default()
            };
            assert!(
                engine.search_endpoint(&opts).is_err(),
                "{bad} must be rejected"
            );
        }
    }

    #[test]
    fn feed_defaults_to_latest_and_rejects_unknown() {
        assert_eq!(checked_feed(None).unwrap(), "latest");
        assert_eq!(checked_feed(Some("  ")).unwrap(), "latest");
        assert_eq!(checked_feed(Some("top")).unwrap(), "top");
        assert_eq!(checked_feed(Some("media")).unwrap(), "media");
        assert!(checked_feed(Some("hot")).is_err());
    }

    #[test]
    fn fallback_url_requires_a_snowflake() {
        assert_eq!(
            fallback_status_url("1234567890123456789").as_deref(),
            Some("https://x.com/i/status/1234567890123456789")
        );
        assert_eq!(fallback_status_url(""), None);
        assert_eq!(fallback_status_url("1"), None);
        assert_eq!(fallback_status_url("12a45"), None);
        assert_eq!(fallback_status_url(&"1".repeat(21)), None);
    }
}
