//! DuckDuckGo HTML endpoint (`html.duckduckgo.com/html/`).
//!
//! No API key required. The HTML layout is scraped with `scraper`; the
//! parser lives in [`parse_html`] as a pure function so it is unit-testable.

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use url::Url;

use crate::engines::{dedupe_and_truncate, SearchEngine};
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::normalize_snippet;

pub struct DuckDuckGo {
    /// Endpoint base; overridable for tests and mirrors.
    base: String,
}

const SEARCH_URL: &str = "https://html.duckduckgo.com/html/";

impl Default for DuckDuckGo {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl DuckDuckGo {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl SearchEngine for DuckDuckGo {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let kl = opts.region.as_deref().and_then(crate::region::ddg_kl);
        let mut params: Vec<(&str, &str)> = vec![("q", query)];
        if let Some(kl) = &kl {
            params.push(("kl", kl.as_str()));
        }
        if opts.safe {
            // DuckDuckGo's safe-search parameter is `kp` (1 = strict).
            params.push(("kp", "1"));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = crate::http::send_with_retry(&client.get(url))
            .map_err(|e| Error::Network(format!("duckduckgo request failed: {e}")))?;
        let status = resp.status();
        if status.as_u16() == 202 || status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(Error::RateLimited(format!(
                "duckduckgo answered HTTP {status} (retry later or lower request rate)"
            )));
        }
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        // A 200 that is really an interstitial must be an error, not an empty
        // result set, or fallback never triggers.
        if crate::engines::looks_like_challenge(&body) {
            return Err(Error::RateLimited(
                "duckduckgo served a bot-challenge page instead of results".into(),
            ));
        }
        Ok(dedupe_and_truncate(parse_html(&body), opts.count, |r| {
            &r.url
        }))
    }
}

/// Pure parser for the DDG HTML result page. Unit-tested against fixtures.
pub fn parse_html(html: &str) -> Vec<SearchResult> {
    let doc = Html::parse_document(html);
    let result_sel = Selector::parse(".result").unwrap_or_else(|_| unreachable!("static"));
    let link_sel = Selector::parse("a.result__a").unwrap_or_else(|_| unreachable!("static"));
    let snippet_sel =
        Selector::parse(".result__snippet").unwrap_or_else(|_| unreachable!("static"));

    let mut out = Vec::new();
    for result in doc.select(&result_sel) {
        // Skip ad blocks. Matching the *substring* "ad" also matched innocent
        // class tokens like "shadow", silently dropping real results.
        let is_ad = result
            .value()
            .attr("class")
            .map(|c| {
                c.split_whitespace()
                    .any(|t| t == "ad" || t.ends_with("--ad") || t.starts_with("result--ad"))
            })
            .unwrap_or(false);
        if is_ad {
            continue;
        }
        let mut title = String::new();
        let mut url = String::new();
        if let Some(a) = result.select(&link_sel).next() {
            title = a.text().collect::<String>().trim().to_string();
            if let Some(href) = a.value().attr("href") {
                url = decode_redirect(href).unwrap_or_else(|| href.to_string());
            }
        }
        if title.is_empty() || url.is_empty() {
            continue; // empty rows and dead links
        }
        let snippet = result
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

/// DDG wraps result links in `/l/?uddg=<encoded>` redirects; unwrap them.
/// Hrefs are protocol-relative (`//duckduckgo.com/l/?...`), so a scheme is
/// synthesized before parsing.
fn decode_redirect(href: &str) -> Option<String> {
    let absolute = if let Some(rest) = href.strip_prefix("//") {
        format!("https:{rest}")
    } else {
        href.to_string()
    };
    let url = Url::parse(&absolute).ok()?;
    if url.host_str() != Some("duckduckgo.com") || url.path() != "/l/" {
        return None;
    }
    let raw = url.query_pairs().find(|(k, _)| k == "uddg")?.1;
    Some(raw.into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<html><body>
      <div class="result results_links results_links_deep web-result">
        <div class="links_main links_deep result__body">
          <h2 class="result__title">
            <a class="result__a" rel="nofollow" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc">
              Rust programming language
            </a>
          </h2>
          <a class="result__snippet" href="//duckduckgo.com/l/?uddg=...">
            Rust is a <b>blazingly fast</b> systems language with memory safety.
          </a>
        </div>
      </div>
      <div class="result result--ad"> <!-- ad: no title link -->
        <a class="result__a" href="//duckduckgo.com/l/?uddg=ads">Sponsored ad</a>
      </div>
    </body></html>"#;

    #[test]
    fn parses_results_and_skips_ads() {
        let results = parse_html(FIXTURE);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "Rust programming language");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(
            results[0].snippet,
            "Rust is a blazingly fast systems language with memory safety."
        );
    }

    #[test]
    fn innocent_class_tokens_are_not_mistaken_for_ads() {
        // "shadow" contains "ad"; a substring match dropped the whole result.
        let html = r#"<html><body>
          <div class="result results_links shadow">
            <a class="result__a" href="https://example.com/keep">Kept</a>
          </div>
          <div class="result result--ad">
            <a class="result__a" href="https://ads.example/x">Sponsored</a>
          </div>
        </body></html>"#;
        let r = parse_html(html);
        assert_eq!(r.len(), 1, "got: {r:?}");
        assert_eq!(r[0].url, "https://example.com/keep");
    }

    #[test]
    fn decode_redirect_unwraps_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc";
        assert_eq!(
            decode_redirect(href).as_deref(),
            Some("https://example.com/rust")
        );
        assert_eq!(decode_redirect("https://example.com/plain"), None);
        // Non-redirect DDG links stay untouched (returned by caller as-is).
        assert_eq!(decode_redirect("//duckduckgo.com/about"), None);
    }
}
