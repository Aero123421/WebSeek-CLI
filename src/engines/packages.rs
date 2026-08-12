//! Package-registry sources (no key, official JSON APIs):
//! - **crates.io** — Rust crates (keyword search).
//! - **npm** — JavaScript packages (keyword search).
//! - **PyPI** — Python packages. PyPI has *no* keyword-search API and its HTML
//!   search is bot-protected, so this is an **exact-name lookup** via the
//!   stable JSON API (`/pypi/{name}/json`).

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{join_meta, normalize_snippet};

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
    crates: Vec<Crate>,
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![("q", query), ("per_page", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("crates.io request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(crates_parse(&body))
    }
}

pub fn crates_parse(body: &str) -> Vec<SearchResult> {
    let Ok(resp) = serde_json::from_str::<CratesResp>(body) else {
        return Vec::new();
    };
    resp.crates
        .into_iter()
        .map(|c| {
            let url = c
                .repository
                .filter(|s| !s.is_empty())
                .or(c.documentation)
                .unwrap_or_else(|| format!("https://crates.io/crates/{}", c.id));
            let desc = c.description.unwrap_or_default();
            let version = c.max_version.map(|v| format!("v{v}")).unwrap_or_default();
            let downloads = format!("{} downloads", c.downloads);
            SearchResult {
                title: c.id,
                // Registry descriptions are arbitrary user text: cap them or
                // the documented ~300-char snippet bound is a fiction.
                snippet: normalize_snippet(&join_meta(&[&desc, &version, &downloads])),
                url,
            }
        })
        .collect()
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
    objects: Vec<NpmObj>,
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![("text", query), ("size", &limit)];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("npm request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(npm_parse(&body))
    }
}

pub fn npm_parse(body: &str) -> Vec<SearchResult> {
    let Ok(resp) = serde_json::from_str::<NpmResp>(body) else {
        return Vec::new();
    };
    resp.objects
        .into_iter()
        .map(|o| {
            let url = o
                .package
                .links
                .and_then(|l| l.npm)
                .unwrap_or_else(|| format!("https://www.npmjs.com/package/{}", o.package.name));
            let desc = o.package.description.unwrap_or_default();
            let version = o
                .package
                .version
                .map(|v| format!("v{v}"))
                .unwrap_or_default();
            let monthly = format!(
                "{} downloads/mo",
                o.downloads.and_then(|d| d.monthly).unwrap_or(0)
            );
            SearchResult {
                title: o.package.name,
                snippet: normalize_snippet(&join_meta(&[&desc, &version, &monthly])),
                url,
            }
        })
        .collect()
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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let name = query.trim();
        // Encode the package name as one path segment. Interpolating it raw
        // let `../` in a query walk to unrelated paths on pypi.org.
        let url = Url::parse(&format!(
            "{}/{}/json",
            self.base,
            crate::text::encode_path_segment(name)
        ))
        .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("pypi request failed: {e}")))?;
        let status = resp.status();
        if status.as_u16() == 404 {
            // Unknown package name -> simply no results, not an error.
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(pypi_parse(&body, name))
    }
}

pub fn pypi_parse(body: &str, name: &str) -> Vec<SearchResult> {
    let Ok(resp) = serde_json::from_str::<PyPiResp>(body) else {
        return Vec::new();
    };
    let Some(info) = resp.info else {
        return Vec::new();
    };
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
    let version = info.version.map(|v| format!("v{v}")).unwrap_or_default();
    vec![SearchResult {
        title,
        url,
        snippet: normalize_snippet(&join_meta(&[&summary, &version])),
    }]
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
        let r = crates_parse(body);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "tokio");
        assert_eq!(r[0].url, "https://github.com/tokio-rs/tokio");
        assert_eq!(r[0].snippet, "Async runtime · v1.0.0 · 100 downloads");
        assert_eq!(r[1].url, "https://crates.io/crates/nolib");
    }

    #[test]
    fn npm_parses_objects() {
        let body = r#"{"objects":[
          {"package":{"name":"async","version":"3.2.6","description":"Async utils","links":{"npm":"https://www.npmjs.com/package/async"}},"downloads":{"monthly":1000}}
        ]}"#;
        let r = npm_parse(body);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "async");
        assert_eq!(r[0].url, "https://www.npmjs.com/package/async");
        assert_eq!(r[0].snippet, "Async utils · v3.2.6 · 1000 downloads/mo");
    }

    #[test]
    fn pypi_is_single_result_lookup() {
        let body = r#"{"info":{"name":"requests","summary":"HTTP for humans","version":"2.31.0","package_url":"https://pypi.org/project/requests/"}}"#;
        let r = pypi_parse(body, "requests");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].title, "requests");
        assert_eq!(r[0].url, "https://pypi.org/project/requests/");
        assert_eq!(r[0].snippet, "HTTP for humans · v2.31.0");
    }

    #[test]
    fn bad_json_yields_empty() {
        assert!(crates_parse("x").is_empty());
        assert!(npm_parse("x").is_empty());
        assert!(pypi_parse("x", "y").is_empty());
    }
}
