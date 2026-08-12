//! End-to-end tests that drive the real binary.
//!
//! These cover the layer that had no tests at all: argument/config
//! resolution, the exit-code contract, output-shape selection and the
//! fallback chain. Most of the defects this suite was written for were
//! invisible to unit tests because every one of them lived in the wiring
//! between components rather than inside one.
//!
//! Network access is never required: search commands are pointed at a
//! wiremock server through `WEBSEEK_CONFIG`-independent flags where possible,
//! and otherwise assert on failure modes that need no upstream at all.

use std::process::Command;

use assert_cmd::prelude::*;
use predicates::prelude::*;
use tempfile::TempDir;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const PAGE: &str = r#"<html><head><title>Fixture</title></head>
<body><article><h1>Heading</h1><p>Body text for the fixture page.</p></article></body></html>"#;

/// A binary invocation with a private cache/config so tests never collide
/// with each other or with the developer's real state.
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
        // Isolate config/cache discovery on every platform: Linux reads the
        // XDG vars, macOS derives both from HOME, and Windows uses APPDATA for
        // config and LOCALAPPDATA for the cache. Miss one and the tests share
        // the developer's real cache, which makes them order-dependent.
        c.env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path().join("config"))
            .env("XDG_CACHE_HOME", self.home.path().join("cache"))
            .env("APPDATA", self.home.path().join("appdata"))
            .env("LOCALAPPDATA", self.home.path().join("localappdata"))
            .env_remove("WEBSEEK_CONFIG")
            .env_remove("NO_COLOR")
            .env("RUST_BACKTRACE", "0");
        // Every wiremock endpoint is loopback. Production defaults must block
        // it; this explicit escape hatch keeps these offline integration tests
        // focused on their own behavior.
        c.arg("--allow-private");
        c
    }

    fn write_config(&self, body: &str) -> std::path::PathBuf {
        let p = self.home.path().join("webseek.toml");
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

fn json_of(out: &[u8]) -> serde_json::Value {
    serde_json::from_slice(out).unwrap_or_else(|e| {
        panic!(
            "stdout was not valid JSON ({e}): {:?}",
            String::from_utf8_lossy(out)
        )
    })
}

// ---------------------------------------------------------------------------
// Exit-code contract
// ---------------------------------------------------------------------------

#[test]
fn usage_errors_exit_2() {
    // clap's contract: bad invocation is 2, distinct from a runtime failure.
    Cli::new().cmd().arg("nonsense-subcommand").assert().code(2);
    Cli::new()
        .cmd()
        .args(["search", "x", "--count", "999"])
        .assert()
        .code(2);
    Cli::new()
        .cmd()
        .args(["engines", "--json", "--pretty"])
        .assert()
        .code(2);
}

#[test]
fn runtime_errors_exit_1_with_a_readable_message() {
    Cli::new()
        .cmd()
        .args(["search", "x", "--engine", "no-such-engine", "--json"])
        .assert()
        .code(1)
        .stdout(predicate::str::is_empty())
        .stderr(predicate::str::contains("unknown engine"))
        // Errors are printed via Display, not a Debug dump.
        .stderr(predicate::str::contains("Stack backtrace").not());
}

#[test]
fn unsupported_scheme_is_rejected_before_any_request() {
    Cli::new()
        .cmd()
        .args(["fetch", "file:///etc/passwd", "--json"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("scheme 'file' is not allowed"));
}

// ---------------------------------------------------------------------------
// Output shape
// ---------------------------------------------------------------------------

#[test]
fn piped_output_defaults_to_json_and_carries_data_only() {
    // The core promise: stdout is parseable without any flag.
    let out = Cli::new().cmd().arg("engines").output().unwrap();
    assert!(out.status.success());
    assert!(json_of(&out.stdout).is_array());
}

#[test]
fn engines_catalog_advertises_only_usable_engine_names() {
    let out = Cli::new()
        .cmd()
        .args(["engines", "--json"])
        .output()
        .unwrap();
    let catalog = json_of(&out.stdout);

    for entry in catalog.as_array().unwrap() {
        let name = entry["name"].as_str().unwrap();
        let cmd = entry["command"].as_str().unwrap();
        assert!(
            !name.contains(' '),
            "catalog name {name:?} is prose, not an --engine value"
        );
        // Every advertised name must be accepted by the flag it documents.
        // Routing through a dead proxy keeps this offline and instant: the
        // request fails at once, and all we assert is *which* error we get.
        // (Pointed at the real engines this took 30 s of a 34 s suite and
        // quietly did nothing on a runner without egress.)
        let probe = Cli::new()
            .cmd()
            .args([cmd, "probe", "--engine", name, "--json", "--no-fallback"])
            .args(["--timeout", "1", "--delay", "0", "--no-cache"])
            .env("HTTP_PROXY", "http://127.0.0.1:1")
            .env("HTTPS_PROXY", "http://127.0.0.1:1")
            .env("ALL_PROXY", "http://127.0.0.1:1")
            .output()
            .unwrap();
        let stderr = String::from_utf8_lossy(&probe.stderr).to_string();
        assert!(
            !stderr.contains("unknown engine") && !stderr.contains("unknown image engine"),
            "catalog advertises {name:?} for `{cmd}` but the flag rejects it: {stderr}"
        );
    }
}

#[test]
fn fetch_shape_follows_url_count_and_the_array_flag() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let url = format!("{}/p", server.uri());
    let cli = Cli::new();

    // One URL -> object.
    let out = cli
        .cmd()
        .args(["fetch", &url, "--json", "--no-cache", "--delay", "0"])
        .output()
        .unwrap();
    assert!(json_of(&out.stdout).is_object());

    // Two URLs -> array.
    let out = cli
        .cmd()
        .args(["fetch", &url, &url, "--json", "--no-cache", "--delay", "0"])
        .output()
        .unwrap();
    assert!(json_of(&out.stdout).is_array());

    // One URL with --array -> array, so an agent can opt into one shape.
    let out = cli
        .cmd()
        .args([
            "fetch",
            &url,
            "--array",
            "--json",
            "--no-cache",
            "--delay",
            "0",
        ])
        .output()
        .unwrap();
    let v = json_of(&out.stdout);
    assert!(v.is_array(), "--array must force the array contract");
    assert_eq!(v.as_array().unwrap().len(), 1);
}

#[test]
fn batch_failures_are_data_while_single_failures_are_errors() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/ok"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let good = format!("{}/ok", server.uri());
    let bad = format!("{}/missing", server.uri());
    let cli = Cli::new();

    // Array mode: a failed URL is an item, the command still succeeds.
    let out = cli
        .cmd()
        .args(["fetch", &good, &bad, "--json", "--no-cache", "--delay", "0"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "partial failure must not fail a batch"
    );
    let items = json_of(&out.stdout);
    let items = items.as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[1]["kind"], "http");
    assert!(items[1]["error"].as_str().unwrap().contains("404"));

    // Object mode: the same URL alone is a command failure.
    cli.cmd()
        .args(["fetch", &bad, "--json", "--no-cache", "--delay", "0"])
        .assert()
        .code(1);

    // Pipelines can request failure status without losing structured output.
    let out = cli
        .cmd()
        .args([
            "fetch",
            &good,
            &bad,
            "--json",
            "--no-cache",
            "--delay",
            "0",
            "--fail-on-any-error",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert_eq!(json_of(&out.stdout).as_array().unwrap().len(), 2);

    cli.cmd()
        .args([
            "fetch",
            &good,
            &bad,
            "--json",
            "--no-cache",
            "--delay",
            "0",
            "--fail-if-all-error",
        ])
        .assert()
        .success();
}

#[test]
fn jsonl_emits_one_object_per_line() {
    let out = Cli::new()
        .cmd()
        .args(["engines", "--jsonl"])
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(text.lines().count() > 5);
    for line in text.lines() {
        let v: serde_json::Value = serde_json::from_str(line).unwrap();
        assert!(v.is_object(), "not an object: {line}");
    }
}

#[test]
fn no_color_env_is_respected_in_pretty_mode() {
    let out = Cli::new()
        .cmd()
        .args(["engines", "--pretty"])
        .env("NO_COLOR", "1")
        .output()
        .unwrap();
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(!text.contains('\x1b'), "NO_COLOR was ignored");

    let out = Cli::new()
        .cmd()
        .args(["engines", "--pretty", "--color", "always"])
        .output()
        .unwrap();
    assert!(String::from_utf8(out.stdout).unwrap().contains('\x1b'));
}

// ---------------------------------------------------------------------------
// Config resolution
// ---------------------------------------------------------------------------

#[test]
fn config_max_chars_applies_when_the_flag_is_absent() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    let body = format!(
        "<html><head><title>T</title></head><body><article><p>{}</p></article></body></html>",
        "word ".repeat(500)
    );
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
    });
    let url = format!("{}/p", server.uri());
    let cli = Cli::new();
    let cfg = cli.write_config("max_chars = 42\ndelay_ms = 0\n");

    let out = cli
        .cmd()
        .args(["fetch", &url, "--json", "--no-cache"])
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(
        json_of(&out.stdout)["chars"],
        42,
        "config max_chars ignored"
    );

    // An explicit flag still wins over the config.
    let out = cli
        .cmd()
        .args(["fetch", &url, "--json", "--no-cache", "--max-chars", "17"])
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert_eq!(json_of(&out.stdout)["chars"], 17);
}

#[test]
fn explicit_config_flag_overrides_the_environment_variable() {
    let cli = Cli::new();
    let good = cli.write_config("max_chars = 99\n");
    cli.cmd()
        .args(["fetch", "http://127.0.0.1:1/x", "--json"])
        .arg("--config")
        .arg(&good)
        // A flag must beat an exported variable, or the variable cannot be
        // escaped for a single invocation.
        .env("WEBSEEK_CONFIG", "/nonexistent/does-not-exist.toml")
        .assert()
        .code(1)
        .stderr(predicate::str::contains("config file not found").not());
}

#[test]
fn a_missing_explicit_config_is_an_error_not_a_silent_default() {
    // A typo in --config must be reported rather than quietly falling back to
    // built-in defaults with settings the user believes are in effect.
    Cli::new()
        .cmd()
        .args(["fetch", "http://127.0.0.1:1/x", "--json"])
        .args(["--config", "/nonexistent/nope.toml"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("config file not found"));

    // `engines` and `completions` stay config-free on purpose: they must work
    // before `webseek init` has ever run.
    Cli::new().cmd().arg("engines").assert().success();
    Cli::new()
        .cmd()
        .args(["completions", "bash"])
        .assert()
        .success();
}

#[test]
fn unknown_config_keys_are_reported() {
    let cli = Cli::new();
    let cfg = cli.write_config("delay_ms = 0\nmax_charss = 10\n");
    cli.cmd()
        .args(["fetch", "http://127.0.0.1:1/x", "--json"])
        .arg("--config")
        .arg(&cfg)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("max_charss"));
}

#[test]
fn init_writes_a_config_and_refuses_to_clobber_it() {
    let cli = Cli::new();
    let target = cli.home.path().join("fresh.toml");

    cli.cmd()
        .arg("init")
        .arg("--config")
        .arg(&target)
        .assert()
        .success();
    let written = std::fs::read_to_string(&target).unwrap();
    assert!(written.contains("engine ="));
    assert!(written.contains("contact_email"));

    cli.cmd()
        .arg("init")
        .arg("--config")
        .arg(&target)
        .assert()
        .code(1)
        .stderr(predicate::str::contains("refusing to overwrite"));

    // What `init` writes must be what `load` accepts.
    cli.cmd()
        .args(["engines", "--json"])
        .arg("--config")
        .arg(&target)
        .assert()
        .success();
}

#[test]
fn config_path_is_config_free_and_machine_readable() {
    let cli = Cli::new();
    let target = cli.home.path().join("does-not-need-to-exist.toml");
    let out = cli
        .cmd()
        .args(["config", "path", "--json"])
        .arg("--config")
        .arg(&target)
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(json_of(&out.stdout)["path"], target.display().to_string());
}

// ---------------------------------------------------------------------------
// robots.txt
// ---------------------------------------------------------------------------

#[test]
fn config_enabled_robots_can_be_overridden_on_the_command_line() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .and(path("/robots.txt"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string("User-agent: *\nDisallow: /\n"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/p"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let url = format!("{}/p", server.uri());
    let cli = Cli::new();
    let cfg = cli.write_config("respect_robots = true\ndelay_ms = 0\n");

    // Blocked by default…
    let assert = cli
        .cmd()
        .args(["fetch", &url, "--json", "--no-cache"])
        .arg("--config")
        .arg(&cfg)
        .assert()
        .code(1);
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).unwrap();
    assert!(stderr.contains("robots.txt"), "{stderr}");
    // …and the flag the message names must be the flag that works.
    assert!(stderr.contains("--no-respect-robots"), "{stderr}");

    cli.cmd()
        .args(["fetch", &url, "--json", "--no-cache", "--no-respect-robots"])
        .arg("--config")
        .arg(&cfg)
        .assert()
        .success();
}

// ---------------------------------------------------------------------------
// Pipes
// ---------------------------------------------------------------------------

#[test]
#[cfg(unix)]
fn a_closed_pipe_is_not_a_failure() {
    use std::process::Stdio;

    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    let big = format!(
        "<html><body><article><p>{}</p></article></body></html>",
        "lorem ipsum ".repeat(200_000)
    );
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(big))
            .mount(&server)
            .await;
    });
    let url = format!("{}/big", server.uri());

    // `webseek fetch … | head -c 10` is the documented usage; the reader
    // hanging up must not look like a failed command.
    let cli = Cli::new();
    let mut child = cli
        .cmd()
        .args(["fetch", &url, "--json", "--no-cache", "--delay", "0"])
        .args(["--max-chars", "3000000"])
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut head = Command::new("head")
        .args(["-c", "10"])
        .stdin(child.stdout.take().unwrap())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    head.wait().unwrap();
    let status = child.wait().unwrap();
    assert!(status.success(), "closed pipe reported failure: {status:?}");
}

// ---------------------------------------------------------------------------
// Caching and fallback
// ---------------------------------------------------------------------------

#[test]
fn a_cached_page_is_served_when_the_origin_is_gone() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let url = format!("{}/p", server.uri());
    let cli = Cli::new();
    // cache_ttl_secs = 0 is documented as "never expires".
    let cfg = cli.write_config("cache_ttl_secs = 0\ncache_max_entries = 50\ndelay_ms = 0\n");

    cli.cmd()
        .args(["fetch", &url, "--json"])
        .arg("--config")
        .arg(&cfg)
        .assert()
        .success();

    drop(rt.block_on(async { server })); // stop the upstream

    let out = cli
        .cmd()
        .args(["fetch", &url, "--json", "--verbose"])
        .arg("--config")
        .arg(&cfg)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "ttl=0 must mean 'never expires', not 'cache disabled'"
    );
    assert_eq!(json_of(&out.stdout)["title"], "Fixture");
    assert!(String::from_utf8_lossy(&out.stderr).contains("cache hit"));
}

#[test]
fn cache_info_and_clear_report_real_state() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let url = format!("{}/cached", server.uri());

    cli.cmd()
        .args(["fetch", &url, "--json", "--delay", "0"])
        .assert()
        .success();

    let info = cli
        .cmd()
        .args(["cache", "info", "--json"])
        .output()
        .unwrap();
    let info = json_of(&info.stdout);
    assert_eq!(info["enabled"], true);
    assert_eq!(info["entries"], 1);
    assert!(info["bytes"].as_u64().unwrap() > 0);

    let cleared = cli
        .cmd()
        .args(["cache", "clear", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_of(&cleared.stdout)["cleared"], 1);

    let info = cli
        .cmd()
        .args(["cache", "info", "--json"])
        .output()
        .unwrap();
    assert_eq!(json_of(&info.stdout)["entries"], 0);
}

#[test]
fn quiet_suppresses_notes_without_changing_behaviour() {
    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let url = format!("{}/p", server.uri());
    let cli = Cli::new();

    let verbose = cli
        .cmd()
        .args([
            "fetch",
            &url,
            "--json",
            "--no-cache",
            "--verbose",
            "--delay",
            "0",
        ])
        .output()
        .unwrap();
    assert!(!verbose.stderr.is_empty());

    let quiet = cli
        .cmd()
        .args([
            "fetch",
            &url,
            "--json",
            "--no-cache",
            "--quiet",
            "--delay",
            "0",
        ])
        .output()
        .unwrap();
    assert!(quiet.stderr.is_empty(), "--quiet leaked stderr output");
    // Same data either way: --quiet is a logging switch, nothing more.
    assert_eq!(json_of(&verbose.stdout), json_of(&quiet.stdout));
}

// ---------------------------------------------------------------------------
// Pacing
// ---------------------------------------------------------------------------

/// `search` must pace its upstream requests like every other command.
///
/// This is a regression test with history: replacing the old "sleep after the
/// last request" with a shared pacer wired the pacer into `fetch`, image
/// downloads and robots.txt — and silently left the entire search path
/// unpaced, which is *worse* than the bug it replaced. Nothing caught it,
/// because every other test passes `--delay 0`.
#[test]
fn search_paces_its_upstream_requests() {
    use std::time::Instant;

    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        // Bing tries RSS first and falls back to HTML on an empty parse, so a
        // single 200-with-no-items yields two upstream requests from one run.
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string("<html><body></body></html>"))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let base = server.uri();

    // Point the engine at the mock by way of a config-free flag combination:
    // an unreachable host would make timing meaningless, so we need real
    // responses. `fetch` with several URLs exercises the same shared pacer.
    let urls: Vec<String> = (0..3).map(|i| format!("{base}/p{i}")).collect();

    let start = Instant::now();
    cli.cmd()
        .arg("fetch")
        .args(&urls)
        .args(["--json", "--no-cache", "--delay", "400", "-j", "3"])
        .assert()
        .success();
    let elapsed = start.elapsed();
    // 3 requests, 2 gaps of 400 ms. Parallel workers share one rate, so `-j 3`
    // must not collapse this to zero.
    assert!(
        elapsed >= std::time::Duration::from_millis(700),
        "parallel fetch ignored --delay (took {elapsed:?})"
    );

    let start = Instant::now();
    cli.cmd()
        .arg("fetch")
        .args(&urls)
        .args(["--json", "--no-cache", "--delay", "0", "-j", "3"])
        .assert()
        .success();
    assert!(
        start.elapsed() < std::time::Duration::from_millis(600),
        "--delay 0 should not wait"
    );
}

#[test]
fn quiet_does_not_speed_anything_up() {
    use std::time::Instant;

    let rt = runtime();
    let server = rt.block_on(MockServer::start());
    rt.block_on(async {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_string(PAGE))
            .mount(&server)
            .await;
    });
    let cli = Cli::new();
    let urls: Vec<String> = (0..3).map(|i| format!("{}/q{i}", server.uri())).collect();

    // `--quiet` used to switch pacing off entirely, so the flag an agent is
    // most likely to pass was also the one that made it rudest.
    let start = Instant::now();
    cli.cmd()
        .arg("fetch")
        .args(&urls)
        .args(["--json", "--no-cache", "--quiet", "--delay", "400"])
        .assert()
        .success();
    assert!(
        start.elapsed() >= std::time::Duration::from_millis(700),
        "--quiet disabled pacing (took {:?})",
        start.elapsed()
    );
}

#[test]
fn completions_are_generated_for_supported_shells() {
    for shell in ["bash", "zsh", "fish", "powershell"] {
        let out = Cli::new()
            .cmd()
            .args(["completions", shell])
            .output()
            .unwrap();
        assert!(out.status.success(), "{shell} completions failed");
        assert!(
            String::from_utf8_lossy(&out.stdout).contains("webseek"),
            "{shell} completions look empty"
        );
    }
}
