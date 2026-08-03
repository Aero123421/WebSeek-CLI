//! Integration tests for the stable-source (vertical) engines against a mock
//! HTTP server (wiremock). No real network. Same runtime rule as `engines.rs`:
//! the async runtime is used only for wiremock setup; engine calls run sync.

use std::time::Duration;

use reqwest::blocking::Client;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use webseek::engines::academic::{CrossRef, OpenAlex, PubMed};
use webseek::engines::hackernews::HackerNews;
use webseek::engines::nominatim::Nominatim;
use webseek::engines::packages::{Crates, Npm, PyPi};
use webseek::engines::reddit::Reddit;
use webseek::engines::stackexchange::StackExchange;
use webseek::engines::wikipedia::Wikipedia;
use webseek::engines::SearchEngine;
use webseek::models::SearchOpts;

fn client() -> Client {
    Client::builder()
        .user_agent("webseek-test")
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap()
}

fn opts() -> SearchOpts {
    SearchOpts {
        count: 5,
        lang: None,
        region: None,
        safe: false,
    }
}

fn setup_runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn wikipedia_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/w/api.php"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"query":{"search":[{"title":"Rust","snippet":"a <span>systems</span> language"}]}}"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = Wikipedia::with_base(format!("{}/w/api.php", server.uri()));
    let r = engine.search(&client(), "rust", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].title, "Rust");
    assert_eq!(r[0].snippet, "a systems language");
}

#[test]
fn hackernews_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"hits":[{"objectID":"7","title":"Show HN","url":"https://x.dev","points":3,"num_comments":1,"author":"a"}]}"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = HackerNews::with_base(format!("{}/search", server.uri()));
    let r = engine.search(&client(), "x", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].url, "https://x.dev");
}

#[test]
fn openalex_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"results":[{"id":"https://openalex.org/W1","display_name":"Paper","publication_year":2020,"cited_by_count":5}]}"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = OpenAlex::with_base(format!("{}/works", server.uri()));
    let r = engine.search(&client(), "x", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].title, "Paper");
    assert_eq!(r[0].snippet, "2020 · cited 5");
}

#[test]
fn crossref_engine_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/works"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"message":{"items":[{"DOI":"10.1/x","title":["T"],"URL":"https://doi.org/10.1/x","container-title":["J"],"published":{"date-parts":[[2019]]},"is-referenced-by-count":2}]}}"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = CrossRef::with_base(format!("{}/works", server.uri()));
    let r = engine.search(&client(), "x", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].snippet, "J · 2019 · cited 2");
}

#[test]
fn pubmed_engine_two_step_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/esearch"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"esearchresult":{"idlist":["11"]}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/esummary"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"result":{"uids":["11"],"11":{"uid":"11","title":"A study","source":"Nature","pubdate":"2021"}}}"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = PubMed::with_bases(
        format!("{}/esearch", server.uri()),
        format!("{}/esummary", server.uri()),
    );
    let r = engine.search(&client(), "x", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].title, "A study");
    assert_eq!(r[0].url, "https://pubmed.ncbi.nlm.nih.gov/11/");
}

#[test]
fn package_engines_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/crates"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"crates":[{"id":"tokio","description":"Async","downloads":9,"max_version":"1.0"}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/npm"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"objects":[{"package":{"name":"async","version":"3.0","description":"Utils"}}]}"#,
            ))
            .mount(&server)
            .await;
    });
    let crates = Crates::with_base(format!("{}/crates", server.uri()));
    let r = crates.search(&client(), "async", &opts()).unwrap();
    assert_eq!(r[0].title, "tokio");

    let npm = Npm::with_base(format!("{}/npm", server.uri()));
    let r = npm.search(&client(), "async", &opts()).unwrap();
    assert_eq!(r[0].title, "async");
}

#[test]
fn pypi_404_is_empty_not_error() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404))
            .mount(&server)
            .await;
    });
    let engine = PyPi::with_base(server.uri());
    let r = engine.search(&client(), "nope", &opts()).unwrap();
    assert!(
        r.is_empty(),
        "404 should map to empty results, not an error"
    );
}

#[test]
fn stackexchange_and_nominatim_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/se"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"items":[{"title":"Q &amp; A","link":"https://so/q/1","tags":["rust"],"score":5,"answer_count":2,"is_answered":true}]}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/geo"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"[{"osm_type":"node","osm_id":1,"lat":"1.0","lon":"2.0","type":"city","display_name":"Town"}]"#,
            ))
            .mount(&server)
            .await;
    });
    let se = StackExchange::with_base(format!("{}/se", server.uri()));
    let r = se.search(&client(), "x", &opts()).unwrap();
    assert_eq!(r[0].title, "Q & A");
    assert_eq!(r[0].snippet, "[rust] · score 5 · 2 answers ✓");

    let geo = Nominatim::with_base(format!("{}/geo", server.uri()));
    let r = geo.search(&client(), "town", &opts()).unwrap();
    assert_eq!(r[0].title, "Town");
    assert_eq!(r[0].url, "https://www.openstreetmap.org/node/1");
}

#[test]
fn reddit_atom_full_pipeline() {
    let rt = setup_runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/search.rss"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"<feed><entry>
                  <author><name>/u/bob</name></author>
                  <category term="rust" label="r/rust"/>
                  <content type="html">&lt;p&gt;Hello &amp;amp; world&lt;/p&gt;</content>
                  <link href="https://www.reddit.com/r/rust/comments/z/t/"/>
                  <title>Rust &amp; things</title>
                </entry></feed>"#,
            ))
            .mount(&server)
            .await;
    });
    let engine = Reddit::with_base(format!("{}/search.rss", server.uri()));
    let r = engine.search(&client(), "rust", &opts()).unwrap();
    assert_eq!(r.len(), 1);
    assert_eq!(r[0].title, "Rust & things");
    assert_eq!(r[0].url, "https://www.reddit.com/r/rust/comments/z/t/");
    assert!(r[0].snippet.contains("Hello & world"));
}
