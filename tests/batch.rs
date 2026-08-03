//! Integration tests for batch fetching and robots.txt gating (wiremock).
//! No real network access — all responses are local fixtures.
//!
//! Same runtime rule as `engines.rs`: the async runtime is used ONLY for
//! wiremock setup; the blocking reqwest client and `fetch_many` run in the
//! sync test thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reqwest::blocking::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use webseek::batch::{fetch_many, BatchItem};
use webseek::cache::Cache;
use webseek::models::{FetchOpts, FetchResult};
use webseek::robots::RobotsChecker;

const PAGE_A: &str = r#"<html><head><title>Page A</title></head>
<body><article><h1>Heading A</h1><p>Alpha content here.</p></article></body></html>"#;

const PAGE_B: &str = r#"<html><head><title>Page B</title></head>
<body><article><h1>Heading B</h1><p>Beta content here.</p></article></body></html>"#;

const ROBOTS_TXT: &str = "User-agent: *\nDisallow: /private/\n";

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
        max_chars: 20_000,
        raw_html: false,
        markdown: false,
    }
}

fn disabled_cache() -> Arc<Mutex<Cache>> {
    Arc::new(Mutex::new(Cache::disabled()))
}

fn checker() -> Arc<Mutex<RobotsChecker>> {
    Arc::new(Mutex::new(RobotsChecker::new()))
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
        BatchItem::Err { url, error } => panic!("expected Ok, got Err for {url}: {error}"),
    }
}

fn unwrap_err(item: &BatchItem) -> &str {
    match item {
        BatchItem::Err { error, .. } => error.as_str(),
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
    let items = fetch_many(
        &client(),
        &urls,
        &fetch_opts(),
        2,
        Duration::ZERO,
        &disabled_cache(),
        &checker(),
        false,
    );

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
    let items = fetch_many(
        &client(),
        &urls,
        &fetch_opts(),
        2,
        Duration::ZERO,
        &disabled_cache(),
        &checker(),
        false,
    );

    assert_eq!(items.len(), 2);
    // Order preserved: good first, error second.
    assert!(unwrap_ok(&items[0]).url.ends_with("/good"));
    let err = unwrap_err(&items[1]);
    assert!(err.contains("404"), "unexpected error: {err}");
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
    let items = fetch_many(
        &client(),
        &urls,
        &fetch_opts(),
        2,
        Duration::ZERO,
        &disabled_cache(),
        &checker(),
        true, // respect_robots
    );

    assert_eq!(items.len(), 2);
    let err = unwrap_err(&items[0]);
    assert!(err.contains("robots.txt"), "unexpected error: {err}");
    assert!(unwrap_ok(&items[1]).url.ends_with("/public"));
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
    let items = fetch_many(
        &client(),
        &urls,
        &fetch_opts(),
        1,
        Duration::ZERO,
        &disabled_cache(),
        &checker(),
        false, // respect_robots off -> disallowed path is fetched anyway
    );

    assert_eq!(items.len(), 1);
    assert!(unwrap_ok(&items[0]).url.ends_with("/private/secret"));
}
