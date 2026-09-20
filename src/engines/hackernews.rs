//! Hacker News search via the Algolia HN API (no key, stable JSON).
//!
//! Great for "what's new / what's discussed" in tech. Stories without an
//! external URL (Ask HN, Show HN) link back to the HN item page.

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::{dedupe_and_truncate, SearchEngine};
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{join_meta, normalize_snippet};

const SEARCH_URL: &str = "https://hn.algolia.com/api/v1/search";

pub struct HackerNews {
    base: String,
}

impl Default for HackerNews {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl HackerNews {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct Resp {
    hits: Vec<Hit>,
}
#[derive(Deserialize)]
struct Hit {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    story_title: Option<String>,
    #[serde(default)]
    url: Option<String>,
    #[serde(default, rename = "objectID")]
    object_id: String,
    #[serde(default)]
    points: Option<i64>,
    #[serde(default)]
    num_comments: Option<i64>,
    #[serde(default)]
    author: Option<String>,
    #[serde(default, rename = "created_at_i")]
    created_at: Option<i64>,
}

impl SearchEngine for HackerNews {
    fn name(&self) -> &'static str {
        "hackernews"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let mut params: Vec<(&str, &str)> =
            vec![("query", query), ("hitsPerPage", &limit), ("tags", "story")];
        // Pushed to Algolia: filtering client-side on relevance order can
        // return nothing while recent matches sit past the count cutoff.
        // The generic post-filter still enforces the exact window.
        let filters = numeric_filters(opts)?;
        if !filters.is_empty() {
            params.push(("numericFilters", &filters));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("hackernews request failed: {e}")))?;
        crate::http::api_status(resp.status().as_u16())?;
        let body = crate::http::response_text(resp)?;
        Ok(dedupe_and_truncate(
            parse_results(&body)?,
            opts.count,
            |r| &r.url,
        ))
    }
}

/// `created_at_i` range for `--since`/`--until`, matching the generic
/// `[since, until)` window. Garbage bounds are a config error, never a
/// silent no-filter.
fn numeric_filters(opts: &SearchOpts) -> Result<String> {
    let (since, until) = crate::time::parse_window(
        opts.since.as_deref(),
        opts.until.as_deref(),
        "--since",
        "--until",
    )?;
    let mut parts = Vec::new();
    if let Some(s) = since {
        parts.push(format!("created_at_i>={}", s.timestamp()));
    }
    if let Some(u) = until {
        parts.push(format!("created_at_i<{}", u.timestamp()));
    }
    Ok(parts.join(","))
}

/// Pure parser (unit-tested against fixtures).
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<Resp>(body)
        .map_err(|e| Error::Parse(format!("hackernews response is not valid JSON: {e}")))?;
    Ok(resp
        .hits
        .into_iter()
        .filter_map(|h| {
            let title = h
                .title
                .filter(|t| !t.is_empty())
                .or_else(|| h.story_title.filter(|t| !t.is_empty()))?;
            let url = h
                .url
                .filter(|u| !u.is_empty())
                .unwrap_or_else(|| format!("https://news.ycombinator.com/item?id={}", h.object_id));
            let snippet = join_meta(&[
                &format!("{} points", h.points.unwrap_or(0)),
                &format!("{} comments", h.num_comments.unwrap_or(0)),
                &format!("by {}", h.author.as_deref().unwrap_or("?")),
            ]);
            Some(SearchResult {
                title: normalize_snippet(&title),
                url,
                snippet: normalize_snippet(&snippet),
                published: h.created_at.and_then(crate::time::unix_to_rfc3339),
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"hits":[
      {"objectID":"1","title":"Tokio 1.0","url":"https://tokio.rs/blog","author":"carl","points":512,"num_comments":230},
      {"objectID":"2","title":"Ask HN: favorite runtime?","url":null,"author":"dev","points":42,"num_comments":17}
    ]}"#;

    #[test]
    fn parses_hn_hits_and_falls_back_to_item_url() {
        let r = parse_results(FIXTURE).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Tokio 1.0");
        assert_eq!(r[0].url, "https://tokio.rs/blog");
        assert_eq!(r[0].snippet, "512 points · 230 comments · by carl");
        // Ask HN has no external URL -> link to the HN item.
        assert_eq!(r[1].url, "https://news.ycombinator.com/item?id=2");
    }

    #[test]
    fn skips_titleless_hits_and_bad_json() {
        assert!(parse_results(r#"{"hits":[{"objectID":"9","title":null}]}"#)
            .unwrap()
            .is_empty());
        assert!(parse_results("nope").is_err());
    }
}
