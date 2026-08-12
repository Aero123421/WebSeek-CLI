//! Stack Exchange search (Stack Overflow by default) via the official API.
//!
//! No key required for modest use (quota ~300 req/day). Titles come back
//! HTML-escaped; we unescape them. Snippet summarizes tags/score/answers so an
//! agent can judge relevance without opening the page.

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{join_meta, normalize_snippet, unescape_entities};

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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
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

        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("stackexchange request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(parse_results(&body))
    }
}

/// Pure parser (unit-tested against fixtures).
pub fn parse_results(body: &str) -> Vec<SearchResult> {
    let Ok(resp) = serde_json::from_str::<Resp>(body) else {
        return Vec::new();
    };
    resp.items
        .into_iter()
        .filter(|it| !it.link.is_empty())
        .map(|it| {
            let answered = if it.is_answered { "✓" } else { "–" };
            let tags = if it.tags.is_empty() {
                String::new()
            } else {
                format!("[{}]", it.tags.join(", "))
            };
            let score = format!("score {}", it.score);
            let answers = format!("{} answers {answered}", it.answer_count);
            SearchResult {
                title: normalize_snippet(&unescape_entities(&it.title)),
                url: it.link,
                snippet: normalize_snippet(&join_meta(&[&tags, &score, &answers])),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"items":[
      {"title":"How to synchronise async runtimes","link":"https://stackoverflow.com/q/1","tags":["rust","async"],"score":12,"answer_count":3,"is_answered":true},
      {"title":"Why is my &#39;await&#39; blocked?","link":"https://stackoverflow.com/q/2","tags":["rust"],"score":4,"answer_count":0,"is_answered":false}
    ]}"#;

    #[test]
    fn parses_questions_with_summary_snippet() {
        let r = parse_results(FIXTURE);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "How to synchronise async runtimes");
        assert_eq!(r[0].url, "https://stackoverflow.com/q/1");
        assert_eq!(r[0].snippet, "[rust, async] · score 12 · 3 answers ✓");
        // HTML entities in titles are unescaped.
        assert_eq!(r[1].title, "Why is my 'await' blocked?");
        assert!(r[1].snippet.ends_with('–'));
    }

    #[test]
    fn bad_json_yields_empty() {
        assert!(parse_results("nope").is_empty());
    }
}
