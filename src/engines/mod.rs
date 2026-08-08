//! Search engine abstraction and the engine registry.
//!
//! Engines are behind a small trait so that the HTML scrapers can be swapped,
//! fixed, or extended (e.g. a SearXNG JSON endpoint later) without touching
//! the CLI layer. All engines are API-key-free.
//!
//! Parsing functions are pure (`&str` in, typed data out) so they can be
//! unit-tested against captured fixtures without network access.
//!
//! **Everything that used to be listed in four separate places** — the
//! `engine_by_name`/`image_engine_by_name` match arms, `validate_engine`,
//! the `catalog()` shown by `webseek engines`, and the fallback universe —
//! is now generated from one array of [`EngineDescriptor`]s. Adding an
//! engine or renaming an alias means editing one entry; the old design let
//! these drift (e.g. the catalog listed the image engines as `"bing
//! (images)"`, a string you could not actually pass to `--engine`).

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

use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{ImageResult, SearchOpts, SearchResult};

/// A text search backend. Takes `&Http` (not a raw client) so every engine
/// automatically gets the egress policy, shared rate limiter, and retry
/// behavior — an engine cannot accidentally bypass them by holding its own
/// client reference.
pub trait SearchEngine {
    fn name(&self) -> &'static str;
    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>>;
}

/// An image search backend.
pub trait ImageEngine {
    fn name(&self) -> &'static str;
    fn search(
        &self,
        http: &Http,
        query: &str,
        count: usize,
        safe: bool,
    ) -> Result<Vec<ImageResult>>;
}

/// Fallback-equivalence class. Automatic engine fallback only ever tries
/// another engine with the **same** capability.
///
/// `GeneralWeb` and `Images` are the only capabilities with more than one
/// member, because DuckDuckGo/Bing (and their image-search counterparts) are
/// genuinely interchangeable general-purpose sources. Every vertical below
/// gets its own capability even when another engine is superficially
/// "in the same category" (OpenAlex/CrossRef/PubMed are all "academic", but
/// silently answering a PubMed biomedical-literature query with OpenAlex's
/// all-fields index — or a crates.io Rust-crate query with npm's JS registry
/// — changes what was actually searched just as much as answering it with
/// DuckDuckGo would). Since each vertical is the only member of its own
/// capability, [`fallback_order`] naturally produces a single-element order
/// for it: on failure it reports the error instead of quietly substituting a
/// different data source.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capability {
    GeneralWeb,
    Images,
    Encyclopedia,
    HackerNews,
    Reddit,
    StackExchange,
    OpenAlex,
    CrossRef,
    PubMed,
    CratesIo,
    Npm,
    PyPi,
    Nominatim,
}

impl Capability {
    /// Display category used in `webseek engines` output (`EngineInfo::kind`).
    /// Coarser than `Capability` itself — several verticals share a display
    /// category (e.g. `academic`) while still being distinct fallback
    /// islands; see the type-level doc comment for why.
    fn kind(self) -> &'static str {
        match self {
            Capability::GeneralWeb => "web",
            Capability::Images => "images",
            Capability::Encyclopedia => "encyclopedia",
            Capability::HackerNews | Capability::Reddit => "news",
            Capability::StackExchange => "qa",
            Capability::OpenAlex | Capability::CrossRef | Capability::PubMed => "academic",
            Capability::CratesIo | Capability::Npm | Capability::PyPi => "package",
            Capability::Nominatim => "geo",
        }
    }
}

/// One text engine's full identity: names, capability, factory, and the
/// metadata shown by `webseek engines`.
pub struct EngineDescriptor {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub capability: Capability,
    pub description: &'static str,
    pub example: &'static str,
    factory: fn() -> Box<dyn SearchEngine>,
}

/// One image engine's identity (images have no verticals, so no capability
/// field is needed — all image engines are mutually interchangeable).
pub struct ImageEngineDescriptor {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub example: &'static str,
    factory: fn() -> Box<dyn ImageEngine>,
}

fn new_duckduckgo() -> Box<dyn SearchEngine> {
    Box::<duckduckgo::DuckDuckGo>::default()
}
fn new_bing() -> Box<dyn SearchEngine> {
    Box::<bing::Bing>::default()
}
fn new_wikipedia() -> Box<dyn SearchEngine> {
    Box::<wikipedia::Wikipedia>::default()
}
fn new_hackernews() -> Box<dyn SearchEngine> {
    Box::<hackernews::HackerNews>::default()
}
fn new_reddit() -> Box<dyn SearchEngine> {
    Box::<reddit::Reddit>::default()
}
fn new_stackexchange() -> Box<dyn SearchEngine> {
    Box::<stackexchange::StackExchange>::default()
}
fn new_openalex() -> Box<dyn SearchEngine> {
    Box::<academic::OpenAlex>::default()
}
fn new_crossref() -> Box<dyn SearchEngine> {
    Box::<academic::CrossRef>::default()
}
fn new_pubmed() -> Box<dyn SearchEngine> {
    Box::<academic::PubMed>::default()
}
fn new_crates() -> Box<dyn SearchEngine> {
    Box::<packages::Crates>::default()
}
fn new_npm() -> Box<dyn SearchEngine> {
    Box::<packages::Npm>::default()
}
fn new_pypi() -> Box<dyn SearchEngine> {
    Box::<packages::PyPi>::default()
}
fn new_nominatim() -> Box<dyn SearchEngine> {
    Box::<nominatim::Nominatim>::default()
}
fn new_bing_images() -> Box<dyn ImageEngine> {
    Box::<images::BingImages>::default()
}
fn new_ddg_images() -> Box<dyn ImageEngine> {
    Box::<images::DuckDuckGoImages>::default()
}

/// Every known text engine. This is the single source of truth: CLI help,
/// `validate_engine`, `engine_by_name`, `webseek engines`, and fallback
/// grouping are all generated from it.
pub static TEXT_REGISTRY: &[EngineDescriptor] = &[
    EngineDescriptor {
        name: "duckduckgo",
        aliases: &[],
        capability: Capability::GeneralWeb,
        description: "General web search (HTML endpoint).",
        example: "webseek search \"rust async\" --engine duckduckgo",
        factory: new_duckduckgo,
    },
    EngineDescriptor {
        name: "bing",
        aliases: &[],
        capability: Capability::GeneralWeb,
        description: "General web search via stable RSS (challenge-resistant).",
        example: "webseek search \"rust async\" --engine bing",
        factory: new_bing,
    },
    EngineDescriptor {
        name: "wikipedia",
        aliases: &[],
        capability: Capability::Encyclopedia,
        description: "Encyclopedia articles; --lang selects the language edition.",
        example: "webseek search \"Tokyo\" --engine wikipedia --lang ja",
        factory: new_wikipedia,
    },
    EngineDescriptor {
        name: "hackernews",
        aliases: &["hn"],
        capability: Capability::HackerNews,
        description: "Hacker News stories & comments (tech news/discussion). Alias: hn.",
        example: "webseek search \"rust\" --engine hackernews",
        factory: new_hackernews,
    },
    EngineDescriptor {
        name: "reddit",
        aliases: &[],
        capability: Capability::Reddit,
        description: "Reddit posts via public RSS (no key; rate-limit-strict, use sparingly).",
        example: "webseek search \"rust\" --engine reddit",
        factory: new_reddit,
    },
    EngineDescriptor {
        name: "stackexchange",
        aliases: &["stackoverflow"],
        capability: Capability::StackExchange,
        description: "Stack Overflow programming Q&A. Alias: stackoverflow.",
        example: "webseek search \"async runtime\" --engine stackexchange",
        factory: new_stackexchange,
    },
    EngineDescriptor {
        name: "openalex",
        aliases: &[],
        capability: Capability::OpenAlex,
        description: "Broad open catalog of scholarly works (all fields).",
        example: "webseek search \"transformers\" --engine openalex",
        factory: new_openalex,
    },
    EngineDescriptor {
        name: "crossref",
        aliases: &[],
        capability: Capability::CrossRef,
        description: "DOI and citation metadata for papers.",
        example: "webseek search \"quantum\" --engine crossref",
        factory: new_crossref,
    },
    EngineDescriptor {
        name: "pubmed",
        aliases: &[],
        capability: Capability::PubMed,
        description: "Biomedical literature (NCBI PubMed).",
        example: "webseek search \"immunotherapy\" --engine pubmed",
        factory: new_pubmed,
    },
    EngineDescriptor {
        name: "crates",
        aliases: &["crates.io"],
        capability: Capability::CratesIo,
        description: "Rust crates keyword search (crates.io). Alias: crates.io.",
        example: "webseek search \"async\" --engine crates",
        factory: new_crates,
    },
    EngineDescriptor {
        name: "npm",
        aliases: &[],
        capability: Capability::Npm,
        description: "JavaScript package keyword search (npm).",
        example: "webseek search \"async\" --engine npm",
        factory: new_npm,
    },
    EngineDescriptor {
        name: "pypi",
        aliases: &[],
        capability: Capability::PyPi,
        description: "Python package lookup by exact name (PyPI; no keyword API).",
        example: "webseek search \"requests\" --engine pypi",
        factory: new_pypi,
    },
    EngineDescriptor {
        name: "nominatim",
        aliases: &["osm"],
        capability: Capability::Nominatim,
        description: "OpenStreetMap geocoding (places). Alias: osm.",
        example: "webseek search \"Tokyo\" --engine nominatim",
        factory: new_nominatim,
    },
];

/// Every known image engine.
pub static IMAGE_REGISTRY: &[ImageEngineDescriptor] = &[
    ImageEngineDescriptor {
        name: "bing",
        aliases: &[],
        description: "Bing Images search.",
        example: "webseek images \"cats\" --engine bing",
        factory: new_bing_images,
    },
    ImageEngineDescriptor {
        name: "duckduckgo",
        aliases: &[],
        description: "DuckDuckGo Images search.",
        example: "webseek images \"cats\" --engine duckduckgo",
        factory: new_ddg_images,
    },
];

fn find_text(name: &str) -> Option<&'static EngineDescriptor> {
    let lower = name.to_ascii_lowercase();
    TEXT_REGISTRY
        .iter()
        .find(|d| d.name == lower || d.aliases.contains(&lower.as_str()))
}

fn find_image(name: &str) -> Option<&'static ImageEngineDescriptor> {
    let lower = name.to_ascii_lowercase();
    IMAGE_REGISTRY
        .iter()
        .find(|d| d.name == lower || d.aliases.contains(&lower.as_str()))
}

/// Resolve any accepted alias to its canonical registry name. Used to
/// canonicalize a name *before* it becomes part of a cache key — without
/// this, `--engine hn` and `--engine hackernews` cache under different keys
/// even though they select the same engine.
pub fn canonical_name(name: &str) -> Option<&'static str> {
    find_text(name).map(|d| d.name)
}

pub fn canonical_image_name(name: &str) -> Option<&'static str> {
    find_image(name).map(|d| d.name)
}

/// Build the requested text engine. Unknown names are a usage error (exit
/// code 2): the caller typed or configured a value that isn't one of ours.
pub fn engine_by_name(name: &str) -> Result<Box<dyn SearchEngine>> {
    find_text(name).map(|d| (d.factory)()).ok_or_else(|| {
        Error::Usage(format!(
            "unknown engine '{name}' (run `webseek engines` to list all)"
        ))
    })
}

/// Build the requested image engine.
pub fn image_engine_by_name(name: &str) -> Result<Box<dyn ImageEngine>> {
    find_image(name).map(|d| (d.factory)()).ok_or_else(|| {
        let names: Vec<&str> = IMAGE_REGISTRY.iter().map(|d| d.name).collect();
        Error::Usage(format!(
            "unknown image engine '{name}' (expected one of: {})",
            names.join(", ")
        ))
    })
}

pub fn validate_engine(name: &str) -> Result<()> {
    engine_by_name(name).map(|_| ())
}

pub fn validate_image_engine(name: &str) -> Result<()> {
    image_engine_by_name(name).map(|_| ())
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

/// All known engines (text + images) with usage hints, generated from the
/// registries above. Image engines are listed under their real, passable
/// `--engine` name (previously shown as the non-canonical `"bing (images)"`);
/// `kind: "images"` is what marks them as image-only.
pub fn catalog() -> Vec<EngineInfo> {
    let mut out: Vec<EngineInfo> = TEXT_REGISTRY
        .iter()
        .map(|d| EngineInfo {
            name: d.name,
            kind: d.capability.kind(),
            description: d.description,
            example: d.example,
        })
        .collect();
    out.extend(IMAGE_REGISTRY.iter().map(|d| EngineInfo {
        name: d.name,
        kind: "images",
        description: d.description,
        example: d.example,
    }));
    out
}

/// Keep only unique URLs, preserving engine order.
pub fn dedupe_by_url<T>(items: Vec<T>, url_of: impl Fn(&T) -> &str) -> Vec<T> {
    let mut seen = std::collections::HashSet::new();
    items
        .into_iter()
        .filter(|it| seen.insert(url_of(it).to_string()))
        .collect()
}

/// Build the fallback order for a text engine: the requested engine first,
/// then every other engine sharing its capability (see [`Capability`]'s doc
/// comment for why most verticals end up alone in this list).
pub fn fallback_order(requested: &str) -> Vec<&'static str> {
    let Some(desc) = find_text(requested) else {
        return Vec::new();
    };
    let mut order = vec![desc.name];
    for d in TEXT_REGISTRY {
        if d.name != desc.name && d.capability == desc.capability {
            order.push(d.name);
        }
    }
    order
}

/// Same idea for image engines.
pub fn fallback_order_images(requested: &str) -> Vec<&'static str> {
    let Some(desc) = find_image(requested) else {
        return Vec::new();
    };
    let mut order = vec![desc.name];
    for d in IMAGE_REGISTRY {
        if d.name != desc.name {
            order.push(d.name);
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
    fn general_web_engines_fall_back_to_each_other() {
        let order = fallback_order("bing");
        assert_eq!(order, vec!["bing", "duckduckgo"]);
        let order = fallback_order("duckduckgo");
        assert_eq!(order, vec!["duckduckgo", "bing"]);
    }

    #[test]
    fn verticals_have_no_fallback_siblings() {
        for name in [
            "wikipedia",
            "hackernews",
            "reddit",
            "stackexchange",
            "openalex",
            "crossref",
            "pubmed",
            "crates",
            "npm",
            "pypi",
            "nominatim",
        ] {
            let order = fallback_order(name);
            assert_eq!(
                order,
                vec![canonical_name(name).unwrap()],
                "{name} must not silently fall back to a different source"
            );
        }
    }

    #[test]
    fn unknown_engine_yields_empty_fallback_order() {
        assert!(fallback_order("searxng").is_empty());
    }

    #[test]
    fn image_engines_fall_back_to_each_other() {
        assert_eq!(fallback_order_images("bing"), vec!["bing", "duckduckgo"]);
        assert_eq!(
            fallback_order_images("duckduckgo"),
            vec!["duckduckgo", "bing"]
        );
    }

    #[test]
    fn aliases_canonicalize() {
        assert_eq!(canonical_name("hn"), Some("hackernews"));
        assert_eq!(canonical_name("HN"), Some("hackernews"));
        assert_eq!(canonical_name("stackoverflow"), Some("stackexchange"));
        assert_eq!(canonical_name("crates.io"), Some("crates"));
        assert_eq!(canonical_name("osm"), Some("nominatim"));
        assert_eq!(canonical_name("nope"), None);
        assert_eq!(canonical_image_name("BING"), Some("bing"));
    }

    #[test]
    fn engine_by_name_resolves_aliases_case_insensitively() {
        assert_eq!(engine_by_name("HN").unwrap().name(), "hackernews");
        assert!(engine_by_name("nonexistent").is_err());
    }

    #[test]
    fn unknown_engine_is_a_usage_error() {
        // `.err()` (not `.unwrap_err()`): the Ok type is `Box<dyn
        // SearchEngine>`, which has no reason to implement `Debug`.
        let err = engine_by_name("nope").err().unwrap();
        assert_eq!(err.exit_code(), 2);
        let err = image_engine_by_name("nope").err().unwrap();
        assert_eq!(err.exit_code(), 2);
    }

    #[test]
    fn catalog_uses_real_passable_names_for_image_engines() {
        let cat = catalog();
        let image_entries: Vec<_> = cat.iter().filter(|e| e.kind == "images").collect();
        assert_eq!(image_entries.len(), 2);
        for e in image_entries {
            // Must be a name `image_engine_by_name` actually accepts —
            // the old catalog listed "bing (images)", which it did not.
            assert!(image_engine_by_name(e.name).is_ok(), "{}", e.name);
        }
    }

    #[test]
    fn every_registry_entry_is_constructible_and_named_consistently() {
        for d in TEXT_REGISTRY {
            let engine = (d.factory)();
            assert_eq!(engine.name(), d.name);
        }
        for d in IMAGE_REGISTRY {
            let engine = (d.factory)();
            assert_eq!(engine.name(), d.name);
        }
    }
}
