//! Integration tests: engines against a mock HTTP server (wiremock).
//! No real network access — responses are fixtures served locally.
//!
//! Layout rule: async runtime is used ONLY for wiremock setup; engine calls
//! and the blocking reqwest client stay in the sync test thread (tokio >= 1.53
//! panics when a runtime is dropped inside an async context, and reqwest's
//! blocking client owns a runtime).

use reqwest::blocking::Client;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use webseek::engines::bing::Bing;
use webseek::engines::duckduckgo::DuckDuckGo;
use webseek::engines::images::{BingImages, DuckDuckGoImages};
use webseek::engines::{ImageEngine, SearchEngine};
use webseek::models::SearchOpts;

const DDG_HTML: &str = r#"<html><body>
  <div class="result">
    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F1">First hit</a>
    <a class="result__snippet" href="x">snippet one</a>
  </div>
  <div class="result">
    <a class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2F2">Second hit</a>
  </div>
</body></html>"#;

const BING_HTML: &str = r#"<html><body><ol id="b_results">
  <li class="b_algo">
    <h2><a href="https://www.bing.com/ck/a?u=aHR0cHM6Ly9leGFtcGxlLmNvbS8x">Bing hit one</a></h2>
    <div class="b_caption"><p>bing snippet</p></div>
  </li>
</ol></body></html>"#;

const BING_IMAGES_HTML: &str = r#"<html><body>
  <a class="iusc" m="{&quot;murl&quot;:&quot;https://cdn.example.com/pic.jpg&quot;,&quot;purl&quot;:&quot;https://page.example.com&quot;,&quot;murlw&quot;:640,&quot;murlh&quot;:480}">
    <img alt="Mock photo">
  </a>
</body></html>"#;

const DDG_IMAGES_PAGE: &str = r#"<html><body><script>var vqd="tok-123";</script></body></html>"#;
const DDG_IMAGES_JSON: &str = r#"{"results":[
  {"image":"https://cdn.example.com/ddg.jpg","title":"DDG pic","url":"https://page.example.com/ddg","width":100,"height":200}
]}"#;

fn client() -> Client {
    Client::builder()
        .user_agent("webseek-test")
        .timeout(std::time::Duration::from_secs(10))
        .build()
        .unwrap()
}

fn opts() -> SearchOpts {
    SearchOpts {
        count: 5,
        ..Default::default()
    }
}

/// Short-lived current-thread runtime for wiremock setup only.
fn setup_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn duckduckgo_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/html/"))
            .and(query_param("q", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
    });
    let base = format!("{}/html/", server.uri());

    let engine = DuckDuckGo::with_base(base);
    let results = engine.search(&client(), "rust", &opts()).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "First hit");
    assert_eq!(results[0].url, "https://example.com/1");
    assert_eq!(results[0].snippet, "snippet one");
    assert_eq!(results[1].url, "https://example.com/2");
}

#[test]
fn duckduckgo_engine_count_and_ratelimit() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(202))
            .mount(&server)
            .await;
    });
    let base = format!("{}/html/", server.uri());

    let engine = DuckDuckGo::with_base(base);
    let err = engine.search(&client(), "rust", &opts()).unwrap_err();
    assert!(
        err.to_string().contains("rate-limited"),
        "unexpected error: {err}"
    );
}

#[test]
fn bing_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("q", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BING_HTML))
            .mount(&server)
            .await;
    });
    let base = format!("{}/search", server.uri());

    let engine = Bing::with_base(base);
    let results = engine.search(&client(), "rust", &opts()).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].title, "Bing hit one");
    assert_eq!(results[0].url, "https://example.com/1");
    assert_eq!(results[0].snippet, "bing snippet");
}

#[test]
fn bing_images_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/images/search"))
            .and(query_param("q", "cats"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BING_IMAGES_HTML))
            .mount(&server)
            .await;
    });
    let base = format!("{}/images/search", server.uri());

    let engine = BingImages::with_base(base);
    let results = engine.search(&client(), "cats", 5, false).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].url, "https://cdn.example.com/pic.jpg");
    assert_eq!(results[0].width, Some(640));
    assert_eq!(results[0].height, Some(480));
    assert_eq!(results[0].format, "jpg");
}

#[test]
fn duckduckgo_images_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/"))
            .and(query_param("iax", "images"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_IMAGES_PAGE))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/i.js"))
            .and(query_param("vqd", "tok-123"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_IMAGES_JSON))
            .mount(&server)
            .await;
    });
    let json_base = format!("{}/i.js", server.uri());
    let page_base = server.uri();

    let engine = DuckDuckGoImages::with_bases(page_base, json_base);
    let results = engine.search(&client(), "cats", 5, false).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].url, "https://cdn.example.com/ddg.jpg");
    assert_eq!(results[0].height, Some(200));
    assert_eq!(results[0].format, "jpg");
}

#[test]
fn engine_traits_are_object_safe_and_named() {
    let engines: Vec<Box<dyn SearchEngine>> =
        vec![Box::<DuckDuckGo>::default(), Box::<Bing>::default()];
    assert_eq!(engines[0].name(), "duckduckgo");
    assert_eq!(engines[1].name(), "bing");
    let images: Vec<Box<dyn ImageEngine>> = vec![
        Box::<BingImages>::default(),
        Box::<DuckDuckGoImages>::default(),
    ];
    assert_eq!(images[0].name(), "bing");
    assert_eq!(images[1].name(), "duckduckgo");
}

const BING_RSS: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0"><channel>
  <title>Bing: rust</title><link>http://www.bing.com/search?q=rust</link>
  <item><title>RSS hit one</title><link>https://example.com/rss1</link><description>rss snippet</description></item>
  <item><title>RSS hit two</title><link>https://example.com/rss2</link><description>two</description></item>
</channel></rss>"#;

#[test]
fn bing_engine_prefers_rss_endpoint() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // Only the RSS endpoint is mocked; if the engine wrongly used the HTML
        // path it would get a 404 and fail, so a successful parse proves RSS use.
        Mock::given(method("GET"))
            .and(path("/search"))
            .and(query_param("format", "rss"))
            .respond_with(ResponseTemplate::new(200).set_body_string(BING_RSS))
            .mount(&server)
            .await;
    });
    let base = format!("{}/search", server.uri());

    let engine = Bing::with_base(base);
    let results = engine.search(&client(), "rust", &opts()).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].title, "RSS hit one");
    assert_eq!(results[0].url, "https://example.com/rss1");
    assert_eq!(results[0].snippet, "rss snippet");
}

#[test]
fn search_recovers_after_transient_500_via_retry() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // 200 for all requests, but a higher-precedence 500 mock answers the
        // first request only; the retry loop must ride it out and succeed.
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
    });
    let base = format!("{}/html/", server.uri());

    let engine = DuckDuckGo::with_base(base);
    let results = engine.search(&client(), "rust", &opts()).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, "https://example.com/1");
}
