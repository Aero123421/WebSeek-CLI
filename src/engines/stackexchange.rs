//! Stack Exchange search (Stack Overflow by default) via the official API.
//!
//! No key required for modest use (quota ~300 req/day). Titles come back
//! HTML-escaped; we unescape them. Snippet summarizes tags/score/answers so an
//! agent can judge relevance without opening the page. The API's own
//! `backoff` hint (seconds to wait before the next call) is fed into the
//! shared rate limiter so the *next* Stack Exchange request actually waits,
//! and an `error_id`/`error_message` response body is surfaced as a typed
//! error instead of being parsed into zero results.

use serde::Deserialize;
use std::time::Duration;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};
use crate::ratelimit;
use crate::text::unescape_entities;

const SEARCH_URL: &str = "https://api.stackexchange.com/2.3/search/advanced";

pub struct StackExchange {
    base: String,
    /// Target site, e.g. "stackoverflow".
    site: String,
}

impl Default for StackExchange {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
            site: "stackoverflow".to_string(),
        }
    }
}

impl StackExchange {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            base: base.into(),
            site: "stackoverflow".to_string(),
        }
    }
}

#[derive(Deserialize)]
struct Resp {
    #[serde(default)]
    items: Vec<Item>,
    #[serde(default)]
    backoff: Option<u64>,
    #[serde(default)]
    error_id: Option<i64>,
    #[serde(default)]
    error_message: Option<String>,
    #[serde(default)]
    error_name: Option<String>,
}
#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    title: String,
    #[serde(default)]
    link: String,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    score: i64,
    #[serde(default)]
    answer_count: i64,
    #[serde(default)]
    is_answered: bool,
}

impl SearchEngine for StackExchange {
    fn name(&self) -> &'static str {
        "stackexchange"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![
            ("order", "desc"),
            ("sort", "relevance"),
            ("q", query),
            ("site", &self.site),
            ("pagesize", &limit),
        ];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let origin = ratelimit::origin_of(&url);

        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        let (results, backoff) = parse_results(&body.text)?;
        if let Some(secs) = backoff {
            http.limiter().penalize(&origin, Duration::from_secs(secs));
        }
        Ok(results)
    }
}

/// Pure parser (unit-tested against fixtures). Returns results plus an
/// optional `backoff` hint so the caller can feed it to the rate limiter.
pub fn parse_results(body: &str) -> Result<(Vec<SearchResult>, Option<u64>)> {
    let resp: Resp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid Stack Exchange API response: {e}")))?;
    if let Some(id) = resp.error_id {
        let name = resp.error_name.unwrap_or_default();
        let msg = resp.error_message.unwrap_or_default();
        if name.contains("throttle") || id == 502 {
            return Err(Error::rate_limited(format!(
                "Stack Exchange API throttled the request: {msg}"
            )));
        }
        return Err(Error::Parse(format!(
            "Stack Exchange API error {id} ({name}): {msg}"
        )));
    }
    let results = resp
        .items
        .into_iter()
        .filter(|it| !it.link.is_empty())
        .map(|it| {
            let answered = if it.is_answered { "✓" } else { "–" };
            let snippet = format!(
                "[{}] · score {} · {} answers {}",
                it.tags.join(", "),
                it.score,
                it.answer_count,
                answered
            );
            SearchResult {
                title: unescape_entities(&it.title),
                url: it.link,
                snippet,
            }
        })
        .collect();
    Ok((results, resp.backoff))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"items":[
      {"title":"How to synchronise async runtimes","link":"https://stackoverflow.com/q/1","tags":["rust","async"],"score":12,"answer_count":3,"is_answered":true},
      {"title":"Why is my &#39;await&#39; blocked?","link":"https://stackoverflow.com/q/2","tags":["rust"],"score":4,"answer_count":0,"is_answered":false}
    ],"backoff":5}"#;

    #[test]
    fn parses_questions_with_summary_snippet_and_backoff() {
        let (r, backoff) = parse_results(FIXTURE).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "How to synchronise async runtimes");
        assert_eq!(r[0].url, "https://stackoverflow.com/q/1");
        assert_eq!(r[0].snippet, "[rust, async] · score 12 · 3 answers ✓");
        assert_eq!(r[1].title, "Why is my 'await' blocked?");
        assert!(r[1].snippet.ends_with('–'));
        assert_eq!(backoff, Some(5));
    }

    #[test]
    fn throttle_error_is_rate_limited_not_empty_results() {
        let body = r#"{"error_id":502,"error_message":"too many requests from this IP, more info at ...","error_name":"throttle_violation"}"#;
        let err = parse_results(body).unwrap_err();
        assert_eq!(err.code(), "upstream_rate_limited");
    }

    #[test]
    fn other_api_errors_are_surfaced() {
        let body =
            r#"{"error_id":400,"error_message":"invalid site","error_name":"bad_parameter"}"#;
        let err = parse_results(body).unwrap_err();
        assert_eq!(err.code(), "parse_failed");
        assert!(err.to_string().contains("invalid site"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let err = parse_results("nope").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn no_backoff_means_none() {
        let (_, backoff) = parse_results(r#"{"items":[]}"#).unwrap();
        assert_eq!(backoff, None);
    }
}
