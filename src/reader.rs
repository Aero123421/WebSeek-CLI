//! Page fetching with boilerplate-free text extraction.
//!
//! Goal: give an AI agent the *substance* of a page with minimum tokens.
//! Strategy, roughly "readability-lite":
//! 1. Strip navigation/can scripts/ads (script, style, nav, aside, footer, ...).
//! 2. Prefer semantic containers (`article`, `main`, `[role=main]`); otherwise
//!    score block parents by descendant text density and pick the best.
//! 3. Walk the chosen subtree in document order, emitting headings/paragraphs/
//!    lists as plain lines (or light markdown with `--markdown`).
//! 4. Collapse whitespace, cap at `--max-chars`.

use std::io::Read;

use ego_tree::NodeRef;
use reqwest::blocking::Client;
use scraper::node::Node;
use scraper::{Html, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::models::{FetchOpts, FetchResult};
use crate::text::truncate_chars;

/// Default hard cap on downloaded body bytes (protects memory and bandwidth).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

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

    // Stream with a byte cap so a huge page can't blow up memory.
    let mut reader = resp.take(opts.max_bytes as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Network(format!("read body failed for {url}: {e}")))?;

    let raw = String::from_utf8_lossy(&bytes);
    if opts.raw_html {
        return Ok(FetchResult {
            url: url.to_string(),
            title: extract_title(&raw),
            chars: raw.chars().count(),
            truncated: bytes.len() >= opts.max_bytes,
            text: raw.into_owned(),
        });
    }

    let (title, mut text) = extract_text(&raw, opts.markdown);
    let total = text.chars().count();
    let truncated = total > opts.max_chars;
    if truncated {
        text = truncate_chars(&text, opts.max_chars);
    }
    Ok(FetchResult {
        url: url.to_string(),
        title,
        chars: text.chars().count(),
        truncated,
        text,
    })
}

fn extract_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract the main content of a document. Returns `(title, text)`.
pub fn extract_text(html: &str, markdown: bool) -> (Option<String>, String) {
    let doc = Html::parse_document(html);
    let title = extract_title(html);

    let container = pick_container(&doc);
    let mut out = String::new();
    walk(container, &mut out, markdown);
    let text = post_process(&out);
    (title, text)
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

/// Recursive renderer: text nodes as-is; elements by tag semantics.
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
            if markdown
                && (tag == "h1"
                    || tag == "h2"
                    || tag == "h3"
                    || tag == "h4"
                    || tag == "h5"
                    || tag == "h6")
            {
                let level = tag[1..].parse::<u8>().unwrap_or(1);
                out.push_str(&"#".repeat(level as usize));
                out.push(' ');
            }
            for child in node.children() {
                walk(child, out, markdown);
            }
            if markdown {
                match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "p" | "li" | "pre" | "blockquote"
                    | "tr" | "br" | "dt" | "dd" => {
                        out.push('\n');
                    }
                    _ => {}
                }
            } else {
                match tag {
                    "p" | "li" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6" | "pre" | "blockquote"
                    | "tr" | "br" => {
                        out.push('\n');
                    }
                    "td" | "th" => out.push_str(" | "),
                    _ => {}
                }
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
fn post_process(raw: &str) -> String {
    let mut lines: Vec<String> = raw
        .split('\n')
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    // Cap pathological line counts (e.g. giant code tables).
    if lines.len() > 10_000 {
        lines.truncate(10_000);
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn markdown_mode_keeps_links_and_headings() {
        let html = r#"<html><body><article>
            <h2>Intro</h2>
            <p>See <a href="https://rust-lang.org">Rust</a> website.</p>
        </article></body></html>"#;
        let (_, text) = extract_text(html, true);
        assert!(text.contains("## Intro"), "got: {text}");
        assert!(
            text.contains("[Rust](https://rust-lang.org)"),
            "got: {text}"
        );
    }

    #[test]
    fn post_process_collapses_whitespace() {
        let out = post_process("  a\n\n\n   b  \nc ");
        assert_eq!(out, "a\nb\nc");
    }
}
