//! Data models shared across the crate.
//!
//! These structs define the stable JSON contract consumed by AI agents.
//! Field names are kept short and stable: adding a field is fine, renaming
//! is a breaking change (the last one was the 0.3.0 `fetch` reshape — see
//! CHANGELOG).

use serde::{Deserialize, Serialize};

/// Marks page/search content as attacker-reachable: an agent that picked a
/// URL from search results, or is reading a page's body, is consuming text
/// nobody at webseek wrote or vetted. It may contain instructions aimed at
/// an LLM reader ("ignore previous instructions..."). This value is
/// constant and always present so a caller can rely on the field existing
/// without reading documentation first — see README "Trust model".
pub const SOURCE_TRUST_UNTRUSTED: &str = "untrusted_external_content";

/// Reasons `FetchResult::truncated` can be true. More than one may apply.
pub mod truncation_reason {
    /// The download hit the byte cap before the response finished.
    pub const RESPONSE_BYTES: &str = "response_bytes";
    /// Extracted text was cut to fit `--max-chars`.
    pub const MAX_CHARS: &str = "max_chars";
    /// The line count was capped (pathological markup, e.g. giant tables).
    pub const LINE_LIMIT: &str = "line_limit";
}

/// One text search hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    /// Short excerpt, trimmed and single-line (context-friendly).
    pub snippet: String,
}

/// One image search hit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ImageResult {
    pub title: String,
    /// Direct URL of the full-size image.
    pub url: String,
    /// URL of the page that contains the image.
    pub page_url: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// File extension guessed from the URL, e.g. "jpg".
    pub format: String,
}

/// Result of fetching a single page.
///
/// `requested_url` and `final_url` are deliberately separate: they differ
/// whenever the server redirected, which matters both for trust (a redirect
/// can land on a different domain) and for resolving relative links.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchResult {
    /// The URL webseek was asked to fetch.
    pub requested_url: String,
    /// The URL the content actually came from, after redirects.
    pub final_url: String,
    /// HTTP status of the final response.
    pub status: u16,
    /// `Content-Type` of the final response, when present.
    pub content_type: Option<String>,
    pub title: Option<String>,
    /// Number of characters in `text`.
    pub chars: usize,
    /// True when `text` is incomplete for any reason in `truncation_reasons`.
    pub truncated: bool,
    /// Which cap(s) triggered truncation; see [`truncation_reason`]. Empty
    /// when `truncated` is false.
    pub truncation_reasons: Vec<String>,
    /// Always [`SOURCE_TRUST_UNTRUSTED`] — see its doc comment.
    pub source_trust: String,
    /// Unix timestamp (seconds) when the fetch completed.
    pub fetched_at: u64,
    /// Boilerplate-free main content (or raw HTML with `--html`).
    pub text: String,
}

/// Options passed to a search engine.
#[derive(Debug, Clone)]
pub struct SearchOpts {
    pub count: usize,
    pub lang: Option<String>,
    pub region: Option<String>,
    pub safe: bool,
}

/// Options for a fetch/reader request.
#[derive(Debug, Clone)]
pub struct FetchOpts {
    /// Hard cap on the downloaded response body, in bytes.
    pub max_bytes: usize,
    /// Cap on the extracted text, in characters.
    pub max_chars: usize,
    /// Emit raw HTML instead of extracted text.
    pub raw_html: bool,
    /// Keep light markdown formatting (headings, lists, links).
    pub markdown: bool,
}
