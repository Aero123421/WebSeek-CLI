//! Package-registry sources (no key, official JSON APIs):
//! - **crates.io** — Rust crates (keyword search).
//! - **npm** — JavaScript packages (keyword search).
//! - **PyPI** — Python packages. PyPI has *no* keyword-search API and its HTML
//!   search is bot-protected, so this is an **exact-name lookup** via the
//!   stable JSON API (`/pypi/{name}/json`).

use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};

// ---------------------------------------------------------------------------
// crates.io
// ---------------------------------------------------------------------------

const CRATES_URL: &str = "https://crates.io/api/v1/crates";

pub struct Crates {
    base: String,
}

impl Default for Crates {
    fn default() -> Self {
        Self {
            base: CRATES_URL.to_string(),
        }
    }
}

impl Crates {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct CratesResp {
    #[serde(default)]
    crates: Option<Vec<Crate>>,
    #[serde(default)]
    errors: Option<Vec<CratesError>>,
}
#[derive(Deserialize)]
struct CratesError {
    #[serde(default)]
    detail: String,
}
#[derive(Deserialize)]
struct Crate {
    #[serde(default)]
    id: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    downloads: i64,
    #[serde(default)]
    repository: Option<String>,
    #[serde(default)]
    documentation: Option<String>,
    #[serde(default)]
    max_version: Option<String>,
}

impl SearchEngine for Crates {
    fn name(&self) -> &'static str {
        "crates"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![("q", query), ("per_page", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        crates_parse(&body.text)
    }
}

pub fn crates_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp: CratesResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid crates.io API response: {e}")))?;
    if let Some(errors) = resp.errors {
        let msg = errors
            .into_iter()
            .map(|e| e.detail)
            .collect::<Vec<_>>()
            .join("; ");
        return Err(Error::Parse(format!("crates.io API error: {msg}")));
    }
    let crates = resp
        .crates
        .ok_or_else(|| Error::Parse("crates.io response missing `crates`".into()))?;
    Ok(crates
        .into_iter()
        .map(|c| {
            let url = c
                .repository
                .filter(|s| !s.is_empty())
                .or(c.documentation)
                .unwrap_or_else(|| format!("https://crates.io/crates/{}", c.id));
            let desc = c.description.unwrap_or_default();
            let version = c.max_version.unwrap_or_default();
            SearchResult {
                title: c.id,
                url,
                snippet: format!("{desc} · v{version} · {} downloads", c.downloads),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// npm
// ---------------------------------------------------------------------------

const NPM_URL: &str = "https://registry.npmjs.org/-/v1/search";

pub struct Npm {
    base: String,
}

impl Default for Npm {
    fn default() -> Self {
        Self {
            base: NPM_URL.to_string(),
        }
    }
}

impl Npm {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct NpmResp {
    #[serde(default)]
    objects: Option<Vec<NpmObj>>,
    #[serde(default)]
    error: Option<String>,
}
#[derive(Deserialize)]
struct NpmObj {
    package: NpmPkg,
    #[serde(default)]
    downloads: Option<NpmDl>,
}
#[derive(Deserialize)]
struct NpmPkg {
    #[serde(default)]
    name: String,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    links: Option<NpmLinks>,
}
#[derive(Deserialize)]
struct NpmLinks {
    #[serde(default)]
    npm: Option<String>,
}
#[derive(Deserialize)]
struct NpmDl {
    #[serde(default)]
    monthly: Option<i64>,
}

impl SearchEngine for Npm {
    fn name(&self) -> &'static str {
        "npm"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![("text", query), ("size", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        npm_parse(&body.text)
    }
}

pub fn npm_parse(body: &str) -> Result<Vec<SearchResult>> {
    let resp: NpmResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid npm registry response: {e}")))?;
    if let Some(err) = resp.error {
        return Err(Error::Parse(format!("npm registry API error: {err}")));
    }
    let objects = resp
        .objects
        .ok_or_else(|| Error::Parse("npm registry response missing `objects`".into()))?;
    Ok(objects
        .into_iter()
        .map(|o| {
            let url = o
                .package
                .links
                .and_then(|l| l.npm)
                .unwrap_or_else(|| format!("https://www.npmjs.com/package/{}", o.package.name));
            let desc = o.package.description.unwrap_or_default();
            let version = o.package.version.unwrap_or_default();
            let monthly = o.downloads.and_then(|d| d.monthly).unwrap_or(0);
            SearchResult {
                title: o.package.name,
                url,
                snippet: format!("{desc} · v{version} · {monthly} downloads/mo"),
            }
        })
        .collect())
}

// ---------------------------------------------------------------------------
// PyPI (exact-name lookup)
// ---------------------------------------------------------------------------

const PYPI_BASE: &str = "https://pypi.org/pypi";

pub struct PyPi {
    base: String,
}

impl Default for PyPi {
    fn default() -> Self {
        Self {
            base: PYPI_BASE.to_string(),
        }
    }
}

impl PyPi {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct PyPiResp {
    info: Option<PyPiInfo>,
}
#[derive(Deserialize)]
struct PyPiInfo {
    #[serde(default)]
    name: String,
    #[serde(default)]
    summary: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    package_url: Option<String>,
    #[serde(default)]
    home_page: Option<String>,
}

impl SearchEngine for PyPi {
    fn name(&self) -> &'static str {
        "pypi"
    }

    fn search(&self, http: &Http, query: &str, _opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let name = query.trim();
        // `push()` percent-encodes the segment, so a name containing `/`,
        // `?` or `#` cannot redirect the request to a different path or add
        // a query string — the old `format!("{base}/{name}/json")` could.
        let mut url =
            Url::parse(&self.base).map_err(|e| Error::Config(format!("bad PyPI base URL: {e}")))?;
        {
            let mut segs = url
                .path_segments_mut()
                .map_err(|_| Error::Config("PyPI base URL cannot be a base".into()))?;
            segs.push(name);
            segs.push("json");
        }
        let body = match http.get_text(url, crate::http::MAX_API_BODY_BYTES) {
            Ok(b) => b,
            Err(Error::Http(404)) => return Ok(Vec::new()), // unknown package name -> no results
            Err(e) => return Err(e),
        };
        pypi_parse(&body.text, name)
    }
}

pub fn pypi_parse(body: &str, name: &str) -> Result<Vec<SearchResult>> {
    let resp: PyPiResp = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid PyPI API response: {e}")))?;
    let info = resp
        .info
        .ok_or_else(|| Error::Parse("PyPI response missing `info`".into()))?;
    let url = info
        .package_url
        .filter(|s| !s.is_empty())
        .or(info.home_page)
        .unwrap_or_else(|| format!("https://pypi.org/project/{name}/"));
    let title = if info.name.is_empty() {
        name.to_string()
    } else {
        info.name
    };
    let summary = info.summary.unwrap_or_default();
    let version = info.version.unwrap_or_default();
    Ok(vec![SearchResult {
        title,
        url,
        snippet: format!("{summary} · v{version}"),
    }])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crates_parses_and_prefers_repository_url() {
        let body = r#"{"crates":[
          {"id":"tokio","description":"Async runtime","downloads":100,"repository":"https://github.com/tokio-rs/tokio","max_version":"1.0.0"},
          {"id":"nolib","description":null,"downloads":5,"repository":null,"documentation":null,"max_version":"0.1.0"}
        ]}"#;
        let r = crates_parse(body).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "tokio");
        assert_eq!(r[0].url, "https://github.com/tokio-rs/tokio");
        assert_eq!(r[0].snippet, "Async runtime · v1.0.0 · 100 downloads");
        assert_eq!(r[1].url, "https://crates.io/crates/nolib");
    }

    #[test]
    fn crates_error_response_is_surfaced() {
        let body = r#"{"errors":[{"detail":"too many requests"}]}"#;
        let err = crates_parse(body).unwrap_err();
        assert!(err.to_string().contains("too many requests"));
    }

    #[test]
    fn npm_parses_objects() {
        let body = r#"{"objects":[
          {"package":{"name":"async","version":"3.2.6","description":"Async utils","links":{"npm":"https://www.npmjs.com/package/async"}},"downloads":{"monthly":1000}}
        ]}"#;
        let r = npm_parse(body).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "async");
        assert_eq!(r[0].url, "https://www.npmjs.com/package/async");
        assert_eq!(r[0].snippet, "Async utils · v3.2.6 · 1000 downloads/mo");
    }

    #[test]
    fn pypi_is_single_result_lookup() {
        let body = r#"{"info":{"name":"requests","summary":"HTTP for humans","version":"2.31.0","package_url":"https://pypi.org/project/requests/"}}"#;
        let r = pypi_parse(body, "requests").unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "requests");
        assert_eq!(r[0].url, "https://pypi.org/project/requests/");
        assert_eq!(r[0].snippet, "HTTP for humans · v2.31.0");
    }

    #[test]
    fn pypi_name_with_path_characters_stays_in_one_segment() {
        // Regression: this used to be string-concatenated into the URL, so
        // a name like "x/../evil" could change the request path/query.
        let mut url = Url::parse(PYPI_BASE).unwrap();
        {
            let mut segs = url.path_segments_mut().unwrap();
            segs.push("x/../evil?y=1");
            segs.push("json");
        }
        assert_eq!(url.path(), "/pypi/x%2F..%2Fevil%3Fy=1/json");
        assert_eq!(url.query(), None);
    }

    #[test]
    fn bad_json_is_an_error_everywhere() {
        assert!(crates_parse("x").is_err());
        assert!(npm_parse("x").is_err());
        assert!(pypi_parse("x", "y").is_err());
    }
}
