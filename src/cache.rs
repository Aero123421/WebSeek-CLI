//! Persistent response cache (best-effort, never fatal).
//!
//! Design goals:
//! - **One file, one lock.** A sidecar `.lock` file is held while the cache is
//!   read-merged-written, so two `webseek` processes can no longer lose each
//!   other's updates, and the temp file is unique per write.
//! - **True LRU eviction**: [`Cache::get`] refreshes recency, so a hot entry
//!   survives. (It used to be FIFO while documented as LRU.)
//! - **TTL**: `cache_ttl_secs = 0` means *no expiry*, matching the docs. The
//!   old condition (`ttl == 0 || elapsed >= ttl`) expired those entries
//!   immediately — the exact opposite.
//! - **Two budgets**: entry count *and* total bytes, because 1000 cached page
//!   bodies is a lot of disk.
//! - **Corrupt-safe**: an unreadable cache file is discarded with one warning;
//!   an undeserializable *entry* is dropped and refetched, never surfaced as an
//!   error.
//! - **Keyed by intent**: every option that changes the answer is part of the
//!   key, plus a schema version so an extractor change cannot serve stale text.
//! - **Private by default**: the file is created `0600` on Unix; queries and
//!   page bodies are personal data.

use std::collections::{HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Default on-disk budget. Entry count alone is not enough when entries can
/// contain full extracted pages.
pub const DEFAULT_MAX_BYTES: u64 = 100 * 1024 * 1024;

/// Bump when a change to extraction/serialization makes old entries wrong.
pub const CACHE_SCHEMA_VERSION: u32 = 3;

/// Injectable clock (unix seconds) so TTL tests need no sleeping.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CachedValue {
    ts: u64,
    /// Monotonic recency counter, refreshed on every hit (LRU).
    #[serde(default)]
    seq: u64,
    /// Approximate serialized size, used for the byte budget.
    #[serde(default, skip_serializing_if = "is_zero")]
    bytes: u64,
    value: Value,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// Cache statistics for `webseek cache info`.
#[derive(Debug, Clone, Serialize)]
pub struct CacheInfo {
    pub path: String,
    pub enabled: bool,
    pub entries: usize,
    pub bytes: u64,
    pub expired: usize,
    pub ttl_secs: u64,
    pub max_entries: usize,
    pub max_bytes: u64,
    pub schema_version: u32,
}

pub struct Cache {
    path: PathBuf,
    ttl_secs: u64,
    max_entries: usize,
    max_bytes: u64,
    entries: HashMap<String, CachedValue>,
    /// Keys this process created, updated or deleted — the ones that must win
    /// over whatever another process wrote meanwhile.
    touched: HashSet<String>,
    next_seq: u64,
    dirty: bool,
    clock: Clock,
}

fn system_clock() -> Clock {
    Arc::new(|| {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    })
}

impl Cache {
    /// Load from `path`, or silently start empty.
    pub fn load(path: PathBuf, ttl_secs: u64, max_entries: usize, max_bytes: u64) -> Self {
        Self::load_with_clock(path, ttl_secs, max_entries, max_bytes, system_clock())
    }

    pub fn load_with_clock(
        path: PathBuf,
        ttl_secs: u64,
        max_entries: usize,
        max_bytes: u64,
        clock: Clock,
    ) -> Self {
        let mut entries = read_file(&path).unwrap_or_default();
        for v in entries.values_mut() {
            if v.bytes == 0 {
                v.bytes = estimate(&v.value);
            }
        }
        let next_seq = entries
            .values()
            .map(|v| v.seq)
            .max()
            .map(|m| m.saturating_add(1))
            .unwrap_or(0);
        let mut cache = Self {
            path,
            ttl_secs,
            max_entries,
            max_bytes,
            entries,
            touched: HashSet::new(),
            next_seq,
            dirty: false,
            clock,
        };
        // A cache that shrank its limits must shrink on startup, not on the
        // next write.
        if cache.enforce_budgets() {
            cache.dirty = true;
        }
        cache
    }

    /// Disabled cache (used with `--no-cache`).
    pub fn disabled() -> Self {
        Self {
            path: PathBuf::new(),
            ttl_secs: 0,
            max_entries: 0,
            max_bytes: 0,
            entries: HashMap::new(),
            touched: HashSet::new(),
            next_seq: 0,
            dirty: false,
            clock: system_clock(),
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.max_entries > 0
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Look up a key; returns `None` on miss or TTL expiry. Refreshes recency.
    pub fn get(&mut self, key: &str) -> Option<Value> {
        if !self.is_enabled() {
            return None;
        }
        let expired = {
            let entry = self.entries.get(key)?;
            self.expired(entry.ts)
        };
        if expired {
            // Drop it now so the file shrinks instead of accumulating garbage.
            self.remove(key);
            return None;
        }
        let seq = self.next_seq;
        let entry = self.entries.get_mut(key)?;
        // Only rewrite when recency actually moved, so repeated hits on the
        // newest entry do not force a save.
        if entry.seq.saturating_add(1) < seq {
            entry.seq = seq;
            self.next_seq = self.next_seq.saturating_add(1);
            self.dirty = true;
            self.touched.insert(key.to_string());
        }
        Some(entry.value.clone())
    }

    /// Insert a value, evicting least-recently-used entries when over budget.
    pub fn put(&mut self, key: String, value: Value) {
        if !self.is_enabled() {
            return;
        }
        let seq = self.next_seq;
        self.next_seq = self.next_seq.saturating_add(1);
        let bytes = estimate(&value);
        // A single entry larger than the whole budget is not worth caching.
        if self.max_bytes > 0 && bytes > self.max_bytes {
            return;
        }
        self.touched.insert(key.clone());
        self.entries.insert(
            key,
            CachedValue {
                ts: (self.clock)(),
                seq,
                bytes,
                value,
            },
        );
        self.enforce_budgets();
        self.dirty = true;
    }

    /// Forget a key (corrupt entry, expiry, explicit invalidation).
    pub fn remove(&mut self, key: &str) {
        if self.entries.remove(key).is_some() {
            self.touched.insert(key.to_string());
            self.dirty = true;
        }
    }

    /// Drop every entry and delete the file.
    pub fn clear(&mut self) -> std::io::Result<usize> {
        let n = Self::clear_path(&self.path)?;
        self.entries.clear();
        self.touched.clear();
        self.dirty = false;
        Ok(n)
    }

    /// Clear a cache file without first applying runtime entry/byte budgets.
    /// The sidecar lock keeps this from racing a concurrent save.
    pub fn clear_path(path: &Path) -> std::io::Result<usize> {
        let _lock = FileLock::acquire(path);
        let n = read_file(path).map(|entries| entries.len()).unwrap_or(0);
        match std::fs::remove_file(path) {
            Ok(()) => Ok(n),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(n),
            Err(e) => Err(e),
        }
    }

    pub fn info(&self) -> CacheInfo {
        let expired = self.entries.values().filter(|v| self.expired(v.ts)).count();
        CacheInfo {
            path: self.path.display().to_string(),
            enabled: self.is_enabled(),
            entries: self.entries.len(),
            bytes: self.entries.values().map(|v| v.bytes).sum(),
            expired,
            ttl_secs: self.ttl_secs,
            max_entries: self.max_entries,
            max_bytes: self.max_bytes,
            schema_version: CACHE_SCHEMA_VERSION,
        }
    }

    /// Persist to disk if anything changed, merging concurrent writers.
    /// Never fatal.
    pub fn save(&mut self) {
        if !self.is_enabled() || !self.dirty {
            return;
        }
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() && std::fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        // Hold the lock across read-merge-write so a parallel process cannot
        // clobber our additions (or we theirs).
        let _lock = FileLock::acquire(&self.path);
        if let Some(on_disk) = read_file(&self.path) {
            self.merge(on_disk);
        }
        self.enforce_budgets();
        if let Err(e) = self.write_atomic() {
            crate::output::warn(&format!("could not write cache: {e}"));
        } else {
            self.dirty = false;
            self.touched.clear();
        }
    }

    /// Keep another process's entries, but let ours win for keys we touched.
    fn merge(&mut self, mut on_disk: HashMap<String, CachedValue>) {
        let mine = std::mem::take(&mut self.entries);
        // A key we deliberately dropped (expired / corrupt) must stay dropped
        // even if the on-disk copy still has it.
        for k in &self.touched {
            if !mine.contains_key(k) {
                on_disk.remove(k);
            }
        }
        for (k, mut v) in mine {
            if v.bytes == 0 {
                v.bytes = estimate(&v.value);
            }
            if self.touched.contains(&k) {
                on_disk.insert(k, v);
            } else {
                on_disk.entry(k).or_insert(v);
            }
        }
        for v in on_disk.values_mut() {
            if v.bytes == 0 {
                v.bytes = estimate(&v.value);
            }
        }
        self.next_seq = on_disk
            .values()
            .map(|v| v.seq)
            .max()
            .map(|m| m.saturating_add(1))
            .unwrap_or(0)
            .max(self.next_seq);
        self.entries = on_disk;
    }

    fn write_atomic(&self) -> std::io::Result<()> {
        let dir = self
            .path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let bytes =
            serde_json::to_vec(&self.entries).map_err(|e| std::io::Error::other(e.to_string()))?;
        // Random name in the destination directory: same filesystem (so the
        // rename is atomic) and no fixed `cache.json.tmp` to collide on.
        let mut tmp = tempfile::Builder::new()
            .prefix(".webseek-cache-")
            .suffix(".tmp")
            .tempfile_in(&dir)?;
        restrict_permissions(tmp.as_file());
        tmp.write_all(&bytes)?;
        tmp.flush()?;
        tmp.persist(&self.path)
            .map_err(|e| std::io::Error::other(e.to_string()))?;
        Ok(())
    }

    /// Evict least-recently-used entries until both budgets are satisfied.
    /// Returns true when something was dropped.
    fn enforce_budgets(&mut self) -> bool {
        let mut evicted = false;
        // Expired entries go first — they are worthless.
        let stale: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, v)| self.expired(v.ts))
            .map(|(k, _)| k.clone())
            .collect();
        for k in stale {
            self.entries.remove(&k);
            evicted = true;
        }
        let mut total: u64 = self.entries.values().map(|v| v.bytes).sum();
        loop {
            let over_count = self.entries.len() > self.max_entries;
            let over_bytes = self.max_bytes > 0 && total > self.max_bytes;
            if !over_count && !over_bytes {
                break;
            }
            // Least-recently-used first; the key breaks seq ties so eviction is
            // deterministic regardless of HashMap iteration order.
            let Some(victim) = self
                .entries
                .iter()
                .min_by(|a, b| a.1.seq.cmp(&b.1.seq).then_with(|| a.0.cmp(b.0)))
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(v) = self.entries.remove(&victim) {
                total = total.saturating_sub(v.bytes);
            }
            evicted = true;
        }
        evicted
    }

    /// `ttl_secs == 0` means "never expire".
    fn expired(&self, ts: u64) -> bool {
        self.ttl_secs != 0 && (self.clock)().saturating_sub(ts) >= self.ttl_secs
    }
}

fn estimate(value: &Value) -> u64 {
    serde_json::to_vec(value)
        .map(|v| v.len() as u64)
        .unwrap_or(0)
}

fn read_file(path: &Path) -> Option<HashMap<String, CachedValue>> {
    let raw = std::fs::read_to_string(path).ok()?;
    match serde_json::from_str::<HashMap<String, CachedValue>>(&raw) {
        Ok(map) => Some(map),
        Err(_) => {
            crate::output::warn(&format!("ignoring corrupt cache file {}", path.display()));
            None
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(file: &File) {
    use std::os::unix::fs::PermissionsExt;
    let _ = file.set_permissions(std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict_permissions(_file: &File) {}

/// Advisory cross-process lock on a sidecar file.
///
/// Best-effort by design: if locking is unavailable the cache still works, it
/// just loses the concurrency guarantee.
struct FileLock {
    _file: Option<File>,
}

impl FileLock {
    fn acquire(target: &Path) -> Self {
        let mut lock_path = target.as_os_str().to_os_string();
        lock_path.push(".lock");
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(PathBuf::from(lock_path))
            .ok();
        if let Some(f) = &file {
            restrict_permissions(f);
            let _ = fs2::FileExt::lock_exclusive(f);
        }
        Self { _file: file }
    }
}

impl Drop for FileLock {
    fn drop(&mut self) {
        if let Some(f) = &self._file {
            let _ = fs2::FileExt::unlock(f);
        }
    }
}

/// Canonical cache key: SHA-256 over a JSON-serialized part list.
///
/// JSON is used instead of joining with a separator byte because `query` and
/// `url` are arbitrary Unicode and may themselves contain any separator we
/// pick — `["a\u{1f}b"]` and `["a","b"]` used to hash identically.
/// [`CACHE_SCHEMA_VERSION`] is part of the key so a change to extraction or
/// serialization invalidates old entries instead of returning stale text.
pub fn cache_key(parts: &[&str]) -> String {
    let doc = serde_json::json!({
        "v": CACHE_SCHEMA_VERSION,
        "parts": parts,
    });
    let bytes = serde_json::to_vec(&doc).unwrap_or_default();
    hex(&Sha256::digest(&bytes))
}

/// Key for a page fetch. Shared by single and batch fetch paths.
pub fn fetch_key(url: &str, opts: &crate::models::FetchOpts) -> String {
    cache_key(&[
        "fetch",
        url,
        &opts.max_bytes.to_string(),
        &opts.max_chars.to_string(),
        &opts.raw_html.to_string(),
        &opts.markdown.to_string(),
    ])
}

/// Key by requested engine, not fallback responder, so a repeated request can
/// find the answer that was stored after fallback.
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
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "webseek-cache-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Clock we can advance by hand — no `thread::sleep` in TTL tests.
    fn fake_clock() -> (Clock, Arc<AtomicU64>) {
        let now = Arc::new(AtomicU64::new(1_000));
        let handle = now.clone();
        (Arc::new(move || now.load(Ordering::SeqCst)), handle)
    }

    fn cache_with(dir: &Path, ttl: u64, max_entries: usize) -> (Cache, Arc<AtomicU64>) {
        let (clock, handle) = fake_clock();
        (
            Cache::load_with_clock(dir.join("cache.json"), ttl, max_entries, 0, clock),
            handle,
        )
    }

    #[test]
    fn put_get_roundtrip() {
        let dir = temp_dir("roundtrip");
        let (mut c, _) = cache_with(&dir, 3600, 3);
        let key = cache_key(&["search", "rust"]);
        assert!(c.get(&key).is_none());
        c.put(key.clone(), serde_json::json!([1, 2, 3]));
        assert_eq!(c.get(&key), Some(serde_json::json!([1, 2, 3])));
        let other = cache_key(&["search", "tokio"]);
        assert_ne!(key, other);
        assert!(c.get(&other).is_none());
    }

    #[test]
    fn ttl_zero_means_no_expiry() {
        let dir = temp_dir("ttl0");
        let (mut c, clock) = cache_with(&dir, 0, 10);
        let key = cache_key(&["x"]);
        c.put(key.clone(), serde_json::json!("v"));
        // Ten years later it is still a hit.
        clock.fetch_add(315_360_000, Ordering::SeqCst);
        assert_eq!(c.get(&key), Some(serde_json::json!("v")));
    }

    #[test]
    fn ttl_expiry_uses_the_injected_clock() {
        let dir = temp_dir("ttl");
        let (mut c, clock) = cache_with(&dir, 60, 10);
        let key = cache_key(&["x"]);
        c.put(key.clone(), serde_json::json!("v"));
        assert!(c.get(&key).is_some());
        clock.fetch_add(59, Ordering::SeqCst);
        assert!(c.get(&key).is_some());
        clock.fetch_add(1, Ordering::SeqCst);
        assert!(c.get(&key).is_none());
        // The expired entry is dropped, not merely hidden.
        assert_eq!(c.info().entries, 0);
    }

    #[test]
    fn eviction_is_lru_not_fifo() {
        let dir = temp_dir("lru");
        let (mut c, _) = cache_with(&dir, 0, 3);
        for i in 0..3 {
            c.put(cache_key(&["q", &i.to_string()]), serde_json::json!(i));
        }
        // Touch the oldest so it becomes the most recent.
        assert!(c.get(&cache_key(&["q", "0"])).is_some());
        c.put(cache_key(&["q", "3"]), serde_json::json!(3));
        assert_eq!(c.info().entries, 3);
        // "q|0" was refreshed, so "q|1" must be the victim.
        assert!(
            c.get(&cache_key(&["q", "0"])).is_some(),
            "hot entry evicted"
        );
        assert!(c.get(&cache_key(&["q", "1"])).is_none());
        assert!(c.get(&cache_key(&["q", "3"])).is_some());
    }

    #[test]
    fn byte_budget_evicts_and_rejects_oversized_entries() {
        let dir = temp_dir("bytes");
        let (clock, _) = fake_clock();
        let mut c = Cache::load_with_clock(dir.join("cache.json"), 0, 100, 400, clock);
        for i in 0..10 {
            c.put(
                cache_key(&["big", &i.to_string()]),
                serde_json::json!("x".repeat(100)),
            );
        }
        assert!(c.info().bytes <= 400, "bytes {}", c.info().bytes);
        assert!(c.info().entries < 10);
        // One entry bigger than the whole budget is simply not cached.
        c.put(cache_key(&["huge"]), serde_json::json!("y".repeat(1000)));
        assert!(c.get(&cache_key(&["huge"])).is_none());
    }

    #[test]
    fn corrupt_file_is_discarded() {
        let dir = temp_dir("corrupt");
        let path = dir.join("cache.json");
        std::fs::write(&path, "{ not json !!!").unwrap();
        let c = Cache::load(path, 3600, 10, 0);
        assert_eq!(c.info().entries, 0);
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
    fn separator_bytes_in_input_cannot_collide() {
        // The old key joined parts with U+001F, which a query may contain.
        let a = cache_key(&["search", "a\u{1f}b"]);
        let b = cache_key(&["search", "a", "b"]);
        assert_ne!(a, b, "unit separator in a query collides with the joiner");
        let c = cache_key(&["a\"b"]);
        let d = cache_key(&["a", "b"]);
        assert_ne!(c, d);
    }

    #[test]
    fn real_key_builders_include_every_result_changing_option() {
        let fetch = crate::models::FetchOpts::default();
        let base = fetch_key("https://x", &fetch);
        for changed in [
            crate::models::FetchOpts {
                max_bytes: 17,
                ..fetch.clone()
            },
            crate::models::FetchOpts {
                max_chars: 17,
                ..fetch.clone()
            },
            crate::models::FetchOpts {
                raw_html: true,
                ..fetch.clone()
            },
            crate::models::FetchOpts {
                markdown: true,
                ..fetch.clone()
            },
        ] {
            assert_ne!(base, fetch_key("https://x", &changed));
        }
        assert_ne!(base, fetch_key("https://y", &fetch));

        let search = crate::models::SearchOpts::default();
        let base = search_key("bing", "q", &search);
        assert_ne!(base, search_key("duckduckgo", "q", &search));
        assert_ne!(base, search_key("bing", "other", &search));
        assert_ne!(
            base,
            search_key(
                "bing",
                "q",
                &crate::models::SearchOpts {
                    count: 9,
                    ..search.clone()
                }
            )
        );

        let base = images_key("bing", "q", 5, false);
        assert_ne!(base, images_key("bing", "q", 6, false));
        assert_ne!(base, images_key("bing", "q", 5, true));
        assert_ne!(base, images_key("duckduckgo", "q", 5, false));
    }

    #[test]
    fn saves_and_reloads_across_instances() {
        let dir = temp_dir("persist");
        let path = dir.join("cache.json");
        let key = cache_key(&["persisted"]);
        {
            let mut c = Cache::load(path.clone(), 0, 10, 0);
            c.put(key.clone(), serde_json::json!("kept"));
            c.save();
        }
        let mut c = Cache::load(path, 0, 10, 0);
        assert_eq!(c.get(&key), Some(serde_json::json!("kept")));
    }

    #[test]
    fn concurrent_writers_do_not_lose_updates() {
        let dir = temp_dir("merge");
        let path = dir.join("cache.json");
        let key_a = cache_key(&["proc", "a"]);
        let key_b = cache_key(&["proc", "b"]);

        // Both "processes" load the same (empty) cache...
        let mut a = Cache::load(path.clone(), 0, 100, 0);
        let mut b = Cache::load(path.clone(), 0, 100, 0);
        a.put(key_a.clone(), serde_json::json!("from-a"));
        b.put(key_b.clone(), serde_json::json!("from-b"));
        // ...and save one after the other.
        a.save();
        b.save();

        let mut reloaded = Cache::load(path, 0, 100, 0);
        assert_eq!(reloaded.get(&key_a), Some(serde_json::json!("from-a")));
        assert_eq!(reloaded.get(&key_b), Some(serde_json::json!("from-b")));
    }

    #[test]
    fn shrinking_max_entries_trims_on_load() {
        let dir = temp_dir("shrink");
        let path = dir.join("cache.json");
        {
            let mut c = Cache::load(path.clone(), 0, 100, 0);
            for i in 0..20 {
                c.put(cache_key(&["k", &i.to_string()]), serde_json::json!(i));
            }
            c.save();
        }
        let c = Cache::load(path, 0, 5, 0);
        assert_eq!(c.info().entries, 5);
    }

    #[test]
    fn clear_empties_the_cache_and_removes_the_file() {
        let dir = temp_dir("clear");
        let path = dir.join("cache.json");
        let mut c = Cache::load(path.clone(), 0, 10, 0);
        c.put(cache_key(&["a"]), serde_json::json!(1));
        c.save();
        assert!(path.exists());
        assert_eq!(c.clear().unwrap(), 1);
        assert!(!path.exists());
        assert_eq!(c.info().entries, 0);
    }

    #[test]
    fn disabled_cache_never_stores() {
        let mut c = Cache::disabled();
        let key = cache_key(&["x"]);
        c.put(key.clone(), serde_json::json!(1));
        assert!(c.get(&key).is_none());
        assert!(!c.is_enabled());
    }

    #[test]
    fn no_stray_tmp_files_are_left_behind() {
        let dir = temp_dir("tmp");
        let mut c = Cache::load(dir.join("cache.json"), 0, 10, 0);
        c.put(cache_key(&["a"]), serde_json::json!(1));
        c.save();
        let leftovers: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(leftovers.is_empty(), "left: {leftovers:?}");
    }
}
