//! Seen-state for `--watch NAME`: emit only results earlier runs did not.
//!
//! A watch is a named set of URLs that runs with the same name have already
//! emitted. `search --watch NAME` and `run --watch NAME` drop results whose
//! URL is in the set, print the rest, and then add those to the set — so
//! running the same query from cron yields only what is new since last time.
//!
//! Rules this module owns:
//!
//! - **Recorded only after output succeeds.** A result is marked seen once it
//!   was actually written, so a failed write or a closed pipe never swallows
//!   an item.
//! - **Serialized across processes.** An advisory lock is held from reading
//!   the set to saving it, so two overlapping cron runs cannot both emit the
//!   same item.
//! - **Corruption is loud.** An unreadable state file is an error, not a
//!   silent reset: a reset would re-announce every old item as new.
//! - **Bounded.** At most [`MAX_SEEN`] URLs are kept per watch; the oldest
//!   are forgotten first.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cache::FileLock;
use crate::error::{Error, Result};

/// Most URLs remembered per watch. Oldest-first eviction beyond this.
pub const MAX_SEEN: usize = 20_000;
/// Longest accepted watch name.
pub const MAX_NAME_LEN: usize = 64;
const STATE_VERSION: u32 = 1;

/// Directory holding watch state: `WEBSEEK_WATCH_DIR`, else the platform
/// data dir + `webseek/watch`.
pub fn watch_dir() -> PathBuf {
    if let Some(dir) = std::env::var_os("WEBSEEK_WATCH_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir);
    }
    directories::ProjectDirs::from("dev", "webseek", "webseek")
        .map(|d| d.data_dir().join("watch"))
        .unwrap_or_else(|| PathBuf::from("webseek-watch"))
}

/// Names become file names, so they are restricted to a portable, traversal-
/// free alphabet: ASCII letters, digits, `-`, `_` and `.`, not starting
/// with `.`.
pub fn validate_name(name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'));
    if ok {
        Ok(())
    } else {
        Err(Error::Config(format!(
            "invalid watch name '{name}': use 1..={MAX_NAME_LEN} of A-Z a-z 0-9 - _ . \
             (not starting with '.')"
        )))
    }
}

fn state_path(dir: &Path, name: &str) -> PathBuf {
    dir.join(format!("{name}.json"))
}

#[derive(Serialize, Deserialize)]
struct StateFile {
    version: u32,
    /// URL -> unix seconds when it was first emitted.
    seen: HashMap<String, u64>,
}

/// An open watch: its seen-set, locked until dropped.
pub struct Watch {
    name: String,
    path: PathBuf,
    seen: HashMap<String, u64>,
    _lock: FileLock,
}

impl Watch {
    /// Open (or start) the watch `name` in the default directory.
    pub fn open(name: &str) -> Result<Self> {
        Self::open_in(&watch_dir(), name)
    }

    pub fn open_in(dir: &Path, name: &str) -> Result<Self> {
        validate_name(name)?;
        std::fs::create_dir_all(dir).map_err(|e| {
            Error::Config(format!("cannot create watch dir {}: {e}", dir.display()))
        })?;
        let path = state_path(dir, name);
        let lock = FileLock::acquire(&path);
        let seen = match std::fs::read(&path) {
            Ok(bytes) => {
                let state: StateFile = serde_json::from_slice(&bytes).map_err(|e| {
                    Error::Config(format!(
                        "watch '{name}' state {} is unreadable ({e}); \
                         run `webseek watch clear {name}` to start over",
                        path.display()
                    ))
                })?;
                if state.version != STATE_VERSION {
                    return Err(Error::Config(format!(
                        "watch '{name}' state has unsupported version {}",
                        state.version
                    )));
                }
                state.seen
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => HashMap::new(),
            Err(e) => {
                return Err(Error::Config(format!(
                    "cannot read watch state {}: {e}",
                    path.display()
                )))
            }
        };
        Ok(Self {
            name: name.to_string(),
            path,
            seen,
            _lock: lock,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Keep only items whose URL is new — not seen in an earlier run and not
    /// a repeat within `items`. Returns the new items and how many were
    /// dropped as already known.
    pub fn filter_new<T>(&self, items: Vec<T>, url_of: impl Fn(&T) -> &str) -> (Vec<T>, usize) {
        let total = items.len();
        let mut batch = HashSet::new();
        let fresh: Vec<T> = items
            .into_iter()
            .filter(|it| {
                let url = url_of(it);
                !self.seen.contains_key(url) && batch.insert(url.to_string())
            })
            .collect();
        let known = total - fresh.len();
        (fresh, known)
    }

    /// Mark `urls` as seen now (first-seen time is kept for known URLs).
    pub fn record<'a>(&mut self, urls: impl IntoIterator<Item = &'a str>) {
        let now = now_secs();
        for url in urls {
            self.seen.entry(url.to_string()).or_insert(now);
        }
    }

    /// Persist the set (evicting the oldest beyond [`MAX_SEEN`]) and release
    /// the lock.
    pub fn save(mut self) -> Result<()> {
        if self.seen.len() > MAX_SEEN {
            let mut by_age: Vec<(u64, String)> =
                self.seen.iter().map(|(u, t)| (*t, u.clone())).collect();
            by_age.sort();
            for (_, url) in by_age.into_iter().take(self.seen.len() - MAX_SEEN) {
                self.seen.remove(&url);
            }
        }
        let state = StateFile {
            version: STATE_VERSION,
            seen: std::mem::take(&mut self.seen),
        };
        let bytes = serde_json::to_vec(&state).map_err(|e| Error::Config(e.to_string()))?;
        crate::cache::write_private_atomic(&self.path, &bytes).map_err(|e| {
            Error::Config(format!(
                "cannot save watch state {}: {e}",
                self.path.display()
            ))
        })
    }
}

/// One line of `webseek watch list`.
#[derive(Debug, Serialize)]
pub struct WatchInfo {
    pub name: String,
    /// URLs remembered.
    pub seen: usize,
    /// Most recent first-seen time (RFC 3339), `null` when empty.
    pub last_new: Option<String>,
    pub path: String,
}

/// Every watch in `dir`, sorted by name.
pub fn list_in(dir: &Path) -> Result<Vec<WatchInfo>> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Config(format!(
                "cannot read watch dir {}: {e}",
                dir.display()
            )))
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let Some(name) = path
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if validate_name(name).is_err() {
            continue;
        }
        let state = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<StateFile>(&b).ok());
        let (seen, last_new) = match state {
            Some(s) => (
                s.seen.len(),
                s.seen
                    .values()
                    .max()
                    .and_then(|t| crate::time::unix_to_rfc3339(*t as i64)),
            ),
            None => (0, None),
        };
        out.push(WatchInfo {
            name: name.to_string(),
            seen,
            last_new,
            path: path.display().to_string(),
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

pub fn list() -> Result<Vec<WatchInfo>> {
    list_in(&watch_dir())
}

/// Forget the watch `name`. Returns whether it existed.
pub fn clear_in(dir: &Path, name: &str) -> Result<bool> {
    validate_name(name)?;
    let path = state_path(dir, name);
    let _lock = FileLock::acquire(&path);
    let existed = match std::fs::remove_file(&path) {
        Ok(()) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => {
            return Err(Error::Config(format!(
                "cannot remove {}: {e}",
                path.display()
            )))
        }
    };
    drop(_lock);
    let mut lock_path = path.into_os_string();
    lock_path.push(".lock");
    let _ = std::fs::remove_file(lock_path);
    Ok(existed)
}

pub fn clear(name: &str) -> Result<bool> {
    clear_in(&watch_dir(), name)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "webseek-watch-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn names_are_restricted_to_a_safe_alphabet() {
        for ok in ["news", "rust-jp", "a.b_c", "X1"] {
            assert!(validate_name(ok).is_ok(), "{ok}");
        }
        for bad in [
            "",
            ".hidden",
            "../x",
            "a/b",
            "a b",
            "ラーメン",
            &"x".repeat(65),
        ] {
            assert!(validate_name(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn second_run_sees_only_new_urls() {
        let d = dir("runs");
        let w = Watch::open_in(&d, "t").unwrap();
        let (fresh, known) = w.filter_new(vec!["a", "b", "a"], |s| s);
        assert_eq!(fresh, vec!["a", "b"], "repeats within one run are dropped");
        assert_eq!(known, 1);
        let mut w = w;
        w.record(fresh.iter().copied());
        w.save().unwrap();

        let w = Watch::open_in(&d, "t").unwrap();
        let (fresh, known) = w.filter_new(vec!["b", "c"], |s| s);
        assert_eq!(fresh, vec!["c"]);
        assert_eq!(known, 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn unrecorded_items_stay_new() {
        let d = dir("unrecorded");
        let w = Watch::open_in(&d, "t").unwrap();
        let (fresh, _) = w.filter_new(vec!["a"], |s| s);
        assert_eq!(fresh.len(), 1);
        w.save().unwrap(); // nothing recorded: e.g. output failed
        let w = Watch::open_in(&d, "t").unwrap();
        assert_eq!(w.filter_new(vec!["a"], |s| s).0, vec!["a"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn corrupt_state_is_an_error_not_a_reset() {
        let d = dir("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        std::fs::write(d.join("t.json"), b"{not json").unwrap();
        let err = Watch::open_in(&d, "t").err().unwrap().to_string();
        assert!(err.contains("webseek watch clear t"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn oldest_urls_are_evicted_beyond_the_cap() {
        let d = dir("cap");
        let mut w = Watch::open_in(&d, "t").unwrap();
        for i in 0..MAX_SEEN {
            w.seen.insert(format!("old{i}"), 1);
        }
        w.seen.insert("newest".into(), 10);
        w.save().unwrap();
        let w = Watch::open_in(&d, "t").unwrap();
        assert_eq!(w.seen.len(), MAX_SEEN);
        assert!(w.seen.contains_key("newest"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn list_and_clear() {
        let d = dir("list");
        assert!(list_in(&d).unwrap().is_empty(), "missing dir lists nothing");
        let mut w = Watch::open_in(&d, "b").unwrap();
        w.record(["https://x"]);
        w.save().unwrap();
        Watch::open_in(&d, "a").unwrap().save().unwrap();

        let all = list_in(&d).unwrap();
        let names: Vec<&str> = all.iter().map(|w| w.name.as_str()).collect();
        assert_eq!(names, vec!["a", "b"]);
        assert_eq!(all[1].seen, 1);
        assert!(all[1].last_new.is_some());
        assert_eq!(all[0].last_new, None);

        assert!(clear_in(&d, "b").unwrap());
        assert!(!clear_in(&d, "b").unwrap());
        assert_eq!(list_in(&d).unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }
}
