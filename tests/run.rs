//! End-to-end tests for `webseek run` (YAML recipes).
//!
//! The real binary runs recipes whose engines point at wiremock servers
//! through `fxtwitter_base_url`, so no test touches the network. Config,
//! cache and recipe files all live in a private temp dir per test.

use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

const FX_HITS: &str = r#"{"code":200,"results":[
  {"id":"1111222233334444555","url":"https://x.com/alice/status/1111222233334444555",
   "text":"first post","likes":5,"reposts":1,"replies":0,"author":{"screen_name":"alice"}},
  {"id":"2222333344445555666","url":"https://x.com/bob/status/2222333344445555666",
   "text":"second post","likes":1,"reposts":0,"replies":0,"author":{"screen_name":"bob"}}
],"cursor":{"top":null,"bottom":null}}"#;

const PAGE: &str = r#"<html><head><title>Fixture page</title></head>
<body><article><p>Body text for the fixture page.</p></article></body></html>"#;

/// A binary invocation with a private home so tests never collide with each
/// other or with the developer's real config/cache.
struct Cli {
    home: TempDir,
}

impl Cli {
    fn new() -> Self {
        Self {
            home: TempDir::new().unwrap(),
        }
    }

    fn cmd(&self) -> Command {
        let mut c = Command::cargo_bin("webseek").unwrap();
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env("WEBSEEK_CACHE_DIR", self.home.path().join("webseek-cache"))
            .env("WEBSEEK_WATCH_DIR", self.home.path().join("webseek-watch"))
            .env("APPDATA", self.home.path().join("appdata"))
            .env("LOCALAPPDATA", self.home.path().join("localappdata"))
            .env_remove("WEBSEEK_CONFIG")
            .env_remove("NO_COLOR")
            .env("RUST_BACKTRACE", "0");
        // Wiremock endpoints are loopback; the explicit escape hatch keeps
        // these offline tests focused on recipe behavior, not egress policy.
        c.arg("--allow-private");
        c
    }

    fn write(&self, name: &str, body: &str) -> std::path::PathBuf {
        let p = self.home.path().join(name);
        std::fs::write(&p, body).unwrap();
        p
    }
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn fx_config(cli: &Cli, server: &MockServer) -> std::path::PathBuf {
    cli.write(
        "webseek.toml",
        &format!("fxtwitter_base_url = \"{}\"\n", server.uri()),
    )
}

#[test]
fn run_combines_search_steps_with_dedupe_and_limit() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        r#"version: 1
steps:
  - search: {engine: fxtwitter, query: "rust", count: 2}
  - search: {engine: fxtwitter, query: "tokio", count: 2}
combine: {dedupe_by: url, sort: url, limit: 2}
"#,
    );
    // Four raw hits collapse to [alice, bob]: dedupe drops the second pair,
    // sort orders by URL, limit keeps both.
    let assert = cli
        .cmd()
        .arg("--config")
        .arg(&config)
        .args(["run", "--json"])
        .arg(&recipe)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(doc["count"], serde_json::json!(2));
    let urls: Vec<&str> = doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap())
        .collect();
    assert_eq!(
        urls,
        [
            "https://x.com/alice/status/1111222233334444555",
            "https://x.com/bob/status/2222333344445555666"
        ]
    );
}

#[test]
fn run_unrolls_for_each_and_maps_fetch_steps() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // Distinct bodies per query prove the loop really ran twice: a
        // single execution could never produce both URLs.
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .and(query_param("q", "rust"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"code":200,"results":[
                  {"id":"1111222233334444555","url":"https://x.com/alice/status/1111222233334444555",
                   "text":"rust hit","author":{"screen_name":"alice"}}
                ],"cursor":{"top":null,"bottom":null}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .and(query_param("q", "tokio"))
            .respond_with(ResponseTemplate::new(200).set_body_string(
                r#"{"code":200,"results":[
                  {"id":"2222333344445555666","url":"https://x.com/bob/status/2222333344445555666",
                   "text":"tokio hit","author":{"screen_name":"bob"}}
                ],"cursor":{"top":null,"bottom":null}}"#,
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/page"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        &format!(
            r#"version: 1
steps:
  - for_each: {{items: ["rust", "tokio"], do: {{search: {{engine: fxtwitter, query: "${{item}}"}}}}}}
  - fetch: {{urls: ["{}/page"]}}
combine: {{dedupe_by: url}}
"#,
            server.uri()
        ),
    );
    let assert = cli
        .cmd()
        .arg("--config")
        .arg(&config)
        .args(["run", "--json"])
        .arg(&recipe)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&out).unwrap();
    // One hit per loop iteration plus the fetched page as a digest item.
    assert_eq!(doc["count"], serde_json::json!(3));
    let urls: Vec<&str> = doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap())
        .collect();
    assert!(urls.iter().any(|u| u.ends_with("/page")), "{urls:?}");
    let page = doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["url"].as_str().unwrap().ends_with("/page"))
        .unwrap();
    assert_eq!(page["title"], serde_json::json!("Fixture page"));
    assert!(page["snippet"].as_str().unwrap().contains("Body text"));
}

#[test]
fn run_file_sink_writes_nothing_to_stdout() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let sink = cli.home.path().join("out.jsonl");
    let recipe = cli.write(
        "flow.yaml",
        &format!(
            "version: 1\nsteps:\n  - search: {{engine: fxtwitter, query: x}}\noutput: {{format: jsonl, file: {}}}\n",
            sink.to_str().unwrap().replace('\\', "/")
        ),
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .arg(&recipe)
        .assert()
        .success()
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("wrote 2 result(s)"));
    let body = std::fs::read_to_string(&sink).unwrap();
    assert_eq!(body.lines().count(), 2);
    for line in body.lines() {
        assert!(serde_json::from_str::<serde_json::Value>(line)
            .unwrap()
            .is_object());
    }
}

#[test]
fn run_validation_error_names_the_step_and_sends_nothing() {
    // No mock server: an unknown engine must fail before any request exists.
    let cli = Cli::new();
    let config = cli.write("webseek.toml", "");
    let recipe = cli.write(
        "flow.yaml",
        "version: 1\nsteps:\n  - search: {engine: nope, query: x}\n",
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .arg(&recipe)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("step 1"))
        .stderr(predicate::str::contains("unknown engine 'nope'"));
}

#[test]
fn run_partial_failure_warns_and_continues() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // Specific mocks mount first: the plain path mock matches any query.
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .and(query_param("q", "fail"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        r#"version: 1
steps:
  - for_each: {items: ["fail", "ok"], do: {search: {engine: fxtwitter, query: "${item}"}}}
"#,
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .args(["run", "--json"])
        .arg(&recipe)
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""count":2"#))
        // Origin numbering: both clones are recipe step 1, never "step 2".
        .stderr(predicate::str::contains("step 1 failed"))
        .stderr(predicate::str::contains("step 2").not());
}

#[test]
fn run_all_steps_failed_is_an_error() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        "version: 1\nsteps:\n  - search: {engine: fxtwitter, query: x}\n",
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .arg(&recipe)
        .assert()
        .failure()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("all 1 step(s) failed"));
}

#[test]
fn run_recipe_format_applies_without_cli_flag() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        "version: 1\nsteps:\n  - search: {engine: fxtwitter, query: x}\noutput: {format: jsonl}\n",
    );
    // Piped stdout would auto-select JSON; the recipe asks for JSONL instead.
    let assert = cli
        .cmd()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .arg(&recipe)
        .assert()
        .success();
    let out = String::from_utf8(assert.get_output().stdout.clone()).unwrap();
    assert_eq!(out.lines().count(), 2);
    for line in out.lines() {
        assert!(serde_json::from_str::<serde_json::Value>(line)
            .unwrap()
            .is_object());
    }
}

#[test]
fn run_cli_flag_overrides_recipe_format() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        "version: 1\nsteps:\n  - search: {engine: fxtwitter, query: x}\noutput: {format: jsonl}\n",
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .args(["run", "--json"])
        .arg(&recipe)
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""count":2"#));
}

#[test]
fn run_file_sink_defaults_to_json() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let sink = cli.home.path().join("out.json");
    let recipe = cli.write(
        "flow.yaml",
        &format!(
            "version: 1\nsteps:\n  - search: {{engine: fxtwitter, query: x}}\noutput: {{file: {}}}\n",
            sink.to_str().unwrap().replace('\\', "/")
        ),
    );
    cli.cmd()
        .arg("--config")
        .arg(&config)
        .arg("run")
        .arg(&recipe)
        .assert()
        .success()
        .stdout(predicate::str::is_empty());
    let body = std::fs::read_to_string(&sink).unwrap();
    let doc: serde_json::Value = serde_json::from_str(&body).unwrap();
    assert_eq!(doc["count"], serde_json::json!(2));
}

#[test]
fn run_reads_stdin_with_dash() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    // `write_stdin` lives on assert_cmd's Command wrapper, not std's.
    let mut c = assert_cmd::Command::cargo_bin("webseek").unwrap();
    c.env("HOME", cli.home.path())
        .env("XDG_CONFIG_HOME", cli.home.path().join("config"))
        .env("XDG_CACHE_HOME", cli.home.path().join("cache"))
        .env("WEBSEEK_CACHE_DIR", cli.home.path().join("webseek-cache"))
        .env("WEBSEEK_WATCH_DIR", cli.home.path().join("webseek-watch"))
        .env_remove("WEBSEEK_CONFIG")
        .env_remove("NO_COLOR")
        .env("RUST_BACKTRACE", "0");
    c.arg("--allow-private")
        .arg("--config")
        .arg(&config)
        .args(["run", "--json", "-"])
        .write_stdin("version: 1\nsteps:\n  - search: {engine: fxtwitter, query: x, count: 1}\n")
        .assert()
        .success()
        .stdout(predicate::str::contains(r#""count":1"#));
}

// ---------------------------------------------------------------------------
// --watch
// ---------------------------------------------------------------------------

const FX_THIRD: &str = r#"{"code":200,"results":[
  {"id":"3333444455556666777","url":"https://x.com/carol/status/3333444455556666777",
   "text":"third post","likes":0,"reposts":0,"replies":0,"author":{"screen_name":"carol"}},
  {"id":"1111222233334444555","url":"https://x.com/alice/status/1111222233334444555",
   "text":"first post","likes":5,"reposts":1,"replies":0,"author":{"screen_name":"alice"}}
],"cursor":{"top":null,"bottom":null}}"#;

fn urls_of(stdout: &[u8]) -> Vec<String> {
    let doc: serde_json::Value = serde_json::from_slice(stdout).unwrap();
    doc["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["url"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn search_watch_emits_only_new_urls_and_bypasses_the_cache() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    let mount = |body: &'static str| {
        rt.block_on(async {
            server.reset().await;
            Mock::given(method("GET"))
                .and(path("/2/search"))
                .respond_with(ResponseTemplate::new(200).set_body_string(body))
                .mount(&server)
                .await;
        })
    };
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let search = || {
        let out = cli
            .cmd()
            .arg("--config")
            .arg(&config)
            .args(["search", "rust", "--engine", "fxtwitter", "--json"])
            .args(["--delay", "0", "--watch", "fx-rust"])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        urls_of(&out.stdout)
    };

    mount(FX_HITS);
    assert_eq!(
        search().len(),
        2,
        "first run is the baseline: everything is new"
    );
    assert!(search().is_empty(), "nothing new on an unchanged upstream");

    // Upstream changes; the stale cached answer must not hide it.
    mount(FX_THIRD);
    assert_eq!(
        search(),
        vec!["https://x.com/carol/status/3333444455556666777".to_string()]
    );

    // `watch list` reports the remembered URLs; `clear` resets the baseline.
    let out = cli
        .cmd()
        .args(["watch", "list", "--json"])
        .output()
        .unwrap();
    let list: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(list[0]["name"], serde_json::json!("fx-rust"));
    assert_eq!(list[0]["seen"], serde_json::json!(3));
    cli.cmd()
        .args(["watch", "clear", "fx-rust", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"{"cleared":1}"#));
    assert_eq!(search().len(), 2);
}

#[test]
fn watch_names_are_validated_and_clear_needs_a_target() {
    let cli = Cli::new();
    cli.cmd()
        .args([
            "search",
            "rust",
            "--engine",
            "fxtwitter",
            "--watch",
            "../etc",
        ])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("invalid watch name"));
    cli.cmd().args(["watch", "clear"]).assert().code(2);
    cli.cmd()
        .args(["watch", "clear", "--all", "--json"])
        .assert()
        .success()
        .stdout(predicate::str::contains(r#"{"cleared":0}"#));
}

#[test]
fn run_watch_limit_counts_new_items_and_leaves_the_rest_for_later() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/2/search"))
            .respond_with(ResponseTemplate::new(200).set_body_string(FX_HITS))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let config = fx_config(&cli, &server);
    let recipe = cli.write(
        "flow.yaml",
        r#"version: 1
watch: digest
steps:
  - search: {engine: fxtwitter, query: "rust", count: 2}
combine: {limit: 1}
"#,
    );
    let run = || {
        let out = cli
            .cmd()
            .arg("--config")
            .arg(&config)
            .args(["run", "--json", "--delay", "0"])
            .arg(&recipe)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        urls_of(&out.stdout)
    };
    // The item `limit` cut on the first run is still new on the second.
    assert_eq!(
        run(),
        vec!["https://x.com/alice/status/1111222233334444555"]
    );
    assert_eq!(run(), vec!["https://x.com/bob/status/2222333344445555666"]);
    assert!(run().is_empty());
}
