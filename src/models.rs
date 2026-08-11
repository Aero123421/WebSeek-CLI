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
    /// True when content was dropped by any cap (bytes, characters or lines).
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
    /// User-Agent for engines backed by official APIs.
    ///
    /// Those endpoints ask, in their usage policies, to be told who is calling.
    /// Scraped endpoints keep the browser-like agent from the client, because
    /// they serve challenge pages to anything that looks automated; APIs get
    /// the truth. See `config::api_user_agent`.
    pub api_user_agent: String,
    /// Contact address for APIs with a "polite pool" (OpenAlex, NCBI).
    /// `None` means webseek makes no claim about who is calling.
    pub contact_email: Option<String>,
}

impl Default for SearchOpts {
    fn default() -> Self {
        Self {
            count: 5,
            lang: None,
            region: None,
            safe: false,
            api_user_agent: crate::config::api_user_agent(),
            contact_email: None,
        }
    }
}

impl SearchOpts {
    /// Attach webseek's honest identification to an API request.
    pub fn identify(
        &self,
        rb: reqwest::blocking::RequestBuilder,
    ) -> reqwest::blocking::RequestBuilder {
        rb.header(reqwest::header::USER_AGENT, self.api_user_agent.as_str())
    }
}

/// Options for a fetch/reader request.
#[derive(Debug, Clone)]
pub struct FetchOpts {
    /// Hard cap on the downloaded response body, in bytes.
    pub max_bytes: usize,
    /// Cap on the emitted text, in characters (applies to `--html` too).
    pub max_chars: usize,
    /// Emit raw HTML instead of extracted text.
    pub raw_html: bool,
    /// Keep light markdown formatting (headings, lists, links).
    pub markdown: bool,
}

impl Default for FetchOpts {
    fn default() -> Self {
        Self {
            max_bytes: crate::reader::DEFAULT_MAX_BYTES,
            max_chars: 20_000,
            raw_html: false,
            markdown: false,
        }
    }
}
