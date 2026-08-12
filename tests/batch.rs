//! Integration tests for batch fetching and robots.txt gating (wiremock).
//! No real network access — all responses are local fixtures.
//!
//! Same runtime rule as `engines.rs`: the async runtime is used ONLY for
//! wiremock setup; the blocking `reqwest` client and `fetch_many` run in the
//! sync test thread.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use reqwest::blocking::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use webseek::batch::{fetch_many, fetch_one, BatchCtx, BatchItem};
use webseek::cache::Cache;
use webseek::models::{FetchOpts, FetchResult};
use webseek::pace::Pacer;
use webseek::robots::RobotsChecker;

const PAGE_A: &str = r#"<html><head><title>Page A</title></head>
<body><article><h1>Heading A</h1><p>Alpha content here.</p></article></body></html>"#;

const PAGE_B: &str = r#"<html><head><title>Page B</title></head>
<body><article><h1>Heading B</h1><p>Beta content here.</p></article></body></html>"#;

const ROBOTS_TXT: &str = "User-agent: *\nDisallow: /private/\n";
/// The blanket form that a prefix-only matcher used to ignore entirely.
const ROBOTS_BLANKET: &str = "User-agent: *\nDisallow: /*\n";

fn client() -> Client {
    Client::builder()
        .user_agent("webseek-test")
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn fetch_opts() -> FetchOpts {
    FetchOpts {
        max_bytes: 1024 * 1024,
        ..Default::default()
    }
}

struct Harness {
    client: Client,
    opts: FetchOpts,
    cache: Arc<Mutex<Cache>>,
    robots: Arc<Mutex<RobotsChecker>>,
    pacer: Arc<Pacer>,
}

impl Harness {
    fn new() -> Self {
        Self::with_delay(Duration::ZERO)
    }

    fn with_delay(delay: Duration) -> Self {
        Self {
            client: client(),
            opts: fetch_opts(),
            cache: Arc::new(Mutex::new(Cache::disabled())),
            robots: Arc::new(Mutex::new(RobotsChecker::new())),
            pacer: Arc::new(Pacer::new(delay)),
        }
    }

    fn ctx(&self, respect_robots: bool) -> BatchCtx<'_> {
        BatchCtx {
            client: &self.client,
            opts: &self.opts,
            cache: &self.cache,
            robots: &self.robots,
            pacer: &self.pacer,
            respect_robots,
        }
    }
}

fn setup_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn unwrap_ok(item: &BatchItem) -> &FetchResult {
    match item {
        BatchItem::Ok(f) => f,
        BatchItem::Err { url, error, .. } => panic!("expected Ok, got Err for {url}: {error}"),
    }
}

fn unwrap_err(item: &BatchItem) -> (&str, &str) {
    match item {
        BatchItem::Err { error, kind, .. } => (error.as_str(), kind),
        BatchItem::Ok(f) => panic!("expected Err, got Ok for {}", f.url),
    }
}

#[test]
fn batch_fetch_preserves_input_order() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/a"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/b"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_B))
            .mount(&server)
            .await;
    });

    let urls = vec![format!("{}/a", server.uri()), format!("{}/b", server.uri())];
    let h = Harness::new();
    let items = fetch_many(&h.ctx(false), &urls, 2);

    assert_eq!(items.len(), 2);
    let a = unwrap_ok(&items[0]);
    let b = unwrap_ok(&items[1]);
    assert!(a.url.ends_with("/a"));
    assert!(b.url.ends_with("/b"));
    assert_eq!(a.title.as_deref(), Some("Page A"));
    assert!(a.text.contains("Alpha content here."));
    assert!(b.text.contains("Beta content here."));
}

#[test]
fn batch_fetch_reports_per_url_errors_without_aborting() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/good"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
        // "/missing" is intentionally unmocked -> wiremock answers 404.
    });

    let urls = vec![
        format!("{}/good", server.uri()),
        format!("{}/missing", server.uri()),
    ];
    let h = Harness::new();
    let items = fetch_many(&h.ctx(false), &urls, 2);

    assert_eq!(items.len(), 2);
    assert!(unwrap_ok(&items[0]).url.ends_with("/good"));
    let (err, kind) = unwrap_err(&items[1]);
    assert!(err.contains("404"), "unexpected error: {err}");
    assert_eq!(kind, "http", "agents branch on kind, not on prose");
}

#[test]
fn robots_blocks_disallowed_url_when_enabled() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ROBOTS_TXT))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/private/secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/public"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_B))
            .mount(&server)
            .await;
    });

    let urls = vec![
        format!("{}/private/secret", server.uri()),
        format!("{}/public", server.uri()),
    ];
    let h = Harness::new();
    let items = fetch_many(&h.ctx(true), &urls, 2);

    assert_eq!(items.len(), 2);
    let (err, kind) = unwrap_err(&items[0]);
    assert!(err.contains("robots.txt"), "unexpected error: {err}");
    assert_eq!(kind, "robots");
    assert!(unwrap_ok(&items[1]).url.ends_with("/public"));
}

#[test]
fn blanket_wildcard_disallow_is_honored_end_to_end() {
    // `Disallow: /*` is how a site says "no bots". Prefix-only matching read
    // it as a literal and crawled everything anyway.
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ROBOTS_BLANKET))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/anything"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });

    let urls = vec![format!("{}/anything", server.uri())];
    let h = Harness::new();
    let items = fetch_many(&h.ctx(true), &urls, 1);
    let (err, kind) = unwrap_err(&items[0]);
    assert_eq!(kind, "robots", "got: {err}");
}

#[test]
fn robots_is_ignored_when_disabled() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ROBOTS_TXT))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/private/secret"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });

    let urls = vec![format!("{}/private/secret", server.uri())];
    let h = Harness::new();
    let items = fetch_many(&h.ctx(false), &urls, 1);

    assert_eq!(items.len(), 1);
    assert!(unwrap_ok(&items[0]).url.ends_with("/private/secret"));
}

#[test]
fn single_and_batch_paths_agree_on_robots_refusal() {
    // The same condition used to produce two different messages depending
    // only on how many URLs the caller passed.
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ROBOTS_TXT))
            .mount(&server)
            .await;
    });
    let url = format!("{}/private/x", server.uri());
    let h = Harness::new();

    let single = fetch_one(&h.ctx(true), &url).unwrap_err();
    let batch = fetch_many(&h.ctx(true), std::slice::from_ref(&url), 1);
    let (batch_msg, batch_kind) = unwrap_err(&batch[0]);

    assert_eq!(single.to_string(), batch_msg);
    assert_eq!(single.kind(), batch_kind);
    assert!(
        batch_msg.contains("--no-respect-robots"),
        "the escape hatch named must exist: {batch_msg}"
    );
}

#[test]
fn cached_pages_still_go_through_the_robots_gate() {
    // Serving from cache is not retroactive permission to have fetched it.
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ROBOTS_TXT))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/private/p"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });
    let url = format!("{}/private/p", server.uri());

    let dir = std::env::temp_dir().join(format!("webseek-robots-cache-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = Harness {
        cache: Arc::new(Mutex::new(Cache::load(dir.join("c.json"), 3600, 10))),
        ..Harness::new()
    };

    // Warm the cache with robots off, then ask again with robots on.
    assert!(fetch_one(&h.ctx(false), &url).is_ok());
    let blocked = fetch_one(&h.ctx(true), &url).unwrap_err();
    assert_eq!(blocked.kind(), "robots");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn workers_share_one_request_rate_instead_of_multiplying_it() {
    // `-j 4` should overlap latency, not make webseek four times ruder.
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });

    let urls: Vec<String> = (0..4).map(|i| format!("{}/p{i}", server.uri())).collect();
    let h = Harness::with_delay(Duration::from_millis(120));
    let start = Instant::now();
    let items = fetch_many(&h.ctx(false), &urls, 4);
    let elapsed = start.elapsed();

    assert_eq!(items.len(), 4);
    assert!(items.iter().all(|i| !i.is_err()));
    // 4 requests, 3 gaps of 120 ms; the first one is free.
    assert!(
        elapsed >= Duration::from_millis(300),
        "parallel workers bypassed pacing (took {elapsed:?})"
    );
}

#[test]
fn a_single_request_does_not_pay_the_delay() {
    // The pause belongs *between* requests. Sleeping after the only request a
    // process makes just made every invocation slower for nobody's benefit.
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });

    let h = Harness::with_delay(Duration::from_millis(1500));
    let url = format!("{}/only", server.uri());
    let start = Instant::now();
    fetch_one(&h.ctx(false), &url).unwrap();
    assert!(
        start.elapsed() < Duration::from_millis(500),
        "a lone request waited on the inter-request delay: {:?}",
        start.elapsed()
    );
}

#[test]
fn duplicate_urls_in_one_batch_reuse_the_cache() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/dup"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE_A))
            .mount(&server)
            .await;
    });

    let dir = std::env::temp_dir().join(format!("webseek-dup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let h = Harness {
        cache: Arc::new(Mutex::new(Cache::load(dir.join("c.json"), 3600, 10))),
        ..Harness::new()
    };
    let url = format!("{}/dup", server.uri());
    let items = fetch_many(&h.ctx(false), &[url.clone(), url], 1);
    assert_eq!(items.len(), 2);
    assert_eq!(unwrap_ok(&items[0]).text, unwrap_ok(&items[1]).text);
    let _ = std::fs::remove_dir_all(&dir);
}
