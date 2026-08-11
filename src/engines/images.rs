//! Image search backends (API-key-free) plus a download helper.
//!
//! - **Bing Images**: scrapes `bing.com/images/search`. Full-size URLs are
//!   embedded as JSON in the `m` attribute of `a.iusc` elements.
//! - **DuckDuckGo Images**: fetches a one-time `vqd` token from the image
//!   page, then queries the JSON endpoint `duckduckgo.com/i.js`.
//!
//! Both parsers are pure functions, unit-tested with fixtures.

use std::io::Read;
use std::path::Path;
use std::sync::Mutex;

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::engines::{dedupe_and_truncate, ImageEngine};
use crate::error::{Error, Result};
use crate::models::ImageResult;
use crate::pace::Pacer;
use crate::robots::RobotsChecker;
use crate::text::sanitize_name;

/// Default cap for downloaded images, and the default for the config's
/// `image_max_bytes`. One constant so the two cannot drift apart.
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
        let status = resp.status();
        if matches!(status.as_u16(), 202 | 403 | 429) {
            return Err(Error::RateLimited(format!(
                "bing images answered HTTP {status} (retry later or lower request rate)"
            )));
        }
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        // Without this the text engine reported challenges but the image
        // engine silently returned zero results, so fallback never fired.
        if crate::engines::looks_like_challenge(&body) {
            return Err(Error::RateLimited(
                "bing images served a bot-challenge page instead of results".into(),
            ));
        }
        Ok(dedupe_and_truncate(parse_bing_html(&body), count, |r| {
            &r.url
        }))
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
        Ok(dedupe_and_truncate(parse_ddg_json(&body)?, count, |r| {
            &r.url
        }))
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

/// Everything [`download`] needs beyond the results themselves.
pub struct DownloadCtx<'a> {
    pub client: &'a Client,
    /// Shared politeness limiter; downloads are upstream requests too.
    pub pacer: &'a Pacer,
    /// Consulted when `--respect-robots` is on, exactly like page fetches.
    pub robots: Option<&'a Mutex<RobotsChecker>>,
    pub limit: usize,
    pub max_bytes: usize,
}

/// Download image results into `dir`, at most `ctx.limit` of them, skipping
/// anything larger than `ctx.max_bytes`. Returns saved paths.
///
/// Notes on the request pattern, which used to be neither gentle nor safe:
/// - **One request per image.** The size pre-check is a `HEAD`, and falls back
///   to the streaming cap when a server does not support it — the old code
///   issued a throwaway `GET` and then a second `GET` for the same file.
/// - **Bounded in memory.** The body is streamed with a hard cap instead of
///   being buffered in full and measured afterwards.
/// - **Paced.** Downloads go through the same limiter as every other request.
/// - **Per-file failures are skipped**, not fatal: one unwritable name must
///   not discard the images that already downloaded successfully.
pub fn download(ctx: &DownloadCtx<'_>, results: &[ImageResult], dir: &Path) -> Result<Vec<String>> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Config(format!("cannot create download dir {}: {e}", dir.display())))?;

    let mut saved = Vec::new();
    for (i, img) in results.iter().take(ctx.limit).enumerate() {
        if let Some(robots) = ctx.robots {
            let allowed = match robots.lock() {
                Ok(mut c) => c
                    .is_allowed(ctx.client, ctx.pacer, &img.url)
                    .unwrap_or(true),
                Err(_) => true,
            };
            if !allowed {
                crate::output::warn(&format!("skipping {} (robots.txt)", img.url));
                continue;
            }
        }

        // Cheap size pre-check. A server that rejects HEAD just means we rely
        // on the streaming cap below instead of paying for a second request.
        ctx.pacer.wait();
        if let Ok(head) = ctx.client.head(&img.url).send() {
            if head.status().is_success() {
                if let Some(len) = head.content_length() {
                    if len > ctx.max_bytes as u64 {
                        crate::output::warn(&format!(
                            "skipping {} ({len} bytes > {} byte limit)",
                            img.url, ctx.max_bytes
                        ));
                        continue;
                    }
                }
            }
        }

        ctx.pacer.wait();
        let resp = match ctx.client.get(&img.url).send() {
            Ok(r) if r.status().is_success() => r,
            Ok(r) => {
                crate::output::warn(&format!("skipping {} (HTTP {})", img.url, r.status()));
                continue;
            }
            Err(e) => {
                crate::output::warn(&format!("skipping {} ({e})", img.url));
                continue;
            }
        };

        let ext = extension_for(img, &resp);

        // Read one byte past the limit so an oversized body is detected
        // without ever holding more than the cap in memory.
        let mut bytes = Vec::new();
        let read = resp
            .take(ctx.max_bytes as u64 + 1)
            .read_to_end(&mut bytes)
            .is_ok();
        if !read {
            crate::output::warn(&format!("skipping {} (read failed)", img.url));
            continue;
        }
        if bytes.len() > ctx.max_bytes {
            crate::output::warn(&format!(
                "skipping {} (larger than the {} byte limit)",
                img.url, ctx.max_bytes
            ));
            continue;
        }

        let name = sanitize_name(&img.title);
        let filename = dir.join(format!("{:04}_{name}.{ext}", i + 1));
        if let Err(e) = std::fs::write(&filename, &bytes) {
            crate::output::warn(&format!("cannot write {}: {e}", filename.display()));
            continue;
        }
        saved.push(filename.display().to_string());
    }
    Ok(saved)
}

/// File extension for a downloaded image: the URL's, else the `Content-Type`.
///
/// URLs that route through a redirector carry no extension, and a file called
/// `.img` opens in nothing.
fn extension_for(img: &ImageResult, resp: &reqwest::blocking::Response) -> String {
    if !img.format.is_empty() {
        return img.format.clone();
    }
    let mime = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .split(';')
        .next()
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    extension_for_mime(&mime).unwrap_or("img").to_string()
}

/// Map an image MIME type to a conventional extension.
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    Some(match mime {
        "image/jpeg" | "image/jpg" => "jpg",
        "image/png" => "png",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/avif" => "avif",
        "image/bmp" => "bmp",
        "image/x-icon" | "image/vnd.microsoft.icon" => "ico",
        "image/tiff" => "tiff",
        _ => return None,
    })
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

    #[test]
    fn content_type_supplies_an_extension_when_the_url_has_none() {
        // Redirector URLs carry no extension; ".img" opens in nothing.
        assert_eq!(extension_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for_mime("image/webp"), Some("webp"));
        assert_eq!(extension_for_mime("image/svg+xml"), Some("svg"));
        assert_eq!(extension_for_mime("text/html"), None);
    }
}
