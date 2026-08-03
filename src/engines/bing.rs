//! Bing HTML search endpoint.
//!
//! No API key required. Result links are `bing.com/ck/a?...&u=<base64>` —
//! the `u` query parameter is base64-decode to the real URL.

use base64::{engine::general_purpose::STANDARD, Engine as _};
use reqwest::blocking::Client;
use scraper::{Html, Selector};
use url::Url;

use crate::engines::{dedupe_by_url, SearchEngine};
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::normalize_snippet;

pub struct Bing {
    /// Endpoint base; overridable for tests and mirrors.
    base: String,
}

const SEARCH_URL: &str = "https://www.bing.com/search";

impl Default for Bing {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl Bing {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl SearchEngine for Bing {
    fn name(&self) -> &'static str {
        "bing"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let cc = opts.region.as_deref().and_then(crate::region::bing_cc);
        let region_lang = opts
            .region
            .as_deref()
            .and_then(crate::region::bing_language);
        let lang = opts.lang.clone().or(region_lang);

        // Preferred path: the RSS endpoint. It is a stable, structured format
        // and — unlike the HTML page — is not behind a bot challenge.
        if let Some(results) = self.search_rss(client, query, &lang, cc.as_deref(), opts) {
            return Ok(results);
        }

        // Fallback: scrape the HTML result page.
        let mut params: Vec<(&str, &str)> = vec![("q", query), ("form", "QBLH")];
        if let Some(lang) = &lang {
            params.push(("setlang", lang.as_str()));
        }
        if let Some(cc) = &cc {
            params.push(("cc", cc.as_str()));
        }
        if opts.safe {
            params.push(("adlt", "strict"));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = crate::http::send_with_retry(&client.get(url))
            .map_err(|e| Error::Network(format!("bing request failed: {e}")))?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        if crate::engines::looks_like_challenge(&body) {
            return Err(Error::RateLimited(
                "bing served a bot-challenge page instead of results (IP reputation)".into(),
            ));
        }
        let mut results = parse_html(&body);
        results.truncate(opts.count);
        Ok(dedupe_by_url(results, |r| &r.url))
    }
}

impl Bing {
    /// Try the RSS endpoint; `None` means "RSS unusable, fall back to HTML".
    fn search_rss(
        &self,
        client: &Client,
        query: &str,
        lang: &Option<String>,
        cc: Option<&str>,
        opts: &SearchOpts,
    ) -> Option<Vec<SearchResult>> {
        let mut params: Vec<(&str, &str)> = vec![("q", query), ("format", "rss")];
        if let Some(lang) = lang {
            params.push(("setlang", lang.as_str()));
        }
        if let Some(cc) = cc {
            params.push(("cc", cc));
        }
        if opts.safe {
            params.push(("adlt", "strict"));
        }
        let url = Url::parse_with_params(&self.base, &params).ok()?;
        let resp = crate::http::send_with_retry(&client.get(url)).ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().ok()?;
        let mut results = parse_rss(&body);
        if results.is_empty() {
            return None;
        }
        results.truncate(opts.count);
        Some(dedupe_by_url(results, |r| &r.url))
    }
}

/// Pure parser for the Bing HTML result page. Unit-tested against fixtures.
pub fn parse_html(html: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let algo_sel = Selector::parse("li.b_algo").unwrap_or_else(|_| unreachable!("static"));
    let link_sel = Selector::parse("h2 a").unwrap_or_else(|_| unreachable!("static"));
    let snippet_sel = Selector::parse("p, .b_lineclamp2, .b_caption p")
        .unwrap_or_else(|_| unreachable!("static"));

    let mut out = Vec::new();
    for li in doc.select(&algo_sel) {
        let mut title = String::new();
        let mut url = String::new();
        if let Some(a) = li.select(&link_sel).next() {
            title = a.text().collect::<String>().trim().to_string();
            if let Some(href) = a.value().attr("href") {
                url = decode_bing_url(href).unwrap_or_else(|| href.to_string());
            }
        }
        if title.is_empty() {
            continue;
        }
        let snippet = li
            .select(&snippet_sel)
            .next()
            .map(|s| s.text().collect::<String>())
            .unwrap_or_default();
        out.push(SearchResult {
            title,
            url,
            snippet: normalize_snippet(&snippet),
        });
    }
    out
}

/// Parse Bing's RSS output (`&format=rss`). RSS is a stable, structured
/// format and is not behind a bot challenge, so it is Bing's preferred path.
/// Only `<item>` segments are read, so channel-level `<title>`/`<link>` are
/// never mistaken for results.
pub fn parse_rss(body: &str) -> Vec<SearchResult> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start_rel) = rest.find("<item>") {
        let after = &rest[start_rel + "<item>".len()..];
        let Some(end_rel) = after.find("</item>") else {
            break;
        };
        let item = &after[..end_rel];
        rest = &after[end_rel + "</item>".len()..];

        let link = crate::text::extract_tag(item, "link");
        if link.is_empty() {
            continue;
        }
        let title = crate::text::extract_tag(item, "title");
        let desc = crate::text::extract_tag(item, "description");
        out.push(SearchResult {
            title: clean_text(&title),
            url: crate::text::unescape_entities(link.trim()),
            snippet: normalize_snippet(&clean_text(&desc)),
        });
    }
    out
}

/// Strip an optional CDATA wrapper, then clean markup/entities to plain text.
fn clean_text(raw: &str) -> String {
    crate::text::strip_html(strip_cdata(raw))
}

fn strip_cdata(s: &str) -> &str {
    let t = s.trim();
    if let Some(inner) = t.strip_prefix("<![CDATA[") {
        inner.strip_suffix("]]>").unwrap_or(inner)
    } else {
        t
    }
}

/// Bing wraps external links in `bing.com/ck/a?...&u=<base64>` redirects.
fn decode_bing_url(href: &str) -> Option<String> {
    let url = Url::parse(href).ok()?;
    let is_redirect = url
        .host_str()
        .map(|h| h.ends_with("bing.com"))
        .unwrap_or(false)
        && (url.path().starts_with("/ck/a") || url.path().starts_with("/rd"));
    if !is_redirect {
        return None;
    }
    let raw = url.query_pairs().find(|(k, _)| k == "u")?.1;
    let decoded = STANDARD.decode(raw.as_bytes()).ok()?;
    Some(String::from_utf8_lossy(&decoded).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<html><body><ol id="b_results">
      <li class="b_algo">
        <h2><a href="https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS9ydXN0&ntb=1">Rust async runtime</a></h2>
        <div class="b_caption"><p>Tokio is the <strong>most used</strong> async runtime.</p></div>
      </li>
      <li class="b_algo">
        <h2><a href="https://plain.example.org/direct">Direct link</a></h2>
        <p>No redirect wrapper here.</p>
      </li>
    </ol></body></html>"#;

    #[test]
    fn parses_bing_results() {
        let results = parse_html(FIXTURE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust async runtime");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].snippet, "Tokio is the most used async runtime.");
        assert_eq!(results[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn decodes_base64_u_param() {
        let href = "https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS9ydXN0&ntb=1";
        assert_eq!(
            decode_bing_url(href).as_deref(),
            Some("https://example.com/rust")
        );
        assert_eq!(decode_bing_url("https://plain.example.org/direct"), None);
    }

    #[test]
    fn challenge_detection_is_triggered() {
        let body = r#"<html><body><div id="b_captcha">Verify you are human</div></body></html>"#;
        assert!(crate::engines::looks_like_challenge(body));
        assert!(!crate::engines::looks_like_challenge(FIXTURE));
    }

    const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0"><channel>
  <title>Bing: rust</title>
  <link>http://www.bing.com/search?q=rust</link>
  <description>Search results</description>
  <item>
    <title>First &amp; result</title>
    <link>https://example.com/1</link>
    <description>Snippet &lt;b&gt;one&lt;/b&gt; here.</description>
  </item>
  <item>
    <title>Second</title>
    <link>https://example.com/2</link>
    <description>Two</description>
  </item>
</channel></rss>"#;

    #[test]
    fn parses_bing_rss_and_ignores_channel() {
        let results = parse_rss(RSS_FIXTURE);
        // Channel-level <title>/<link> must not be counted as results.
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "First & result");
        assert_eq!(results[0].url, "https://example.com/1");
        assert_eq!(results[0].snippet, "Snippet one here.");
        assert_eq!(results[1].title, "Second");
        assert_eq!(results[1].url, "https://example.com/2");
    }

    #[test]
    fn rss_empty_or_html_yields_nothing() {
        assert!(parse_rss("").is_empty());
        assert!(parse_rss(FIXTURE).is_empty()); // HTML, not RSS
    }

    #[test]
    fn unescapes_xml_entities() {
        assert_eq!(crate::text::unescape_entities("a &amp; b"), "a & b");
        assert_eq!(crate::text::unescape_entities("&#39;&#x41;&lt;"), "'A<");
        assert_eq!(crate::text::unescape_entities("no entities"), "no entities");
    }
}
