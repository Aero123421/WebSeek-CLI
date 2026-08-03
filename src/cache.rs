//! Persistent response cache (best-effort, never fatal).
//!
//! Design goals:
//! - **One file, zero extra dependencies** (`serde_json` document).
//! - **LRU-ish eviction**: capped entry count, oldest evicted first.
//! - **TTL**: entries expire after `cache_ttl_secs`.
//! - **Corrupt-safe**: unreadable/corrupt cache files are discarded silently
//!   (a warning goes to stderr once); writes are atomic (tmp + rename).
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
    /// Load from `path`, or silently start empty.
    pub fn load(path: PathBuf, ttl_secs: u64, max_entries: usize) -> Self {
        let entries = match std::fs::read_to_string(&path) {
            Ok(raw) => match serde_json::from_str::<HashMap<String, CachedValue>>(&raw) {
                Ok(map) => map,
                Err(_) => {
                    eprintln!("[webseek] ignoring corrupt cache file {}", path.display());
                    HashMap::new()
                }
            },
            Err(_) => HashMap::new(),
        };
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
            dirty: false,
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
        if self.expired(entry.ts) {
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
        if self.entries.len() > self.max_entries {
            let oldest_key = self
                .entries
                .iter()
                .min_by_key(|(_, v)| v.seq)
                .map(|(k, _)| k.clone());
            if let Some(k) = oldest_key {
                self.entries.remove(&k);
            }
        }
        self.dirty = true;
    }

    /// Persist to disk atomically if anything changed. Never fatal.
    pub fn save(&self) {
        if !self.is_enabled() || !self.dirty {
            return;
        }
        if let Some(parent) = self.path.parent() {
            if std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let tmp = self.path.with_extension("json.tmp");
        let result = serde_json::to_vec(&self.entries)
            .map_err(|e| e.to_string())
            .and_then(|bytes| {
                std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
                std::fs::rename(&tmp, &self.path).map_err(|e| e.to_string())
            });
        if let Err(e) = result {
            eprintln!("[webseek] could not write cache: {e}");
        }
    }

    fn expired(&self, ts: u64) -> bool {
        self.ttl_secs == 0 || now_secs().saturating_sub(ts) >= self.ttl_secs
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Canonical cache key: SHA-256 of the joined intent parts.
pub fn cache_key(parts: &[&str]) -> String {
    let joined = parts.join("\u{1f}"); // unit separator, cannot appear in parts
    hex(&Sha256::digest(joined.as_bytes()))
}

fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
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

    fn temp_cache() -> (Cache, PathBuf) {
        let dir = std::env::temp_dir().join(format!("webseek-cache-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        (Cache::load(dir.join("cache.json"), 3600, 3), dir)
    }

    #[test]
    fn put_get_roundtrip() {
        let (cache, _dir) = temp_cache();
        let key = cache_key(&["search", "rust"]);
        assert!(cache.get(&key).is_none());
        let mut c = cache;
        c.put(key.clone(), serde_json::json!([1, 2, 3]));
        assert_eq!(c.get(&key), Some(serde_json::json!([1, 2, 3])));
        // Same intent, different query -> different key.
        let other = cache_key(&["search", "tokio"]);
        assert_ne!(key, other);
        assert!(c.get(&other).is_none());
    }

    #[test]
    fn evicts_oldest_when_full() {
        let (mut cache, _dir) = temp_cache(); // max_entries = 3
        for i in 0..4 {
            cache.put(cache_key(&["q", &i.to_string()]), serde_json::json!(i));
        }
        assert_eq!(cache.entries.len(), 3);
        // "q|0" was inserted first and must be gone.
        assert!(cache.get(&cache_key(&["q", "0"])).is_none());
        assert_eq!(
            cache.get(&cache_key(&["q", "3"])),
            Some(serde_json::json!(3))
        );
    }

    #[test]
    fn ttl_expiry() {
        let dir = std::env::temp_dir().join(format!("webseek-cache-ttl-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut cache = Cache::load(dir.join("cache.json"), 1, 10);
        let key = cache_key(&["x"]);
        cache.put(key.clone(), serde_json::json!("v"));
        assert_eq!(cache.get(&key), Some(serde_json::json!("v")));
        std::thread::sleep(std::time::Duration::from_millis(1100));
        assert!(cache.get(&key).is_none());
    }

    #[test]
    fn corrupt_file_is_discarded() {
        let dir =
            std::env::temp_dir().join(format!("webseek-cache-corrupt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("cache.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let cache = Cache::load(path, 3600, 10);
        assert_eq!(cache.entries.len(), 0);
    }

    #[test]
    fn keys_differ_by_options() {
        let a = cache_key(&["search", "duckduckgo", "5", "rust"]);
        let b = cache_key(&["search", "duckduckgo", "10", "rust"]);
        let c = cache_key(&["search", "bing", "5", "rust"]);
        assert_ne!(a, b);
        assert_ne!(a, c);
    }
}
