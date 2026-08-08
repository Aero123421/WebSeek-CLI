//! Bing HTML + RSS search endpoints.
//!
//! No API key required. Result links are `bing.com/ck/a?...&u=<base64>` —
//! the `u` query parameter decodes (standard or URL-safe base64, with or
//! without padding — Bing has used both) to the real URL.

use base64::engine::general_purpose;
use base64::Engine;
use url::Url;

use crate::engines::{dedupe_by_url, SearchEngine};
use crate::error::{Error, Result};
use crate::feed;
use crate::http::Http;
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

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let cc = opts.region.as_deref().and_then(crate::region::bing_cc);
        let region_lang = opts
            .region
            .as_deref()
            .and_then(crate::region::bing_language);
        let lang = opts.lang.clone().or(region_lang);

        // Preferred path: the RSS endpoint. It is a stable, structured format
        // and — unlike the HTML page — is not behind a bot challenge.
        if let Some(results) = self.search_rss(http, query, &lang, cc.as_deref(), opts) {
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

        let resp = http.get(url)?;
        let status = resp.status();
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = crate::http::text_capped(resp, crate::http::MAX_API_BODY_BYTES)?;
        if crate::engines::looks_like_challenge(&body.text) {
            return Err(Error::rate_limited(
                "bing served a bot-challenge page instead of results (IP reputation)",
            ));
        }
        let mut results = parse_html(&body.text);
        results.truncate(opts.count);
        Ok(dedupe_by_url(results, |r| &r.url))
    }
}

impl Bing {
    /// Try the RSS endpoint; `None` means "RSS unusable, fall back to HTML"
    /// (network error, non-2xx, not a feed, or a feed with zero items).
    fn search_rss(
        &self,
        http: &Http,
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
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES).ok()?;
        let entries = feed::parse_entries(&body.text).ok()?;
        if entries.is_empty() {
            return None;
        }
        let mut results: Vec<SearchResult> = entries
            .into_iter()
            .filter(|e| !e.link.is_empty())
            .map(|e| SearchResult {
                title: normalize_snippet(&e.title),
                url: e.link,
                snippet: normalize_snippet(&e.summary),
            })
            .collect();
        if results.is_empty() {
            return None;
        }
        results.truncate(opts.count);
        Some(dedupe_by_url(results, |r| &r.url))
    }
}

/// Pure parser for the Bing HTML result page. Unit-tested against fixtures.
pub fn parse_html(html: &str) -> Vec<SearchResult> {
    let doc = scraper::Html::parse_document(html);
    let algo_sel = scraper::Selector::parse("li.b_algo").unwrap_or_else(|_| unreachable!("static"));
    let link_sel = scraper::Selector::parse("h2 a").unwrap_or_else(|_| unreachable!("static"));
    let snippet_sel = scraper::Selector::parse("p, .b_lineclamp2, .b_caption p")
        .unwrap_or_else(|_| unreachable!("static"));

    let mut out = Vec::new();
    for li in doc.select(&algo_sel) {
        let mut title = String::new();
        let mut url: Option<String> = None;
        if let Some(a) = li.select(&link_sel).next() {
            title = a.text().collect::<String>().trim().to_string();
            if let Some(href) = a.value().attr("href") {
                let candidate = decode_bing_url(href).unwrap_or_else(|| href.to_string());
                // A bare "#" or similar placeholder href is neither empty
                // nor a redirect, but it also isn't a usable http(s) URL.
                url = Url::parse(&candidate)
                    .ok()
                    .filter(|u| matches!(u.scheme(), "http" | "https"))
                    .map(|_| candidate);
            }
        }
        let Some(url) = url.filter(|_| !title.is_empty()) else {
            continue; // a title with no usable URL is not a usable result
        };
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

/// Bing wraps external links in `bing.com/ck/a?...&u=<base64>` redirects.
/// The host check is an exact match on `bing.com` or a `*.bing.com`
/// subdomain — `ends_with("bing.com")` alone also matches `evilbing.com`.
fn decode_bing_url(href: &str) -> Option<String> {
    let url = Url::parse(href).ok()?;
    let host = url.host_str()?;
    let is_bing_host = host == "bing.com" || host.ends_with(".bing.com");
    let is_redirect =
        is_bing_host && (url.path().starts_with("/ck/a") || url.path().starts_with("/rd"));
    if !is_redirect {
        return None;
    }
    let raw = url.query_pairs().find(|(k, _)| k == "u")?.1;
    decode_base64_any(raw.as_bytes())
}

/// Try every base64 variant Bing has been observed to use for the `u` param.
/// `base64::Engine` has generic methods, so it isn't object-safe — each
/// candidate is tried through a generic helper instead of a `dyn` list.
fn decode_base64_any(raw: &[u8]) -> Option<String> {
    try_decode(general_purpose::STANDARD, raw)
        .or_else(|| try_decode(general_purpose::URL_SAFE, raw))
        .or_else(|| try_decode(general_purpose::STANDARD_NO_PAD, raw))
        .or_else(|| try_decode(general_purpose::URL_SAFE_NO_PAD, raw))
}

fn try_decode(engine: impl Engine, raw: &[u8]) -> Option<String> {
    let decoded = engine.decode(raw).ok()?;
    let s = String::from_utf8_lossy(&decoded).into_owned();
    (s.starts_with("http://") || s.starts_with("https://")).then_some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r##"<html><body><ol id="b_results">
      <li class="b_algo">
        <h2><a href="https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS9ydXN0&ntb=1">Rust async runtime</a></h2>
        <div class="b_caption"><p>Tokio is the <strong>most used</strong> async runtime.</p></div>
      </li>
      <li class="b_algo">
        <h2><a href="https://plain.example.org/direct">Direct link</a></h2>
        <p>No redirect wrapper here.</p>
      </li>
      <li class="b_algo">
        <h2><a href="#">No URL result</a></h2>
        <p>Should be skipped.</p>
      </li>
    </ol></body></html>"##;

    #[test]
    fn parses_bing_results_and_skips_urlless_hits() {
        let results = parse_html(FIXTURE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust async runtime");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(results[0].snippet, "Tokio is the most used async runtime.");
        assert_eq!(results[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn decodes_standard_base64_u_param() {
        let href = "https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS9ydXN0&ntb=1";
        assert_eq!(
            decode_bing_url(href).as_deref(),
            Some("https://example.com/rust")
        );
    }

    #[test]
    fn decodes_url_safe_base64_u_param() {
        // "https://example.com/a?b=c" contains '?' and '=', which differ
        // between standard and URL-safe base64 alphabets once padding varies.
        let target = "https://example.com/a?b=c";
        let encoded = general_purpose::URL_SAFE_NO_PAD.encode(target);
        let href = format!("https://www.bing.com/ck/a?u={encoded}");
        assert_eq!(decode_bing_url(&href).as_deref(), Some(target));
    }

    #[test]
    fn lookalike_host_is_not_treated_as_a_bing_redirect() {
        let href = "https://evilbing.com/ck/a?u=aHR0cHM6Ly9ldmlsLmV4YW1wbGUvcA";
        assert_eq!(decode_bing_url(href), None);
        assert_eq!(decode_bing_url("https://plain.example.org/direct"), None);
    }

    #[test]
    fn subdomain_of_bing_is_still_trusted() {
        let href = "https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS9ydXN0";
        assert_eq!(
            decode_bing_url(href).as_deref(),
            Some("https://example.com/rust")
        );
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
        let entries = feed::parse_entries(RSS_FIXTURE).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "First & result");
        assert_eq!(entries[0].link, "https://example.com/1");
        assert_eq!(entries[1].title, "Second");
    }

    #[test]
    fn html_body_is_not_mistaken_for_a_feed() {
        assert!(feed::parse_entries(FIXTURE).is_err());
    }
}
