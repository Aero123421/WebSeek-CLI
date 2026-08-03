//! Output layer.
//!
//! Context-saving rules (the core requirement for AI agents):
//! - **stdout carries data only.** Progress/notes go to stderr (`eprintln!`).
//! - **Default mode is JSON when stdout is not a TTY** (piped into an agent);
//!   pretty text only when a human is watching.
//! - JSON keys are short and stable; nothing is printed before/after the JSON
//!   document, so it can be parsed directly.
//! - Snippets/text are pre-capped, so token cost is bounded.
//!
//! Every writer has a `*_to(w, ...)` variant taking any `Write`, which keeps
//! the JSON contract unit-testable (golden tests).

use std::io::{IsTerminal, Write};

use serde::Serialize;

use crate::batch::BatchItem;
use crate::engines::EngineInfo;
use crate::models::{FetchResult, ImageResult, SearchResult};

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
struct SearchDoc {
    query: String,
    engine: String,
    count: usize,
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct ImageDoc {
    query: String,
    engine: String,
    count: usize,
    results: Vec<ImageResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downloaded: Option<Vec<String>>,
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
                query: query.to_string(),
                engine: engine.to_string(),
                count: results.len(),
                results: results.to_vec(),
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
                writeln!(w, "\x1b[1m{:>2}. {}\x1b[0m", i + 1, r.title)?;
                writeln!(w, "     \x1b[36m{}\x1b[0m", r.url)?;
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
                query: query.to_string(),
                engine: engine.to_string(),
                count: results.len(),
                results: results.to_vec(),
                downloaded,
            };
            writeln!(w, "{}", serde_json::to_string(&doc)?)?;
        }
        Mode::Jsonl => {
            for r in results {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
            if let Some(dl) = downloaded {
                if !dl.is_empty() {
                    writeln!(w, "{}", serde_json::to_string(&dl)?)?;
                }
            }
        }
        Mode::Pretty => {
            for (i, r) in results.iter().enumerate() {
                writeln!(w, "\x1b[1m{:>2}. {}\x1b[0m", i + 1, r.title)?;
                writeln!(w, "     \x1b[36m{}\x1b[0m", r.url)?;
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
                        writeln!(w, "  \x1b[32m{p}\x1b[0m")?;
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
                writeln!(w, "\x1b[1m{title}\x1b[0m")?;
            }
            writeln!(w, "\x1b[36m{}\x1b[0m", fetch.url)?;
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
                    BatchItem::Err { url, error } => {
                        eprintln!("[webseek] {url}: {error}");
                    }
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
                writeln!(
                    w,
                    "\x1b[1m{:<22}\x1b[0m [\x1b[36m{:<12}\x1b[0m] {}",
                    e.name, e.kind, e.description
                )?;
                writeln!(w, "    \x1b[2m{}\x1b[0m", e.example)?;
            }
        }
    }
    Ok(())
}

/// Note for stderr; only shown in verbose mode by the caller.
pub fn note(msg: &str) {
    eprintln!("[webseek] {msg}");
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
        // Exact-string golden: guards both keys and formatting stability.
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
    fn fetch_batch_json_is_ordered_array_with_errors() {
        let items = vec![
            BatchItem::Ok(sample_fetch()),
            BatchItem::Err {
                url: "https://bad".into(),
                error: "boom".into(),
            },
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
                { "url": "https://bad", "error": "boom" }
            ])
        );
    }

    #[test]
    fn fetch_batch_jsonl_is_one_item_per_line() {
        let items = vec![
            BatchItem::Ok(sample_fetch()),
            BatchItem::Err {
                url: "https://bad".into(),
                error: "boom".into(),
            },
        ];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Jsonl, &items).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 2);
        let first: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(first["url"], json!("https://example.com"));
        let second: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(second, json!({"url":"https://bad","error":"boom"}));
    }
}
