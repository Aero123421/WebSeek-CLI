//! Batch fetching: fetch many URLs with a worker pool, preserving input
//! order in the output. Per-URL failures become error items instead of
//! aborting the batch (essential for bulk research).
//!
//! Politeness is delegated entirely to the shared [`crate::ratelimit::RateLimiter`]
//! inside [`Http`] — every worker's requests go through the same per-origin
//! limiter, so `-j 10` against one host is paced exactly like `-j 1` would
//! be. There is no separate post-request sleep here anymore (the old one ran
//! *after* each worker's own request, which is the wrong moment to enforce
//! politeness between concurrent workers sharing a host).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::ser::SerializeStruct;

use crate::cache::Cache;
use crate::http::Http;
use crate::models::{FetchOpts, FetchResult};
use crate::robots::RobotsChecker;

/// One item of batch output: a fetched page or a per-URL error.
#[derive(Debug, Clone)]
pub enum BatchItem {
    Ok(FetchResult),
    Err { url: String, error: String },
}

/// Tagged (`{"ok":true,"value":{...}}` / `{"ok":false,"error":{...}}`)
/// rather than shape-sniffed: a field added to `FetchResult` in the future
/// can never again be confused with the error item's shape the way an
/// untagged union risks.
impl serde::Serialize for BatchItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            BatchItem::Ok(f) => {
                let mut m = s.serialize_struct("BatchItem", 2)?;
                m.serialize_field("ok", &true)?;
                m.serialize_field("value", f)?;
                m.end()
            }
            BatchItem::Err { url, error } => {
                let mut m = s.serialize_struct("BatchItem", 2)?;
                m.serialize_field("ok", &false)?;
                m.serialize_field(
                    "error",
                    &BatchError {
                        url,
                        message: error,
                    },
                )?;
                m.end()
            }
        }
    }
}

#[derive(serde::Serialize)]
struct BatchError<'a> {
    url: &'a str,
    message: &'a str,
}

/// Fetch `urls` (in order) using up to `jobs` workers. When `respect_robots`
/// is false, robots.txt is never consulted.
///
/// Duplicate URLs in the input are fetched only once (singleflight): without
/// this, two workers could both miss the cache for the same URL and issue
/// the same request concurrently. A panic while processing one URL is
/// caught and turned into an error item for that URL alone — the rest of
/// the batch still completes.
pub fn fetch_many(
    http: &Http,
    urls: &[String],
    opts: &FetchOpts,
    jobs: usize,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<RobotsChecker>,
    respect_robots: bool,
) -> Vec<BatchItem> {
    let mut unique: Vec<String> = Vec::new();
    let mut index_of: HashMap<&str, usize> = HashMap::new();
    for u in urls {
        if !index_of.contains_key(u.as_str()) {
            index_of.insert(u.as_str(), unique.len());
            unique.push(u.clone());
        }
    }

    let jobs = jobs.clamp(1, unique.len().max(1));
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let next = next.clone();
            let tx = tx.clone();
            let unique = &unique;
            scope.spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= unique.len() {
                    break;
                }
                let url = &unique[idx];
                let item = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    fetch_one(http, url, opts, cache, robots, respect_robots)
                }))
                .unwrap_or_else(|_| BatchItem::Err {
                    url: url.clone(),
                    error: "internal error: worker panicked while fetching this URL".into(),
                });
                if tx.send((idx, item)).is_err() {
                    break; // receiver gone
                }
            });
        }
    });

    // Drop the original sender so `recv()` terminates once all workers
    // (which hold clones) finish; otherwise the channel never closes.
    drop(tx);

    let mut unique_items: Vec<Option<BatchItem>> = Vec::with_capacity(unique.len());
    unique_items.resize_with(unique.len(), || None);
    while let Ok((idx, item)) = rx.recv() {
        unique_items[idx] = Some(item);
    }

    urls.iter()
        .map(|u| {
            let idx = index_of[u.as_str()];
            unique_items[idx]
                .clone()
                .expect("every unique URL is processed by exactly one worker, panic or not")
        })
        .collect()
}

fn fetch_one(
    http: &Http,
    url: &str,
    opts: &FetchOpts,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<RobotsChecker>,
    respect_robots: bool,
) -> BatchItem {
    // robots.txt gate *before* the cache, matching single-URL fetch: a page
    // that is now disallowed must not be served from a cache entry created
    // before the site added the rule. Lookup failures are advisory (allowed).
    if respect_robots {
        let allowed = robots.is_allowed(http, url).unwrap_or(true);
        if !allowed {
            return BatchItem::Err {
                url: url.to_string(),
                error: "blocked by robots.txt (use --ignore-robots to override)".into(),
            };
        }
    }

    let key = crate::cache::cache_key(&[
        "fetch",
        url,
        &opts.max_chars.to_string(),
        &opts.raw_html.to_string(),
        &opts.markdown.to_string(),
    ]);
    let cached = cache.lock().ok().and_then(|mut c| c.get(&key));
    if let Some(v) = cached {
        match serde_json::from_value::<FetchResult>(v) {
            Ok(f) => return BatchItem::Ok(f),
            Err(_) => {
                // Corrupt/stale entry: drop it and fetch fresh, exactly like
                // the single-URL path does — this used to surface as a
                // fatal-looking "corrupt cache entry" error item instead.
                if let Ok(mut c) = cache.lock() {
                    c.remove(&key);
                }
            }
        }
    }

    match crate::reader::fetch(http, url, opts) {
        Ok(fetched) => {
            if let Ok(mut c) = cache.lock() {
                if let Ok(value) = serde_json::to_value(&fetched) {
                    c.put(key, value);
                }
            }
            BatchItem::Ok(fetched)
        }
        Err(e) => BatchItem::Err {
            url: url.to_string(),
            error: e.to_string(),
        },
    }
}
