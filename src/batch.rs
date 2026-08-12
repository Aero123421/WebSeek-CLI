//! Batch fetching: fetch many URLs with a worker pool, preserving input
//! order in the output. Per-URL failures become error items instead of
//! aborting the batch (essential for bulk research).
//!
//! Two properties this module owns:
//!
//! - **Isolation is real.** A failing URL — including one that panics the HTML
//!   parser — becomes an error item; it never takes the other results with it.
//! - **Concurrency does not multiply the request rate.** Workers share one
//!   [`Pacer`], so `-j 8` overlaps latency without being eight times ruder
//!   than `-j 1`.

use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use reqwest::blocking::Client;
use serde::ser::SerializeStruct;

use crate::cache::Cache;
use crate::error::Error;
use crate::models::{FetchOpts, FetchResult};
use crate::net::EgressPolicy;
use crate::pace::Pacer;
use crate::robots::RobotsChecker;

/// One item of batch output: a fetched page or a per-URL error.
#[derive(Debug, Clone)]
pub enum BatchItem {
    Ok(FetchResult),
    Err {
        url: String,
        error: String,
        /// Stable error class (see [`Error::kind`]).
        kind: &'static str,
    },
}

impl BatchItem {
    /// Error item from a typed error, keeping message and class in sync.
    pub fn from_error(url: &str, e: &Error) -> Self {
        BatchItem::Err {
            url: url.to_string(),
            error: e.to_string(),
            kind: e.kind(),
        }
    }

    /// Error item with an explicit message (used by tests and internals).
    pub fn err(url: &str, error: &str) -> Self {
        BatchItem::Err {
            url: url.to_string(),
            error: error.to_string(),
            kind: "network",
        }
    }

    pub fn is_err(&self) -> bool {
        matches!(self, BatchItem::Err { .. })
    }
}

impl serde::Serialize for BatchItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            BatchItem::Ok(f) => f.serialize(s),
            BatchItem::Err { url, error, kind } => {
                let mut m = s.serialize_struct("ErrorItem", 3)?;
                m.serialize_field("url", url)?;
                m.serialize_field("error", error)?;
                m.serialize_field("kind", kind)?;
                m.end()
            }
        }
    }
}

/// Everything a batch worker needs, bundled so the signature stays readable.
pub struct BatchCtx<'a> {
    pub client: &'a Client,
    pub opts: &'a FetchOpts,
    pub cache: &'a Arc<Mutex<Cache>>,
    pub robots: &'a Arc<Mutex<RobotsChecker>>,
    pub pacer: &'a Arc<Pacer>,
    pub respect_robots: bool,
    pub policy: EgressPolicy,
}

/// Fetch `urls` (in order) using up to `jobs` workers.
pub fn fetch_many(ctx: &BatchCtx<'_>, urls: &[String], jobs: usize) -> Vec<BatchItem> {
    let jobs = jobs.clamp(1, crate::cli::MAX_JOBS).min(urls.len().max(1));
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();

    // `thread::scope` joins every worker before returning, so all sends have
    // happened by the time we drain; the channel is unbounded, so no worker
    // can block on a full queue.
    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let next = next.clone();
            let tx = tx.clone();
            scope.spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= urls.len() {
                    break;
                }
                let url = &urls[idx];
                let item = fetch_one_isolated(ctx, url);
                if tx.send((idx, item)).is_err() {
                    break; // receiver gone
                }
            });
        }
    });
    drop(tx);

    let mut items: Vec<Option<BatchItem>> = Vec::with_capacity(urls.len());
    items.resize_with(urls.len(), || None);
    while let Ok((idx, item)) = rx.recv() {
        items[idx] = Some(item);
    }
    items
        .into_iter()
        .zip(urls)
        .map(|(item, url)| item.unwrap_or_else(|| BatchItem::err(url, "worker produced no result")))
        .collect()
}

/// `fetch_one`, but a panic in the HTML parser degrades to an error item
/// instead of unwinding out of the worker and killing the whole batch.
fn fetch_one_isolated(ctx: &BatchCtx<'_>, url: &str) -> BatchItem {
    match std::panic::catch_unwind(AssertUnwindSafe(|| fetch_one(ctx, url))) {
        Ok(Ok((f, _cached))) => BatchItem::Ok(f),
        Ok(Err(e)) => BatchItem::from_error(url, &e),
        Err(_) => BatchItem::Err {
            url: url.to_string(),
            error: "internal error while parsing this page".to_string(),
            kind: "parse",
        },
    }
}

/// Fetch exactly one URL with cache + robots handling.
///
/// The single-URL and batch commands both go through here, so robots ordering,
/// cache keys and error wording cannot drift apart between them. Only the
/// *presentation* differs: in array mode a failure is a data item, in object
/// mode it is a command failure.
/// Returns the page and whether it came from the cache (for `--verbose`).
pub fn fetch_one(ctx: &BatchCtx<'_>, url: &str) -> crate::error::Result<(FetchResult, bool)> {
    // A strict invocation must never serve a cached response for a destination
    // it would now refuse to contact.
    crate::net::parse_checked(url, ctx.policy)?;

    // robots.txt is consulted before the cache: a cached copy is not
    // permission to have fetched it, and the two paths must agree on order.
    if ctx.respect_robots {
        let allowed = match ctx.robots.lock() {
            Ok(mut c) => c.is_allowed(ctx.client, ctx.pacer, url).unwrap_or(true),
            Err(_) => true,
        };
        if !allowed {
            return Err(Error::robots_blocked(url));
        }
    }

    let key = crate::cache::fetch_key(url, ctx.opts);
    if let Some(v) = ctx.cache.lock().ok().and_then(|mut c| c.get(&key)) {
        if let Ok(f) = serde_json::from_value::<FetchResult>(v) {
            return Ok((f, true));
        }
        // A malformed entry is a cache bug, not a page error: fall through and
        // fetch it for real rather than reporting a failure to the caller.
        if let Ok(mut cache) = ctx.cache.lock() {
            cache.remove(&key);
        }
    }

    let fetched = crate::reader::fetch_paced(ctx.client, url, ctx.opts, ctx.policy, ctx.pacer)?;
    if let Ok(mut c) = ctx.cache.lock() {
        if let Ok(value) = serde_json::to_value(&fetched) {
            c.put(key, value);
        }
    }
    Ok((fetched, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_items_carry_a_stable_kind() {
        let item = BatchItem::from_error("https://x", &Error::Http(404));
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["kind"], serde_json::json!("http"));
        assert_eq!(v["url"], serde_json::json!("https://x"));
        assert!(v["error"].as_str().unwrap().contains("404"));
    }

    #[test]
    fn robots_refusal_has_the_same_message_everywhere() {
        let item = BatchItem::from_error("https://x/p", &Error::robots_blocked("https://x/p"));
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["kind"], serde_json::json!("robots"));
        assert_eq!(
            v["error"].as_str().unwrap(),
            Error::robots_blocked("https://x/p").to_string(),
            "single-URL and batch paths must not diverge in wording"
        );
    }
}
