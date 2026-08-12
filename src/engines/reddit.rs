//! Reddit search via the public RSS/Atom feed (no key).
//!
//! Reddit's JSON endpoints return 403 and its official API requires OAuth
//! credentials, so — like Bing — we use the published **RSS** output
//! (`search.rss`), which returns a stable Atom feed without a key.
//!
//! ⚠️ Reddit rate-limits aggressively (bursts get HTTP 429). The transport
//! layer retries 429 with backoff, but keep request volume low (the global
//! `delay` helps); this engine suits occasional queries, not bulk scraping.

use reqwest::blocking::Client;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::feed;
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> =
            vec![("q", query), ("sort", "relevance"), ("limit", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("reddit request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        parse_results(&body)
    }
}

/// Parse a Reddit Atom feed with namespace and attribute awareness. A
/// malformed/non-feed body is a typed parse error rather than zero results.
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    Ok(feed::parse_entries(body)?
        .into_iter()
        .filter(|entry| !entry.link.is_empty())
        .map(|entry| {
            let mut snippet = String::new();
            if !entry.category.is_empty() {
                snippet.push_str(&entry.category);
                snippet.push_str(" · ");
            }
            if !entry.author.is_empty() {
                snippet.push_str("by ");
                snippet.push_str(&entry.author);
                snippet.push_str(" · ");
            }
            snippet.push_str(&strip_html(&entry.summary));
            SearchResult {
                title: entry.title,
                url: entry.link,
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
        // Snippet carries subreddit + author + cleaned content (tags stripped).
        assert!(r[0].snippet.starts_with("r/rust · by /u/alice ·"));
        assert!(r[0].snippet.contains("Some & content"));
        // The escaped <a href=...> inside content must not leak as a tag/URL.
        assert!(!r[0].snippet.contains("<a"));
    }

    #[test]
    fn ignores_feed_level_title_and_bad_input() {
        // The feed-level <title> is not inside an <entry>, so it's not a result.
        assert_eq!(parse_results(FIXTURE).unwrap().len(), 1);
        assert!(parse_results("not a feed").is_err());
    }

    #[test]
    fn namespaces_and_attribute_order_are_supported() {
        let body = r#"<atom:feed xmlns:atom="http://www.w3.org/2005/Atom">
          <atom:entry><atom:author><atom:name>/u/bob</atom:name></atom:author>
          <atom:link rel="self" href="https://api.reddit.com/self"/>
          <atom:link href="https://www.reddit.com/r/x/comments/1/y/"/>
          <atom:title>Prefixed entry</atom:title></atom:entry></atom:feed>"#;
        let results = parse_results(body).unwrap();
        assert_eq!(results[0].url, "https://www.reddit.com/r/x/comments/1/y/");
        assert!(results[0].snippet.contains("/u/bob"));
    }
}
