//! Batch fetching: fetch many URLs with a worker pool, preserving input
//! order in the output. Per-URL failures become error items instead of
//! aborting the batch (essential for bulk research).

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use reqwest::blocking::Client;
use serde::ser::SerializeStruct;

use crate::cache::Cache;
use crate::models::{FetchOpts, FetchResult};
use crate::robots::RobotsChecker;

/// One item of batch output: a fetched page or a per-URL error.
#[derive(Debug, Clone)]
pub enum BatchItem {
    Ok(FetchResult),
    Err { url: String, error: String },
}

impl serde::Serialize for BatchItem {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        match self {
            BatchItem::Ok(f) => f.serialize(s),
            BatchItem::Err { url, error } => {
                let mut m = s.serialize_struct("ErrorItem", 2)?;
                m.serialize_field("url", url)?;
                m.serialize_field("error", error)?;
                m.end()
            }
        }
    }
}

/// Fetch `urls` (in order) using up to `jobs` workers. `delay` is applied by
/// each worker between its own requests (approximate politeness). When
/// `respect_robots` is false, robots.txt is never consulted.
#[allow(clippy::too_many_arguments)]
pub fn fetch_many(
    client: &Client,
    urls: &[String],
    opts: &FetchOpts,
    jobs: usize,
    delay: Duration,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<Mutex<RobotsChecker>>,
    respect_robots: bool,
) -> Vec<BatchItem> {
    let jobs = jobs.max(1);
    let next = Arc::new(AtomicUsize::new(0));
    let (tx, rx) = mpsc::channel();
    let client = client.clone();

    std::thread::scope(|scope| {
        for _ in 0..jobs {
            let next = next.clone();
            let tx = tx.clone();
            let client = client.clone();
            let cache = cache.clone();
            let robots = robots.clone();
            scope.spawn(move || loop {
                let idx = next.fetch_add(1, Ordering::Relaxed);
                if idx >= urls.len() {
                    break;
                }
                let url = &urls[idx];
                let item = fetch_one(&client, url, opts, &cache, &robots, respect_robots);
                if tx.send((idx, item)).is_err() {
                    break; // receiver gone
                }
                if !delay.is_zero() {
                    std::thread::sleep(delay);
                }
            });
        }
    });

    // Drop the original sender so `recv()` terminates once all workers
    // (which hold clones) finish; otherwise the channel never closes.
    drop(tx);

    let mut items: Vec<Option<BatchItem>> = Vec::with_capacity(urls.len());
    items.resize_with(urls.len(), || None);
    while let Ok((idx, item)) = rx.recv() {
        items[idx] = Some(item);
    }
    // Infallible: every index was processed by exactly one worker.
    items.into_iter().map(|it| it.unwrap()).collect()
}

fn fetch_one(
    client: &Client,
    url: &str,
    opts: &FetchOpts,
    cache: &Arc<Mutex<Cache>>,
    robots: &Arc<Mutex<RobotsChecker>>,
    respect_robots: bool,
) -> BatchItem {
    let key = crate::cache::cache_key(&[
        "fetch",
        url,
        &opts.max_chars.to_string(),
        &opts.raw_html.to_string(),
        &opts.markdown.to_string(),
    ]);
    if let Some(v) = cache.lock().ok().and_then(|c| c.get(&key)) {
        return match serde_json::from_value::<FetchResult>(v) {
            Ok(f) => BatchItem::Ok(f),
            Err(_) => BatchItem::Err {
                url: url.to_string(),
                error: "corrupt cache entry".into(),
            },
        };
    }

    // robots.txt gate (opt-in; lookup failures are advisory -> allowed).
    if respect_robots {
        let allowed = match robots.lock() {
            Ok(mut c) => c.is_allowed(client, url).unwrap_or(true),
            Err(_) => true,
        };
        if !allowed {
            return BatchItem::Err {
                url: url.to_string(),
                error: "blocked by robots.txt (opt out: unset respect_robots)".into(),
            };
        }
    }

    match crate::reader::fetch(client, url, opts) {
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
