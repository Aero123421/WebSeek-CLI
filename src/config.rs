//! Persistent configuration (`config.toml`).
//!
//! Resolution order:
//! 1. `WEBSEEK_CONFIG` environment variable (path to a TOML file)
//! 2. `--config <path>` CLI flag
//! 3. Platform config dir: `~/.config/webseek/config.toml` (Linux/macOS),
//!    `%APPDATA%\webseek\config.toml` (Windows)
//!
//! Run `webseek init` to write a documented default file.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36 webseek/0.1";

/// TOML-mirror of `Config`. Kept separate so serialization stays TOML-shaped
/// and so new fields can be added with `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct TomlConfig {
    /// Default text search engine: "duckduckgo" | "bing".
    pub engine: String,
    /// Default image engine: "bing" | "duckduckgo".
    pub image_engine: String,
    /// Pause between upstream requests (ms). Be gentle to avoid 429s.
    pub delay_ms: u64,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    pub user_agent: String,
    pub safe_search: bool,
    pub lang: Option<String>,
    pub region: Option<String>,
    /// Default character cap for `fetch` text output.
    pub max_chars: usize,
    /// Default result count.
    pub max_results: usize,
    /// Response cache TTL in seconds (0 disables expiry).
    pub cache_ttl_secs: u64,
    /// Max cached responses (0 disables the cache entirely).
    pub cache_max_entries: usize,
    /// Fall back to another engine when the primary is rate-limited/fails.
    pub fallback: bool,
    /// Honor robots.txt before fetching pages.
    pub respect_robots: bool,
    /// Skip downloaded images larger than this many bytes.
    pub image_max_bytes: usize,
}

impl Default for TomlConfig {
    fn default() -> Self {
        Self {
            engine: "duckduckgo".into(),
            image_engine: "bing".into(),
            delay_ms: 300,
            timeout_secs: 15,
            user_agent: DEFAULT_USER_AGENT.into(),
            safe_search: false,
            lang: None,
            region: None,
            max_chars: 20_000,
            max_results: 5,
            cache_ttl_secs: 3600,
            cache_max_entries: 1000,
            fallback: true,
            respect_robots: false,
            image_max_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Runtime configuration, loaded and validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub engine: String,
    pub image_engine: String,
    pub delay: std::time::Duration,
    pub timeout: std::time::Duration,
    pub user_agent: String,
    pub safe_search: bool,
    pub lang: Option<String>,
    pub region: Option<String>,
    pub max_chars: usize,
    pub max_results: usize,
    pub cache_ttl_secs: u64,
    pub cache_max_entries: usize,
    pub fallback: bool,
    pub respect_robots: bool,
    pub image_max_bytes: usize,
    /// Path the config was loaded from (if any).
    pub source: Option<PathBuf>,
}

impl Config {
    pub fn load(cli_path: Option<&Path>) -> Result<Config> {
        let path = resolve_path(cli_path)?;
        let (toml, source) = match &path {
            Some(p) if p.is_file() => {
                let raw = std::fs::read_to_string(p)
                    .map_err(|e| Error::Config(format!("cannot read {}: {e}", p.display())))?;
                let toml: TomlConfig = toml::from_str(&raw)
                    .map_err(|e| Error::Config(format!("invalid {}: {e}", p.display())))?;
                (toml, Some(p.clone()))
            }
            Some(p) if !p.exists() && cli_path.is_some() => {
                return Err(Error::Config(format!(
                    "config file not found: {} (run `webseek init`)",
                    p.display()
                )));
            }
            _ => (TomlConfig::default(), None),
        };
        Ok(Config::from_toml(toml, source))
    }

    fn from_toml(t: TomlConfig, source: Option<PathBuf>) -> Config {
        Config {
            delay: std::time::Duration::from_millis(t.delay_ms),
            timeout: std::time::Duration::from_secs(t.timeout_secs.max(1)),
            engine: t.engine,
            image_engine: t.image_engine,
            user_agent: t.user_agent,
            safe_search: t.safe_search,
            lang: t.lang,
            region: t.region,
            max_chars: t.max_chars,
            max_results: t.max_results.clamp(1, 50),
            cache_ttl_secs: t.cache_ttl_secs,
            cache_max_entries: t.cache_max_entries,
            fallback: t.fallback,
            respect_robots: t.respect_robots,
            image_max_bytes: t.image_max_bytes.max(1024),
            source,
        }
    }

    /// Write a documented default config. Refuses to overwrite.
    pub fn write_default(path: Option<&Path>) -> Result<PathBuf> {
        let path = path.map(Path::to_path_buf).unwrap_or(default_path());
        if path.exists() {
            return Err(Error::Config(format!(
                "refusing to overwrite existing config: {}",
                path.display()
            )));
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| Error::Config(format!("cannot create {}: {e}", parent.display())))?;
        }
        let doc = format!(
            "# webseek configuration\n\
             # Generated by `webseek init` — edit freely.\n\
             # Docs: see README section \"Configuration\"\n\n\
             {}\n",
            toml::to_string_pretty(&TomlConfig::default())
                .map_err(|e| Error::Config(e.to_string()))?
        );
        std::fs::write(&path, doc)
            .map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))?;
        Ok(path)
    }
}

fn default_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "webseek", "webseek")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("webseek-config.toml"))
}

fn resolve_path(cli_path: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Ok(env_path) = std::env::var("WEBSEEK_CONFIG") {
        return Ok(Some(PathBuf::from(env_path)));
    }
    if let Some(p) = cli_path {
        return Ok(Some(p.to_path_buf()));
    }
    let p = default_path();
    Ok(if p.exists() { Some(p) } else { None })
}

pub fn validate_engine(name: &str) -> anyhow::Result<()> {
    let known = [
        "duckduckgo",
        "bing",
        "wikipedia",
        "hackernews",
        "hn",
        "stackexchange",
        "stackoverflow",
        "openalex",
        "crossref",
        "pubmed",
        "crates",
        "crates.io",
        "npm",
        "pypi",
        "nominatim",
        "osm",
        "reddit",
    ];
    if known.contains(&name) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "unknown engine '{name}' (run `webseek engines` to list all)"
        ))
    }
}

pub fn validate_image_engine(name: &str) -> anyhow::Result<()> {
    let known = ["duckduckgo", "bing"];
    if known.contains(&name) {
        Ok(())
    } else {
        Err(anyhow::anyhow!(
            "unknown image engine '{name}' (expected one of: {})",
            known.join(", ")
        ))
    }
}

use toml;
