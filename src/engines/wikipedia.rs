//! Wikipedia search via the MediaWiki action API (no key, stable JSON).
//!
//! Language editions are addressed by subdomain: `--lang ja` searches
//! `ja.wikipedia.org`. `--lang` is validated against a strict label shape
//! before it ever reaches a URL host — an unchecked value let a caller build
//! a request to a host other than Wikipedia (e.g. a lang value containing a
//! port spec or a slash). Titles are placed with `Url::path_segments_mut`
//! rather than string concatenation, so a title containing `/`, `?` or `#`
//! becomes one correctly percent-encoded path segment instead of reshaping
//! the URL. Snippets carry light `<span class="searchmatch">` markup which we
//! strip to plain text.

use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
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
    #[serde(default)]
    query: Option<Query>,
    #[serde(default)]
    error: Option<ApiError>,
}
#[derive(Deserialize)]
struct Query {
    #[serde(default)]
    search: Vec<Hit>,
}
#[derive(Deserialize)]
struct ApiError {
    #[serde(default)]
    code: String,
    #[serde(default)]
    info: String,
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

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let lang = opts.lang.as_deref().unwrap_or("en");
        if !is_valid_wiki_lang(lang) {
            return Err(Error::Usage(format!(
                "invalid --lang '{lang}' for wikipedia (expected a short \
                 language-subdomain label, e.g. 'en', 'ja', 'zh-yue')"
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

        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        parse_results(&body.text, lang)
    }
}

/// A Wikipedia language subdomain label: one lowercase letter, then up to 11
/// more lowercase letters/digits/hyphens (covers real editions like `en`,
/// `ja`, `zh-yue`, `simple`, `be-tarask`) — never long enough or shaped like
/// something that could carry a port or path component.
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
}

/// Pure parser (unit-tested against fixtures). Distinguishes "zero hits" from
/// "the API returned something we don't understand": a JSON parse failure, an
/// explicit `{"error": ...}` object, or a response missing `query` entirely
/// are all errors, not an empty result list.
pub fn parse_results(body: &str, lang: &str) -> Result<Vec<SearchResult>> {
    let resp: ApiResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid Wikipedia API response: {e}")))?;
    if let Some(err) = resp.error {
        return Err(Error::Parse(format!(
            "Wikipedia API error {}: {}",
            err.code, err.info
        )));
    }
    let query = resp.query.ok_or_else(|| {
        Error::Parse("Wikipedia response missing `query` (unexpected API shape)".into())
    })?;
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

/// `https://{lang}.wikipedia.org/wiki/{Title_With_Underscores}`, built as a
/// single percent-encoded path segment so a title containing `/`, `?` or `#`
/// cannot reshape the URL's path/query/fragment.
fn wiki_url(lang: &str, title: &str) -> String {
    let segment = title.replace(' ', "_");
    let fallback = || format!("https://{lang}.wikipedia.org/wiki/{segment}");
    let Ok(mut url) = Url::parse(&format!("https://{lang}.wikipedia.org")) else {
        return fallback();
    };
    // Scoped so the mutable borrow of `url` ends before `url.to_string()`
    // needs an immutable one.
    let pushed = if let Ok(mut segs) = url.path_segments_mut() {
        segs.push("wiki");
        segs.push(&segment);
        true
    } else {
        false
    };
    if !pushed {
        // Unreachable in practice: an "https://host" URL can always be a base.
        return fallback();
    }
    url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::blocking::Client;

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
        // A literal '/' in the title is now a real path-segment boundary
        // character, so it is percent-encoded rather than becoming an extra
        // path segment (the old string-concat implementation produced
        // ".../wiki/Async/await", which is not the same resource path).
        assert_eq!(r[1].url, "https://en.wikipedia.org/wiki/Async%2Fawait");
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
    fn query_and_fragment_characters_in_titles_stay_inside_the_segment() {
        let r = parse_results(
            r#"{"query":{"search":[{"title":"What? #1","snippet":""}]}}"#,
            "en",
        )
        .unwrap();
        let url = Url::parse(&r[0].url).unwrap();
        assert_eq!(url.query(), None, "url: {}", r[0].url);
        assert_eq!(url.fragment(), None, "url: {}", r[0].url);
        assert!(url.path().starts_with("/wiki/What"));
    }

    #[test]
    fn malformed_json_is_an_error() {
        let err = parse_results("not json", "en").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn missing_query_field_is_an_error_not_empty_results() {
        let err = parse_results("{}", "en").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn genuinely_empty_search_array_is_ok() {
        let r = parse_results(r#"{"query":{"search":[]}}"#, "en").unwrap();
        assert!(r.is_empty());
    }

    #[test]
    fn api_error_object_is_surfaced() {
        let err = parse_results(
            r#"{"error":{"code":"badsearch","info":"bad srsearch"}}"#,
            "en",
        )
        .unwrap_err();
        assert!(err.to_string().contains("badsearch"));
    }

    #[test]
    fn lang_validation_rejects_hostile_values() {
        assert!(!is_valid_wiki_lang("en/../evil"));
        assert!(!is_valid_wiki_lang("en:8080"));
        assert!(!is_valid_wiki_lang("EN"));
        assert!(!is_valid_wiki_lang(""));
        assert!(!is_valid_wiki_lang("waaaaaaaaaaaaay-too-long"));
        assert!(is_valid_wiki_lang("en"));
        assert!(is_valid_wiki_lang("zh-yue"));
        assert!(is_valid_wiki_lang("be-tarask"));
    }

    #[test]
    fn search_rejects_invalid_lang_before_any_network_call() {
        let http = Http::for_tests(
            Client::builder()
                .user_agent("webseek-test")
                .build()
                .unwrap(),
        );
        let engine = Wikipedia::with_base("https://unused.invalid/api.php");
        let opts = SearchOpts {
            count: 5,
            lang: Some("en/evil".into()),
            region: None,
            safe: false,
        };
        let err = engine.search(&http, "x", &opts).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
