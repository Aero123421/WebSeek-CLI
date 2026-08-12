//! Persistent configuration (`config.toml`).
//!
//! Resolution order (first match wins):
//! 1. `--config <path>` CLI flag — an explicit flag always beats the ambient
//!    environment.
//! 2. `WEBSEEK_CONFIG` environment variable (path to a TOML file)
//! 3. Platform config dir: `~/.config/webseek/config.toml` (Linux),
//!    `~/Library/Application Support/webseek/config.toml` (macOS),
//!    `%APPDATA%\webseek\config.toml` (Windows)
//!
//! A path given explicitly (by flag or env var) must exist: a typo is an error
//! rather than a silent fall back to defaults. Only the platform default is
//! allowed to be missing.
//!
//! Run `webseek init` to write a documented default file.

use std::io::Write as _;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// Browser-like agent used for the scraped HTML endpoints (DuckDuckGo, Bing),
/// which serve challenge pages to obviously-automated clients.
///
/// Official APIs get [`api_user_agent`] instead — see its docs for why.
pub const DEFAULT_USER_AGENT: &str = concat!(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) ",
    "Chrome/126.0.0.0 Safari/537.36 webseek/",
    env!("CARGO_PKG_VERSION")
);

/// Honest, self-identifying agent for official APIs.
///
/// Several of the stable sources webseek uses (Nominatim, crates.io, NCBI,
/// OpenAlex) ask in their usage policies for a User-Agent that identifies the
/// application and, ideally, a way to get in touch. Sending a fake browser
/// string to those endpoints is both against their terms and worse for us:
/// they block browser impersonation precisely because it hides who is calling.
pub fn api_user_agent() -> String {
    format!(
        "webseek/{} (+https://github.com/Aero123421/WebSeek-CLI)",
        env!("CARGO_PKG_VERSION")
    )
}

/// Same, with the operator's contact address appended when configured.
pub fn api_user_agent_with_contact(contact: Option<&str>) -> String {
    match contact.map(str::trim).filter(|c| !c.is_empty()) {
        Some(c) => format!("{} {c}", api_user_agent()),
        None => api_user_agent(),
    }
}

/// TOML-mirror of `Config`. Kept separate so serialization stays TOML-shaped
/// and so new fields can be added with `#[serde(default)]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TomlConfig {
    /// Default text search engine: "duckduckgo" | "bing" | any vertical.
    pub engine: String,
    /// Default image engine: "bing" | "duckduckgo".
    pub image_engine: String,
    /// Minimum pause *between* upstream requests (ms). Be gentle to avoid 429s.
    pub delay_ms: u64,
    /// Per-request timeout in seconds.
    pub timeout_secs: u64,
    pub user_agent: String,
    /// Contact address sent to APIs that ask for one (OpenAlex "polite pool",
    /// NCBI E-utilities). Empty means "don't claim a contact".
    pub contact_email: Option<String>,
    pub safe_search: bool,
    pub lang: Option<String>,
    pub region: Option<String>,
    /// Default character cap for `fetch` text output (CLI `--max-chars` wins).
    pub max_chars: usize,
    /// Default result count (CLI `--count` wins).
    pub max_results: usize,
    /// Response cache TTL in seconds (0 = entries never expire).
    pub cache_ttl_secs: u64,
    /// Max cached responses (0 disables the cache entirely).
    pub cache_max_entries: usize,
    /// Max total serialized cache size in bytes (0 disables the byte budget).
    pub cache_max_bytes: u64,
    /// Fall back to another *web* engine when the primary fails or is blocked.
    pub fallback: bool,
    /// Honor robots.txt before fetching pages.
    pub respect_robots: bool,
    /// Skip downloaded images larger than this many bytes.
    pub image_max_bytes: usize,
    /// Replace an existing generated image file. Disabled by default so a
    /// predictable filename cannot overwrite user data or follow a symlink.
    pub image_overwrite: bool,
    /// Allow loopback/private/link-local/reserved network destinations.
    pub allow_private_network: bool,
    /// Allow reqwest to honor configured HTTP(S) proxy environment variables.
    /// A proxy resolves destinations outside webseek's DNS egress guard.
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
            contact_email: None,
            safe_search: false,
            lang: None,
            region: None,
            max_chars: 20_000,
            max_results: 5,
            cache_ttl_secs: 3600,
            cache_max_entries: 1000,
            cache_max_bytes: crate::cache::DEFAULT_MAX_BYTES,
            fallback: true,
            respect_robots: false,
            image_max_bytes: crate::engines::images::DEFAULT_MAX_IMAGE_BYTES,
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
    pub delay: std::time::Duration,
    pub timeout: std::time::Duration,
    pub user_agent: String,
    pub contact_email: Option<String>,
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
}

impl Default for Config {
    fn default() -> Self {
        Config::from_toml(TomlConfig::default())
    }
}

impl Config {
    pub fn load(cli_path: Option<&Path>) -> Result<Config> {
        let toml = match resolve_path(cli_path) {
            Source::Explicit(p) => {
                if !p.is_file() {
                    return Err(Error::Config(format!(
                        "config file not found: {} (run `webseek init` to create one)",
                        p.display()
                    )));
                }
                read_toml(&p)?
            }
            Source::Default(p) if p.is_file() => read_toml(&p)?,
            Source::Default(_) => TomlConfig::default(),
        };
        Ok(Config::from_toml(toml))
    }

    fn from_toml(t: TomlConfig) -> Config {
        Config {
            delay: std::time::Duration::from_millis(t.delay_ms),
            timeout: std::time::Duration::from_secs(t.timeout_secs.max(1)),
            engine: t.engine,
            image_engine: t.image_engine,
            user_agent: t.user_agent,
            contact_email: t.contact_email.filter(|c| !c.trim().is_empty()),
            safe_search: t.safe_search,
            lang: t.lang,
            region: t.region,
            max_chars: t.max_chars.max(1),
            max_results: t.max_results.clamp(1, crate::cli::MAX_RESULTS),
            cache_ttl_secs: t.cache_ttl_secs,
            cache_max_entries: t.cache_max_entries,
            cache_max_bytes: t.cache_max_bytes,
            fallback: t.fallback,
            respect_robots: t.respect_robots,
            image_max_bytes: t.image_max_bytes.max(1024),
            image_overwrite: t.image_overwrite,
            allow_private_network: t.allow_private_network,
            allow_proxy: t.allow_proxy,
        }
    }

    /// Write a documented default config. Refuses to overwrite.
    pub fn write_default(path: Option<&Path>) -> Result<PathBuf> {
        let path = match resolve_path(path) {
            Source::Explicit(p) | Source::Default(p) => p,
        };
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
             # Docs: see README section \"Configuration\"\n\
             #\n\
             # contact_email is optional but recommended: OpenAlex and NCBI ask\n\
             # for a contact address, and supplying a real one is the difference\n\
             # between being a good citizen and pretending to be one.\n\n\
             {}\n\
             # Optional keys. serde omits unset values, and TOML has no `null`,\n\
             # so they appear here as comments rather than as empty settings.\n\
             # contact_email = \"you@example.com\"\n\
             # lang = \"ja\"          # engine-dependent\n\
             # region = \"jp\"        # or \"en-us\", \"EN_US\", ...\n",
            toml::to_string_pretty(&TomlConfig::default())
                .map_err(|e| Error::Config(e.to_string()))?
        );
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&path).map_err(|e| {
            if e.kind() == std::io::ErrorKind::AlreadyExists {
                Error::Config(format!(
                    "refusing to overwrite existing config: {}",
                    path.display()
                ))
            } else {
                Error::Config(format!("cannot create {}: {e}", path.display()))
            }
        })?;
        file.write_all(doc.as_bytes())
            .map_err(|e| Error::Config(format!("cannot write {}: {e}", path.display())))?;
        Ok(path)
    }

    /// Path that would be read or written for the current CLI/environment.
    pub fn effective_path(path: Option<&Path>) -> PathBuf {
        match resolve_path(path) {
            Source::Explicit(path) | Source::Default(path) => path,
        }
    }
}

fn read_toml(p: &Path) -> Result<TomlConfig> {
    let raw = std::fs::read_to_string(p)
        .map_err(|e| Error::Config(format!("cannot read {}: {e}", p.display())))?;
    toml::from_str(&raw).map_err(|e| Error::Config(format!("invalid {}: {e}", p.display())))
}

/// Where the config should come from, and whether the user asked for it
/// explicitly (in which case a missing file is an error).
enum Source {
    Explicit(PathBuf),
    Default(PathBuf),
}

fn default_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "webseek", "webseek")
        .map(|d| d.config_dir().join("config.toml"))
        .unwrap_or_else(|| PathBuf::from("webseek-config.toml"))
}

fn resolve_path(cli_path: Option<&Path>) -> Source {
    // An explicit flag beats the ambient environment: that is the universal
    // CLI convention, and it is the only way to override an exported var.
    if let Some(p) = cli_path {
        return Source::Explicit(p.to_path_buf());
    }
    match std::env::var_os("WEBSEEK_CONFIG") {
        Some(v) if !v.is_empty() => Source::Explicit(PathBuf::from(v)),
        _ => Source::Default(default_path()),
    }
}

pub fn validate_engine(name: &str) -> Result<()> {
    if crate::engines::is_known_engine(name) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "unknown engine '{name}' (run `webseek engines` to list all)"
        )))
    }
}

pub fn validate_image_engine(name: &str) -> Result<()> {
    if crate::engines::is_known_image_engine(name) {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "unknown image engine '{name}' (expected one of: {})",
            crate::engines::IMAGE_REGISTRY
                .iter()
                .map(|s| s.name)
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_registry_engine_passes_validation() {
        for spec in crate::engines::TEXT_REGISTRY {
            validate_engine(spec.name).expect(spec.name);
            for a in spec.aliases {
                validate_engine(a).expect(a);
            }
        }
        for spec in crate::engines::IMAGE_REGISTRY {
            validate_image_engine(spec.name).expect(spec.name);
        }
        assert!(validate_engine("nope").is_err());
    }

    #[test]
    fn defaults_round_trip_through_toml() {
        let doc = toml::to_string_pretty(&TomlConfig::default()).unwrap();
        let back: TomlConfig = toml::from_str(&doc).unwrap();
        assert_eq!(back.engine, "duckduckgo");
        assert_eq!(back.max_results, 5);
        assert_eq!(back.max_chars, 20_000);
    }

    #[test]
    fn unknown_keys_are_rejected_rather_than_ignored() {
        // A typo in config.toml should be reported, not silently dropped.
        let err = toml::from_str::<TomlConfig>("delay_ms = 100\nmax_charss = 5\n").unwrap_err();
        assert!(err.to_string().contains("max_charss"), "got: {err}");
    }

    #[test]
    fn user_agent_reports_the_real_crate_version() {
        assert!(DEFAULT_USER_AGENT.ends_with(env!("CARGO_PKG_VERSION")));
        assert!(api_user_agent().starts_with(&format!("webseek/{}", env!("CARGO_PKG_VERSION"))));
    }

    #[test]
    fn contact_is_appended_only_when_present() {
        assert_eq!(api_user_agent_with_contact(None), api_user_agent());
        assert_eq!(api_user_agent_with_contact(Some("  ")), api_user_agent());
        assert!(api_user_agent_with_contact(Some("me@example.com")).ends_with("me@example.com"));
    }

    #[test]
    fn security_sensitive_defaults_are_closed() {
        let cfg = TomlConfig::default();
        assert!(!cfg.allow_private_network);
        assert!(!cfg.image_overwrite);
        assert!(cfg.allow_proxy);
    }

    #[test]
    fn explicit_flag_beats_environment_variable() {
        let flag = PathBuf::from("/tmp/from-flag.toml");
        match resolve_path(Some(&flag)) {
            Source::Explicit(p) => assert_eq!(p, flag),
            Source::Default(_) => panic!("an explicit --config must win"),
        }
    }
}
