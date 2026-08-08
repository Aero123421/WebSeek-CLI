//! DuckDuckGo HTML endpoint (`html.duckduckgo.com/html/`).
//!
//! No API key required. The HTML layout is scraped with `scraper`; the
//! parser lives in [`parse_html`] as a pure function so it is unit-testable.

use url::Url;

use crate::engines::{dedupe_by_url, SearchEngine};
use crate::error::{Error, Result};
use crate::http::Http;
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

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let kl = opts.region.as_deref().and_then(crate::region::ddg_kl);
        let mut params: Vec<(&str, &str)> = vec![("q", query)];
        if let Some(kl) = &kl {
            params.push(("kl", kl.as_str()));
        }
        if opts.safe {
            params.push(("p", "1"));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = http.get(url)?;
        let status = resp.status().as_u16();
        if status == 202 || status == 429 || status == 403 {
            return Err(Error::rate_limited(format!(
                "duckduckgo answered HTTP {status} (retry later or lower request rate)"
            )));
        }
        if !resp.status().is_success() {
            return Err(Error::Http(status));
        }
        let body = crate::http::text_capped(resp, crate::http::MAX_API_BODY_BYTES)?;
        // Dedupe *before* truncating to `count`: otherwise a duplicate near
        // the top of the page silently shrinks the result count below what
        // was asked for even though more unique hits exist further down.
        let mut results = dedupe_by_url(parse_html(&body.text), |r| &r.url);
        results.truncate(opts.count);
        Ok(results)
    }
}

/// Class tokens DuckDuckGo actually uses to mark a sponsored result. A bare
/// substring check (`token.contains("ad")`) also matches unrelated tokens
/// like `shadow`, `loaded` or `gradient` — this is an exact, word-boundary
/// match instead (tokens are already split on whitespace by the caller).
const AD_TOKENS: &[&str] = &["ad", "ads", "result--ad", "badge--ad", "sponsored"];

fn is_ad_result(class_attr: Option<&str>) -> bool {
    class_attr
        .map(|c| c.split_whitespace().any(|t| AD_TOKENS.contains(&t)))
        .unwrap_or(false)
}

/// Pure parser for the DDG HTML result page. Unit-tested against fixtures.
pub fn parse_html(html: &str) -> Vec<SearchResult> {
    let doc = scraper::Html::parse_document(html);
    let result_sel = scraper::Selector::parse(".result").unwrap_or_else(|_| unreachable!("static"));
    let link_sel =
        scraper::Selector::parse("a.result__a").unwrap_or_else(|_| unreachable!("static"));
    let snippet_sel =
        scraper::Selector::parse(".result__snippet").unwrap_or_else(|_| unreachable!("static"));

    let mut out = Vec::new();
    for result in doc.select(&result_sel) {
        if is_ad_result(result.value().attr("class")) {
            continue;
        }
        let mut title = String::new();
        let mut url = None;
        if let Some(a) = result.select(&link_sel).next() {
            title = a.text().collect::<String>().trim().to_string();
            if let Some(href) = a.value().attr("href") {
                url = normalize_result_url(href);
            }
        }
        let (Some(url), false) = (url, title.is_empty()) else {
            continue; // empty rows, dead links, or an unsafe/invalid URL
        };
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

/// Turn a raw `href` into a safe absolute http(s) URL, or `None` to drop the
/// result. Handles three shapes: DDG's `/l/?uddg=` redirect wrapper,
/// protocol-relative links (`//example.com/...`), and ordinary absolute
/// links — the first two used to reach the output either un-decoded or
/// without a scheme.
fn normalize_result_url(href: &str) -> Option<String> {
    if let Some(decoded) = decode_redirect(href) {
        return only_http(&decoded);
    }
    let absolute = match href.strip_prefix("//") {
        Some(rest) => format!("https://{rest}"),
        None => href.to_string(),
    };
    only_http(&absolute)
}

/// Parse `candidate`, keeping only http(s) results — and returning the
/// *parsed* URL's canonical string, not the raw input. Returning the raw
/// string would be wrong for a bare `https:host/path` input: the URL parser
/// normalizes that (WHATWG's special-scheme handling inserts the missing
/// `//`), so the un-reparsed original string would still be missing it.
fn only_http(candidate: &str) -> Option<String> {
    let url = Url::parse(candidate).ok()?;
    matches!(url.scheme(), "http" | "https").then(|| url.to_string())
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
      <div class="result">
        <a class="result__a" href="//plain.example.org/direct">Protocol relative link</a>
      </div>
      <div class="result">
        <a class="result__a" href="javascript:alert(1)">Dangerous scheme</a>
      </div>
    </body></html>"#;

    #[test]
    fn parses_results_and_skips_ads() {
        let results = parse_html(FIXTURE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].title, "Rust programming language");
        assert_eq!(results[0].url, "https://example.com/rust");
        assert_eq!(
            results[0].snippet,
            "Rust is a blazingly fast systems language with memory safety."
        );
    }

    #[test]
    fn protocol_relative_links_get_an_https_scheme() {
        let results = parse_html(FIXTURE);
        assert_eq!(results[1].url, "https://plain.example.org/direct");
    }

    #[test]
    fn dangerous_schemes_are_dropped_not_passed_through() {
        let results = parse_html(FIXTURE);
        assert!(results.iter().all(|r| r.url.starts_with("http")));
        assert!(!results.iter().any(|r| r.title == "Dangerous scheme"));
    }

    #[test]
    fn ad_detection_does_not_false_positive_on_unrelated_classes() {
        assert!(!is_ad_result(Some("shadow loaded gradient header")));
        assert!(is_ad_result(Some("result result--ad")));
        assert!(is_ad_result(Some("ad")));
    }

    #[test]
    fn decode_redirect_unwraps_uddg() {
        let href = "//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Frust&rut=abc";
        assert_eq!(
            decode_redirect(href).as_deref(),
            Some("https://example.com/rust")
        );
        assert_eq!(decode_redirect("https://example.com/plain"), None);
        assert_eq!(decode_redirect("//duckduckgo.com/about"), None);
    }

    #[test]
    fn dedupe_runs_before_truncate() {
        let html = r#"<html><body>
          <div class="result"><a class="result__a" href="https://x.example/a">A</a></div>
          <div class="result"><a class="result__a" href="https://x.example/a">A dup</a></div>
          <div class="result"><a class="result__a" href="https://x.example/b">B</a></div>
        </body></html>"#;
        let mut results = dedupe_by_url(parse_html(html), |r| &r.url);
        results.truncate(2);
        // Both unique URLs must survive even though a duplicate came first.
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://x.example/a");
        assert_eq!(results[1].url, "https://x.example/b");
    }
}
