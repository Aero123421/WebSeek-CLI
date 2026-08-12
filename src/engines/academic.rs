//! Stable scholarly sources (no key, official JSON APIs):
//! - **OpenAlex** — broad open catalog of academic works.
//! - **CrossRef** — DOI / citation metadata.
//! - **PubMed** — biomedical literature (two-step esearch → esummary).
//!
//! These complement general web search with durable, structured endpoints.

use reqwest::blocking::Client;
use serde::Deserialize;
use serde_json::Value;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{join_meta, normalize_snippet, strip_html};

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
    results: Vec<OaWork>,
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let mut params: Vec<(&str, &str)> = vec![("search", query), ("per-page", &limit)];
        // OpenAlex's "polite pool" exists so they can contact whoever is
        // calling. A placeholder address would claim that benefit while making
        // the promise unkeepable, so we only send a real, user-configured one.
        if let Some(mail) = opts.contact_email.as_deref() {
            params.push(("mailto", mail));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("openalex request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        openalex_parse(&body)
    }
}

pub fn openalex_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<OaResp>(body)
        .map_err(|e| Error::Parse(format!("openalex response is not valid JSON: {e}")))?;
    Ok(resp
        .results
        .into_iter()
        .filter_map(|w| {
            let title = w.display_name.or(w.title)?;
            let url = w.doi.or(w.id)?;
            let venue = w
                .primary_location
                .and_then(|l| l.source)
                .and_then(|s| s.display_name)
                .unwrap_or_default();
            let year = w
                .publication_year
                .map(|y| y.to_string())
                .unwrap_or_default();
            let cited = w
                .cited_by_count
                .map(|c| format!("cited {c}"))
                .unwrap_or_default();
            Some(SearchResult {
                title: normalize_snippet(&strip_html(&title)),
                url,
                snippet: normalize_snippet(&join_meta(&[&year, &venue, &cited])),
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
    message: Option<CrMsg>,
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let mut params: Vec<(&str, &str)> = vec![
            ("query", query),
            ("rows", &limit),
            (
                "select",
                "DOI,title,URL,container-title,published,is-referenced-by-count",
            ),
        ];
        // CrossRef has the same polite-pool convention as OpenAlex.
        if let Some(mail) = opts.contact_email.as_deref() {
            params.push(("mailto", mail));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("crossref request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        crossref_parse(&body)
    }
}

pub fn crossref_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<CrResp>(body)
        .map_err(|e| Error::Parse(format!("crossref response is not valid JSON: {e}")))?;
    let msg = resp
        .message
        .ok_or_else(|| Error::Parse("crossref response omitted message.items".into()))?;
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
            let year = if year > 0 {
                year.to_string()
            } else {
                String::new()
            };
            let cited = format!("cited {}", w.citations);
            Some(SearchResult {
                title: normalize_snippet(&strip_html(&title)),
                url,
                snippet: normalize_snippet(&join_meta(&[&container, &year, &cited])),
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
    esearchresult: Option<IdList>,
}
#[derive(Deserialize)]
struct IdList {
    #[serde(default)]
    idlist: Vec<String>,
}
#[derive(Deserialize)]
struct ESumResp {
    result: Option<Value>,
}

impl SearchEngine for PubMed {
    fn name(&self) -> &'static str {
        "pubmed"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let ids = self.fetch_ids(client, query, &limit, opts)?;
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids.join(",");
        let mut params: Vec<(&str, &str)> =
            vec![("db", "pubmed"), ("id", &joined), ("retmode", "json")];
        eutils_identity(&mut params, opts);
        let url = Url::parse_with_params(&self.esummary_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("pubmed esummary failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        pubmed_parse(&body, &ids)
    }
}

/// NCBI's E-utilities usage policy asks every client to identify itself with a
/// `tool` name and, where possible, an `email`. Both are cheap to send and are
/// the difference between being a known caller and an anonymous one.
fn eutils_identity<'a>(params: &mut Vec<(&'a str, &'a str)>, opts: &'a SearchOpts) {
    params.push(("tool", "webseek"));
    if let Some(mail) = opts.contact_email.as_deref() {
        params.push(("email", mail));
    }
}

impl PubMed {
    fn fetch_ids(
        &self,
        client: &Client,
        query: &str,
        limit: &str,
        opts: &SearchOpts,
    ) -> Result<Vec<String>> {
        let mut params: Vec<(&str, &str)> = vec![
            ("db", "pubmed"),
            ("term", query),
            ("retmode", "json"),
            ("retmax", limit),
        ];
        eutils_identity(&mut params, opts);
        let url = Url::parse_with_params(&self.esearch_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("pubmed esearch failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = crate::http::response_text(resp)?;
        let parsed = serde_json::from_str::<ESearchResp>(&body)
            .map_err(|e| Error::Parse(format!("pubmed esearch response is not valid JSON: {e}")))?;
        parsed
            .esearchresult
            .map(|r| r.idlist)
            .ok_or_else(|| Error::Parse("pubmed esearch response omitted esearchresult".into()))
    }
}

/// Build results from an esummary body, preserving esearch's `ids` order.
pub fn pubmed_parse(body: &str, ids: &[String]) -> Result<Vec<SearchResult>> {
    let resp = serde_json::from_str::<ESumResp>(body)
        .map_err(|e| Error::Parse(format!("pubmed esummary response is not valid JSON: {e}")))?;
    let result = resp
        .result
        .ok_or_else(|| Error::Parse("pubmed esummary response omitted result".into()))?;
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
            Some(SearchResult {
                title: normalize_snippet(&strip_html(&title)),
                url: format!("https://pubmed.ncbi.nlm.nih.gov/{uid}/"),
                snippet: normalize_snippet(&join_meta(&[source, pubdate])),
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
        // Second work has no doi/id-as-url? it has id -> url ok.
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "On Async");
        assert_eq!(r[0].url, "https://doi.org/10.1/x");
        assert_eq!(r[0].snippet, "2021 · J. Systems · cited 7");
        assert_eq!(r[1].url, "https://openalex.org/W2");
    }

    #[test]
    fn crossref_parses_items() {
        let body = r#"{"message":{"items":[
          {"DOI":"10.1/x","title":["ASYNC 2020"],"URL":"https://doi.org/10.1/x","container-title":["IEEE ASYNC"],"published":{"date-parts":[[2020,5]]},"is-referenced-by-count":3}
        ]}}"#;
        let r = crossref_parse(body).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "ASYNC 2020");
        assert_eq!(r[0].snippet, "IEEE ASYNC · 2020 · cited 3");
    }

    #[test]
    fn pubmed_preserves_order_and_builds_urls() {
        let body = r#"{"result":{"uids":["2","1"],
          "1":{"uid":"1","title":"First paper","source":"Nature","pubdate":"2020 Jan"},
          "2":{"uid":"2","title":"Second paper","source":"Cell","pubdate":"2021 Feb"}
        }}"#;
        // Order follows the ids we pass (esearch order), not the JSON order.
        let r = pubmed_parse(body, &["1".to_string(), "2".to_string()]).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "First paper");
        assert_eq!(r[0].url, "https://pubmed.ncbi.nlm.nih.gov/1/");
        assert_eq!(r[0].snippet, "Nature · 2020 Jan");
        assert_eq!(r[1].title, "Second paper");
    }

    #[test]
    fn bad_json_is_an_error() {
        assert!(openalex_parse("x").is_err());
        assert!(crossref_parse("x").is_err());
        assert!(pubmed_parse("x", &["1".into()]).is_err());
    }
}
