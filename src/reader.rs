//! Page fetching with boilerplate-free text extraction.
//!
//! Goal: give an AI agent the *substance* of a page with minimum tokens, and
//! be explicit about what happened to the bytes along the way — an agent
//! deciding whether to trust or re-fetch a page needs to know not just "was
//! this truncated" but *why*, and it needs a URL that reflects where the
//! content actually came from (redirects can land on a different host).
//!
//! Strategy, roughly "readability-lite":
//! 1. Strip navigation/scripts/ads (script, style, nav, aside, footer, ...).
//! 2. Score every semantic-container candidate (`article`, `main`,
//!    `[role=main]`, `#content`, `.content`) *and* every density-scored block
//!    parent, then take the best-scoring one — not just the first semantic
//!    match, which can be a sidebar or cookie box that happens to come first
//!    in document order.
//! 3. Walk the chosen subtree in document order, emitting headings/paragraphs/
//!    lists/tables as plain lines (or light markdown with `--markdown`);
//!    `<pre>` content is preserved verbatim instead of being whitespace-
//!    collapsed, so code samples keep their indentation.
//! 4. Collapse whitespace, cap at `--max-chars`, and report every cap that
//!    actually fired in `truncation_reasons`.

use std::collections::HashSet;

use ego_tree::NodeRef;
use scraper::node::Node;
use scraper::{Html, Selector};
use url::Url;

use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{truncation_reason, FetchOpts, FetchResult, SOURCE_TRUST_UNTRUSTED};
use crate::net;
use crate::text::truncate_chars;

/// Default hard cap on downloaded body bytes (protects memory and bandwidth).
pub const DEFAULT_MAX_BYTES: usize = 8 * 1024 * 1024;

/// Hard cap on emitted lines — protects against pathological markup (a giant
/// generated table, a minified blob that parses as one element per line).
const MAX_LINES: usize = 10_000;

/// Fetch `url` and extract the main content.
pub fn fetch(http: &Http, url: &str, opts: &FetchOpts) -> Result<FetchResult> {
    let requested = net::parse_checked(url, http.policy())?;
    let resp = http.get(requested.clone())?;
    let status = resp.status().as_u16();
    if !resp.status().is_success() {
        return Err(http.status_error(&resp, status));
    }

    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    if !is_extractable_content_type(content_type.as_deref()) {
        return Err(Error::UnsupportedContent {
            content_type: content_type.unwrap_or_default(),
        });
    }

    let raw = crate::http::read_capped(resp, opts.max_bytes)?;
    let mut reasons = Vec::new();
    if raw.truncated {
        reasons.push(truncation_reason::RESPONSE_BYTES.to_string());
    }
    let decoded = crate::http::decode_text(&raw.bytes, raw.content_type.as_deref());
    let final_url = Url::parse(&raw.final_url).unwrap_or_else(|_| requested.clone());

    let (title, text, total_chars) = if opts.raw_html {
        let title = extract_title(&decoded);
        let total = decoded.chars().count();
        (title, decoded, total)
    } else {
        let (title, extracted, line_capped) = extract_text(&decoded, opts.markdown, &final_url);
        if line_capped {
            reasons.push(truncation_reason::LINE_LIMIT.to_string());
        }
        let total = extracted.chars().count();
        (title, extracted, total)
    };

    let (text, chars) = if !opts.raw_html && total_chars > opts.max_chars {
        reasons.push(truncation_reason::MAX_CHARS.to_string());
        let cut = truncate_chars(&text, opts.max_chars);
        let n = cut.chars().count();
        (cut, n)
    } else {
        (text, total_chars)
    };

    Ok(FetchResult {
        requested_url: url.to_string(),
        final_url: final_url.to_string(),
        status,
        content_type: raw.content_type,
        title,
        chars,
        truncated: !reasons.is_empty(),
        truncation_reasons: reasons,
        source_trust: SOURCE_TRUST_UNTRUSTED.to_string(),
        fetched_at: now_unix(),
        text,
    })
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Content types worth running through an HTML/text extractor. Everything
/// else (PDF, images, archives, JSON, ...) is a typed error instead of being
/// lossily decoded as if it were text.
fn is_extractable_content_type(ct: Option<&str>) -> bool {
    match ct {
        // Many servers omit Content-Type or send a wrong one; be lenient
        // rather than reject pages we can plausibly still parse as HTML.
        None => true,
        Some(raw) => {
            let base = raw
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            base.is_empty()
                || matches!(
                    base.as_str(),
                    "text/html" | "application/xhtml+xml" | "text/plain"
                )
        }
    }
}

fn extract_title(html: &str) -> Option<String> {
    let doc = Html::parse_document(html);
    let sel = Selector::parse("title").ok()?;
    doc.select(&sel)
        .next()
        .map(|t| t.text().collect::<String>().trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Extract the main content of a document. Returns `(title, text, line_capped)`.
pub fn extract_text(html: &str, markdown: bool, base: &Url) -> (Option<String>, String, bool) {
    let doc = Html::parse_document(html);
    let title = extract_title(html);

    let container = pick_container(&doc);
    let ctx = RenderCtx { markdown, base };
    let mut renderer = Renderer::default();
    walk(container, &mut renderer, &ctx);
    let (text, line_capped) = renderer.finish();
    (title, text, line_capped)
}

/// Non-content elements we never render or score.
///
/// `header` is deliberately *not* here: HTML5 uses `<header>` both for
/// site-wide chrome (logo/nav) and for an article's own title/byline block,
/// and the latter is real content. Site-chrome headers are instead caught by
/// [`class_marks_excluded`] via their class/id (`site-header`, `masthead`, ...).
fn is_excluded(tag: &str, e: &scraper::node::Element) -> bool {
    const TAGS: &[&str] = &[
        "script", "style", "noscript", "template", "svg", "canvas", "iframe", "form", "nav",
        "aside", "footer",
    ];
    if TAGS.contains(&tag) {
        return true;
    }
    if e.attr("hidden").is_some() || e.attr("aria-hidden") == Some("true") {
        return true;
    }
    if let Some(class) = e.attr("class") {
        if class_marks_excluded(class) {
            return true;
        }
    }
    if let Some(id) = e.attr("id") {
        if class_marks_excluded(id) {
            return true;
        }
    }
    false
}

/// Exact class/id tokens (word-boundary, via whitespace split) that mark an
/// element as chrome/noise. Kept separate from [`SUBSTRING_MARKERS`] because
/// short tokens like `ad` are only safe to match as a *whole* token —
/// matching them as a substring would exclude anything containing "ad"
/// (`shadow`, `header`, `load`, `gradient`, ...), which is a real bug seen
/// elsewhere in ad-detection code that does exactly that.
const EXACT_TOKENS: &[&str] = &[
    "ad", "ads", "advert", "banner", "cookie", "popup", "modal", "share", "comment",
];

/// Substrings safe to match anywhere in a class/id string because they are
/// long and specific enough not to collide with unrelated words. This is
/// what catches real-world compound classes like `cookie-banner` or
/// `comments-section`, which the old exact-token-only check missed entirely
/// (`"cookie-banner".split_whitespace()` yields the single token
/// `"cookie-banner"`, which is not `== "cookie"`).
const SUBSTRING_MARKERS: &[&str] = &[
    "cookie",
    "comment",
    "advert",
    "banner",
    "popup",
    "modal",
    "newsletter",
    "social-share",
    "sponsor",
    // Site-chrome headers specifically (see `is_excluded`'s doc comment) —
    // an in-article `<header>` almost never carries these.
    "site-header",
    "masthead",
    "global-header",
    "page-header",
    "sitewide-header",
];

fn class_marks_excluded(attr: &str) -> bool {
    let lower = attr.to_ascii_lowercase();
    if lower.split_whitespace().any(|t| EXACT_TOKENS.contains(&t)) {
        return true;
    }
    SUBSTRING_MARKERS.iter().any(|m| lower.contains(m))
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

/// A small, fixed bonus for containers found via the semantic-selector pass
/// (`article`, `main`, `[role=main]`, `#content`, `.content`): enough to win
/// close calls against a similarly-sized generic `<div>`, never enough to
/// beat a block that is clearly the real content by a wide margin.
const SEMANTIC_BONUS: i64 = 200;
/// Minimum score for *any* candidate to be trusted over just using `<body>`.
const MIN_CONTAINER_SCORE: i64 = 200;

/// Choose the main container without mutating the document.
///
/// Every semantic-selector match *and* every density-scored block parent is
/// scored, and the best one wins — not just the first semantic match, which
/// can be a sidebar, a cookie-consent box, or an empty template slot that
/// happens to come first in document order while the real `<article>`
/// appears later.
fn pick_container(doc: &Html) -> NodeRef<'_, Node> {
    let semantic_sel = Selector::parse("article, main, [role='main'], #content, .content")
        .unwrap_or_else(|_| unreachable!("static"));
    let block_sel = Selector::parse("p, li, pre, blockquote, h1, h2, h3, h4, h5, h6, td")
        .unwrap_or_else(|_| unreachable!("static"));
    let body = doc
        .select(&Selector::parse("body").unwrap_or_else(|_| unreachable!("static")))
        .next()
        .unwrap_or_else(|| doc.root_element());

    let mut seen = HashSet::new();
    let mut candidates: Vec<(NodeRef<'_, Node>, bool)> = Vec::new();

    for el in doc.select(&semantic_sel) {
        if is_excluded(el.value().name(), el.value()) {
            continue;
        }
        if seen.insert(el.id()) {
            candidates.push((*el, true));
        }
    }
    for block in body.select(&block_sel) {
        if in_excluded(*block) {
            continue;
        }
        let Some(parent) = block.parent() else {
            continue;
        };
        if seen.insert(parent.id()) {
            candidates.push((parent, false));
        }
    }

    let mut best: Option<(i64, NodeRef<'_, Node>)> = None;
    for (node, is_semantic) in candidates {
        let score = text_len(&node) as i64 + if is_semantic { SEMANTIC_BONUS } else { 0 };
        if best.map(|(s, _)| score > s).unwrap_or(true) {
            best = Some((score, node));
        }
    }
    match best {
        Some((score, node)) if score >= MIN_CONTAINER_SCORE => node,
        _ => *body,
    }
}

/// Sum of trimmed text length under `node`, excluding anything inside an
/// excluded subtree — an ad/comment block nested in an otherwise-good parent
/// no longer inflates that parent's score.
fn text_len(node: &NodeRef<'_, Node>) -> usize {
    node.descendants()
        .filter(|n| !in_excluded(*n))
        .filter_map(|n| n.value().as_text())
        .map(|t| t.text.trim().chars().count())
        .sum()
}

struct RenderCtx<'a> {
    markdown: bool,
    base: &'a Url,
}

/// Accumulates rendered output as committed lines. Most lines are collapsed
/// (leading/trailing space trimmed, blank lines dropped) exactly like the
/// rest of the extracted text; lines from `<pre>` are marked `verbatim` and
/// bypass that collapsing so code indentation survives.
#[derive(Default)]
struct Renderer {
    lines: Vec<(String, bool)>,
    cur: String,
}

impl Renderer {
    fn commit(&mut self) {
        let text = std::mem::take(&mut self.cur);
        let trimmed = text.trim();
        if !trimmed.is_empty() {
            self.lines.push((trimmed.to_string(), false));
        }
    }

    fn commit_verbatim(&mut self, text: &str) {
        self.commit();
        for line in text.trim_matches('\n').split('\n') {
            self.lines.push((line.to_string(), true));
        }
    }

    fn finish(mut self) -> (String, bool) {
        self.commit();
        let mut out = Vec::with_capacity(self.lines.len().min(MAX_LINES));
        let mut capped = false;
        for (text, _verbatim) in self.lines {
            if out.len() >= MAX_LINES {
                capped = true;
                break;
            }
            out.push(text);
        }
        (out.join("\n"), capped)
    }
}

const BLOCK_TAGS: &[&str] = &[
    "h1",
    "h2",
    "h3",
    "h4",
    "h5",
    "h6",
    "p",
    "li",
    "blockquote",
    "tr",
    "br",
    "dt",
    "dd",
];

/// Recursive renderer: text nodes as-is; elements by tag semantics.
fn walk(node: NodeRef<'_, Node>, r: &mut Renderer, ctx: &RenderCtx<'_>) {
    match node.value() {
        Node::Text(t) => r.cur.push_str(&t.text),
        Node::Element(e) => {
            let tag = e.name();
            if is_excluded(tag, e) {
                return;
            }

            if tag == "pre" {
                let raw = collect_verbatim_text(node);
                if !raw.trim().is_empty() {
                    let block = if ctx.markdown {
                        format!("```\n{}\n```", raw.trim_matches('\n'))
                    } else {
                        raw
                    };
                    r.commit_verbatim(&block);
                }
                return;
            }

            if ctx.markdown && tag == "a" {
                if let Some(href) = e.attr("href") {
                    let inner = inline_text(node, ctx);
                    if !inner.is_empty() {
                        push_markdown_link(&mut r.cur, &inner, href, ctx.base);
                        return;
                    }
                }
            }
            if ctx.markdown && matches!(tag, "strong" | "b" | "em" | "i") {
                let marker = if matches!(tag, "strong" | "b") {
                    "**"
                } else {
                    "_"
                };
                let inner = inline_text(node, ctx);
                if !inner.is_empty() {
                    r.cur.push_str(marker);
                    r.cur.push_str(&inner);
                    r.cur.push_str(marker);
                }
                return;
            }

            if ctx.markdown {
                match tag {
                    "h1" | "h2" | "h3" | "h4" | "h5" | "h6" => {
                        let level = tag[1..].parse::<u8>().unwrap_or(1);
                        r.cur.push_str(&"#".repeat(level as usize));
                        r.cur.push(' ');
                    }
                    "li" => r.cur.push_str("- "),
                    "blockquote" => r.cur.push_str("> "),
                    _ => {}
                }
            }

            for child in node.children() {
                walk(child, r, ctx);
            }

            if tag == "td" || tag == "th" {
                r.cur.push_str(" | ");
            } else if BLOCK_TAGS.contains(&tag) {
                r.commit();
            }
        }
        Node::Document
        | Node::Fragment
        | Node::Comment(_)
        | Node::Doctype(_)
        | Node::ProcessingInstruction(_) => {}
    }
}

/// Render just the *content* of `node` (not `node` itself) for use inside a
/// link or emphasis wrapper: no block commits, so nested formatting composes
/// into one inline run instead of being split across lines.
fn inline_text(node: NodeRef<'_, Node>, ctx: &RenderCtx<'_>) -> String {
    let mut out = String::new();
    for child in node.children() {
        inline_text_into(child, &mut out, ctx);
    }
    out.trim().to_string()
}

fn inline_text_into(node: NodeRef<'_, Node>, out: &mut String, ctx: &RenderCtx<'_>) {
    match node.value() {
        Node::Text(t) => out.push_str(&t.text),
        Node::Element(e) => {
            let tag = e.name();
            if is_excluded(tag, e) {
                return;
            }
            if tag == "br" {
                out.push(' ');
                return;
            }
            if ctx.markdown && tag == "a" {
                if let Some(href) = e.attr("href") {
                    let inner = inline_text(node, ctx);
                    if !inner.is_empty() {
                        push_markdown_link(out, &inner, href, ctx.base);
                        return;
                    }
                }
            }
            if ctx.markdown && matches!(tag, "strong" | "b" | "em" | "i") {
                let marker = if matches!(tag, "strong" | "b") {
                    "**"
                } else {
                    "_"
                };
                let inner = inline_text(node, ctx);
                if !inner.is_empty() {
                    out.push_str(marker);
                    out.push_str(&inner);
                    out.push_str(marker);
                }
                return;
            }
            for child in node.children() {
                inline_text_into(child, out, ctx);
            }
        }
        _ => {}
    }
}

fn push_markdown_link(out: &mut String, text: &str, href: &str, base: &Url) {
    match resolve_link_href(href, base) {
        Some(url) => {
            out.push('[');
            out.push_str(text);
            out.push_str("](");
            out.push_str(url.as_str());
            out.push(')');
        }
        // Unresolvable or unsafe-scheme target (javascript:/data:/file:/...):
        // keep the visible text, drop the link so it can never be followed.
        None => out.push_str(text),
    }
}

/// Resolve a possibly-relative `href` against `base`, accepting only the
/// result if it is `http`/`https` — `javascript:`, `data:`, `file:` and any
/// other scheme are never turned into a followable markdown link.
fn resolve_link_href(href: &str, base: &Url) -> Option<Url> {
    let trimmed = href.trim();
    if trimmed.is_empty() {
        return None;
    }
    let joined = base.join(trimmed).ok()?;
    matches!(joined.scheme(), "http" | "https").then_some(joined)
}

fn collect_verbatim_text(node: NodeRef<'_, Node>) -> String {
    let mut out = String::new();
    collect_verbatim_into(node, &mut out);
    out
}

fn collect_verbatim_into(node: NodeRef<'_, Node>, out: &mut String) {
    match node.value() {
        Node::Text(t) => out.push_str(&t.text),
        Node::Element(e) => {
            if is_excluded(e.name(), e) {
                return;
            }
            if e.name() == "br" {
                out.push('\n');
            }
            for c in node.children() {
                collect_verbatim_into(c, out);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://example.com/articles/rust").unwrap()
    }

    fn extract(html: &str, markdown: bool) -> (Option<String>, String) {
        let (title, text, _capped) = extract_text(html, markdown, &base());
        (title, text)
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
        let (title, text) = extract(html, false);
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
        let (_, text) = extract(html, false);
        assert!(text.contains("Lorem ipsum"));
        assert!(!text.contains("Site Footer"));
    }

    #[test]
    fn best_scoring_container_wins_even_when_a_smaller_one_is_first() {
        // A `.content` sidebar comes first in document order but is small;
        // the real `<article>` is bigger and must win despite being later.
        let filler = "Real article body text that goes on for quite a while. ".repeat(20);
        let html = format!(
            r#"<html><body>
                <div class="content"><p>Related links sidebar.</p></div>
                <article><p>{filler}</p></article>
            </body></html>"#
        );
        let (_, text) = extract(&html, false);
        assert!(text.contains("Real article body"));
        assert!(!text.contains("Related links sidebar"));
    }

    #[test]
    fn excluded_descendant_text_does_not_inflate_parent_score() {
        // Without the text_len fix, the ad's text would count toward the
        // otherwise-thin wrapper div's density score.
        let ad_text = "AD ".repeat(200);
        let real = "Genuine article content worth reading here and there. ".repeat(10);
        let html = format!(
            r#"<html><body>
                <div><div class="ad">{ad_text}</div><p>tiny</p></div>
                <article><p>{real}</p></article>
            </body></html>"#
        );
        let (_, text) = extract(&html, false);
        assert!(text.contains("Genuine article content"));
        assert!(!text.contains("AD AD"));
    }

    #[test]
    fn in_article_header_with_byline_is_kept() {
        let html = r#"<html><body><article>
            <header><h1>Big News</h1><p>By Jane Doe, 2026</p></header>
            <p>The rest of the story follows and is reasonably long so the
               container scores well above the density threshold used here.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, false);
        assert!(text.contains("Big News"), "got: {text}");
        assert!(text.contains("Jane Doe"), "got: {text}");
    }

    #[test]
    fn site_chrome_header_class_is_excluded() {
        let html = r#"<html><body>
            <header class="site-header"><a href="/">Logo</a><nav>Menu</nav></header>
            <article><p>Actual page content that is long enough to win the density check easily here.</p></article>
        </body></html>"#;
        let (_, text) = extract(html, false);
        assert!(!text.contains("Logo"));
        assert!(text.contains("Actual page content"));
    }

    #[test]
    fn compound_hyphenated_noise_classes_are_excluded() {
        let html = r#"<html><body><article>
            <div class="cookie-banner">Accept cookies?</div>
            <div class="comments-section">Great post!</div>
            <div class="advertisement-container">Buy stuff</div>
            <p>Real content that is long enough to be the winning container by density scoring here.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, false);
        assert!(!text.contains("Accept cookies"));
        assert!(!text.contains("Great post"));
        assert!(!text.contains("Buy stuff"));
        assert!(text.contains("Real content"));
    }

    #[test]
    fn markdown_mode_keeps_links_and_headings() {
        let html = r#"<html><body><article>
            <h2>Intro</h2>
            <p>See <a href="https://rust-lang.org">Rust</a> website.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(text.contains("## Intro"), "got: {text}");
        assert!(
            text.contains("[Rust](https://rust-lang.org/)"),
            "got: {text}"
        );
    }

    #[test]
    fn markdown_relative_links_resolve_against_final_url() {
        let html = r#"<html><body><article>
            <p><a href="/docs">docs</a> and <a href="../up">up</a>.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(text.contains("[docs](https://example.com/docs)"), "{text}");
        assert!(text.contains("[up](https://example.com/up)"), "{text}");
    }

    #[test]
    fn markdown_rejects_dangerous_link_schemes() {
        let html = r#"<html><body><article>
            <p><a href="javascript:alert(1)">click</a> and
               <a href="data:text/html,x">data</a> and
               <a href="file:///etc/passwd">file</a>.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(
            !text.contains("]("),
            "no markdown link syntax should survive: {text}"
        );
        assert!(text.contains("click"));
        assert!(text.contains("data"));
        assert!(text.contains("file"));
    }

    #[test]
    fn markdown_lists_and_blockquotes_get_prefixes() {
        let html = r#"<html><body><article>
            <ul><li>First</li><li>Second</li></ul>
            <blockquote>Wise words.</blockquote>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(text.contains("- First"), "{text}");
        assert!(text.contains("- Second"), "{text}");
        assert!(text.contains("> Wise words."), "{text}");
    }

    #[test]
    fn markdown_emphasis_is_rendered() {
        let html = r#"<html><body><article>
            <p>This is <strong>bold</strong> and <em>italic</em> text that is
               long enough for the container to win on density scoring.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(text.contains("**bold**"), "{text}");
        assert!(text.contains("_italic_"), "{text}");
    }

    #[test]
    fn markdown_table_cells_are_separated() {
        let html = r#"<html><body><article>
            <table><tr><td>A</td><td>B</td></tr></table>
            <p>Enough filler text around the table to make this container clearly win the density check.</p>
        </article></body></html>"#;
        let (_, text) = extract(html, true);
        assert!(text.contains("A | B"), "{text}");
    }

    #[test]
    fn pre_blocks_preserve_indentation() {
        let html = "<html><body><article><p>Example code below, with enough \
            surrounding text for this container to win on density scoring \
            against any other candidate on the page.</p>\
            <pre>fn main() {\n    println!(\"hi\");\n}</pre></article></body></html>";
        let (_, text) = extract(html, false);
        assert!(
            text.contains("    println!(\"hi\");"),
            "indentation lost: {text:?}"
        );
    }

    #[test]
    fn pre_blocks_become_fenced_code_in_markdown_mode() {
        let html = "<html><body><article><p>Some prose here that is long \
            enough to make this the winning container over any alternative \
            on the page for sure.</p>\
            <pre>let x = 1;\n  let y = 2;</pre></article></body></html>";
        let (_, text) = extract(html, true);
        assert!(text.contains("```"), "{text}");
        assert!(text.contains("  let y = 2;"), "{text}");
    }

    #[test]
    fn post_process_collapses_whitespace_but_not_verbatim_blocks() {
        let mut r = Renderer::default();
        r.cur.push_str("  a  ");
        r.commit();
        r.cur.push_str("   b  ");
        r.commit();
        r.commit_verbatim("  keep\n  this  \n");
        let (text, capped) = r.finish();
        assert_eq!(text, "a\nb\n  keep\n  this  ");
        assert!(!capped);
    }

    #[test]
    fn line_cap_is_reported() {
        let mut html = String::from("<html><body><article>");
        for i in 0..(MAX_LINES + 50) {
            html.push_str(&format!(
                "<p>line {i} of filler text to pad it out a bit</p>"
            ));
        }
        html.push_str("</article></body></html>");
        let (_, text, capped) = extract_text(&html, false, &base());
        assert!(capped);
        assert_eq!(text.lines().count(), MAX_LINES);
    }

    #[test]
    fn raw_html_mode_is_selected_by_caller_not_this_module() {
        // extract_text always does readability extraction; raw-HTML mode is
        // handled directly in `fetch()`. This test just documents the split.
        let (_, text, _) = extract_text("<html><body><p>hi</p></body></html>", false, &base());
        assert_eq!(text, "hi");
    }
}
