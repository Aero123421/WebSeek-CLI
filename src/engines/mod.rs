//! Search engine abstraction.
//!
//! Engines are behind a small trait so that the HTML scrapers can be swapped,
//! fixed, or extended (e.g. a SearXNG JSON endpoint later) without touching
//! the CLI layer. All engines are API-key-free.
//!
//! Parsing functions are pure (`&str` in, typed data out) so they can be
//! unit-tested against captured fixtures without network access.

pub mod academic;
pub mod bing;
pub mod duckduckgo;
pub mod hackernews;
pub mod images;
pub mod nominatim;
pub mod packages;
pub mod reddit;
pub mod stackexchange;
pub mod wikipedia;

use reqwest::blocking::Client;

use crate::error::Result;
use crate::models::{ImageResult, SearchOpts, SearchResult};

/// A text search backend.
pub trait SearchEngine {
    fn name(&self) -> &'static str;
    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>>;
}

/// An image search backend.
pub trait ImageEngine {
    fn name(&self) -> &'static str;
    fn search(
        &self,
        client: &Client,
        query: &str,
        count: usize,
        safe: bool,
    ) -> Result<Vec<ImageResult>>;
}

/// Build the requested text engine.
pub fn engine_by_name(name: &str) -> Result<Box<dyn SearchEngine>> {
    match name {
        "duckduckgo" => Ok(Box::<duckduckgo::DuckDuckGo>::default()),
        "bing" => Ok(Box::<bing::Bing>::default()),
        "wikipedia" => Ok(Box::<wikipedia::Wikipedia>::default()),
        "hackernews" | "hn" => Ok(Box::<hackernews::HackerNews>::default()),
        "stackexchange" | "stackoverflow" => Ok(Box::<stackexchange::StackExchange>::default()),
        "openalex" => Ok(Box::<academic::OpenAlex>::default()),
        "crossref" => Ok(Box::<academic::CrossRef>::default()),
        "pubmed" => Ok(Box::<academic::PubMed>::default()),
        "crates" | "crates.io" => Ok(Box::<packages::Crates>::default()),
        "npm" => Ok(Box::<packages::Npm>::default()),
        "pypi" => Ok(Box::<packages::PyPi>::default()),
        "nominatim" | "osm" => Ok(Box::<nominatim::Nominatim>::default()),
        "reddit" => Ok(Box::<reddit::Reddit>::default()),
        other => Err(crate::error::Error::Config(format!(
            "unknown engine '{other}' (run `webseek engines` to list all)"
        ))),
    }
}

/// Machine-readable description of an engine, for the `engines` subcommand so
/// an agent can discover which source fits a query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineInfo {
    pub name: &'static str,
    /// web | encyclopedia | news | qa | academic | package | geo | images
    pub kind: &'static str,
    pub description: &'static str,
    pub example: &'static str,
}

/// All known engines (text + images) with usage hints.
pub fn catalog() -> Vec<EngineInfo> {
    vec![
        EngineInfo {
            name: "duckduckgo",
            kind: "web",
            description: "General web search (HTML endpoint).",
            example: "webseek search \"rust async\" --engine duckduckgo",
        },
        EngineInfo {
            name: "bing",
            kind: "web",
            description: "General web search via stable RSS (challenge-resistant).",
            example: "webseek search \"rust async\" --engine bing",
        },
        EngineInfo {
            name: "wikipedia",
            kind: "encyclopedia",
            description: "Encyclopedia articles; --lang selects the language edition.",
            example: "webseek search \"Tokyo\" --engine wikipedia --lang ja",
        },
        EngineInfo {
            name: "hackernews",
            kind: "news",
            description: "Hacker News stories & comments (tech news/discussion). Alias: hn.",
            example: "webseek search \"rust\" --engine hackernews",
        },
        EngineInfo {
            name: "reddit",
            kind: "news",
            description: "Reddit posts via public RSS (no key; rate-limit-strict, use sparingly).",
            example: "webseek search \"rust\" --engine reddit",
        },
        EngineInfo {
            name: "stackexchange",
            kind: "qa",
            description: "Stack Overflow programming Q&A. Alias: stackoverflow.",
            example: "webseek search \"async runtime\" --engine stackexchange",
        },
        EngineInfo {
            name: "openalex",
            kind: "academic",
            description: "Broad open catalog of scholarly works (all fields).",
            example: "webseek search \"transformers\" --engine openalex",
        },
        EngineInfo {
            name: "crossref",
            kind: "academic",
            description: "DOI and citation metadata for papers.",
            example: "webseek search \"quantum\" --engine crossref",
        },
        EngineInfo {
            name: "pubmed",
            kind: "academic",
            description: "Biomedical literature (NCBI PubMed).",
            example: "webseek search \"immunotherapy\" --engine pubmed",
        },
        EngineInfo {
            name: "crates",
            kind: "package",
            description: "Rust crates keyword search (crates.io). Alias: crates.io.",
            example: "webseek search \"async\" --engine crates",
        },
        EngineInfo {
            name: "npm",
            kind: "package",
            description: "JavaScript package keyword search (npm).",
            example: "webseek search \"async\" --engine npm",
        },
        EngineInfo {
            name: "pypi",
            kind: "package",
            description: "Python package lookup by exact name (PyPI; no keyword API).",
            example: "webseek search \"requests\" --engine pypi",
        },
        EngineInfo {
            name: "nominatim",
            kind: "geo",
            description: "OpenStreetMap geocoding (places). Alias: osm.",
            example: "webseek search \"Tokyo\" --engine nominatim",
        },
        EngineInfo {
            name: "bing (images)",
            kind: "images",
            description: "Image search; use the `images` subcommand.",
            example: "webseek images \"cats\" --engine bing",
        },
        EngineInfo {
            name: "duckduckgo (images)",
            kind: "images",
            description: "Image search; use the `images` subcommand.",
            example: "webseek images \"cats\" --engine duckduckgo",
        },
    ]
}

/// Build the requested image engine.
pub fn image_engine_by_name(name: &str) -> Result<Box<dyn ImageEngine>> {
    match name {
        "duckduckgo" => Ok(Box::<images::DuckDuckGoImages>::default()),
        "bing" => Ok(Box::<images::BingImages>::default()),
        other => Err(crate::error::Error::Config(format!(
            "unknown image engine '{other}' (expected one of: duckduckgo, bing)"
        ))),
    }
}

/// Keep only unique URLs, preserving engine order.
pub fn dedupe_by_url<T>(items: Vec<T>, url_of: impl Fn(&T) -> &str) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|it| seen.insert(url_of(it).to_string()))
        .collect()
}

/// Known text search engines, in default preference order.
pub const TEXT_ENGINES: &[&str] = &["duckduckgo", "bing"];

/// Known image search engines, in default preference order.
pub const IMAGE_ENGINES: &[&str] = &["bing", "duckduckgo"];

/// Build the fallback order: the requested engine first, then every other
/// known engine. Used to try engines in turn until one succeeds.
pub fn fallback_order<'a>(requested: &'a str, universe: &[&'a str]) -> Vec<&'a str> {
    let mut order = vec![requested];
    for e in universe {
        if *e != requested {
            order.push(e);
        }
    }
    order
}

/// Heuristic markers that a search page is a bot challenge rather than a
/// result page. Engines check this before parsing so agents get an explicit
/// rate-limit error instead of silently empty results.
pub fn looks_like_challenge(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    const MARKERS: &[&str] = &[
        "captcha",
        "b_captcha",
        "unusual traffic",
        "verify you are human",
        "checking your browser",
        "robot check",
    ];
    MARKERS.iter().any(|m| lower.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fallback_order_puts_requested_first_then_others() {
        assert_eq!(
            fallback_order("bing", TEXT_ENGINES),
            vec!["bing", "duckduckgo"]
        );
        assert_eq!(
            fallback_order("duckduckgo", TEXT_ENGINES),
            vec!["duckduckgo", "bing"]
        );
        // Unknown requested engine still leads, others follow.
        assert_eq!(
            fallback_order("searxng", TEXT_ENGINES),
            vec!["searxng", "duckduckgo", "bing"]
        );
    }
}
