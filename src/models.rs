//! Data models shared across the crate.
//!
//! These structs define the stable JSON contract consumed by AI agents.
//! Field names are kept short and stable: adding a field is fine, renaming
//! is a breaking change.

use serde::{Deserialize, Serialize};

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
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct FetchResult {
    pub url: String,
    pub title: Option<String>,
    /// Number of characters in `text`.
    pub chars: usize,
    /// True when `text` was truncated by `--max-chars`.
    pub truncated: bool,
    /// Boilerplate-free main content.
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
