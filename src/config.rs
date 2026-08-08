//! Persistent configuration (`config.toml`).
//!
//! Resolution order (identical for reading *and* for `webseek init`'s write
//! target — they used to differ, so `WEBSEEK_CONFIG=... webseek init` wrote
//! to the platform default path instead of the intended one):
//! 1. `WEBSEEK_CONFIG` environment variable (path to a TOML file)
//! 2. `--config <path>` CLI flag
//! 3. Platform config dir: `~/.config/webseek/config.toml` (Linux/macOS),
//!    `%APPDATA%\webseek\config.toml` (Windows)
//!
//! An explicitly given source (env var or flag) must exist and be a regular
//! file — silently falling back to defaults when someone mistyped a path
//! hides the mistake. Unknown keys in the TOML file are also rejected
//! (`deny_unknown_fields`): a typo like `max_result` used to parse fine and
//! quietly keep the default value instead of failing loudly.
//!
//! Run `webseek init` to write a documented default file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::engines::images::DEFAULT_MAX_IMAGE_BYTES;
use crate::error::{Error, Result};

pub const DEFAULT_USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Default cap on total cache size (separate from the entry-count cap: 1000
/// cached full page bodies can be much larger than 1000 small search-result
/// lists).
pub const DEFAULT_CACHE_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// TOML-mirror of `Config`. Kept separate so serialization stays TOML-shaped
/// and so new fields can be added with `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TomlConfig {
    /// Default text search engine: run `webseek engines` for the full list.
    pub engine: String,
    /// Default image engine: "bing" | "duckduckgo".
    pub image_engine: String,
    /// Pause between requests to the *same host* (ms). Be gentle to avoid
    /// 429s; this is a floor, not a fixed sleep — concurrent requests to
    /// different hosts are not delayed by it.
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
    /// Max total cache size in bytes, regardless of entry count.
    pub cache_max_bytes: u64,
    /// Fall back to another engine when the primary is rate-limited/fails.
    /// Only ever tries another engine with the *same* capability — see
    /// `engines::Capability` — so this never silently swaps, say, PubMed
    /// results for DuckDuckGo's.
    pub fallback: bool,
    /// Honor robots.txt before fetching pages.
    pub respect_robots: bool,
    /// Skip downloaded images larger than this many bytes.
    pub image_max_bytes: usize,
    /// Overwrite an existing file when downloading an image with the same
    /// generated name. Off by default: a silent overwrite can destroy data
    /// the caller didn't know was there.
    pub image_overwrite: bool,
    /// Allow connections to loopback/private/link-local/reserved addresses
    /// (including a redirect that lands on one). Off by default: an agent
    /// following a URL from search results or page content should not be
    /// able to reach your LAN or a cloud metadata endpoint just because a
    /// page told it to.
    pub allow_private_network: bool,
    /// Use the system HTTP(S) proxy if one is configured. When a proxy is in
    /// use, *it* resolves the destination host, so webseek's own DNS-based
    /// egress check cannot see (and therefore cannot block) the real target
    /// IP for that hop — only the initial URL/scheme checks still apply. Set
    /// this to `false` for the strongest guarantee if you don't need a proxy.
    pub allow_proxy: bool,
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
            cache_max_bytes: DEFAULT_CACHE_MAX_BYTES,
            fallback: true,
            respect_robots: false,
            image_max_bytes: DEFAULT_MAX_IMAGE_BYTES,
            image_overwrite: false,
            allow_private_network: false,
            allow_proxy: true,
        }
    }
}

/// Runtime configuration, loaded and validated.
#[derive(Debug, Clone)]
pub struct Config {
    pub engine: String,
    pub image_engine: String,
    pub delay: Duration,
    pub timeout: Duration,
    pub user_agent: String,
    pub safe_search: bool,
    pub lang: Option<String>,
    pub region: Option<String>,
    pub max_chars: usize,
    pub max_results: usize,
    pub cache_ttl_secs: u64,
    pub cache_max_entries: usize,
    pub cache_max_bytes: u64,
    pub fallback: bool,
    pub respect_robots: bool,
    pub image_max_bytes: usize,
    pub image_overwrite: bool,
    pub allow_private_network: bool,
    pub allow_proxy: bool,
    /// Path the config was loaded from (if any).
    pub source: Option<PathBuf>,
}

impl Config {
    pub fn load(cli_path: Option<&Path>) -> Result<Config> {
        let path = resolve_source(cli_path)?;
        let (toml, source) = match &path {
            Some(p) => {
                let raw = std::fs::read_to_string(p)
                    .map_err(|e| Error::Config(format!("cannot read {}: {e}", p.display())))?;
                let toml: TomlConfig = toml::from_str(&raw)
                    .map_err(|e| Error::Config(format!("invalid {}: {e}", p.display())))?;
                (toml, Some(p.clone()))
            }
            None => (TomlConfig::default(), None),
        };
        Ok(Config::from_toml(toml, source))
    }

    fn from_toml(t: TomlConfig, source: Option<PathBuf>) -> Config {
        Config {
            delay: Duration::from_millis(t.delay_ms),
            timeout: Duration::from_secs(t.timeout_secs.max(1)),
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
            cache_max_bytes: t.cache_max_bytes,
            fallback: t.fallback,
            respect_robots: t.respect_robots,
            image_max_bytes: t.image_max_bytes.max(1024),
            image_overwrite: t.image_overwrite,
            allow_private_network: t.allow_private_network,
            allow_proxy: t.allow_proxy,
            source,
        }
    }

    /// Write a documented default config. Refuses to overwrite. Honors the
    /// same `WEBSEEK_CONFIG` / `--config` resolution as [`Config::load`].
    pub fn write_default(cli_path: Option<&Path>) -> Result<PathBuf> {
        let path = target_path(cli_path);
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

    /// The path that would be read or written for this `--config`/env
    /// combination, whether or not it currently exists. Used by
    /// `webseek config path` so users don't have to guess what
    /// `ProjectDirs` resolved to on their platform.
    pub fn effective_path(cli_path: Option<&Path>) -> PathBuf {
        target_path(cli_path)
    }
}

fn default_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "webseek", "webseek")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("webseek-config.toml"))
}

/// Where a config file should be written, or read from if `--config`/the env
/// var wasn't given (regardless of whether anything exists there yet).
fn target_path(cli_path: Option<&Path>) -> PathBuf {
    if let Ok(env_path) = std::env::var("WEBSEEK_CONFIG") {
        if !env_path.is_empty() {
            return PathBuf::from(env_path);
        }
    }
    if let Some(p) = cli_path {
        return p.to_path_buf();
    }
    default_path()
}

/// Where to *read* from: `Some(path)` only when that path is confirmed to
/// exist and be a regular file. An explicitly given source (env var or CLI
/// flag) that doesn't check out is a hard error rather than a silent
/// fallback to defaults, which would hide a typo'd path.
fn resolve_source(cli_path: Option<&Path>) -> Result<Option<PathBuf>> {
    if let Ok(env_path) = std::env::var("WEBSEEK_CONFIG") {
        if !env_path.is_empty() {
            let p = PathBuf::from(env_path);
            validate_explicit(&p, "WEBSEEK_CONFIG")?;
            return Ok(Some(p));
        }
    }
    if let Some(p) = cli_path {
        validate_explicit(p, "--config")?;
        return Ok(Some(p.to_path_buf()));
    }
    let p = default_path();
    Ok(if p.is_file() { Some(p) } else { None })
}

fn validate_explicit(p: &Path, source: &str) -> Result<()> {
    if !p.exists() {
        return Err(Error::Config(format!(
            "{source} points to a file that does not exist: {} (run `webseek init --config {}`)",
            p.display(),
            p.display()
        )));
    }
    if !p.is_file() {
        return Err(Error::Config(format!(
            "{source} does not point to a regular file: {}",
            p.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // WEBSEEK_CONFIG is process-global state; serialize tests that touch it.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    fn with_env_config<T>(value: Option<&str>, f: impl FnOnce() -> T) -> T {
        let _guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var("WEBSEEK_CONFIG").ok();
        match value {
            Some(v) => std::env::set_var("WEBSEEK_CONFIG", v),
            None => std::env::remove_var("WEBSEEK_CONFIG"),
        }
        let result = f();
        match prev {
            Some(v) => std::env::set_var("WEBSEEK_CONFIG", v),
            None => std::env::remove_var("WEBSEEK_CONFIG"),
        }
        result
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let err = toml::from_str::<TomlConfig>("max_result = 20\n").unwrap_err();
        assert!(err.to_string().to_lowercase().contains("unknown"));
    }

    #[test]
    fn known_keys_still_parse_with_partial_overrides() {
        let cfg: TomlConfig = toml::from_str("max_results = 17\n").unwrap();
        assert_eq!(cfg.max_results, 17);
        assert_eq!(cfg.max_chars, 20_000); // default preserved
    }

    #[test]
    fn nonexistent_env_config_is_an_error_not_a_silent_default() {
        with_env_config(Some("/nonexistent/webseek-config-test.toml"), || {
            let err = Config::load(None).unwrap_err();
            assert!(err.to_string().contains("WEBSEEK_CONFIG"));
        });
    }

    #[test]
    fn nonexistent_cli_config_is_an_error() {
        with_env_config(None, || {
            let err =
                Config::load(Some(Path::new("/nonexistent/webseek-config-test.toml"))).unwrap_err();
            assert!(err.to_string().contains("--config"));
        });
    }

    #[test]
    fn directory_as_config_path_is_an_error() {
        with_env_config(None, || {
            let dir = std::env::temp_dir();
            let err = Config::load(Some(&dir)).unwrap_err();
            assert!(err.to_string().contains("regular file"));
        });
    }

    #[test]
    fn init_and_load_agree_on_the_env_var_target() {
        // A single (non-nested) `with_env_config` call: `ENV_LOCK` is a plain
        // `std::sync::Mutex`, which is not reentrant, so nesting two calls on
        // the same thread would deadlock the second `.lock()` against itself.
        let dir = std::env::temp_dir().join(format!(
            "webseek-config-test-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        let target = dir.join("config.toml");
        with_env_config(Some(target.to_str().unwrap()), || {
            let written = Config::write_default(None).unwrap();
            assert_eq!(written, target);
            let loaded = Config::load(None).unwrap();
            assert_eq!(loaded.source.as_deref(), Some(target.as_path()));
        });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn effective_path_matches_target_used_by_init() {
        with_env_config(Some("/tmp/webseek-effective-path-test.toml"), || {
            assert_eq!(
                Config::effective_path(None),
                PathBuf::from("/tmp/webseek-effective-path-test.toml")
            );
        });
    }

    #[test]
    fn defaults_are_safe() {
        let cfg = TomlConfig::default();
        assert!(!cfg.allow_private_network);
        assert!(!cfg.image_overwrite);
        assert!(!cfg.respect_robots);
        assert_eq!(cfg.cache_max_bytes, DEFAULT_CACHE_MAX_BYTES);
    }
}
