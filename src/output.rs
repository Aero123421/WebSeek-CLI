//! Output layer.
//!
//! Context-saving rules (the core requirement for AI agents):
//! - **stdout carries data only.** Progress/notes go to stderr (`eprintln!`).
//! - **Default mode is JSON when stdout is not a TTY** (piped into an agent);
//!   pretty text only when a human is watching.
//! - JSON keys are short and stable; nothing is printed before/after the JSON
//!   document, so it can be parsed directly.
//! - Snippets/text are pre-capped, so token cost is bounded.
//! - **Every string that came from the open web is sanitized before it
//!   touches a terminal** (`Mode::Pretty`). Titles, URLs, and snippets can
//!   contain raw ESC/CSI/OSC bytes (an HTML numeric entity can decode to one),
//!   which a terminal would interpret as cursor moves, a window-title
//!   rewrite, or worse. JSON/JSONL output is not re-sanitized here, since a
//!   JSON string is data, not a terminal command — but see the README "Trust
//!   model" section regardless: agents must still treat every field as
//!   untrusted content, never as instructions.
//!
//! Every writer has a `*_to(w, ...)` variant taking any `Write`, which keeps
//! the JSON contract unit-testable (golden tests).

use std::io::{IsTerminal, Write};

use serde::Serialize;

use crate::batch::BatchItem;
use crate::cache::CacheInfo;
use crate::engines::images::DownloadRecord;
use crate::engines::EngineInfo;
use crate::error::Error;
use crate::models::{FetchResult, ImageResult, SearchResult};
use crate::text::{sanitize_line, sanitize_text};

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

/// Resolve `--color` against `NO_COLOR` and TTY status. Only meaningful for
/// `Mode::Pretty` — JSON/JSONL never carry ANSI codes.
pub fn resolve_color(mode: crate::cli::ColorMode) -> bool {
    match mode {
        crate::cli::ColorMode::Always => true,
        crate::cli::ColorMode::Never => false,
        crate::cli::ColorMode::Auto => {
            std::env::var_os("NO_COLOR").is_none() && std::io::stdout().is_terminal()
        }
    }
}

/// Wrap `s` in an ANSI SGR code when `use_color`, otherwise return it as-is.
fn c(use_color: bool, code: &str, s: &str) -> String {
    if use_color {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// A fully-formed JSON document written by `search`.
#[derive(Serialize)]
struct SearchDoc {
    query: String,
    engine_requested: String,
    engine_used: String,
    fallback: bool,
    cache_hit: bool,
    count: usize,
    results: Vec<SearchResult>,
}

#[derive(Serialize)]
struct ImageDoc {
    query: String,
    engine_requested: String,
    engine_used: String,
    fallback: bool,
    cache_hit: bool,
    count: usize,
    results: Vec<ImageResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    downloaded: Option<Vec<DownloadRecord>>,
}

/// What a search/images call actually did, independent of its results —
/// this is what lets an agent tell "PubMed answered" from "PubMed failed and
/// DuckDuckGo answered instead" instead of just seeing a bare result list.
#[derive(Debug, Clone)]
pub struct EngineOutcome {
    pub requested: String,
    pub used: String,
    pub cache_hit: bool,
}

impl EngineOutcome {
    pub fn fallback(&self) -> bool {
        self.requested != self.used
    }
}

pub fn write_search(
    mode: Mode,
    use_color: bool,
    query: &str,
    outcome: &EngineOutcome,
    results: &[SearchResult],
) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_search_to(&mut out.lock(), mode, use_color, query, outcome, results)
}

pub fn write_search_to(
    w: &mut impl Write,
    mode: Mode,
    use_color: bool,
    query: &str,
    outcome: &EngineOutcome,
    results: &[SearchResult],
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            let doc = SearchDoc {
                query: query.to_string(),
                engine_requested: outcome.requested.clone(),
                engine_used: outcome.used.clone(),
                fallback: outcome.fallback(),
                cache_hit: outcome.cache_hit,
                count: results.len(),
                results: results.to_vec(),
            };
            writeln!(w, "{}", serde_json::to_string(&doc)?)?;
        }
        Mode::Jsonl => {
            for r in results {
                writeln!(w, "{}", serde_json::to_string(r)?)?;
            }
            // Always present, even (especially) when `results` is empty:
            // otherwise a genuine "0 results" run is indistinguishable from
            // a crash before any output was produced.
            let summary = serde_json::json!({
                "type": "summary",
                "query": query,
                "engine_requested": outcome.requested,
                "engine_used": outcome.used,
                "fallback": outcome.fallback(),
                "cache_hit": outcome.cache_hit,
                "count": results.len(),
            });
            writeln!(w, "{}", serde_json::to_string(&summary)?)?;
        }
        Mode::Pretty => {
            if results.is_empty() {
                writeln!(
                    w,
                    "No results for \"{}\" (engine: {}).",
                    sanitize_line(query),
                    outcome.used
                )?;
                return Ok(());
            }
            if outcome.fallback() {
                writeln!(
                    w,
                    "{}",
                    c(
                        use_color,
                        "33",
                        &format!(
                            "note: {} failed or was unavailable; showing {} results instead",
                            outcome.requested, outcome.used
                        )
                    )
                )?;
            }
            for (i, r) in results.iter().enumerate() {
                let title = sanitize_line(&r.title);
                let url = sanitize_line(&r.url);
                writeln!(
                    w,
                    "{}",
                    c(use_color, "1", &format!("{:>2}. {title}", i + 1))
                )?;
                writeln!(w, "     {}", c(use_color, "36", &url))?;
                if !r.snippet.is_empty() {
                    writeln!(w, "     {}", sanitize_line(&r.snippet))?;
                }
            }
        }
    }
    Ok(())
}

pub fn write_images(
    mode: Mode,
    use_color: bool,
    query: &str,
    outcome: &EngineOutcome,
    results: &[ImageResult],
    downloaded: Option<Vec<DownloadRecord>>,
) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_images_to(
        &mut out.lock(),
        mode,
        use_color,
        query,
        outcome,
        results,
        downloaded,
    )
}

pub fn write_images_to(
    w: &mut impl Write,
    mode: Mode,
    use_color: bool,
    query: &str,
    outcome: &EngineOutcome,
    results: &[ImageResult],
    downloaded: Option<Vec<DownloadRecord>>,
) -> std::io::Result<()> {
    match mode {
        Mode::Json => {
            let doc = ImageDoc {
                query: query.to_string(),
                engine_requested: outcome.requested.clone(),
                engine_used: outcome.used.clone(),
                fallback: outcome.fallback(),
                cache_hit: outcome.cache_hit,
                count: results.len(),
                results: results.to_vec(),
                downloaded,
            };
            writeln!(w, "{}", serde_json::to_string(&doc)?)?;
        }
        Mode::Jsonl => {
            // Event-tagged stream (fixes the old contract violation where a
            // bare JSON *array* of filenames was printed as its own line
            // after the per-result objects — not one object per line).
            for r in results {
                let event = serde_json::json!({"type": "image_result", "result": r});
                writeln!(w, "{}", serde_json::to_string(&event)?)?;
            }
            let mut ok_count = 0usize;
            let mut failed_count = 0usize;
            if let Some(records) = &downloaded {
                for rec in records {
                    if rec.ok {
                        ok_count += 1;
                    } else {
                        failed_count += 1;
                    }
                    let event = serde_json::json!({"type": "download", "record": rec});
                    writeln!(w, "{}", serde_json::to_string(&event)?)?;
                }
            }
            let summary = serde_json::json!({
                "type": "summary",
                "query": query,
                "engine_requested": outcome.requested,
                "engine_used": outcome.used,
                "fallback": outcome.fallback(),
                "cache_hit": outcome.cache_hit,
                "count": results.len(),
                "downloaded_ok": ok_count,
                "downloaded_failed": failed_count,
            });
            writeln!(w, "{}", serde_json::to_string(&summary)?)?;
        }
        Mode::Pretty => {
            for (i, r) in results.iter().enumerate() {
                let title = sanitize_line(&r.title);
                let url = sanitize_line(&r.url);
                let page = sanitize_line(&r.page_url);
                writeln!(
                    w,
                    "{}",
                    c(use_color, "1", &format!("{:>2}. {title}", i + 1))
                )?;
                writeln!(w, "     {}", c(use_color, "36", &url))?;
                let dims = match (r.width, r.height) {
                    (Some(w_), Some(h)) => format!("{w_}x{h}"),
                    _ => "?".into(),
                };
                let fmt = if r.format.is_empty() { "?" } else { &r.format };
                writeln!(w, "     [{fmt}] {dims}  page: {page}")?;
            }
            if let Some(records) = &downloaded {
                let ok: Vec<_> = records.iter().filter(|r| r.ok).collect();
                let failed: Vec<_> = records.iter().filter(|r| !r.ok).collect();
                if !ok.is_empty() {
                    writeln!(w, "Downloaded {} file(s):", ok.len())?;
                    for r in &ok {
                        writeln!(
                            w,
                            "  {}",
                            c(use_color, "32", r.path.as_deref().unwrap_or(""))
                        )?;
                    }
                }
                for r in &failed {
                    eprintln!(
                        "[webseek] {}: {}",
                        r.url,
                        r.error.as_deref().unwrap_or("failed")
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn write_fetch(mode: Mode, use_color: bool, fetch: &FetchResult) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_fetch_to(&mut out.lock(), mode, use_color, fetch)
}

pub fn write_fetch_to(
    w: &mut impl Write,
    mode: Mode,
    use_color: bool,
    fetch: &FetchResult,
) -> std::io::Result<()> {
    match mode {
        Mode::Json | Mode::Jsonl => {
            writeln!(w, "{}", serde_json::to_string(fetch)?)?;
        }
        Mode::Pretty => {
            if let Some(title) = &fetch.title {
                writeln!(w, "{}", c(use_color, "1", &sanitize_line(title)))?;
            }
            writeln!(
                w,
                "{}",
                c(use_color, "36", &sanitize_line(&fetch.final_url))
            )?;
            if fetch.final_url != fetch.requested_url {
                writeln!(
                    w,
                    "(redirected from {})",
                    sanitize_line(&fetch.requested_url)
                )?;
            }
            let reasons = if fetch.truncated {
                format!(" (truncated: {})", fetch.truncation_reasons.join(", "))
            } else {
                String::new()
            };
            writeln!(w, "{} chars{reasons}", fetch.chars)?;
            writeln!(w, "---")?;
            writeln!(w, "{}", sanitize_text(&fetch.text))?;
        }
    }
    Ok(())
}

/// Batch output contract:
/// - `Json`: a single JSON **array** of per-URL items, preserving input order.
/// - `Jsonl`: one item object per line.
/// - `Pretty`: fetched pages on stdout; failures as notes on stderr.
///
/// Each item is tagged (`{"ok":true,"value":{...}}` /
/// `{"ok":false,"error":{...}}`) rather than shape-sniffed — a future field
/// added to `FetchResult` can never again be confused with the error shape.
pub fn write_fetch_batch(mode: Mode, use_color: bool, items: &[BatchItem]) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_fetch_batch_to(&mut out.lock(), mode, use_color, items)
}

pub fn write_fetch_batch_to(
    w: &mut impl Write,
    mode: Mode,
    use_color: bool,
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
                    BatchItem::Ok(fetch) => write_fetch_to(w, mode, use_color, fetch)?,
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

pub fn write_cache_info(mode: Mode, info: &CacheInfo) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_cache_info_to(&mut out.lock(), mode, info)
}

pub fn write_cache_info_to(
    w: &mut impl Write,
    mode: Mode,
    info: &CacheInfo,
) -> std::io::Result<()> {
    match mode {
        Mode::Json | Mode::Jsonl => writeln!(w, "{}", serde_json::to_string(info)?)?,
        Mode::Pretty => {
            writeln!(w, "path:      {}", info.path)?;
            writeln!(w, "enabled:   {}", info.enabled)?;
            writeln!(w, "entries:   {}", info.entries)?;
            writeln!(w, "bytes:     {}", info.bytes)?;
            writeln!(w, "expired:   {} (not yet evicted)", info.expired)?;
            writeln!(w, "ttl_secs:  {} (0 = no expiry)", info.ttl_secs)?;
            writeln!(w, "max_entries: {}", info.max_entries)?;
            writeln!(w, "max_bytes:   {}", info.max_bytes)?;
        }
    }
    Ok(())
}

/// A single machine-readable path, for `webseek config path` / cache clear
/// confirmations.
pub fn write_path(mode: Mode, key: &str, path: &str) -> std::io::Result<()> {
    let out = std::io::stdout();
    write_path_to(&mut out.lock(), mode, key, path)
}

pub fn write_path_to(w: &mut impl Write, mode: Mode, key: &str, path: &str) -> std::io::Result<()> {
    match mode {
        Mode::Json | Mode::Jsonl => {
            writeln!(
                w,
                "{}",
                serde_json::to_string(&serde_json::json!({key: path}))?
            )?;
        }
        Mode::Pretty => writeln!(w, "{path}")?,
    }
    Ok(())
}

/// Structured error envelope for JSON/JSONL modes, so a failure is still
/// machine-parseable stdout output instead of only free-text on stderr.
/// Never used in Pretty mode (the caller prints a plain message there).
pub fn write_error_json(mode: Mode, err: &Error) -> std::io::Result<()> {
    if mode == Mode::Pretty {
        return Ok(());
    }
    let mut body = serde_json::json!({
        "code": err.code(),
        "message": err.to_string(),
    });
    if let Some(retry_after) = err.retry_after() {
        body["retry_after_ms"] = serde_json::json!(retry_after.as_millis() as u64);
    }
    let envelope = serde_json::json!({ "error": body });
    println!("{}", serde_json::to_string(&envelope)?);
    Ok(())
}

/// Note for stderr; only shown in verbose mode by the caller.
pub fn note(msg: &str) {
    eprintln!("[webseek] {msg}");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::{truncation_reason, ImageResult, SearchResult, SOURCE_TRUST_UNTRUSTED};
    use serde_json::{json, Value};

    fn sample_search() -> SearchResult {
        SearchResult {
            title: "T".into(),
            url: "https://example.com".into(),
            snippet: "S".into(),
        }
    }

    fn sample_outcome() -> EngineOutcome {
        EngineOutcome {
            requested: "duckduckgo".into(),
            used: "duckduckgo".into(),
            cache_hit: false,
        }
    }

    fn sample_fetch() -> FetchResult {
        FetchResult {
            requested_url: "https://example.com".into(),
            final_url: "https://example.com".into(),
            status: 200,
            content_type: Some("text/html".into()),
            title: Some("Title".into()),
            chars: 5,
            truncated: false,
            truncation_reasons: vec![],
            source_trust: SOURCE_TRUST_UNTRUSTED.into(),
            fetched_at: 1_700_000_000,
            text: "hello".into(),
        }
    }

    fn to_value(buf: &[u8]) -> Value {
        let s = std::str::from_utf8(buf).expect("utf8");
        serde_json::from_str(s).expect("valid json")
    }

    #[test]
    fn search_json_contract_includes_engine_metadata() {
        let mut buf = Vec::new();
        write_search_to(
            &mut buf,
            Mode::Json,
            false,
            "q",
            &sample_outcome(),
            &[sample_search()],
        )
        .unwrap();
        assert_eq!(
            to_value(&buf),
            json!({
                "query": "q",
                "engine_requested": "duckduckgo",
                "engine_used": "duckduckgo",
                "fallback": false,
                "cache_hit": false,
                "count": 1,
                "results": [{"title":"T","url":"https://example.com","snippet":"S"}]
            })
        );
    }

    #[test]
    fn search_json_reports_fallback() {
        let outcome = EngineOutcome {
            requested: "pubmed".into(),
            used: "pubmed".into(),
            cache_hit: true,
        };
        let mut buf = Vec::new();
        write_search_to(&mut buf, Mode::Json, false, "q", &outcome, &[]).unwrap();
        let v = to_value(&buf);
        assert_eq!(v["fallback"], json!(false));
        assert_eq!(v["cache_hit"], json!(true));
    }

    #[test]
    fn search_jsonl_is_one_object_per_line_plus_a_summary() {
        let mut buf = Vec::new();
        write_search_to(
            &mut buf,
            Mode::Jsonl,
            false,
            "q",
            &sample_outcome(),
            &[sample_search(), sample_search()],
        )
        .unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 3); // 2 results + 1 summary
        for line in &lines[..2] {
            let v: Value = serde_json::from_str(line).unwrap();
            assert_eq!(
                v,
                json!({"title":"T","url":"https://example.com","snippet":"S"})
            );
        }
        let summary: Value = serde_json::from_str(lines[2]).unwrap();
        assert_eq!(summary["type"], json!("summary"));
        assert_eq!(summary["count"], json!(2));
    }

    #[test]
    fn empty_search_jsonl_still_emits_a_summary_line() {
        let mut buf = Vec::new();
        write_search_to(&mut buf, Mode::Jsonl, false, "q", &sample_outcome(), &[]).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 1, "zero results must not mean zero output");
        let summary: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(summary["type"], json!("summary"));
        assert_eq!(summary["count"], json!(0));
    }

    #[test]
    fn pretty_output_sanitizes_control_characters() {
        let evil = SearchResult {
            title: "safe\u{1b}[2Jtitle".into(),
            url: "https://example.com".into(),
            snippet: "hello\u{1b}]0;pwnedworld".into(),
        };
        let mut buf = Vec::new();
        write_search_to(
            &mut buf,
            Mode::Pretty,
            false,
            "q",
            &sample_outcome(),
            &[evil],
        )
        .unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(!s.contains('\u{1b}'), "escape sequence leaked: {s:?}");
    }

    #[test]
    fn fetch_json_contract_uses_requested_and_final_url() {
        let mut buf = Vec::new();
        write_fetch_to(&mut buf, Mode::Json, false, &sample_fetch()).unwrap();
        let v = to_value(&buf);
        assert_eq!(v["requested_url"], json!("https://example.com"));
        assert_eq!(v["final_url"], json!("https://example.com"));
        assert_eq!(v["status"], json!(200));
        assert_eq!(v["source_trust"], json!(SOURCE_TRUST_UNTRUSTED));
        assert_eq!(v["truncation_reasons"], json!([]));
    }

    #[test]
    fn fetch_pretty_notes_a_redirect() {
        let mut f = sample_fetch();
        f.final_url = "https://example.com/final".into();
        let mut buf = Vec::new();
        write_fetch_to(&mut buf, Mode::Pretty, false, &f).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("redirected from https://example.com"), "{s}");
    }

    #[test]
    fn fetch_pretty_reports_truncation_reasons() {
        let mut f = sample_fetch();
        f.truncated = true;
        f.truncation_reasons = vec![truncation_reason::MAX_CHARS.into()];
        let mut buf = Vec::new();
        write_fetch_to(&mut buf, Mode::Pretty, false, &f).unwrap();
        let s = std::str::from_utf8(&buf).unwrap();
        assert!(s.contains("truncated: max_chars"), "{s}");
    }

    #[test]
    fn images_json_omits_downloaded_when_none() {
        let mut buf = Vec::new();
        write_images_to(
            &mut buf,
            Mode::Json,
            false,
            "q",
            &sample_outcome(),
            &[],
            None,
        )
        .unwrap();
        let v = to_value(&buf);
        assert!(v.get("downloaded").is_none());
    }

    #[test]
    fn images_jsonl_uses_typed_events_not_a_bare_array() {
        let img = ImageResult {
            title: "I".into(),
            url: "https://cdn/x.jpg".into(),
            page_url: "https://p".into(),
            width: Some(100),
            height: Some(50),
            format: "jpg".into(),
        };
        let downloaded = vec![
            DownloadRecord {
                url: "https://cdn/x.jpg".into(),
                ok: true,
                path: Some("f1.jpg".into()),
                error: None,
            },
            DownloadRecord {
                url: "https://cdn/y.jpg".into(),
                ok: false,
                path: None,
                error: Some("HTTP 404".into()),
            },
        ];
        let mut buf = Vec::new();
        write_images_to(
            &mut buf,
            Mode::Jsonl,
            false,
            "q",
            &sample_outcome(),
            &[img],
            Some(downloaded),
        )
        .unwrap();
        let lines: Vec<Value> = std::str::from_utf8(&buf)
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines.len(), 4); // 1 image_result + 2 download + 1 summary
        assert_eq!(lines[0]["type"], json!("image_result"));
        assert_eq!(lines[1]["type"], json!("download"));
        assert_eq!(lines[1]["record"]["ok"], json!(true));
        assert_eq!(lines[2]["type"], json!("download"));
        assert_eq!(lines[2]["record"]["ok"], json!(false));
        assert_eq!(lines[3]["type"], json!("summary"));
        assert_eq!(lines[3]["downloaded_ok"], json!(1));
        assert_eq!(lines[3]["downloaded_failed"], json!(1));
        // Every line must independently be a single JSON object -- never a
        // bare array, which is what the old contract violated.
        for l in &lines {
            assert!(l.is_object());
        }
    }

    #[test]
    fn fetch_batch_json_is_tagged_ok_value_or_error() {
        let items = vec![
            BatchItem::Ok(sample_fetch()),
            BatchItem::Err {
                url: "https://bad".into(),
                error: "boom".into(),
            },
        ];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Json, false, &items).unwrap();
        let v = to_value(&buf);
        assert_eq!(v[0]["ok"], json!(true));
        assert_eq!(v[0]["value"]["requested_url"], json!("https://example.com"));
        assert_eq!(v[1]["ok"], json!(false));
        assert_eq!(v[1]["error"]["url"], json!("https://bad"));
        assert_eq!(v[1]["error"]["message"], json!("boom"));
    }

    #[test]
    fn fetch_batch_jsonl_is_one_tagged_item_per_line() {
        let items = vec![BatchItem::Ok(sample_fetch())];
        let mut buf = Vec::new();
        write_fetch_batch_to(&mut buf, Mode::Jsonl, false, &items).unwrap();
        let lines: Vec<&str> = std::str::from_utf8(&buf).unwrap().lines().collect();
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(v["ok"], json!(true));
    }

    #[test]
    fn error_envelope_is_a_noop_in_pretty_mode() {
        // write_error_json always writes to real stdout in JSON/JSONL modes;
        // Pretty mode must be a pure no-op (the caller prints plain text).
        let err = Error::rate_limited("slow down");
        assert!(write_error_json(Mode::Pretty, &err).is_ok());
    }

    #[test]
    fn no_color_env_forces_plain_output() {
        std::env::set_var("NO_COLOR", "1");
        let use_color = resolve_color(crate::cli::ColorMode::Auto);
        std::env::remove_var("NO_COLOR");
        assert!(!use_color);
    }

    #[test]
    fn color_always_ignores_no_color() {
        std::env::set_var("NO_COLOR", "1");
        let use_color = resolve_color(crate::cli::ColorMode::Always);
        std::env::remove_var("NO_COLOR");
        assert!(use_color);
    }
}
