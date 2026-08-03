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
use crate::models::{SearchOpts, SearchResult};
use crate::text::{extract_attr, extract_tag, normalize_snippet, strip_html, unescape_entities};

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

        let resp = crate::http::send_with_retry(&client.get(url))
            .map_err(|e| Error::Network(format!("reddit request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(parse_results(&body))
    }
}

/// Parse a Reddit Atom feed. Only `<entry>` blocks are read; escaped markup
/// inside `<content>` is ignored by the tag scanner (its `<` is `&lt;`).
pub fn parse_results(body: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start_rel) = rest.find("<entry>") {
        let after = &rest[start_rel + "<entry>".len()..];
        let Some(end_rel) = after.find("</entry>") else {
            break;
        };
        let entry = &after[..end_rel];
        rest = &after[end_rel + "</entry>".len()..];

        let url = extract_attr(entry, "href");
        if url.is_empty() {
            continue;
        }
        let title = unescape_entities(&extract_tag(entry, "title"));
        let author = extract_tag(entry, "name");
        let subreddit = extract_attr(entry, "label");
        // Content is double-escaped: the feed XML-escapes HTML that itself
        // contains entities. Unescape the feed layer first, then strip_html
        // (which unescapes the HTML entities and drops tags).
        let content = strip_html(&unescape_entities(&extract_tag(entry, "content")));

        let mut snippet = String::new();
        if !subreddit.is_empty() {
            snippet.push_str(&subreddit);
            snippet.push_str(" · ");
        }
        if !author.is_empty() {
            snippet.push_str("by ");
            snippet.push_str(&author);
            snippet.push_str(" · ");
        }
        snippet.push_str(&content);

        out.push(SearchResult {
            title,
            url,
            snippet: normalize_snippet(&snippet),
        });
    }
    out
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
        let r = parse_results(FIXTURE);
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
        assert_eq!(parse_results(FIXTURE).len(), 1);
        assert!(parse_results("not a feed").is_empty());
    }
}
