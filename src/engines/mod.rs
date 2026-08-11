//! Search engine abstraction.
//!
//! Engines are behind a small trait so that the HTML scrapers can be swapped,
//! fixed, or extended (e.g. a SearXNG JSON endpoint later) without touching
//! the CLI layer. All engines are API-key-free.
//!
//! Parsing functions are pure (`&str` in, typed data out) so they can be
//! unit-tested against captured fixtures without network access.
//!
//! **One registry, three views.** [`TEXT_REGISTRY`] and [`IMAGE_REGISTRY`] are
//! the single source of truth: engine lookup, config validation and the
//! `engines` catalog are all derived from them, so adding an engine in one
//! place makes it visible everywhere.

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

/// Engine kind. `Web` engines are interchangeable general-purpose search and
/// are the only ones that participate in automatic fallback: swapping a
/// *vertical* (say `pubmed`) for a web engine would silently answer a
/// different question. See [`fallback_order`].
pub const KIND_WEB: &str = "web";

/// One entry of the engine registry: everything the CLI needs to know about
/// an engine, including how to build it.
pub struct EngineSpec {
    /// Canonical name — this is exactly what `--engine` accepts.
    pub name: &'static str,
    /// Additional accepted spellings.
    pub aliases: &'static [&'static str],
    /// web | encyclopedia | news | qa | academic | package | geo
    pub kind: &'static str,
    pub description: &'static str,
    pub example: &'static str,
    build: fn() -> Box<dyn SearchEngine>,
}

impl EngineSpec {
    /// Does `name` address this engine (canonical name or alias)?
    pub fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }
}

/// Same, for image engines.
pub struct ImageEngineSpec {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub example: &'static str,
    build: fn() -> Box<dyn ImageEngine>,
}

impl ImageEngineSpec {
    pub fn matches(&self, name: &str) -> bool {
        self.name == name || self.aliases.contains(&name)
    }
}

/// Every text engine. Order matters: web engines are listed in fallback
/// preference order.
pub static TEXT_REGISTRY: &[EngineSpec] = &[
    EngineSpec {
        name: "duckduckgo",
        aliases: &["ddg"],
        kind: KIND_WEB,
        description: "General web search (HTML endpoint).",
        example: "webseek search \"rust async\" --engine duckduckgo",
        build: || Box::<duckduckgo::DuckDuckGo>::default(),
    },
    EngineSpec {
        name: "bing",
        aliases: &[],
        kind: KIND_WEB,
        description: "General web search via stable RSS (challenge-resistant).",
        example: "webseek search \"rust async\" --engine bing",
        build: || Box::<bing::Bing>::default(),
    },
    EngineSpec {
        name: "wikipedia",
        aliases: &["wiki"],
        kind: "encyclopedia",
        description: "Encyclopedia articles; --lang selects the language edition.",
        example: "webseek search \"Tokyo\" --engine wikipedia --lang ja",
        build: || Box::<wikipedia::Wikipedia>::default(),
    },
    EngineSpec {
        name: "hackernews",
        aliases: &["hn"],
        kind: "news",
        description: "Hacker News stories & comments (tech news/discussion).",
        example: "webseek search \"rust\" --engine hackernews",
        build: || Box::<hackernews::HackerNews>::default(),
    },
    EngineSpec {
        name: "reddit",
        aliases: &[],
        kind: "news",
        description: "Reddit posts via public RSS (no key; rate-limit-strict, use sparingly).",
        example: "webseek search \"rust\" --engine reddit",
        build: || Box::<reddit::Reddit>::default(),
    },
    EngineSpec {
        name: "stackexchange",
        aliases: &["stackoverflow", "so"],
        kind: "qa",
        description: "Stack Overflow programming Q&A.",
        example: "webseek search \"async runtime\" --engine stackexchange",
        build: || Box::<stackexchange::StackExchange>::default(),
    },
    EngineSpec {
        name: "openalex",
        aliases: &[],
        kind: "academic",
        description: "Broad open catalog of scholarly works (all fields).",
        example: "webseek search \"transformers\" --engine openalex",
        build: || Box::<academic::OpenAlex>::default(),
    },
    EngineSpec {
        name: "crossref",
        aliases: &[],
        kind: "academic",
        description: "DOI and citation metadata for papers.",
        example: "webseek search \"quantum\" --engine crossref",
        build: || Box::<academic::CrossRef>::default(),
    },
    EngineSpec {
        name: "pubmed",
        aliases: &[],
        kind: "academic",
        description: "Biomedical literature (NCBI PubMed).",
        example: "webseek search \"immunotherapy\" --engine pubmed",
        build: || Box::<academic::PubMed>::default(),
    },
    EngineSpec {
        name: "crates",
        aliases: &["crates.io"],
        kind: "package",
        description: "Rust crates keyword search (crates.io).",
        example: "webseek search \"async\" --engine crates",
        build: || Box::<packages::Crates>::default(),
    },
    EngineSpec {
        name: "npm",
        aliases: &[],
        kind: "package",
        description: "JavaScript package keyword search (npm).",
        example: "webseek search \"async\" --engine npm",
        build: || Box::<packages::Npm>::default(),
    },
    EngineSpec {
        name: "pypi",
        aliases: &[],
        kind: "package",
        description: "Python package lookup by exact name (PyPI; no keyword API).",
        example: "webseek search \"requests\" --engine pypi",
        build: || Box::<packages::PyPi>::default(),
    },
    EngineSpec {
        name: "nominatim",
        aliases: &["osm"],
        kind: "geo",
        description: "OpenStreetMap geocoding (places).",
        example: "webseek search \"Tokyo\" --engine nominatim",
        build: || Box::<nominatim::Nominatim>::default(),
    },
];

/// Every image engine, in fallback preference order.
pub static IMAGE_REGISTRY: &[ImageEngineSpec] = &[
    ImageEngineSpec {
        name: "bing",
        aliases: &[],
        description: "Bing image search.",
        example: "webseek images \"cats\" --engine bing",
        build: || Box::<images::BingImages>::default(),
    },
    ImageEngineSpec {
        name: "duckduckgo",
        aliases: &["ddg"],
        description: "DuckDuckGo image search.",
        example: "webseek images \"cats\" --engine duckduckgo",
        build: || Box::<images::DuckDuckGoImages>::default(),
    },
];

fn text_spec(name: &str) -> Option<&'static EngineSpec> {
    TEXT_REGISTRY.iter().find(|s| s.matches(name))
}

fn image_spec(name: &str) -> Option<&'static ImageEngineSpec> {
    IMAGE_REGISTRY.iter().find(|s| s.matches(name))
}

fn unknown_engine(name: &str) -> crate::error::Error {
    crate::error::Error::Config(format!(
        "unknown engine '{name}' (run `webseek engines` to list all)"
    ))
}

/// Build the requested text engine.
pub fn engine_by_name(name: &str) -> Result<Box<dyn SearchEngine>> {
    text_spec(name)
        .map(|s| (s.build)())
        .ok_or_else(|| unknown_engine(name))
}

/// Build the requested image engine.
pub fn image_engine_by_name(name: &str) -> Result<Box<dyn ImageEngine>> {
    image_spec(name).map(|s| (s.build)()).ok_or_else(|| {
        crate::error::Error::Config(format!(
            "unknown image engine '{name}' (expected one of: {})",
            IMAGE_REGISTRY
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(", ")
        ))
    })
}

/// Canonical name for an engine spelling (resolves aliases). `None` if unknown.
pub fn canonical_engine(name: &str) -> Option<&'static str> {
    text_spec(name).map(|s| s.name)
}

/// Canonical name for an image engine spelling. `None` if unknown.
pub fn canonical_image_engine(name: &str) -> Option<&'static str> {
    image_spec(name).map(|s| s.name)
}

/// Is `name` a known text engine (canonical or alias)?
pub fn is_known_engine(name: &str) -> bool {
    text_spec(name).is_some()
}

/// Is `name` a known image engine (canonical or alias)?
pub fn is_known_image_engine(name: &str) -> bool {
    image_spec(name).is_some()
}

/// Machine-readable description of an engine, for the `engines` subcommand so
/// an agent can discover which source fits a query.
///
/// `name` and every entry of `aliases` are literal `--engine` values.
#[derive(Debug, Clone, serde::Serialize)]
pub struct EngineInfo {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    /// web | encyclopedia | news | qa | academic | package | geo | images
    pub kind: &'static str,
    /// Subcommand this engine is used with: "search" or "images".
    pub command: &'static str,
    pub description: &'static str,
    pub example: &'static str,
    /// Whether automatic fallback may substitute another engine for this one.
    pub fallback: bool,
}

/// All known engines (text + images) with usage hints, derived from the
/// registries so it can never drift out of sync with `--engine`.
pub fn catalog() -> Vec<EngineInfo> {
    TEXT_REGISTRY
        .iter()
        .map(|s| EngineInfo {
            name: s.name,
            aliases: s.aliases,
            kind: s.kind,
            command: "search",
            description: s.description,
            example: s.example,
            fallback: s.kind == KIND_WEB,
        })
        .chain(IMAGE_REGISTRY.iter().map(|s| EngineInfo {
            name: s.name,
            aliases: s.aliases,
            kind: "images",
            command: "images",
            description: s.description,
            example: s.example,
            fallback: true,
        }))
        .collect()
}

/// Keep only unique URLs, preserving engine order.
pub fn dedupe_by_url<T>(items: Vec<T>, url_of: impl Fn(&T) -> &str) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|it| seen.insert(url_of(it).to_string()))
        .collect()
}

/// Deduplicate, then cut to `count`.
///
/// The order matters: truncating first would let duplicates eat into the
/// requested result count and silently under-deliver.
pub fn dedupe_and_truncate<T>(items: Vec<T>, count: usize, url_of: impl Fn(&T) -> &str) -> Vec<T> {
    let mut out = dedupe_by_url(items, url_of);
    out.truncate(count);
    out
}

/// Build the fallback order for `requested`.
///
/// Only general-purpose **web** engines are interchangeable, so only they get
/// alternatives. A vertical (`pubmed`, `crates`, ...) answers a different
/// question than a web search does; silently substituting one would hand an
/// agent results it cannot tell apart from the ones it asked for.
pub fn fallback_order(requested: &str) -> Vec<&'static str> {
    let Some(spec) = text_spec(requested) else {
        return Vec::new();
    };
    if spec.kind != KIND_WEB {
        return vec![spec.name];
    }
    let mut order = vec![spec.name];
    for s in TEXT_REGISTRY.iter().filter(|s| s.kind == KIND_WEB) {
        if s.name != spec.name {
            order.push(s.name);
        }
    }
    order
}

/// Fallback order for image engines (all image engines are interchangeable).
pub fn image_fallback_order(requested: &str) -> Vec<&'static str> {
    let Some(spec) = image_spec(requested) else {
        return Vec::new();
    };
    let mut order = vec![spec.name];
    for s in IMAGE_REGISTRY.iter() {
        if s.name != spec.name {
            order.push(s.name);
        }
    }
    order
}

/// Heuristic markers that a search page is a bot challenge rather than a
/// result page. Engines check this before parsing so agents get an explicit
/// rate-limit error instead of silently empty results.
///
/// Detection is **structural**. It deliberately does *not* scan body prose:
/// a legitimate results page for the query "captcha" contains every keyword a
/// naive substring scan would look for, and search engines echo the query into
/// the `<title>`, so title keywords alone are not evidence either.
///
/// Two signals are used:
/// 1. **Challenge markup** — element ids/classes and script sources that only
///    ever appear on an interstitial (`#b_captcha`, reCAPTCHA, Turnstile, …).
/// 2. **A challenge title on a tiny page.** Interstitials are a few KB; a real
///    result page is tens of KB. The size gate is what keeps a search *for*
///    "just a moment" from being mistaken for the Cloudflare page of that name.
pub fn looks_like_challenge(body: &str) -> bool {
    /// Markup fragments that only appear on real challenge pages.
    const MARKUP_MARKERS: &[&str] = &[
        "id=\"b_captcha\"",
        "id='b_captcha'",
        "id=\"captcha-form\"",
        "class=\"g-recaptcha\"",
        "id=\"challenge-form\"",
        "id=\"cf-challenge-running\"",
        "cf-browser-verification",
        "/recaptcha/api.js",
        "challenges.cloudflare.com/turnstile",
        "name=\"captcha_answer\"",
    ];
    /// Phrases that indicate a challenge in the <title> of a very small page.
    const TITLE_MARKERS: &[&str] = &[
        "captcha",
        "unusual traffic",
        "verify you are human",
        "just a moment",
        "checking your browser",
        "robot check",
        "are you a robot",
    ];
    /// Interstitials are small; real result pages are not.
    const SMALL_PAGE_BYTES: usize = 8 * 1024;

    let lower = body.to_ascii_lowercase();
    if MARKUP_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    if body.len() >= SMALL_PAGE_BYTES {
        return false;
    }
    let title = crate::text::extract_tag(&lower, "title");
    !title.is_empty() && TITLE_MARKERS.iter().any(|m| title.contains(m))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_engines_fall_back_to_each_other() {
        assert_eq!(fallback_order("bing"), vec!["bing", "duckduckgo"]);
        assert_eq!(fallback_order("duckduckgo"), vec!["duckduckgo", "bing"]);
        // Aliases resolve to the canonical name.
        assert_eq!(fallback_order("ddg"), vec!["duckduckgo", "bing"]);
    }

    #[test]
    fn verticals_never_fall_back_to_web_search() {
        // A vertical answers a different question; substituting a web engine
        // would hand back results the agent cannot distinguish.
        for vertical in ["pubmed", "crates", "wikipedia", "nominatim", "reddit"] {
            assert_eq!(
                fallback_order(vertical),
                vec![vertical],
                "{vertical} must not fall back"
            );
        }
    }

    #[test]
    fn unknown_engine_has_no_fallback_order() {
        assert!(fallback_order("searxng").is_empty());
    }

    #[test]
    fn registry_is_the_only_source_of_truth() {
        // Every catalog entry must be a usable `--engine` value.
        for info in catalog() {
            match info.command {
                "search" => assert!(
                    is_known_engine(info.name),
                    "catalog lists text engine '{}' that --engine rejects",
                    info.name
                ),
                "images" => assert!(
                    is_known_image_engine(info.name),
                    "catalog lists image engine '{}' that --engine rejects",
                    info.name
                ),
                other => panic!("unexpected command {other}"),
            }
            for alias in info.aliases {
                assert!(
                    is_known_engine(alias) || is_known_image_engine(alias),
                    "advertised alias '{alias}' is not accepted"
                );
            }
        }
    }

    #[test]
    fn engine_names_and_aliases_are_unique() {
        let mut seen = std::collections::HashSet::new();
        for spec in TEXT_REGISTRY {
            assert!(seen.insert(spec.name), "duplicate engine {}", spec.name);
            for a in spec.aliases {
                assert!(seen.insert(a), "alias {a} collides");
            }
        }
    }

    #[test]
    fn dedupe_and_truncate_delivers_the_requested_count() {
        let items = vec!["a", "a", "b", "c"];
        // Truncate-then-dedupe would return 2 here; we must return 3.
        let out = dedupe_and_truncate(items, 3, |s| s);
        assert_eq!(out, vec!["a", "b", "c"]);
    }

    /// A realistically-sized results page for a query about challenges.
    fn results_page_for(query: &str) -> String {
        let rows = (0..40)
            .map(|i| {
                format!(
                    "<li class=\"b_algo\"><h2><a href=\"https://example.com/{i}\">{query} \
                     explained in detail</a></h2><p>A {query} is a challenge-response test \
                     used to tell humans and bots apart. Verify you are human, checking your \
                     browser, robot check — all common phrasings.</p></li>"
                )
            })
            .collect::<String>();
        format!("<html><head><title>{query} - Bing</title></head><body><ol id=\"b_results\">{rows}</ol></body></html>")
    }

    #[test]
    fn challenge_detection_ignores_pages_that_merely_discuss_captchas() {
        // The exact false positive that made `search "captcha"` unusable.
        for query in [
            "captcha",
            "unusual traffic",
            "verify you are human",
            "robot check",
            "just a moment",
        ] {
            let page = results_page_for(query);
            assert!(
                !looks_like_challenge(&page),
                "a real results page for '{query}' must not be flagged"
            );
        }
    }

    #[test]
    fn challenge_detection_catches_real_challenge_markup() {
        assert!(looks_like_challenge(
            r#"<html><body><div id="b_captcha">…</div></body></html>"#
        ));
        assert!(looks_like_challenge(
            r#"<html><head><title>Just a moment...</title></head><body></body></html>"#
        ));
        assert!(looks_like_challenge(
            r#"<html><body><script src="https://www.google.com/recaptcha/api.js"></script></body></html>"#
        ));
    }
}
