//! Reddit search via the public RSS/Atom feed (no key).
//!
//! Reddit's JSON endpoints return 403 and its official API requires OAuth
//! credentials, so — like Bing — we use the published **RSS** output
//! (`search.rss`), which returns a stable Atom feed without a key. Parsed
//! with the shared [`crate::feed`] XML parser rather than string scanning,
//! so namespaced tags, attribute ordering, and `data-href`-style false
//! matches are no longer a concern.
//!
//! ⚠️ Reddit rate-limits aggressively (bursts get HTTP 429). The transport
//! layer retries 429 with backoff, but keep request volume low (the shared
//! rate limiter and global `delay` help); this engine suits occasional
//! queries, not bulk scraping.

use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::feed;
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};
use crate::text::{normalize_snippet, strip_html};

const SEARCH_URL: &str = "https://www.reddit.com/search.rss";

pub struct Reddit {
    base: String,
}

impl Default for Reddit {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl Reddit {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl SearchEngine for Reddit {
    fn name(&self) -> &'static str {
        "reddit"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> =
            vec![("q", query), ("sort", "relevance"), ("limit", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        parse_results(&body.text)
    }
}

/// Parse a Reddit Atom feed. A malformed or non-feed body is a typed parse
/// error, not zero results.
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    let entries = feed::parse_entries(body)?;
    Ok(entries
        .into_iter()
        .filter(|e| !e.link.is_empty())
        .map(|e| {
            let content = strip_html(&e.summary);
            let mut snippet = String::new();
            if !e.category.is_empty() {
                snippet.push_str(&e.category);
                snippet.push_str(" · ");
            }
            if !e.author.is_empty() {
                snippet.push_str("by ");
                snippet.push_str(&e.author);
                snippet.push_str(" · ");
            }
            snippet.push_str(&content);
            SearchResult {
                title: e.title,
                url: e.link,
                snippet: normalize_snippet(&snippet),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<?xml version="1.0"?><feed xmlns="http://www.w3.org/2005/Atom">
      <title>search results</title>
      <entry>
        <author><name>/u/alice</name><uri>https://www.reddit.com/user/alice</uri></author>
        <category term="rust" label="r/rust"/>
        <content type="html">&lt;p&gt;Some &amp;amp; content &lt;a href=&quot;http://x&quot;&gt;link&lt;/a&gt;&lt;/p&gt;</content>
        <id>t3_abc</id>
        <link href="https://www.reddit.com/r/rust/comments/abc/post/"/>
        <updated>2026-08-02T10:00:00+00:00</updated>
        <title>A post about Rust &amp; async</title>
      </entry>
    </feed>"#;

    #[test]
    fn parses_reddit_atom_entries() {
        let r = parse_results(FIXTURE).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "A post about Rust & async");
        assert_eq!(r[0].url, "https://www.reddit.com/r/rust/comments/abc/post/");
        assert!(r[0].snippet.starts_with("r/rust · by /u/alice ·"));
        assert!(r[0].snippet.contains("Some & content"));
        assert!(!r[0].snippet.contains("<a"));
    }

    #[test]
    fn feed_level_title_is_not_a_result() {
        assert_eq!(parse_results(FIXTURE).unwrap().len(), 1);
    }

    #[test]
    fn non_feed_body_is_an_error() {
        let err = parse_results("not a feed").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn attributes_and_namespace_prefixes_do_not_break_parsing() {
        let body = r#"<atom:feed xmlns:atom="http://www.w3.org/2005/Atom" xml:lang="en">
          <atom:entry>
            <atom:author><atom:name>/u/bob</atom:name></atom:author>
            <atom:link rel="self" href="https://api.reddit.com/self"/>
            <atom:link href="https://www.reddit.com/r/x/comments/1/y/"/>
            <atom:title>Prefixed entry</atom:title>
          </atom:entry>
        </atom:feed>"#;
        let r = parse_results(body).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].url, "https://www.reddit.com/r/x/comments/1/y/");
        assert!(r[0].snippet.contains("/u/bob"));
    }

    #[test]
    fn data_href_inside_content_is_not_mistaken_for_the_link() {
        let body = r#"<feed><entry>
          <content type="html">&lt;div data-href="https://evil.example"&gt;x&lt;/div&gt;</content>
          <link href="https://www.reddit.com/r/rust/comments/z/z/"/>
          <title>T</title>
        </entry></feed>"#;
        let r = parse_results(body).unwrap();
        assert_eq!(r[0].url, "https://www.reddit.com/r/rust/comments/z/z/");
    }
}
