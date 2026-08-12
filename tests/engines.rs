//! Integration tests: engines against a mock HTTP server (wiremock).
//! No real network access — responses are fixtures served locally.
//!
//! Layout rule: async runtime is used ONLY for wiremock setup; engine calls
//! and the blocking reqwest client stay in the sync test thread (tokio >= 1.53
//! panics when a runtime is dropped inside an async context, and reqwest's
//! blocking client owns a runtime).

use reqwest::blocking::Client;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use webseek::engines::bing::Bing;
use webseek::engines::duckduckgo::DuckDuckGo;
use webseek::engines::images::{BingImages, DuckDuckGoImages};
use webseek::engines::wikipedia::Wikipedia;
use webseek::engines::{ImageEngine, SearchEngine};
use webseek::models::{ImageOpts, SearchOpts};

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

fn image_opts() -> ImageOpts {
    ImageOpts {
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
    let results = engine.search(&client(), "cats", &image_opts()).unwrap();

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
    let results = engine.search(&client(), "cats", &image_opts()).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].url, "https://cdn.example.com/ddg.jpg");
    assert_eq!(results[0].height, Some(200));
    assert_eq!(results[0].format, "jpg");
}

/// The browser-like header set is one of the two documented anti-bot levers,
/// and nothing verified it reached the wire — `build_client`'s own test only
/// checked that the builder returns Ok.
///
/// Asserted against the recorded request rather than with `header()`
/// matchers: wiremock splits comma-separated values, so a single
/// `accept-language: en-US,en;q=0.9` never matches as one value.
#[test]
fn browser_headers_reach_the_server() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
    });
    let client =
        webseek::http::build_client(std::time::Duration::from_secs(10), "webseek-header-test")
            .unwrap();
    let engine = DuckDuckGo::with_base(format!("{}/html/", server.uri()));
    assert_eq!(engine.search(&client, "rust", &opts()).unwrap().len(), 2);

    let requests = rt.block_on(server.received_requests()).unwrap();
    let sent = &requests.first().expect("one request was made").headers;
    for (name, expected) in [
        ("user-agent", "webseek-header-test"),
        ("accept-language", "en-US,en;q=0.9"),
        ("sec-ch-ua-mobile", "?0"),
        ("sec-ch-ua-platform", "\"Windows\""),
        ("upgrade-insecure-requests", "1"),
    ] {
        let got = sent
            .get(name)
            .unwrap_or_else(|| panic!("header {name} was never sent"));
        assert_eq!(got, expected, "header {name}");
    }
    assert!(
        sent.get("accept")
            .is_some_and(|v| v.to_str().unwrap().contains("text/html")),
        "the Accept header is part of the anti-bot lever too"
    );
}

/// Official APIs must receive webseek's honest identification, not the
/// browser string — the whole point of the user-agent policy.
#[test]
fn api_engines_identify_themselves_rather_than_impersonating_a_browser() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(header(
                "user-agent",
                "webseek/test (+contact: me@example.com)",
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"query":{"search":[{"title":"Rust","snippet":"s"}]}}"#),
            )
            .mount(&server)
            .await;
    });
    let opts = SearchOpts {
        api_user_agent: "webseek/test (+contact: me@example.com)".into(),
        ..Default::default()
    };
    let engine = Wikipedia::with_base(format!("{}/w/api.php", server.uri()));
    assert_eq!(
        engine.search(&client(), "rust", &opts).unwrap().len(),
        1,
        "the mock only answers requests carrying the API user-agent"
    );

    // The browser client default must not leak through on that path.
    let browser = webseek::http::build_client(
        std::time::Duration::from_secs(10),
        webseek::config::DEFAULT_USER_AGENT,
    )
    .unwrap();
    assert_eq!(engine.search(&browser, "rust", &opts).unwrap().len(), 1);
}

/// `Retry-After` is honoured in preference to our own backoff.
#[test]
fn retry_after_header_is_obeyed() {
    use std::time::Instant;

    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // Mounted FIRST: wiremock answers with the earliest matching mock, and
        // an exhausted `up_to_n_times` stops matching, so the 200 below picks
        // up the retry.
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "1"))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
    });
    let engine = DuckDuckGo::with_base(format!("{}/html/", server.uri()));
    let start = Instant::now();
    let results = engine.search(&client(), "rust", &opts()).unwrap();
    let waited = start.elapsed();

    assert_eq!(results.len(), 2, "the retry must eventually succeed");
    assert_eq!(
        rt.block_on(server.received_requests()).unwrap().len(),
        2,
        "the 503 must actually have been served, or this proves nothing"
    );
    // The default backoff for attempt 0 is jittered within [0, 400ms]; waiting
    // a full second is only explicable by the header.
    assert!(
        waited >= std::time::Duration::from_millis(900),
        "Retry-After was ignored (waited {waited:?})"
    );
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
        // Order matters: wiremock serves the first matching mock, so the
        // transient failure has to be mounted ahead of the success. Mounted
        // the other way round the 500 never fired at all and this test passed
        // without a single retry taking place.
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(500))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/html/"))
            .respond_with(ResponseTemplate::new(200).set_body_string(DDG_HTML))
            .mount(&server)
            .await;
    });
    let base = format!("{}/html/", server.uri());

    let engine = DuckDuckGo::with_base(base);
    let results = engine.search(&client(), "rust", &opts()).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].url, "https://example.com/1");
    assert_eq!(
        rt.block_on(server.received_requests()).unwrap().len(),
        2,
        "the retry path was never exercised"
    );
}
