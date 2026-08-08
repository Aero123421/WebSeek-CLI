//! Hacker News search via the Algolia HN API (no key, stable JSON).
//!
//! Great for "what's new / what's discussed" in tech. Stories without an
//! external URL (Ask HN, Show HN) link back to the HN item page; comment
//! hits (which have no `title` at all) are skipped rather than silently
//! dropped by an unchecked `Option`.

use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};

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
    #[serde(default)]
    hits: Vec<Hit>,
}
#[derive(Deserialize)]
struct Hit {
    #[serde(default)]
    title: Option<String>,
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
    /// Present on comment hits, which have no `title`. Kept so a query that
    /// matches only comments doesn't quietly vanish — it still links to the
    /// discussion, with the comment text as the snippet.
    #[serde(default)]
    comment_text: Option<String>,
    #[serde(default)]
    story_title: Option<String>,
    #[serde(default)]
    story_id: Option<i64>,
}

impl SearchEngine for HackerNews {
    fn name(&self) -> &'static str {
        "hackernews"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![("query", query), ("hitsPerPage", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        parse_results(&body.text)
    }
}

/// Pure parser (unit-tested against fixtures).
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    let resp: Resp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid Hacker News API response: {e}")))?;
    Ok(resp
        .hits
        .into_iter()
        .filter_map(|h| {
            if let Some(title) = h.title.filter(|t| !t.is_empty()) {
                let url = h
                    .url
                    .filter(|u| !u.is_empty())
                    .unwrap_or_else(|| item_url(&h.object_id));
                let snippet = format!(
                    "{} points · {} comments · by {}",
                    h.points.unwrap_or(0),
                    h.num_comments.unwrap_or(0),
                    h.author.as_deref().unwrap_or("?")
                );
                return Some(SearchResult {
                    title,
                    url,
                    snippet,
                });
            }
            // Comment hit: no `title`, but still a real, useful result.
            let comment = h.comment_text.filter(|t| !t.is_empty())?;
            let story = h.story_title.unwrap_or_else(|| "a discussion".to_string());
            let target = h
                .story_id
                .map(|id| item_url(&id.to_string()))
                .unwrap_or_else(|| item_url(&h.object_id));
            Some(SearchResult {
                title: format!("Comment on: {story}"),
                url: target,
                snippet: crate::text::normalize_snippet(&crate::text::strip_html(&comment)),
            })
        })
        .collect())
}

fn item_url(id: &str) -> String {
    format!("https://news.ycombinator.com/item?id={id}")
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
        assert_eq!(r[1].url, "https://news.ycombinator.com/item?id=2");
    }

    #[test]
    fn comment_hits_are_returned_not_dropped() {
        let body = r#"{"hits":[
          {"objectID":"99","title":null,"comment_text":"Use tokio, it is great.","story_title":"Async runtimes?","story_id":42,"author":"jo"}
        ]}"#;
        let r = parse_results(body).unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].title.contains("Async runtimes?"));
        assert_eq!(r[0].url, "https://news.ycombinator.com/item?id=42");
        assert!(r[0].snippet.contains("Use tokio"));
    }

    #[test]
    fn titleless_non_comment_hits_are_skipped() {
        let r = parse_results(r#"{"hits":[{"objectID":"9","title":null}]}"#).unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn malformed_json_is_an_error() {
        let err = parse_results("nope").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }
}
