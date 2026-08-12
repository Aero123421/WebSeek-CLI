//! Output layer.
//!
//! Context-saving rules (the core requirement for AI agents):
//! - **stdout carries data only.** Progress/notes go to stderr.
//! - **Default mode is JSON when stdout is not a TTY** (piped into an agent);
//!   pretty text only when a human is watching.
//! - JSON keys are short and stable; nothing is printed before/after the JSON
//!   document, so it can be parsed directly.
//! - **JSONL is one JSON *object* per line** — never a bare array or scalar,
//!   so a line-by-line reader never has to special-case a line.
//! - Snippets/text are pre-capped, so token cost is bounded.
//!
//! Every writer has a `*_to(w, ...)` variant taking any `Write`, which keeps
//! the JSON contract unit-testable (golden tests).

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};

use serde::Serialize;

use crate::batch::BatchItem;
use crate::cli::ColorChoice;
use crate::engines::EngineInfo;
use crate::models::{FetchResult, ImageResult, SearchResult};

/// Set once at startup; suppresses notes and warnings on stderr.
static QUIET: AtomicBool = AtomicBool::new(false);
/// Set once at startup; whether pretty output may emit ANSI escapes.
static COLOR: AtomicBool = AtomicBool::new(false);

/// Configure stderr verbosity. Applies to *every* stderr channel, including
/// cache warnings and batch failures, so `--quiet` really is quiet.
pub fn set_quiet(quiet: bool) {
    QUIET.store(quiet, Ordering::Relaxed);
}

pub fn is_quiet() -> bool {
    QUIET.load(Ordering::Relaxed)
}

/// Resolve the colour policy once, honouring `NO_COLOR` (no-color.org).
pub fn set_color(choice: ColorChoice) {
    let enabled = match choice {
        ColorChoice::Always => true,
        ColorChoice::Never => false,
        ColorChoice::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    };
    COLOR.store(enabled, Ordering::Relaxed);
}

fn color_enabled() -> bool {
    COLOR.load(Ordering::Relaxed)
}

/// Wrap `s` in an ANSI sequence, or return it untouched when colour is off.
fn paint(s: &str, code: &str) -> String {
    if color_enabled() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// JSON document on stdout.
    Json,
    /// One JSON object per line (streaming-friendly).
    Jsonl,
    /// Human-friendly text with light ANSI colors.
    Pretty,
}

impl Mode {
    /// Auto mode: machines get JSON, humans get pretty text.
    pub fn auto() -> Self {
        if std::io::stdout().is_terminal() {
            Mode::Pretty
        } else {
            Mode::Json
        }
    }
}

/// A fully-formed JSON document written by `search`.
#[derive(Serialize)]
struct SearchDoc<'a> {
    query: &'a str,
    engine: &'a str,
    count: usize,
    results: &'a [SearchResult],
}

#[derive(Serialize)]
struct ImageDoc<'a> {
    query: &'a str,
    engine: &'a str,
    count: usize,
    results: &'a [ImageResult],
    #[serde(skip_serializing_if = "Option::is_none")]
    downloaded: Option<Vec<String>>,
}

/// JSONL envelope for the download list, so every JSONL line is an object.
#[derive(Serialize)]
struct DownloadedLine<'a> {
    downloaded: &'a [String],
}

pub fn write_search(
    mode: Mode,
    query: &str,
    engine: &str,
    results: &[SearchResult],
) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_search_to(&mut out.lock(), mode, query, engine, results)
}

pub fn write_search_to(
    w: &mut impl Write,
    mode: Mode,
    query: &str,
    engine: &str,
    results: &[SearchResult],
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            let doc = SearchDoc {
                query,
                engine,
                count: results.len(),
                results,
            };
            writeln!(w, "{}", serde_json::to_string(&doc)?)?;
        }
        Mode::Jsonl => {
            for r in results {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
        }
        Mode::Pretty => {
            if results.is_empty() {
                writeln!(w, "No results for \"{query}\" (engine: {engine}).")?;
                return Ok(());
            }
            for (i, r) in results.iter().enumerate() {
                writeln!(w, "{}", paint(&format!("{:>2}. {}", i + 1, r.title), "1"))?;
                writeln!(w, "     {}", paint(&r.url, "36"))?;
                if !r.snippet.is_empty() {
                    writeln!(w, "     {}", r.snippet)?;
                }
            }
        }
    }
    Ok(())
}

pub fn write_images(
    mode: Mode,
    query: &str,
    engine: &str,
    results: &[ImageResult],
    downloaded: Option<Vec<String>>,
) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_images_to(&mut out.lock(), mode, query, engine, results, downloaded)
}

pub fn write_images_to(
    w: &mut impl Write,
    mode: Mode,
    query: &str,
    engine: &str,
    results: &[ImageResult],
    downloaded: Option<Vec<String>>,
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            let doc = ImageDoc {
                query,
                engine,
                count: results.len(),
                results,
                downloaded,
            };
            writeln!(w, "{}", serde_json::to_string(&doc)?)?;
        }
        Mode::Jsonl => {
            for r in results {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
            if let Some(dl) = &downloaded {
                // Wrapped in an object: a bare `["a.jpg"]` line would break the
                // "every JSONL line is an object" contract.
                writeln!(
                    w,
                    "{}",
                    serde_json::to_string(&DownloadedLine { downloaded: dl })?
                )?;
            }
        }
        Mode::Pretty => {
            for (i, r) in results.iter().enumerate() {
                writeln!(w, "{}", paint(&format!("{:>2}. {}", i + 1, r.title), "1"))?;
                writeln!(w, "     {}", paint(&r.url, "36"))?;
                let dims = match (r.width, r.height) {
                    (Some(w_), Some(h)) => format!("{w_}x{h}"),
                    _ => "?".into(),
                };
                let fmt = if r.format.is_empty() { "?" } else { &r.format };
                writeln!(w, "     [{fmt}] {dims}  page: {}", r.page_url)?;
            }
            if let Some(dl) = downloaded {
                if !dl.is_empty() {
                    writeln!(w, "Downloaded {} file(s):", dl.len())?;
                    for p in &dl {
                        writeln!(w, "  {}", paint(p, "32"))?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn write_fetch(mode: Mode, fetch: &FetchResult) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_fetch_to(&mut out.lock(), mode, fetch)
}

pub fn write_fetch_to(w: &mut impl Write, mode: Mode, fetch: &FetchResult) -> std::io::Result<()> {
    match mode {
        Mode::Json | Mode::Jsonl => {
            writeln!(w, "{}", serde_json::to_string(fetch)?)?;
        }
        Mode::Pretty => {
            if let Some(title) = &fetch.title {
                writeln!(w, "{}", paint(title, "1"))?;
            }
            writeln!(w, "{}", paint(&fetch.url, "36"))?;
            writeln!(
                w,
                "{} chars{}",
                fetch.chars,
                if fetch.truncated { " (truncated)" } else { "" }
            )?;
            writeln!(w, "---")?;
            writeln!(w, "{}", fetch.text)?;
        }
    }
    Ok(())
}

/// Batch output contract:
/// - `Json`: a single JSON **array** of per-URL items (each item is either a
///   `FetchResult` or `{"url": ..., "error": ...}`), preserving input order.
/// - `Jsonl`: one item object per line.
/// - `Pretty`: fetched pages on stdout; failures as notes on stderr.
pub fn write_fetch_batch(mode: Mode, items: &[BatchItem]) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_fetch_batch_to(&mut out.lock(), mode, items)
}

pub fn write_fetch_batch_to(
    w: &mut impl Write,
    mode: Mode,
    items: &[BatchItem],
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            writeln!(w, "{}", serde_json::to_string(items)?)?;
        }
        Mode::Jsonl => {
            for item in items {
                writeln!(w, "{}", serde_json::to_string(item)?)?;
            }
        }
        Mode::Pretty => {
            for item in items {
                match item {
                    BatchItem::Ok(fetch) => write_fetch_to(w, mode, fetch)?,
                    BatchItem::Err { url, error, .. } => warn(&format!("{url}: {error}")),
                }
            }
        }
    }
    Ok(())
}

/// Engine catalog output (for `webseek engines`).
pub fn write_engines(mode: Mode, engines: &[EngineInfo]) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_engines_to(&mut out.lock(), mode, engines)
}

pub fn write_engines_to(
    w: &mut impl Write,
    mode: Mode,
    engines: &[EngineInfo],
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            writeln!(w, "{}", serde_json::to_string(engines)?)?;
        }
        Mode::Jsonl => {
            for e in engines {
                writeln!(w, "{}", serde_json::to_string(e)?)?;
            }
        }
        Mode::Pretty => {
            for e in engines {
                let alias = if e.aliases.is_empty() {
                    String::new()
                } else {
                    format!(" (aliases: {})", e.aliases.join(", "))
                };
                writeln!(
                    w,
                    "{} [{}] {}{}",
                    paint(&format!("{:<14}", e.name), "1"),
                    paint(&format!("{:<12}", e.kind), "36"),
                    e.description,
                    alias
                )?;
                writeln!(w, "    {}", paint(e.example, "2"))?;
            }
        }
    }
    Ok(())
}

/// Progress note for stderr; callers gate these on `--verbose`.
pub fn note(msg: &str) {
    if !is_quiet() {
        eprintln!("[webseek] {msg}");
    }
}

/// Non-fatal warning for stderr. Suppressed by `--quiet` like any other note.
pub fn warn(msg: &str) {
    if !is_quiet() {
        eprintln!("[webseek] {msg}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{ImageResult, SearchResult};
    use serde_json::{json, Value};

    fn sample_search() -> SearchResult {
        SearchResult {
            title: "T".into(),
            url: "https://example.com".into(),
            snippet: "S".into(),
        }
    }

    fn sample_fetch() -> FetchResult {
        FetchResult {
            url: "https://example.com".into(),
            title: Some("Title".into()),
            chars: 5,
            truncated: false,
            text: "hello".into(),
        }
    }

    fn to_value(buf: &[u8]) -> Value {
        let s = std::str::from_utf8(buf).expect("utf8");
        serde_json::from_str(s).expect("valid json")
    }

    #[test]
    fn search_json_contract_is_stable() {
        let mut buf = Vec::new();
        write_search_to(&mut buf, Mode::Json, "q", "duckduckgo", &[sample_search()]).unwrap();
        assert_eq!(
            std::str::from_utf8(&buf).unwrap(),
            "{\"query\":\"q\",\"engine\":\"duckduckgo\",\"count\":1,\"results\":[{\"title\":\"T\",\"url\":\"https://example.com\",\"snippet\":\"S\"}]}\n"
        );
    }

    #[test]
    fn search_jsonl_is_one_object_per_line() {
        let mut buf = Vec::new();
        write_search_to(
            &mut buf,
            Mode::Jsonl,
            "q",
            "bing",
            &[sample_search(), sample_search()],
        )
        .unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        for line in lines {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                v,
                json!({"title":"T","url":"https://example.com","snippet":"S"})
            );
        }
    }

    #[test]
    fn fetch_json_contract_is_stable() {
        let mut buf = Vec::new();
        write_fetch_to(&mut buf, Mode::Json, &sample_fetch()).unwrap();
        assert_eq!(
            to_value(&buf),
            json!({
                "url": "https://example.com",
                "title": "Title",
                "chars": 5,
                "truncated": false,
                "text": "hello"
            })
        );
    }

    #[test]
    fn images_json_contract_is_stable() {
        let img = ImageResult {
            title: "I".into(),
            url: "https://cdn/x.jpg".into(),
            page_url: "https://p".into(),
            width: Some(100),
            height: Some(50),
            format: "jpg".into(),
        };
        let mut buf = Vec::new();
        write_images_to(
            &mut buf,
            Mode::Json,
            "q",
            "bing",
            &[img],
            Some(vec!["f.jpg".into()]),
        )
        .unwrap();
        assert_eq!(
            to_value(&buf),
            json!({
                "query": "q",
                "engine": "bing",
                "count": 1,
                "results": [{
                    "title": "I",
                    "url": "https://cdn/x.jpg",
                    "page_url": "https://p",
                    "width": 100,
                    "height": 50,
                    "format": "jpg"
                }],
                "downloaded": ["f.jpg"]
            })
        );
    }

    #[test]
    fn images_json_omits_downloaded_when_none() {
        let mut buf = Vec::new();
        write_images_to(&mut buf, Mode::Json, "q", "bing", &[], None).unwrap();
        let v = to_value(&buf);
        assert!(v.get("downloaded").is_none());
    }

    #[test]
    fn every_jsonl_line_is_an_object() {
        let img = ImageResult {
            title: "I".into(),
            url: "https://cdn/x.jpg".into(),
            page_url: "https://p".into(),
            width: None,
            height: None,
            format: "jpg".into(),
        };
        let mut buf = Vec::new();
        write_images_to(
            &mut buf,
            Mode::Jsonl,
            "q",
            "bing",
            &[img],
            Some(vec!["f.jpg".into()]),
        )
        .unwrap();
        for line in std::str::from_utf8(&buf).unwrap().lines() {
            let v: Value = serde_json::from_str(line).unwrap();
            assert!(v.is_object(), "JSONL line must be an object, got: {line}");
        }
        let last: Value =
            serde_json::from_str(std::str::from_utf8(&buf).unwrap().lines().last().unwrap())
                .unwrap();
        assert_eq!(last, json!({"downloaded": ["f.jpg"]}));
    }

    #[test]
    fn fetch_batch_json_is_ordered_array_with_errors() {
        let items = vec![
            BatchItem::Ok(sample_fetch()),
            BatchItem::err("https://bad", "boom"),
        ];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Json, &items).unwrap();
        assert_eq!(
            to_value(&buf),
            json!([
                {
                    "url": "https://example.com",
                    "title": "Title",
                    "chars": 5,
                    "truncated": false,
                    "text": "hello"
                },
                { "url": "https://bad", "error": "boom", "kind": "network" }
            ])
        );
    }

    #[test]
    fn fetch_batch_jsonl_is_one_item_per_line() {
        let items = vec![
            BatchItem::Ok(sample_fetch()),
            BatchItem::err("https://bad", "boom"),
        ];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Jsonl, &items).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["url"], json!("https://example.com"));
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(
            second,
            json!({"url":"https://bad","error":"boom","kind":"network"})
        );
    }

    #[test]
    fn a_single_url_batch_is_still_an_array() {
        // `--array` exists so agents can opt into one shape regardless of the
        // number of URLs they passed.
        let items = vec![BatchItem::Ok(sample_fetch())];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Json, &items).unwrap();
        assert!(to_value(&buf).is_array());
    }

    #[test]
    fn engine_catalog_exposes_usable_names_and_aliases() {
        let mut buf = Vec::new();
        write_engines_to(&mut buf, Mode::Json, &crate::engines::catalog()).unwrap();
        let v = to_value(&buf);
        let arr = v.as_array().unwrap();
        assert!(!arr.is_empty());
        let hn = arr
            .iter()
            .find(|e| e["name"] == json!("hackernews"))
            .expect("hackernews in catalog");
        assert_eq!(hn["command"], json!("search"));
        assert_eq!(hn["aliases"], json!(["hn"]));
        assert_eq!(hn["fallback"], json!(false), "verticals do not fall back");
        let ddg = arr
            .iter()
            .find(|e| e["name"] == json!("duckduckgo") && e["command"] == json!("search"))
            .unwrap();
        assert_eq!(ddg["fallback"], json!(true));
    }

    #[test]
    fn pretty_output_has_no_ansi_when_color_is_off() {
        set_color(ColorChoice::Never);
        let mut buf = Vec::new();
        write_search_to(&mut buf, Mode::Pretty, "q", "ddg", &[sample_search()]).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(
            !s.contains('\x1b'),
            "escapes leaked with --color=never: {s:?}"
        );
    }
}
