//! Wikipedia search via the MediaWiki action API (no key, stable JSON).
//!
//! Language editions are addressed by subdomain: `--lang ja` searches
//! `ja.wikipedia.org`. Snippets carry light `<span class="searchmatch">`
//! markup which we strip to plain text.

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{normalize_snippet, strip_html};

#[derive(Default)]
pub struct Wikipedia {
    /// Explicit endpoint for tests; `None` derives the per-language endpoint.
    endpoint: Option<String>,
}

impl Wikipedia {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self {
            endpoint: Some(base.into()),
        }
    }
}

#[derive(Deserialize)]
struct ApiResp {
    query: Option<Query>,
}
#[derive(Deserialize)]
struct Query {
    #[serde(default)]
    search: Vec<Hit>,
}
#[derive(Deserialize)]
struct Hit {
    title: String,
    #[serde(default)]
    snippet: String,
}

impl SearchEngine for Wikipedia {
    fn name(&self) -> &'static str {
        "wikipedia"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let lang = opts.lang.as_deref().unwrap_or("en");
        if !is_valid_wiki_lang(lang) {
            return Err(Error::Config(format!(
                "invalid --lang '{lang}' for wikipedia (expected a short language label such as en, ja, or zh-yue)"
            )));
        }
        let base = self
            .endpoint
            .clone()
            .unwrap_or_else(|| format!("https://{lang}.wikipedia.org/w/api.php"));
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![
            ("action", "query"),
            ("list", "search"),
            ("srsearch", query),
            ("srprop", "snippet"),
            ("srlimit", &limit),
            ("format", "json"),
            ("utf8", "1"),
        ];
        let url = Url::parse_with_params(&base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("wikipedia request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        parse_results(&body, lang)
    }
}

fn is_valid_wiki_lang(lang: &str) -> bool {
    let mut chars = lang.chars();
    if !matches!(chars.next(), Some(c) if c.is_ascii_lowercase()) {
        return false;
    }
    let rest: Vec<char> = chars.collect();
    rest.len() <= 11
        && rest
            .iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-')
        && !lang.ends_with('-')
}

/// Pure parser (unit-tested against fixtures).
pub fn parse_results(body: &str, lang: &str) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<ApiResp>(body)
        .map_err(|e| Error::Parse(format!("wikipedia response is not valid JSON: {e}")))?;
    let query = resp
        .query
        .ok_or_else(|| Error::Parse("wikipedia response omitted query.search".into()))?;
    Ok(query
        .search
        .into_iter()
        .map(|h| SearchResult {
            title: h.title.clone(),
            url: wiki_url(lang, &h.title),
            snippet: normalize_snippet(&strip_html(&h.snippet)),
        })
        .collect())
}

/// `https://{lang}.wikipedia.org/wiki/{Title_With_Underscores}`.
///
/// The title is percent-encoded as a single path segment. Interpolating it
/// raw and letting `Url::parse` sort it out silently reinterpreted `#` and `?`
/// as a fragment or query — the article "C#" became a link to "C".
fn wiki_url(lang: &str, title: &str) -> String {
    format!(
        "https://{lang}.wikipedia.org/wiki/{}",
        crate::text::encode_path_keep_slashes(&title.replace(' ', "_"))
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"{"query":{"searchinfo":{"totalhits":2},"search":[
      {"ns":0,"title":"Tokio (software)","pageid":1,"snippet":"an <span class=\"searchmatch\">async</span> runtime &amp; more"},
      {"ns":0,"title":"Async/await","pageid":2,"snippet":"the <span class=\"searchmatch\">async</span>/await pattern"}
    ]}}"#;

    #[test]
    fn parses_wikipedia_results_and_strips_markup() {
        let r = parse_results(FIXTURE, "en").unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Tokio (software)");
        assert_eq!(r[0].url, "https://en.wikipedia.org/wiki/Tokio_(software)");
        assert_eq!(r[0].snippet, "an async runtime & more");
        assert_eq!(r[1].url, "https://en.wikipedia.org/wiki/Async/await");
    }

    #[test]
    fn titles_with_url_syntax_stay_on_the_right_article() {
        let r = parse_results(
            r#"{"query":{"search":[
                {"title":"C#","snippet":""},
                {"title":"Who's Next?","snippet":""},
                {"title":"Rust (programming language)","snippet":""}
            ]}}"#,
            "en",
        )
        .unwrap();
        // "C#" used to resolve to .../wiki/C with an empty fragment.
        assert_eq!(r[0].url, "https://en.wikipedia.org/wiki/C%23");
        assert_eq!(r[1].url, "https://en.wikipedia.org/wiki/Who's_Next%3F");
        // Parentheses are legal in a path and must not be over-escaped.
        assert_eq!(
            r[2].url,
            "https://en.wikipedia.org/wiki/Rust_(programming_language)"
        );
        for hit in &r {
            let parsed = url::Url::parse(&hit.url).unwrap();
            assert!(parsed.fragment().is_none(), "{}", hit.url);
            assert!(parsed.query().is_none(), "{}", hit.url);
        }
    }

    #[test]
    fn non_ascii_titles_are_encoded() {
        let r = parse_results(
            r#"{"query":{"search":[{"title":"東京タワー","snippet":""}]}}"#,
            "ja",
        )
        .unwrap();
        assert_eq!(r.len(), 1);
        assert!(r[0].url.starts_with("https://ja.wikipedia.org/wiki/"));
        assert!(r[0].url.contains("%E6%9D%B1")); // 東 in UTF-8 percent-encoding
    }

    #[test]
    fn malformed_json_is_an_error() {
        assert!(parse_results("not json", "en").is_err());
        assert!(parse_results("{}", "en").is_err());
    }

    #[test]
    fn language_cannot_reshape_the_request_host() {
        for lang in ["EN", "en.example.com", "en:443", "../en", "en-"] {
            assert!(!is_valid_wiki_lang(lang), "{lang}");
        }
        for lang in ["en", "ja", "zh-yue", "be-tarask"] {
            assert!(is_valid_wiki_lang(lang), "{lang}");
        }
    }
}
