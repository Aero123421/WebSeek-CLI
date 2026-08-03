//! Image search backends (API-key-free) plus a download helper.
//!
//! - **Bing Images**: scrapes `bing.com/images/search`. Full-size URLs are
//!   embedded as JSON in the `m` attribute of `a.iusc` elements.
//! - **DuckDuckGo Images**: fetches a one-time `vqd` token from the image
//!   page, then queries the JSON endpoint `duckduckgo.com/i.js`.
//!
//! Both parsers are pure functions, unit-tested with fixtures.

use std::path::Path;

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::engines::{dedupe_by_url, ImageEngine};
use crate::error::{Error, Result};
use crate::models::ImageResult;
use crate::text::sanitize_name;

/// Default cap for downloaded images when `--max-bytes` is not given.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;

pub struct BingImages {
    /// Endpoint base; overridable for tests and mirrors.
    base: String,
}

pub struct DuckDuckGoImages {
    /// Image page (vqd token source) and JSON endpoint bases.
    page_base: String,
    json_base: String,
}

const BING_IMAGES_URL: &str = "https://www.bing.com/images/search";
const DDG_IMAGES_PAGE: &str = "https://duckduckgo.com/";
const DDG_IMAGES_JSON: &str = "https://duckduckgo.com/i.js";

impl Default for BingImages {
    fn default() -> Self {
        Self {
            base: BING_IMAGES_URL.to_string(),
        }
    }
}

impl BingImages {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl Default for DuckDuckGoImages {
    fn default() -> Self {
        Self {
            page_base: DDG_IMAGES_PAGE.to_string(),
            json_base: DDG_IMAGES_JSON.to_string(),
        }
    }
}

impl DuckDuckGoImages {
    pub fn with_bases(page_base: impl Into<String>, json_base: impl Into<String>) -> Self {
        Self {
            page_base: page_base.into(),
            json_base: json_base.into(),
        }
    }
}

impl ImageEngine for BingImages {
    fn name(&self) -> &'static str {
        "bing"
    }

    fn search(
        &self,
        client: &Client,
        query: &str,
        count: usize,
        safe: bool,
    ) -> Result<Vec<ImageResult>> {
        let mut params = vec![("q", query), ("form", "HDRSC2")];
        if safe {
            params.push(("adlt", "strict"));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = crate::http::send_with_retry(&client.get(url))
            .map_err(|e| Error::Network(format!("bing images request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        let mut results = parse_bing_html(&body);
        results.truncate(count);
        Ok(dedupe_by_url(results, |r| &r.url))
    }
}

impl ImageEngine for DuckDuckGoImages {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn search(
        &self,
        client: &Client,
        query: &str,
        count: usize,
        safe: bool,
    ) -> Result<Vec<ImageResult>> {
        let vqd = fetch_vqd(client, query, safe, &self.page_base)?;
        let mut params = vec![("q", query), ("o", "json"), ("vqd", vqd.as_str())];
        if safe {
            params.push(("p", "1"));
        }
        let url = Url::parse_with_params(&self.json_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = crate::http::send_with_retry(&client.get(url))
            .map_err(|e| Error::Network(format!("duckduckgo images request failed: {e}")))?;
        let status = resp.status();
        if status.as_u16() == 202 || status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(Error::RateLimited(format!(
                "duckduckgo answered HTTP {status} (retry later or lower request rate)"
            )));
        }
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        let mut results = parse_ddg_json(&body)?;
        results.truncate(count);
        Ok(dedupe_by_url(results, |r| &r.url))
    }
}

/// Grab the one-time `vqd` token DDG requires for its image JSON API.
fn fetch_vqd(client: &Client, query: &str, safe: bool, page_base: &str) -> Result<String> {
    let mut params = vec![("q", query), ("iax", "images"), ("ia", "images")];
    if safe {
        params.push(("p", "1"));
    }
    let url = Url::parse_with_params(page_base, &params)
        .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
    let resp = crate::http::send_with_retry(&client.get(url))
        .map_err(|e| Error::Network(format!("duckduckgo vqd request failed: {e}")))?;
    let status = resp.status();
    if status.as_u16() == 202 || status.as_u16() == 429 || status.as_u16() == 403 {
        return Err(Error::RateLimited(format!(
            "duckduckgo answered HTTP {status} while fetching vqd token"
        )));
    }
    if !status.is_success() {
        return Err(Error::Http(status.as_u16()));
    }
    let body = resp.text().map_err(Error::from)?;
    extract_vqd(&body).ok_or_else(|| {
        Error::Parse(
            "could not locate vqd token in duckduckgo image page (page layout changed?)".into(),
        )
    })
}

/// Extract `vqd="..."` (double or single quotes) from the DDG page.
pub fn extract_vqd(html: &str) -> Option<String> {
    for (pat, terminator) in [(r#"vqd=""#, '"'), (r#"vqd='"#, '\'')] {
        if let Some(idx) = html.find(pat) {
            let start = idx + pat.len();
            let rest = &html[start..];
            if let Some(end) = rest.find(terminator) {
                let token = &rest[..end];
                if !token.is_empty() && token.len() < 64 {
                    return Some(token.to_string());
                }
            }
        }
    }
    None
}

/// Pure parser for Bing Images HTML (`a.iusc` elements with JSON `m` attrs).
pub fn parse_bing_html(html: &str) -> Vec<ImageResult> {
    let doc = Html::parse_document(html);
    let iusc_sel = Selector::parse("a.iusc").unwrap_or_else(|_| unreachable!("static"));

    let mut out = Vec::new();
    for a in doc.select(&iusc_sel) {
        let Some(m) = a.value().attr("m") else {
            continue;
        };
        let Ok(v) = serde_json::from_str::<Value>(m) else {
            continue;
        };
        let Some(url) = v.get("murl").and_then(|u| u.as_str()) else {
            continue;
        };
        // Live Bing puts the title in the `t` key; fall back to the img alt.
        let title = v
            .get("t")
            .and_then(|t| t.as_str())
            .filter(|t| !t.is_empty())
            .map(str::to_string)
            .or_else(|| {
                a.select(&Selector::parse("img").unwrap_or_else(|_| unreachable!("static")))
                    .next()
                    .and_then(|img| img.value().attr("alt"))
                    .map(|alt| alt.trim().to_string())
            })
            .unwrap_or_default();
        out.push(ImageResult {
            title,
            url: url.to_string(),
            page_url: v
                .get("purl")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string(),
            width: v.get("murlw").and_then(|n| n.as_u64()).map(|n| n as u32),
            height: v.get("murlh").and_then(|n| n.as_u64()).map(|n| n as u32),
            format: format_from_url(url),
        });
    }
    out
}

/// Pure parser for the DDG `i.js` JSON endpoint.
pub fn parse_ddg_json(body: &str) -> Result<Vec<ImageResult>> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("i.js response is not JSON: {e}")))?;
    let Some(results) = v.get("results").and_then(|r| r.as_array()) else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for item in results {
        let Some(url) = item.get("image").and_then(|u| u.as_str()) else {
            continue;
        };
        out.push(ImageResult {
            title: item
                .get("title")
                .and_then(|t| t.as_str())
                .unwrap_or_default()
                .to_string(),
            url: url.to_string(),
            page_url: item
                .get("url")
                .and_then(|u| u.as_str())
                .unwrap_or_default()
                .to_string(),
            width: item.get("width").and_then(|n| n.as_u64()).map(|n| n as u32),
            height: item
                .get("height")
                .and_then(|n| n.as_u64())
                .map(|n| n as u32),
            format: format_from_url(url),
        });
    }
    Ok(out)
}

fn format_from_url(url: &str) -> String {
    let path = Url::parse(url)
        .map(|u| u.path().to_string())
        .unwrap_or_else(|_| url.to_string());
    let ext = std::path::Path::new(&path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "avif" | "bmp" | "ico" => ext,
        _ => String::new(),
    }
}

/// Download image results into `dir`, `limit` at most, skipping anything
/// larger than `max_bytes`. Returns saved paths.
pub fn download(
    client: &Client,
    results: &[ImageResult],
    dir: &Path,
    limit: usize,
    max_bytes: usize,
) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Config(format!("cannot create download dir {}: {e}", dir.display())))?;
    let mut saved = Vec::new();
    for (i, img) in results.iter().take(limit).enumerate() {
        // Skip by Content-Length before downloading when possible.
        if let Some(len) = client
            .get(&img.url)
            .send()
            .ok()
            .and_then(|r| r.content_length())
        {
            if len > max_bytes as u64 {
                eprintln!(
                    "[webseek] skipping {} ({} bytes > {} byte limit)",
                    img.url, len, max_bytes
                );
                continue;
            }
        }
        let resp = client
            .get(&img.url)
            .send()
            .map_err(|e| Error::Network(format!("download {} failed: {e}", img.url)))?;
        if !resp.status().is_success() {
            continue; // skip dead links, keep going
        }
        let bytes = resp.bytes().map_err(Error::from)?;
        if bytes.len() > max_bytes {
            eprintln!(
                "[webseek] skipping {} ({} bytes > {} byte limit)",
                img.url,
                bytes.len(),
                max_bytes
            );
            continue;
        }
        let ext = if img.format.is_empty() {
            "img"
        } else {
            &img.format
        };
        let name = sanitize_name(&img.title);
        let filename = dir.join(format!("{:04}_{name}.{ext}", i + 1));
        std::fs::write(&filename, &bytes)
            .map_err(|e| Error::Network(format!("cannot write {}: {e}", filename.display())))?;
        saved.push(filename.display().to_string());
    }
    Ok(saved)
}

#[cfg(test)]
mod tests {
    use super::*;

    const BING_FIXTURE: &str = r#"<html><body>
      <a class="iusc" style="" m="{&quot;murl&quot;:&quot;https://cdn.example.com/photo.jpg&quot;,&quot;turl&quot;:&quot;https://cdn.example.com/thumb.jpg&quot;,&quot;murlw&quot;:1920,&quot;murlh&quot;:1080,&quot;purl&quot;:&quot;https://blog.example.com/post&quot;,&quot;t&quot;:&quot;Mountain sunset&quot;}">
        <img alt="alt fallback" src="https://cdn.example.com/thumb.jpg">
      </a>
      <a class="iusc" m="{&quot;murl&quot;:&quot;https://cdn.example.com/graphic.png&quot;,&quot;turlw&quot;:300}">
        <img alt="Logo">
      </a>
      <div class="masonry">not an image</div>
    </body></html>"#;

    #[test]
    fn parses_bing_images() {
        let results = parse_bing_html(BING_FIXTURE);
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].url, "https://cdn.example.com/photo.jpg");
        // `t` key wins over the img alt.
        assert_eq!(results[0].title, "Mountain sunset");
        assert_eq!(results[0].width, Some(1920));
        assert_eq!(results[0].height, Some(1080));
        assert_eq!(results[0].page_url, "https://blog.example.com/post");
        assert_eq!(results[0].format, "jpg");
        assert_eq!(results[1].width, None);
        assert_eq!(results[1].format, "png");
        // No `t` key -> img alt fallback.
        assert_eq!(results[1].title, "Logo");
    }

    #[test]
    fn extracts_vqd_token() {
        assert_eq!(
            extract_vqd(r#"<html>var vqd="12345-abc";</html>"#).as_deref(),
            Some("12345-abc")
        );
        assert_eq!(extract_vqd(r#"vqd='xyz789'"#).as_deref(), Some("xyz789"));
        assert_eq!(extract_vqd("no token here"), None);
    }

    #[test]
    fn parses_ddg_json() {
        let body = r#"{"results":[
            {"image":"https://cdn.example.com/a.jpg","title":"A","url":"https://page.example.com/a","width":800,"height":600},
            {"image":"https://cdn.example.com/b.png","title":"B"}
        ]}"#;
        let results = parse_ddg_json(body).unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].width, Some(800));
        assert_eq!(results[1].format, "png");
    }

    #[test]
    fn format_from_url_works() {
        assert_eq!(format_from_url("https://x.com/a.JPG?w=1"), "jpg");
        assert_eq!(format_from_url("https://x.com/a.webp"), "webp");
        assert_eq!(format_from_url("https://x.com/redirect?to=/a.jpg"), "");
    }
}
