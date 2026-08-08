//! Stable scholarly sources (no key, official JSON APIs):
//! - **OpenAlex** — broad open catalog of academic works.
//! - **CrossRef** — DOI / citation metadata.
//! - **PubMed** — biomedical literature (two-step esearch → esummary).
//!
//! These complement general web search with durable, structured endpoints.
//!
//! PubMed's esearch and esummary calls share an origin
//! (`eutils.ncbi.nlm.nih.gov`), so the shared per-origin rate limiter already
//! spaces them out — no separate sleep is needed between the two requests.
//!
//! None of these send a fabricated contact address. If
//! `WEBSEEK_CONTACT_EMAIL` is set, OpenAlex's "polite pool" `mailto` and
//! PubMed's `email`/`tool` identification are filled in from it; otherwise
//! they are simply omitted, which every one of these APIs treats as a normal
//! (just slightly lower-priority) anonymous request.

use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};

fn contact_email() -> Option<String> {
    std::env::var("WEBSEEK_CONTACT_EMAIL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

// ---------------------------------------------------------------------------
// OpenAlex
// ---------------------------------------------------------------------------

const OPENALEX_URL: &str = "https://api.openalex.org/works";

pub struct OpenAlex {
    base: String,
}

impl Default for OpenAlex {
    fn default() -> Self {
        Self {
            base: OPENALEX_URL.to_string(),
        }
    }
}

impl OpenAlex {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct OaResp {
    #[serde(default)]
    results: Option<Vec<OaWork>>,
    #[serde(default)]
    error: Option<String>,
    #[serde(default)]
    message: Option<String>,
}
#[derive(Deserialize)]
struct OaWork {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    doi: Option<String>,
    #[serde(default)]
    display_name: Option<String>,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    publication_year: Option<i64>,
    #[serde(default)]
    cited_by_count: Option<i64>,
    #[serde(default)]
    primary_location: Option<OaLoc>,
}
#[derive(Deserialize)]
struct OaLoc {
    #[serde(default)]
    source: Option<OaSource>,
}
#[derive(Deserialize)]
struct OaSource {
    #[serde(default)]
    display_name: Option<String>,
}

impl SearchEngine for OpenAlex {
    fn name(&self) -> &'static str {
        "openalex"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let mut params: Vec<(&str, &str)> = vec![("search", query), ("per-page", &limit)];
        let mailto = contact_email();
        if let Some(m) = &mailto {
            params.push(("mailto", m));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        openalex_parse(&body.text)
    }
}

pub fn openalex_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp: OaResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid OpenAlex API response: {e}")))?;
    if let Some(err) = resp.error {
        let msg = resp.message.unwrap_or_default();
        return Err(Error::Parse(format!("OpenAlex API error: {err}: {msg}")));
    }
    let results = resp
        .results
        .ok_or_else(|| Error::Parse("OpenAlex response missing `results`".into()))?;
    Ok(results
        .into_iter()
        .filter_map(|w| {
            let title = w.display_name.or(w.title)?;
            let url = w.doi.or(w.id)?;
            let venue = w
                .primary_location
                .and_then(|l| l.source)
                .and_then(|s| s.display_name)
                .unwrap_or_default();
            let mut snippet = String::new();
            if let Some(y) = w.publication_year {
                snippet.push_str(&format!("{y}"));
            }
            if !venue.is_empty() {
                if !snippet.is_empty() {
                    snippet.push_str(" · ");
                }
                snippet.push_str(&venue);
            }
            if let Some(c) = w.cited_by_count {
                snippet.push_str(&format!(" · cited {c}"));
            }
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// CrossRef
// ---------------------------------------------------------------------------

const CROSSREF_URL: &str = "https://api.crossref.org/works";

pub struct CrossRef {
    base: String,
}

impl Default for CrossRef {
    fn default() -> Self {
        Self {
            base: CROSSREF_URL.to_string(),
        }
    }
}

impl CrossRef {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct CrResp {
    #[serde(default)]
    status: Option<String>,
    message: Option<Value>,
}
#[derive(Deserialize)]
struct CrMsg {
    #[serde(default)]
    items: Vec<CrWork>,
}
#[derive(Deserialize)]
struct CrWork {
    #[serde(default, rename = "DOI")]
    doi: Option<String>,
    #[serde(default)]
    title: Vec<String>,
    #[serde(default, rename = "URL")]
    url: Option<String>,
    #[serde(default, rename = "container-title")]
    container: Vec<String>,
    #[serde(default)]
    published: Option<CrPub>,
    #[serde(default, rename = "is-referenced-by-count")]
    citations: i64,
}
#[derive(Deserialize)]
struct CrPub {
    #[serde(default, rename = "date-parts")]
    date_parts: Vec<Vec<i64>>,
}

impl SearchEngine for CrossRef {
    fn name(&self) -> &'static str {
        "crossref"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![
            ("query", query),
            ("rows", &limit),
            (
                "select",
                "DOI,title,URL,container-title,published,is-referenced-by-count",
            ),
        ];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        crossref_parse(&body.text)
    }
}

pub fn crossref_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp: CrResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid CrossRef API response: {e}")))?;
    if resp.status.as_deref() == Some("failed") {
        return Err(Error::Parse(format!(
            "CrossRef API reported failure: {}",
            resp.message
                .map(|m| m.to_string())
                .unwrap_or_else(|| "no details".into())
        )));
    }
    let msg_value = resp
        .message
        .ok_or_else(|| Error::Parse("CrossRef response missing `message`".into()))?;
    let msg: CrMsg = serde_json::from_value(msg_value)
        .map_err(|e| Error::Parse(format!("unexpected CrossRef `message` shape: {e}")))?;
    Ok(msg
        .items
        .into_iter()
        .filter_map(|w| {
            let title = w.title.first()?.clone();
            let url = w
                .url
                .clone()
                .or_else(|| w.doi.as_ref().map(|d| format!("https://doi.org/{d}")))?;
            let container = w.container.first().cloned().unwrap_or_default();
            let year = w
                .published
                .as_ref()
                .and_then(|p| p.date_parts.first())
                .and_then(|d| d.first())
                .copied()
                .unwrap_or(0);
            let mut snippet = String::new();
            if !container.is_empty() {
                snippet.push_str(&container);
            }
            if year > 0 {
                snippet.push_str(&format!(" · {year}"));
            }
            snippet.push_str(&format!(" · cited {}", w.citations));
            Some(SearchResult {
                title,
                url,
                snippet,
            })
        })
        .collect())
}

// ---------------------------------------------------------------------------
// PubMed (esearch -> esummary)
// ---------------------------------------------------------------------------

const ESEARCH_URL: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esearch.fcgi";
const ESUMMARY_URL: &str = "https://eutils.ncbi.nlm.nih.gov/entrez/eutils/esummary.fcgi";

pub struct PubMed {
    esearch_base: String,
    esummary_base: String,
}

impl Default for PubMed {
    fn default() -> Self {
        Self {
            esearch_base: ESEARCH_URL.to_string(),
            esummary_base: ESUMMARY_URL.to_string(),
        }
    }
}

impl PubMed {
    /// Override both endpoints (tests): `(esearch_base, esummary_base)`.
    pub fn with_bases(esearch: impl Into<String>, esummary: impl Into<String>) -> Self {
        Self {
            esearch_base: esearch.into(),
            esummary_base: esummary.into(),
        }
    }
}

#[derive(Deserialize)]
struct ESearchResp {
    esearchresult: Option<Value>,
}
#[derive(Deserialize)]
struct IdList {
    #[serde(default)]
    idlist: Vec<String>,
    #[serde(rename = "ERROR")]
    error: Option<String>,
}
#[derive(Deserialize)]
struct ESumResp {
    result: Option<Value>,
}

impl SearchEngine for PubMed {
    fn name(&self) -> &'static str {
        "pubmed"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let ids = self.fetch_ids(http, query, &limit)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids.join(",");
        let mut params: Vec<(&str, &str)> =
            vec![("db", "pubmed"), ("id", &joined), ("retmode", "json")];
        let email = contact_email();
        if let Some(e) = &email {
            params.push(("tool", "webseek"));
            params.push(("email", e));
        }
        let url = Url::parse_with_params(&self.esummary_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        pubmed_parse(&body.text, &ids)
    }
}

impl PubMed {
    fn fetch_ids(&self, http: &Http, query: &str, limit: &str) -> Result<Vec<String>> {
        let mut params: Vec<(&str, &str)> = vec![
            ("db", "pubmed"),
            ("term", query),
            ("retmode", "json"),
            ("retmax", limit),
        ];
        let email = contact_email();
        if let Some(e) = &email {
            params.push(("tool", "webseek"));
            params.push(("email", e));
        }
        let url = Url::parse_with_params(&self.esearch_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        let parsed: ESearchResp = serde_json::from_str(&body.text)
            .map_err(|e| Error::Parse(format!("invalid PubMed esearch response: {e}")))?;
        let value = parsed
            .esearchresult
            .ok_or_else(|| Error::Parse("PubMed esearch missing `esearchresult`".into()))?;
        let list: IdList = serde_json::from_value(value)
            .map_err(|e| Error::Parse(format!("unexpected PubMed esearch shape: {e}")))?;
        if let Some(err) = list.error {
            return Err(Error::Parse(format!("PubMed esearch error: {err}")));
        }
        Ok(list.idlist)
    }
}

/// Build results from an esummary body, preserving esearch's `ids` order.
pub fn pubmed_parse(body: &str, ids: &[String]) -> Result<Vec<SearchResult>> {
    let resp: ESumResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid PubMed esummary response: {e}")))?;
    let result = resp
        .result
        .ok_or_else(|| Error::Parse("PubMed esummary missing `result`".into()))?;
    Ok(ids
        .iter()
        .filter_map(|uid| {
            let doc = result.get(uid)?;
            let title = doc.get("title")?.as_str()?.to_string();
            let source = doc
                .get("source")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let pubdate = doc
                .get("pubdate")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            let mut snippet = String::new();
            if !source.is_empty() {
                snippet.push_str(source);
            }
            if !pubdate.is_empty() {
                if !snippet.is_empty() {
                    snippet.push_str(" · ");
                }
                snippet.push_str(pubdate);
            }
            Some(SearchResult {
                title,
                url: format!("https://pubmed.ncbi.nlm.nih.gov/{uid}/"),
                snippet,
            })
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openalex_parses_works() {
        let body = r#"{"results":[
          {"id":"https://openalex.org/W1","doi":"https://doi.org/10.1/x","display_name":"On Async","publication_year":2021,"cited_by_count":7,"primary_location":{"source":{"display_name":"J. Systems"}}},
          {"id":"https://openalex.org/W2","title":"No Doi Here"}
        ]}"#;
        let r = openalex_parse(body).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "On Async");
        assert_eq!(r[0].url, "https://doi.org/10.1/x");
        assert_eq!(r[0].snippet, "2021 · J. Systems · cited 7");
        assert_eq!(r[1].url, "https://openalex.org/W2");
    }

    #[test]
    fn openalex_error_object_is_surfaced() {
        let body = r#"{"error":"invalid_query","message":"bad filter syntax"}"#;
        let err = openalex_parse(body).unwrap_err();
        assert!(err.to_string().contains("bad filter syntax"));
    }

    #[test]
    fn openalex_missing_results_is_an_error() {
        let err = openalex_parse("{}").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn crossref_parses_items() {
        let body = r#"{"status":"ok","message":{"items":[
          {"DOI":"10.1/x","title":["ASYNC 2020"],"URL":"https://doi.org/10.1/x","container-title":["IEEE ASYNC"],"published":{"date-parts":[[2020,5]]},"is-referenced-by-count":3}
        ]}}"#;
        let r = crossref_parse(body).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "ASYNC 2020");
        assert_eq!(r[0].snippet, "IEEE ASYNC · 2020 · cited 3");
    }

    #[test]
    fn crossref_failed_status_is_an_error() {
        let body = r#"{"status":"failed","message":[{"message":"bad query"}]}"#;
        let err = crossref_parse(body).unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn pubmed_preserves_order_and_builds_urls() {
        let body = r#"{"result":{"uids":["2","1"],
          "1":{"uid":"1","title":"First paper","source":"Nature","pubdate":"2020 Jan"},
          "2":{"uid":"2","title":"Second paper","source":"Cell","pubdate":"2021 Feb"}
        }}"#;
        let r = pubmed_parse(body, &["1".to_string(), "2".to_string()]).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "First paper");
        assert_eq!(r[0].url, "https://pubmed.ncbi.nlm.nih.gov/1/");
        assert_eq!(r[0].snippet, "Nature · 2020 Jan");
        assert_eq!(r[1].title, "Second paper");
    }

    #[test]
    fn bad_json_is_an_error_everywhere() {
        assert!(openalex_parse("x").is_err());
        assert!(crossref_parse("x").is_err());
        assert!(pubmed_parse("x", &["1".into()]).is_err());
    }

    #[test]
    fn contact_email_env_var_is_optional() {
        std::env::remove_var("WEBSEEK_CONTACT_EMAIL");
        assert_eq!(contact_email(), None);
    }
}
