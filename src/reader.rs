//! Page fetching with boilerplate-free text extraction.
//!
//! Goal: give an AI agent the *substance* of a page with minimum tokens.
//! Strategy, roughly "readability-lite":
//! 1. Strip navigation, scripts and ads (script, style, nav, aside, footer, …).
//! 2. Prefer semantic containers (`article`, `main`, `[role=main]`); otherwise
//!    score block parents by descendant text density and pick the best.
//! 3. Walk the chosen subtree in document order, emitting headings/paragraphs/
//!    lists as plain lines (or light markdown with `--markdown`).
//! 4. Collapse whitespace, cap at `--max-chars`.
//!
//! Two invariants matter for agents:
//!
//! - **Every content cap reports itself.** Byte cap, character cap and line cap
//!   all set `truncated`, so "truncated: false" really means "this is the whole
//!   page" and an agent can trust it when deciding whether to re-fetch.
//! - **Hostile input cannot hang the process.** HTML parsing is superlinear in
//!   nesting depth, and `--timeout` only bounds the HTTP request, not the work
//!   afterwards; [`max_nesting_depth`] rejects pathological documents up front.

use std::io::Read;

use ego_tree::NodeRef;
use reqwest::blocking::{Client, Response};
use reqwest::header::CONTENT_TYPE;
use scraper::node::Node;
use scraper::{Html, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::models::{FetchOpts, FetchResult};
use crate::text::truncate_chars;

/// Default hard cap on downloaded body bytes (protects memory and bandwidth).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Maximum number of output lines kept from one page.
pub const MAX_LINES: usize = 10_000;

/// Maximum element nesting webseek is willing to parse.
///
/// Real documents live well under 100. The cap exists because html5ever's
/// tree construction is quadratic in the depth of the open-element stack: a
/// 2 MB page of nested `<div>`s takes minutes, which would let any fetched
/// page stall an agent indefinitely.
pub const MAX_NESTING_DEPTH: usize = 1_500;

/// Fetch `url` and extract the main content.
pub fn fetch(client: &Client, url: &str, opts: &FetchOpts) -> Result<FetchResult> {
    // Validate early so DNS/network errors don't shadow a malformed URL.
    let parsed = Url::parse(url).map_err(|e| Error::Config(format!("invalid URL '{url}': {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(Error::Config(format!(
            "unsupported scheme '{}' (only http/https)",
            parsed.scheme()
        )));
    }

    let resp = crate::http::send_with_retry(&client.get(parsed))
        .map_err(|e| Error::Network(format!("fetch failed for {url}: {e}")))?;
    let status = resp.status();
    if !status.is_success() {
        return Err(Error::Http(status.as_u16()));
    }

    let charset_hint = charset_from_content_type(&resp);

    // Stream with a byte cap so a huge page can't blow up memory.
    let max_bytes = opts.max_bytes.max(1);
    let mut reader = resp.take(max_bytes as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Network(format!("read body failed for {url}: {e}")))?;
    let body_capped = bytes.len() >= max_bytes;

    let raw = decode_body(&bytes, charset_hint.as_deref());
    build_result(url, &raw, opts, body_capped)
}

/// Turn a decoded document into a [`FetchResult`], honoring every cap.
fn build_result(url: &str, raw: &str, opts: &FetchOpts, body_capped: bool) -> Result<FetchResult> {
    let max_chars = opts.max_chars.max(1);

    if opts.raw_html {
        // `--html` is still bounded: an agent asking for raw markup should not
        // be handed megabytes it never asked to pay for.
        let total = raw.chars().count();
        let over_char_cap = total > max_chars;
        let text = if over_char_cap {
            truncate_chars(raw, max_chars)
        } else {
            raw.to_string()
        };
        return Ok(FetchResult {
            url: url.to_string(),
            title: extract_title_from(&Html::parse_document(raw)),
            chars: text.chars().count(),
            truncated: body_capped || over_char_cap,
            text,
        });
    }

    let (title, text, lines_dropped) = extract_text_inner(raw, opts.markdown)?;
    let total = text.chars().count();
    let over_char_cap = total > max_chars;
    let text = if over_char_cap {
        truncate_chars(&text, max_chars)
    } else {
        text
    };
    Ok(FetchResult {
        url: url.to_string(),
        title,
        chars: text.chars().count(),
        truncated: body_capped || over_char_cap || lines_dropped,
        text,
    })
}

/// Charset label advertised by the `Content-Type` response header, if any.
fn charset_from_content_type(resp: &Response) -> Option<String> {
    let value = resp.headers().get(CONTENT_TYPE)?.to_str().ok()?;
    charset_from_content_type_str(value)
}

/// Pure half of [`charset_from_content_type`], for tests.
pub fn charset_from_content_type_str(value: &str) -> Option<String> {
    value.split(';').skip(1).find_map(|param| {
        let (k, v) = param.split_once('=')?;
        if !k.trim().eq_ignore_ascii_case("charset") {
            return None;
        }
        let v = v.trim().trim_matches('"');
        (!v.is_empty()).then(|| v.to_string())
    })
}

/// Decode a response body to text, honoring the declared character encoding.
///
/// Order of preference: BOM, `Content-Type` charset, `<meta charset>` in the
/// document prologue, then UTF-8. Assuming UTF-8 unconditionally turns every
/// Shift_JIS or EUC-JP page into replacement characters — which matters, since
/// regional search is a documented use case.
pub fn decode_body(bytes: &[u8], content_type_charset: Option<&str>) -> String {
    let label = content_type_charset
        .and_then(|c| encoding_rs::Encoding::for_label(c.as_bytes()))
        .or_else(|| meta_charset(bytes).and_then(|c| encoding_rs::Encoding::for_label(&c)));

    // `decode` honors a BOM when present, regardless of the label we pass.
    let encoding = label.unwrap_or(encoding_rs::UTF_8);
    let (text, _actual, _had_errors) = encoding.decode(bytes);
    text.into_owned()
}

/// Look for `<meta charset=…>` / `<meta http-equiv=content-type …>` in the
/// document prologue. Only the first 4 KiB is scanned, as browsers do.
fn meta_charset(bytes: &[u8]) -> Option<Vec<u8>> {
    const PROLOGUE: usize = 4096;
    let head = &bytes[..bytes.len().min(PROLOGUE)];
    let lower: Vec<u8> = head.to_ascii_lowercase();
    let text = String::from_utf8_lossy(&lower);

    let mut rest = text.as_ref();
    while let Some(idx) = rest.find("<meta") {
        rest = &rest[idx + 5..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        // <meta charset="shift_jis">
        if let Some(v) = attr_value(tag, "charset") {
            return Some(v.into_bytes());
        }
        // <meta http-equiv="Content-Type" content="text/html; charset=euc-jp">
        if tag.contains("http-equiv") {
            if let Some(content) = attr_value(tag, "content") {
                if let Some(cs) = charset_from_content_type_str(&content) {
                    return Some(cs.into_bytes());
                }
            }
        }
        rest = &rest[tag_end.min(rest.len())..];
    }
    None
}

/// Read `name=value` from a lower-cased tag body, quoted or bare.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let idx = rest.find(name)?;
        let after = &rest[idx + name.len()..];
        let trimmed = after.trim_start();
        if let Some(eq) = trimmed.strip_prefix('=') {
            let v = eq.trim_start();
            let value = match v.chars().next() {
                Some(q @ ('"' | '\'')) => v[1..].split(q).next().unwrap_or_default(),
                _ => v.split_whitespace().next().unwrap_or_default(),
            };
            let value = value.trim();
            if !value.is_empty() {
                return Some(value.to_string());
            }
        }
        rest = &rest[idx + name.len()..];
    }
}

/// Approximate maximum element nesting depth of a document.
///
/// Only tags that are always closed explicitly are counted, so the common
/// real-world sloppiness that html5ever silently repairs — unclosed `<p>`,
/// `<li>`, `<td>` — cannot inflate the estimate and reject a valid page.
pub fn max_nesting_depth(html: &str) -> usize {
    /// Elements that must be closed explicitly, and that nest arbitrarily.
    const COUNTED: &[&str] = &[
        "div",
        "span",
        "section",
        "article",
        "aside",
        "nav",
        "header",
        "footer",
        "main",
        "table",
        "tbody",
        "thead",
        "ul",
        "ol",
        "dl",
        "blockquote",
        "form",
        "fieldset",
        "figure",
        "details",
        "a",
        "b",
        "i",
        "u",
        "s",
        "em",
        "strong",
        "small",
        "sub",
        "sup",
        "font",
        "center",
        "pre",
        "code",
        "label",
        "iframe",
        "svg",
        "g",
        "q",
        "cite",
        "abbr",
        "address",
        "marquee",
        "video",
        "audio",
        "canvas",
        "picture",
        "template",
        "button",
        "select",
    ];

    let bytes = html.as_bytes();
    let mut depth: usize = 0;
    let mut max = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let after = &bytes[i + 1..];
        if after.is_empty() {
            break;
        }
        // Comments, doctypes and processing instructions carry no depth — and
        // their *contents* must be skipped, or `<!-- <div><div> -->` counts.
        if after[0] == b'!' || after[0] == b'?' {
            let (terminator, from): (&[u8], usize) = if after.starts_with(b"!--") {
                (b"-->", 3)
            } else {
                (b">", 0)
            };
            i += 1 + match find_sub(&after[from..], terminator) {
                Some(rel) => from + rel + terminator.len(),
                None => break, // unterminated: nothing countable can follow
            };
            continue;
        }
        let closing = after[0] == b'/';
        let name_start = if closing { 1 } else { 0 };
        let name: String = after[name_start..]
            .iter()
            .take_while(|c| c.is_ascii_alphanumeric())
            .map(|c| c.to_ascii_lowercase() as char)
            .collect();
        if name.is_empty() || !COUNTED.contains(&name.as_str()) {
            i += 1;
            continue;
        }
        if closing {
            depth = depth.saturating_sub(1);
        } else {
            // A self-closing `<svg ... />` opens nothing.
            let tag_end = after.iter().position(|&c| c == b'>').unwrap_or(after.len());
            let self_closing = after[..tag_end]
                .iter()
                .rev()
                .find(|c| !c.is_ascii_whitespace())
                == Some(&b'/');
            if !self_closing {
                depth += 1;
                max = max.max(depth);
            }
        }
        i += 1;
    }
    max
}

/// Index of the first occurrence of `needle` in `haystack`.
fn find_sub(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn extract_title_from(doc: &Html) -> Option<String> {
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract the main content of a document. Returns `(title, text)`.
///
/// Errors when the document is nested too deeply to process safely.
pub fn extract_text(html: &str, markdown: bool) -> (Option<String>, String) {
    extract_text_inner(html, markdown)
        .map(|(t, text, _)| (t, text))
        .unwrap_or((None, String::new()))
}

/// Returns `(title, text, lines_were_dropped)`.
fn extract_text_inner(html: &str, markdown: bool) -> Result<(Option<String>, String, bool)> {
    let depth = max_nesting_depth(html);
    if depth > MAX_NESTING_DEPTH {
        return Err(Error::Parse(format!(
            "document nesting too deep ({depth} levels, limit {MAX_NESTING_DEPTH}); \
             refusing to parse (use --html to get the raw bytes)"
        )));
    }

    // One parse, reused for both the title and the body: parsing twice doubled
    // the cost of every fetch.
    let doc = Html::parse_document(html);
    let title = extract_title_from(&doc);

    let container = pick_container(&doc);
    let mut out = String::new();
    walk(container, &mut out, markdown);
    let (text, dropped) = post_process(&out);
    Ok((title, text, dropped))
}

/// Non-content elements we never render or score.
fn is_excluded(tag: &str, e: &scraper::node::Element) -> bool {
    const TAGS: &[&str] = &[
        "script", "style", "noscript", "template", "svg", "canvas", "iframe", "form", "nav",
        "aside", "footer", "header",
    ];
    if TAGS.contains(&tag) {
        return true;
    }
    if e.attr("hidden").is_some() || e.attr("aria-hidden") == Some("true") {
        return true;
    }
    if let Some(class) = e.attr("class") {
        const CLASSES: &[&str] = &[
            "ad", "ads", "advert", "banner", "cookie", "popup", "modal", "share", "comment",
        ];
        if class.split_whitespace().any(|c| CLASSES.contains(&c)) {
            return true;
        }
    }
    false
}

/// True if `node` lives inside an excluded subtree.
fn in_excluded(mut node: NodeRef<'_, Node>) -> bool {
    while let Some(p) = node.parent() {
        if let Node::Element(e) = p.value() {
            if is_excluded(e.name(), e) {
                return true;
            }
        }
        node = p;
    }
    false
}

/// Choose the main container without mutating the document.
fn pick_container(doc: &Html) -> NodeRef<'_, Node> {
    // 1. Prefer semantic containers.
    let semantic = Selector::parse("article, main, [role='main'], #content, .content")
        .unwrap_or_else(|_| unreachable!("static"));
    if let Some(first) = doc.select(&semantic).next() {
        if !is_excluded(first.value().name(), first.value()) {
            return *first;
        }
    }

    // 2. Density scoring over block parents, ignoring excluded subtrees.
    let blocks = Selector::parse("p, li, pre, blockquote, h1, h2, h3, h4, h5, h6, td")
        .unwrap_or_else(|_| unreachable!("static"));
    let body = doc
        .select(&Selector::parse("body").unwrap_or_else(|_| unreachable!("static")))
        .next()
        .unwrap_or_else(|| doc.root_element());

    let mut best: Option<(usize, NodeRef<'_, Node>)> = None; // (score, parent)
    let mut seen = std::collections::HashSet::new();
    for block in body.select(&blocks) {
        if in_excluded(*block) {
            continue;
        }
        let Some(parent) = block.parent() else {
            continue;
        };
        if !seen.insert(parent.id()) {
            continue;
        }
        let score = text_len(&parent);
        if best.as_ref().map(|(s, _)| score > *s).unwrap_or(true) {
            best = Some((score, parent));
        }
    }
    match best {
        Some((score, node)) if score >= 200 => node,
        _ => *body,
    }
}

fn text_len(node: &NodeRef<'_, Node>) -> usize {
    node.descendants()
        .filter_map(|n| n.value().as_text())
        .map(|t| t.text.trim().chars().count())
        .sum()
}

const HEADINGS: &[&str] = &["h1", "h2", "h3", "h4", "h5", "h6"];

/// Recursive renderer: text nodes as-is; elements by tag semantics.
///
/// Recursion is safe because [`max_nesting_depth`] has already rejected
/// documents deeper than [`MAX_NESTING_DEPTH`].
fn walk(node: NodeRef<'_, Node>, out: &mut String, markdown: bool) {
    match node.value() {
        Node::Text(t) => out.push_str(&t.text),
        Node::Element(e) => {
            let tag = e.name();
            if is_excluded(tag, e) {
                return;
            }
            if markdown && tag == "a" {
                if let Some(href) = e.attr("href") {
                    let mut inner = String::new();
                    for child in node.children() {
                        walk(child, &mut inner, true);
                    }
                    if !inner.trim().is_empty() {
                        out.push('[');
                        out.push_str(inner.trim());
                        out.push(']');
                        out.push('(');
                        out.push_str(href);
                        out.push(')');
                        return;
                    }
                }
            }
            if markdown {
                // Emit real markdown block markers, not just line breaks:
                // `--markdown` promises headings *and lists*.
                if let Some(level) = HEADINGS.iter().position(|h| *h == tag) {
                    out.push_str(&"#".repeat(level + 1));
                    out.push(' ');
                } else if tag == "li" {
                    out.push_str("- ");
                } else if tag == "blockquote" {
                    out.push_str("> ");
                }
            }
            for child in node.children() {
                walk(child, out, markdown);
            }
            match tag {
                "p" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "blockquote"
                | "tr" | "br" | "dt" | "dd" => out.push('\n'),
                // Table cells keep their column separator in both modes; a
                // markdown reader loses the table otherwise.
                "td" | "th" => out.push_str(" | "),
                _ => {}
            }
        }
        Node::Document
        | Node::Fragment
        | Node::Comment(_)
        | Node::Doctype(_)
        | Node::ProcessingInstruction(_) => {}
    }
}

/// Collapse blank runs, trim trailing whitespace per line.
///
/// Returns `(text, lines_were_dropped)` so the caller can report the line cap
/// as truncation instead of silently shortening the page.
fn post_process(raw: &str) -> (String, bool) {
    let mut lines: Vec<String> = raw
        .split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // Cap pathological line counts (e.g. giant code tables).
    let dropped = lines.len() > MAX_LINES;
    if dropped {
        lines.truncate(MAX_LINES);
    }
    (lines.join("\n"), dropped)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts(max_chars: usize, raw_html: bool, markdown: bool) -> FetchOpts {
        FetchOpts {
            max_bytes: DEFAULT_MAX_BYTES,
            max_chars,
            raw_html,
            markdown,
        }
    }

    #[test]
    fn extracts_semantic_container() {
        let html = r#"<html><head><title>Test Article</title></head><body>
            <nav><a href="/">Home</a><a href="/about">About</a></nav>
            <div class="ad">BUY NOW</div>
            <article>
                <h1>Rust is great</h1>
                <p>Memory safety without garbage collection.</p>
                <p>Zero-cost abstractions.</p>
                <script>alert('nope')</script>
            </article>
            <footer>Copyright</footer>
        </body></html>"#;
        let (title, text) = extract_text(html, false);
        assert_eq!(title.as_deref(), Some("Test Article"));
        assert!(text.contains("Rust is great"));
        assert!(text.contains("Memory safety without garbage collection."));
        assert!(!text.contains("BUY NOW"));
        assert!(!text.contains("Copyright"));
        assert!(!text.contains("alert"));
    }

    #[test]
    fn density_fallback_without_semantic_tags() {
        let html = r#"<html><body>
            <div class="header">Site Title</div>
            <div><p>Short.</p></div>
            <div>
                <p>Lorem ipsum dolor sit amet consectetur adipiscing elit sed do eiusmod tempor incididunt ut labore et dolore magna aliqua.</p>
                <p>Ut enim ad minim veniam quis nostrud exercitation ullamco laboris nisi ut aliquip ex ea commodo consequat.</p>
            </div>
            <div class="footer">Site Footer</div>
        </body></html>"#;
        let (_, text) = extract_text(html, false);
        assert!(text.contains("Lorem ipsum"));
        assert!(!text.contains("Site Footer"));
    }

    #[test]
    fn markdown_mode_keeps_links_headings_and_lists() {
        let html = r#"<html><body><article>
            <h2>Intro</h2>
            <p>See <a href="https://rust-lang.org">Rust</a> website.</p>
            <ul><li>first</li><li>second</li></ul>
            <table><tr><td>a</td><td>b</td></tr></table>
        </article></body></html>"#;
        let (_, text) = extract_text(html, true);
        assert!(text.contains("## Intro"), "got: {text}");
        assert!(
            text.contains("[Rust](https://rust-lang.org)"),
            "got: {text}"
        );
        assert!(text.contains("- first"), "lists need markers: {text}");
        assert!(text.contains("- second"), "got: {text}");
        assert!(text.contains("a | b"), "table cells survive: {text}");
    }

    #[test]
    fn post_process_collapses_whitespace() {
        let (out, dropped) = post_process("  a\n\n\n   b  \nc ");
        assert_eq!(out, "a\nb\nc");
        assert!(!dropped);
    }

    #[test]
    fn line_cap_is_reported_as_truncation() {
        let many = "<p>x</p>".repeat(MAX_LINES + 2_000);
        let html = format!("<html><body><article>{many}</article></body></html>");
        // Well under the char cap, so only the line cap can trigger here.
        let r = build_result("https://x", &html, &opts(1_000_000, false, false), false).unwrap();
        assert_eq!(r.text.lines().count(), MAX_LINES);
        assert!(
            r.truncated,
            "dropping lines must be reported, or agents trust a partial page"
        );
    }

    #[test]
    fn raw_html_respects_the_character_cap() {
        let html = format!("<html><body>{}</body></html>", "x".repeat(5_000));
        let r = build_result("https://x", &html, &opts(100, true, false), false).unwrap();
        assert_eq!(r.chars, 100);
        assert_eq!(r.text.chars().count(), 100);
        assert!(r.truncated, "--html must report its own truncation");
    }

    #[test]
    fn untruncated_page_reports_truncated_false() {
        let html = "<html><head><title>T</title></head><body><article><p>short</p></article></body></html>";
        let r = build_result("https://x", html, &opts(20_000, false, false), false).unwrap();
        assert!(!r.truncated);
        assert_eq!(r.chars, r.text.chars().count());
        assert_eq!(r.title.as_deref(), Some("T"));
    }

    #[test]
    fn body_byte_cap_is_reported_as_truncation() {
        let html = "<html><body><article><p>hello</p></article></body></html>";
        let r = build_result("https://x", html, &opts(20_000, false, false), true).unwrap();
        assert!(r.truncated, "hitting the byte cap is truncation too");
    }

    #[test]
    fn nesting_depth_is_measured_without_counting_auto_closed_tags() {
        assert_eq!(max_nesting_depth("<div><div><div></div></div></div>"), 3);
        assert_eq!(max_nesting_depth("<div><span></span></div>"), 2);
        // Unclosed <p>/<li> are repaired by the parser and must not inflate.
        assert_eq!(max_nesting_depth(&"<p>text".repeat(5_000)), 0);
        assert_eq!(max_nesting_depth(&"<li>item".repeat(5_000)), 0);
        // Self-closing tags open nothing.
        assert_eq!(max_nesting_depth("<div><svg /></div>"), 1);
        assert_eq!(max_nesting_depth("<!-- <div><div> --><br><img>"), 0);
    }

    #[test]
    fn pathological_nesting_is_rejected_instead_of_hanging() {
        let d = MAX_NESTING_DEPTH + 10;
        let html = format!(
            "<html><body>{}deep{}</body></html>",
            "<div>".repeat(d),
            "</div>".repeat(d)
        );
        let err = extract_text_inner(&html, false).unwrap_err();
        assert!(err.to_string().contains("nesting too deep"), "got: {err}");
    }

    #[test]
    fn ordinary_pages_are_never_rejected_by_the_depth_guard() {
        let html = format!(
            "<html><body>{}<p>hi</p>{}</body></html>",
            "<div>".repeat(60),
            "</div>".repeat(60)
        );
        assert!(extract_text_inner(&html, false).is_ok());
    }

    #[test]
    fn shift_jis_pages_decode_instead_of_becoming_mojibake() {
        // "日本語" in Shift_JIS.
        let sjis: Vec<u8> = vec![0x93, 0xfa, 0x96, 0x7b, 0x8c, 0xea];
        let mut body = b"<html><head><meta charset=\"shift_jis\"><title>".to_vec();
        body.extend_from_slice(&sjis);
        body.extend_from_slice(b"</title></head><body><p>ok</p></body></html>");

        let decoded = decode_body(&body, None);
        assert!(
            decoded.contains("日本語"),
            "meta charset ignored: {decoded}"
        );
        assert!(!decoded.contains('\u{fffd}'), "replacement chars leaked");

        // The Content-Type header takes precedence and works on its own.
        let mut headerless = b"<html><head><title>".to_vec();
        headerless.extend_from_slice(&sjis);
        headerless.extend_from_slice(b"</title></head><body></body></html>");
        assert!(decode_body(&headerless, Some("Shift_JIS")).contains("日本語"));
    }

    #[test]
    fn utf8_remains_the_default() {
        let body = "<html><body><p>日本語</p></body></html>".as_bytes();
        assert!(decode_body(body, None).contains("日本語"));
        assert!(decode_body(body, Some("nonsense-charset")).contains("日本語"));
    }

    #[test]
    fn content_type_charset_parsing() {
        assert_eq!(
            charset_from_content_type_str("text/html; charset=EUC-JP").as_deref(),
            Some("EUC-JP")
        );
        assert_eq!(
            charset_from_content_type_str("text/html;charset=\"utf-8\"").as_deref(),
            Some("utf-8")
        );
        assert_eq!(charset_from_content_type_str("text/html"), None);
    }
}
