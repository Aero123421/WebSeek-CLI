//! Persistent response cache (best-effort, never fatal).
//!
//! Design goals:
//! - **One file, zero extra dependencies** (`serde_json` document).
//! - **FIFO eviction**: capped entry count, oldest insertion evicted first.
//! - **TTL**: entries expire after `cache_ttl_secs`; `0` means *never expire*.
//! - **Corrupt-safe**: unreadable/corrupt cache files are discarded (a warning
//!   goes to stderr); writes go to a process-unique temp file and are renamed
//!   into place, so a concurrent writer cannot produce a torn file.
//! - **Keyed by intent**: every option that changes the answer is part of the
//!   key, so cache hits are always semantically correct.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedValue {
    ts: u64,
    /// Monotonic insertion counter for deterministic FIFO eviction.
    #[serde(default)]
    seq: u64,
    value: Value,
}

pub struct Cache {
    path: PathBuf,
    ttl_secs: u64,
    max_entries: usize,
    entries: HashMap<String, CachedValue>,
    /// Next value for `CachedValue::seq`.
    next_seq: u64,
    /// Whether anything changed since load; avoids pointless rewrites.
    dirty: bool,
}

impl Cache {
    /// Load from `path`, or silently start empty. Expired entries are dropped
    /// on load so a long-lived cache file does not fill up with dead weight.
    pub fn load(path: PathBuf, ttl_secs: u64, max_entries: usize) -> Self {
        let mut entries = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, CachedValue>>(&raw) {
                Ok(map) => map,
                Err(_) => {
                    crate::output::warn(&format!("ignoring corrupt cache file {}", path.display()));
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };

        let before = entries.len();
        let now = now_secs();
        entries.retain(|_, v| !is_expired(v.ts, ttl_secs, now));
        let dirty = entries.len() != before;

        let next_seq = entries
            .values()
            .map(|v| v.seq)
            .max()
            .map(|m| m + 1)
            .unwrap_or(0);
        Self {
            path,
            ttl_secs,
            max_entries,
            entries,
            next_seq,
            dirty,
        }
    }

    /// Disabled cache (used with `--no-cache`).
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            ttl_secs: 0,
            max_entries: 0,
            entries: HashMap::new(),
            next_seq: 0,
            dirty: false,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.max_entries > 0
    }

    /// Look up a key; returns `None` on miss or TTL expiry.
    pub fn get(&self, key: &str) -> Option<Value> {
        if !self.is_enabled() {
            return None;
        }
        let entry = self.entries.get(key)?;
        if is_expired(entry.ts, self.ttl_secs, now_secs()) {
            return None;
        }
        Some(entry.value.clone())
    }

    /// Insert a value, evicting the oldest entry when at capacity.
    pub fn put(&mut self, key: String, value: Value) {
        if !self.is_enabled() {
            return;
        }
        let seq = self.next_seq;
        self.next_seq += 1;
        self.entries.insert(
            key,
            CachedValue {
                ts: now_secs(),
                seq,
                value,
            },
        );
        while self.entries.len() > self.max_entries {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.seq)
                .map(|(k, _)| k.clone());
            match oldest_key {
                Some(k) => {
                    self.entries.remove(&k);
                }
                None => break,
            }
        }
        self.dirty = true;
    }

    /// Persist to disk atomically if anything changed. Never fatal.
    ///
    /// The temp file name carries the process id: two webseek runs finishing at
    /// the same time would otherwise write the same `cache.json.tmp` and rename
    /// a half-written file into place.
    pub fn save(&self) {
        if !self.is_enabled() || !self.dirty {
            return;
        }
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        // Unique per process *and* per thread: two savers sharing a temp name
        // is exactly how a half-written file gets renamed into place.
        let tmp = self.path.with_extension(format!(
            "json.{}.{:?}.tmp",
            std::process::id(),
            std::thread::current().id()
        ));
        let result = serde_json::to_vec(&self.entries)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            crate::output::warn(&format!("could not write cache: {e}"));
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.len()
    }
}

/// A TTL of `0` means "never expire", as documented in `config.toml`.
fn is_expired(ts: u64, ttl_secs: u64, now: u64) -> bool {
    ttl_secs != 0 && now.saturating_sub(ts) >= ttl_secs
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonical cache key: SHA-256 of the joined intent parts.
pub fn cache_key(parts: &[&str]) -> String {
    let joined = parts.join("\u{1f}"); // unit separator
    hex(&Sha256::digest(joined.as_bytes()))
}

/// Key for a page fetch. Shared by the single-URL and batch paths so the two
/// cannot drift apart and miss each other's entries.
pub fn fetch_key(url: &str, opts: &crate::models::FetchOpts) -> String {
    cache_key(&[
        "fetch",
        url,
        &opts.max_chars.to_string(),
        &opts.raw_html.to_string(),
        &opts.markdown.to_string(),
    ])
}

/// Key for a text search.
///
/// Keyed by what the caller *asked for*, never by whichever engine ended up
/// answering: keying on the responder means the next identical command looks
/// up a key nothing was ever stored under, and the cache never hits.
pub fn search_key(requested_engine: &str, query: &str, opts: &crate::models::SearchOpts) -> String {
    cache_key(&[
        "search",
        requested_engine,
        query,
        &opts.count.to_string(),
        opts.lang.as_deref().unwrap_or(""),
        opts.region.as_deref().unwrap_or(""),
        &opts.safe.to_string(),
    ])
}

/// Key for an image search.
pub fn images_key(requested_engine: &str, query: &str, count: usize, safe: bool) -> String {
    cache_key(&[
        "images",
        requested_engine,
        query,
        &count.to_string(),
        &safe.to_string(),
    ])
}

fn hex(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Default cache location: platform cache dir + `webseek/cache.json`.
pub fn default_cache_path() -> PathBuf {
    directories::ProjectDirs::from("dev", "webseek", "webseek")
        .map(|d| d.cache_dir().join("cache.json"))
        .unwrap_or_else(|| PathBuf::from("webseek-cache.json"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "webseek-cache-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = temp_dir("roundtrip");
        let mut c = Cache::load(dir.join("cache.json"), 3600, 3);
        let key = cache_key(&["search", "rust"]);
        assert!(c.get(&key).is_none());
        c.put(key.clone(), serde_json::json!([1, 2, 3]));
        assert_eq!(c.get(&key), Some(serde_json::json!([1, 2, 3])));
        let other = cache_key(&["search", "tokio"]);
        assert_ne!(key, other);
        assert!(c.get(&other).is_none());
    }

    #[test]
    fn evicts_oldest_when_full() {
        let dir = temp_dir("evict");
        let mut cache = Cache::load(dir.join("cache.json"), 3600, 3);
        for i in 0..4 {
            cache.put(cache_key(&["q", &i.to_string()]), serde_json::json!(i));
        }
        assert_eq!(cache.len(), 3);
        assert!(cache.get(&cache_key(&["q", "0"])).is_none());
        assert_eq!(
            cache.get(&cache_key(&["q", "3"])),
            Some(serde_json::json!(3))
        );
    }

    #[test]
    fn ttl_expiry() {
        // Timestamps have whole-second resolution, so a 1-second TTL can
        // expire an entry written microseconds before a second boundary. Use
        // 2s for the "still fresh" assertion and sleep past the full window.
        let dir = temp_dir("ttl");
        let mut cache = Cache::load(dir.join("cache.json"), 2, 10);
        let key = cache_key(&["x"]);
        cache.put(key.clone(), serde_json::json!("v"));
        assert_eq!(cache.get(&key), Some(serde_json::json!("v")));
        std::thread::sleep(std::time::Duration::from_millis(2600));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn the_real_key_builders_are_sensitive_to_every_option() {
        // `keys_differ_by_options` only proved SHA-256 is injective — it never
        // called these. Dropping an option here is a live correctness bug:
        // `fetch URL` then `fetch URL --html` would serve the extracted text.
        let base = crate::models::FetchOpts::default();
        let baseline = fetch_key("https://x", &base);
        for (label, opts) in [
            (
                "max_chars",
                crate::models::FetchOpts {
                    max_chars: 10,
                    ..base.clone()
                },
            ),
            (
                "raw_html",
                crate::models::FetchOpts {
                    raw_html: true,
                    ..base.clone()
                },
            ),
            (
                "markdown",
                crate::models::FetchOpts {
                    markdown: true,
                    ..base.clone()
                },
            ),
        ] {
            assert_ne!(
                baseline,
                fetch_key("https://x", &opts),
                "fetch_key ignores {label}"
            );
        }
        assert_ne!(
            baseline,
            fetch_key("https://y", &base),
            "fetch_key ignores the URL"
        );

        let s = crate::models::SearchOpts::default();
        let sbase = search_key("bing", "q", &s);
        for (label, opts) in [
            (
                "count",
                crate::models::SearchOpts {
                    count: 9,
                    ..s.clone()
                },
            ),
            (
                "lang",
                crate::models::SearchOpts {
                    lang: Some("ja".into()),
                    ..s.clone()
                },
            ),
            (
                "region",
                crate::models::SearchOpts {
                    region: Some("jp".into()),
                    ..s.clone()
                },
            ),
            (
                "safe",
                crate::models::SearchOpts {
                    safe: true,
                    ..s.clone()
                },
            ),
        ] {
            assert_ne!(
                sbase,
                search_key("bing", "q", &opts),
                "search_key ignores {label}"
            );
        }
        assert_ne!(
            sbase,
            search_key("duckduckgo", "q", &s),
            "search_key ignores the engine"
        );
        assert_ne!(
            sbase,
            search_key("bing", "other", &s),
            "search_key ignores the query"
        );

        let ibase = images_key("bing", "q", 5, false);
        assert_ne!(
            ibase,
            images_key("bing", "q", 6, false),
            "images_key ignores count"
        );
        assert_ne!(
            ibase,
            images_key("bing", "q", 5, true),
            "images_key ignores safe"
        );
        assert_ne!(
            ibase,
            images_key("ddg", "q", 5, false),
            "images_key ignores the engine"
        );
    }

    #[test]
    fn concurrent_saves_never_leave_a_torn_file() {
        // The documented reason for tmp+rename. A direct write to the final
        // path lets one saver read another's half-written JSON.
        let dir = temp_dir("torn");
        let path = dir.join("cache.json");
        let big: String = "x".repeat(40_000);

        std::thread::scope(|s| {
            for n in 0..8 {
                let path = path.clone();
                let big = big.clone();
                s.spawn(move || {
                    let mut c = Cache::load(path, 3600, 100);
                    for i in 0..20 {
                        c.put(
                            cache_key(&[&n.to_string(), &i.to_string()]),
                            serde_json::json!(big),
                        );
                    }
                    c.save();
                });
            }
        });

        let raw = std::fs::read_to_string(&path).expect("a cache file exists");
        serde_json::from_str::<HashMap<String, CachedValue>>(&raw)
            .expect("the surviving file must be complete JSON, not a torn write");
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(
            leftovers.is_empty(),
            "temp files left behind: {leftovers:?}"
        );
    }

    #[test]
    fn ttl_zero_means_never_expire() {
        // config.toml documents `cache_ttl_secs = 0` as "no expiry"; treating
        // it as "expire immediately" silently disabled the whole cache.
        assert!(!is_expired(0, 0, u64::MAX), "ttl=0 must never expire");

        let dir = temp_dir("ttl-zero");
        let mut cache = Cache::load(dir.join("cache.json"), 0, 10);
        let key = cache_key(&["x"]);
        cache.put(key.clone(), serde_json::json!("v"));
        assert_eq!(
            cache.get(&key),
            Some(serde_json::json!("v")),
            "an entry written with ttl=0 must still be readable"
        );
    }

    #[test]
    fn expired_entries_are_pruned_on_load() {
        let dir = temp_dir("prune");
        let path = dir.join("cache.json");
        {
            let mut c = Cache::load(path.clone(), 3600, 10);
            c.put(cache_key(&["fresh"]), serde_json::json!(1));
            c.save();
        }
        // Reload with a 0-second-old TTL of 1s after the entry "aged".
        let reloaded = Cache::load(path, 1, 10);
        assert_eq!(reloaded.len(), 1, "a fresh entry survives");

        std::thread::sleep(std::time::Duration::from_millis(1100));
        let path2 = dir.join("cache.json");
        let aged = Cache::load(path2, 1, 10);
        assert_eq!(aged.len(), 0, "expired entries are dropped on load");
    }

    #[test]
    fn survives_a_save_load_cycle() {
        let dir = temp_dir("persist");
        let path = dir.join("cache.json");
        {
            let mut c = Cache::load(path.clone(), 3600, 10);
            c.put(cache_key(&["k"]), serde_json::json!("v"));
            c.save();
        }
        let c = Cache::load(path, 3600, 10);
        assert_eq!(c.get(&cache_key(&["k"])), Some(serde_json::json!("v")));
    }

    #[test]
    fn save_leaves_no_temp_files_behind() {
        let dir = temp_dir("tmp-clean");
        let path = dir.join("cache.json");
        let mut c = Cache::load(path.clone(), 3600, 10);
        c.put(cache_key(&["k"]), serde_json::json!("v"));
        c.save();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "temp file was not renamed away");
    }

    #[test]
    fn corrupt_file_is_discarded() {
        let dir = temp_dir("corrupt");
        let path = dir.join("cache.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let cache = Cache::load(path, 3600, 10);
        assert_eq!(cache.len(), 0);
    }

    #[test]
    fn keys_differ_by_options() {
        let a = cache_key(&["search", "duckduckgo", "5", "rust"]);
        let b = cache_key(&["search", "duckduckgo", "10", "rust"]);
        let c = cache_key(&["search", "bing", "5", "rust"]);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn disabled_cache_stores_nothing() {
        let mut c = Cache::disabled();
        c.put(cache_key(&["k"]), serde_json::json!(1));
        assert!(c.get(&cache_key(&["k"])).is_none());
        c.save(); // must not panic or create files
    }
}
