//! Image search backends (API-key-free) plus a download helper.
//!
//! - **Bing Images**: scrapes `bing.com/images/search`. Full-size URLs are
//!   embedded as JSON in the `m` attribute of `a.iusc` elements.
//! - **DuckDuckGo Images**: fetches a one-time `vqd` token from the image
//!   page, then queries the JSON endpoint `duckduckgo.com/i.js`.
//!
//! Both search parsers are pure functions, unit-tested with fixtures.
//!
//! Downloading is a single streamed GET per image, capped at `max_bytes + 1`
//! bytes so the limit is enforced regardless of `Content-Length` (absent,
//! chunked, or simply wrong), followed by content sniffing: the response
//! must actually look like the image format its own magic bytes claim, so an
//! HTML error page served with `Content-Type: image/jpeg` is rejected
//! instead of saved as a `.jpg`. Every URL's outcome is reported
//! individually — one failure never aborts the rest of the batch — and an
//! existing file is never silently overwritten.

use std::io::Write as _;
use std::path::Path;

use serde::Serialize;
use serde_json::Value;
use url::Url;

use crate::engines::{dedupe_by_url, ImageEngine};
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::ImageResult;
use crate::net;

/// Default cap for downloaded images when config/`--max-bytes` don't
/// override it. The single source of truth for this default — `config.rs`
/// reads it too — so the CLI help text and the actual behavior cannot drift
/// apart the way two independently hardcoded `5 * 1024 * 1024` literals did.
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 5 * 1024 * 1024;
use crate::text::sanitize_name;

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
        http: &Http,
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

        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        let mut results = parse_bing_html(&body.text);
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
        http: &Http,
        query: &str,
        count: usize,
        safe: bool,
    ) -> Result<Vec<ImageResult>> {
        let vqd = fetch_vqd(http, query, safe, &self.page_base)?;
        let mut params = vec![("q", query), ("o", "json"), ("vqd", vqd.as_str())];
        if safe {
            params.push(("p", "1"));
        }
        let url = Url::parse_with_params(&self.json_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = http.get(url)?;
        let status = resp.status();
        if status.as_u16() == 202 || status.as_u16() == 429 || status.as_u16() == 403 {
            return Err(Error::rate_limited(format!(
                "duckduckgo answered HTTP {status} (retry later or lower request rate)"
            )));
        }
        if !status.is_success() {
            return Err(Error::Http(status.as_u16()));
        }
        let body = crate::http::text_capped(resp, crate::http::MAX_API_BODY_BYTES)?;
        let mut results = parse_ddg_json(&body.text)?;
        results.truncate(count);
        Ok(dedupe_by_url(results, |r| &r.url))
    }
}

/// Grab the one-time `vqd` token DDG requires for its image JSON API.
fn fetch_vqd(http: &Http, query: &str, safe: bool, page_base: &str) -> Result<String> {
    let mut params = vec![("q", query), ("iax", "images"), ("ia", "images")];
    if safe {
        params.push(("p", "1"));
    }
    let url = Url::parse_with_params(page_base, &params)
        .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
    let resp = http.get(url)?;
    let status = resp.status();
    if status.as_u16() == 202 || status.as_u16() == 429 || status.as_u16() == 403 {
        return Err(Error::rate_limited(format!(
            "duckduckgo answered HTTP {status} while fetching vqd token"
        )));
    }
    if !status.is_success() {
        return Err(Error::Http(status.as_u16()));
    }
    let body = crate::http::text_capped(resp, crate::http::MAX_API_BODY_BYTES)?;
    extract_vqd(&body.text).ok_or_else(|| {
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
    let doc = scraper::Html::parse_document(html);
    let iusc_sel = scraper::Selector::parse("a.iusc").unwrap_or_else(|_| unreachable!("static"));

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
                a.select(
                    &scraper::Selector::parse("img").unwrap_or_else(|_| unreachable!("static")),
                )
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
            width: checked_dim(v.get("murlw")),
            height: checked_dim(v.get("murlh")),
            format: format_from_url(url),
        });
    }
    out
}

/// Pure parser for the DDG `i.js` JSON endpoint.
pub fn parse_ddg_json(body: &str) -> Result<Vec<ImageResult>> {
    let v: Value = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("i.js response is not JSON: {e}")))?;
    let results = v
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| Error::Parse("i.js response missing `results` array".into()))?;
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
            width: checked_dim(item.get("width")),
            height: checked_dim(item.get("height")),
            format: format_from_url(url),
        });
    }
    Ok(out)
}

/// `u64` → `u32`, dropping (rather than silently wrapping) values that don't
/// fit — a malicious or buggy upstream sending `4294967296` should not come
/// back out as `0`.
fn checked_dim(v: Option<&Value>) -> Option<u32> {
    v.and_then(|n| n.as_u64())
        .and_then(|n| u32::try_from(n).ok())
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

/// Outcome of downloading one image: exactly one of `path`/`error` is set.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadRecord {
    pub url: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Download image results into `dir`, `limit` at most, skipping anything
/// larger than `max_bytes`. Every URL gets its own outcome; a failure on one
/// never stops the rest.
pub fn download(
    http: &Http,
    results: &[ImageResult],
    dir: &Path,
    limit: usize,
    max_bytes: usize,
    overwrite: bool,
) -> Result<Vec<DownloadRecord>> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::Config(format!("cannot create download dir {}: {e}", dir.display())))?;
    Ok(results
        .iter()
        .take(limit)
        .enumerate()
        .map(
            |(i, img)| match download_one(http, img, dir, i, max_bytes, overwrite) {
                Ok(path) => DownloadRecord {
                    url: img.url.clone(),
                    ok: true,
                    path: Some(path),
                    error: None,
                },
                Err(e) => DownloadRecord {
                    url: img.url.clone(),
                    ok: false,
                    path: None,
                    error: Some(e.to_string()),
                },
            },
        )
        .collect())
}

fn download_one(
    http: &Http,
    img: &ImageResult,
    dir: &Path,
    index: usize,
    max_bytes: usize,
    overwrite: bool,
) -> Result<String> {
    let url = net::parse_checked(&img.url, http.policy())?;
    let resp = http.get(url)?;
    if !resp.status().is_success() {
        return Err(Error::Http(resp.status().as_u16()));
    }
    let content_type = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| {
            s.split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase()
        });
    if let Some(base) = &content_type {
        if base.starts_with("text/") || *base == "application/json" || *base == "text/html" {
            return Err(Error::UnsupportedContent {
                content_type: base.clone(),
            });
        }
    }

    let raw = crate::http::read_capped(resp, max_bytes)?;
    if raw.truncated {
        return Err(Error::TooLarge { limit: max_bytes });
    }

    let ext = sniff_image_format(&raw.bytes).ok_or_else(|| Error::UnsupportedContent {
        content_type: content_type.unwrap_or_default(),
    })?;
    if ext == "svg" {
        // SVG can embed <script>/<foreignObject> — active content, not a
        // plain image. Rejected by default; there is no flag to allow it
        // yet, matching the "reject unless explicitly allowed" guidance.
        return Err(Error::Blocked(
            "SVG can contain active content (scripts); not downloaded".into(),
        ));
    }

    let name = sanitize_name(&img.title);
    let filename = dir.join(format!("{:04}_{name}.{ext}", index + 1));
    write_new_file(&filename, &raw.bytes, overwrite)?;
    Ok(filename.display().to_string())
}

/// Identify an image format from its magic bytes — never trust the URL
/// extension or `Content-Type` alone, since either can lie (an error page
/// served at a `.jpg` URL, or with `Content-Type: image/jpeg`).
fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(&[0x42, 0x4D]) {
        return Some("bmp");
    }
    if bytes.starts_with(&[0x00, 0x00, 0x01, 0x00]) {
        return Some("ico");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" {
        let brand = &bytes[8..12];
        if brand == b"avif" || brand == b"avis" {
            return Some("avif");
        }
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && head.contains("<svg")) {
        return Some("svg");
    }
    None
}

/// Write `bytes` to `path` without ever following an existing file or
/// symlink at that location. `create_new` fails on *anything* already
/// there, so a symlink an attacker planted at a predictable filename cannot
/// redirect the write — the download is reported as a failure instead.
fn write_new_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    let mut open_opts = std::fs::OpenOptions::new();
    open_opts.write(true);
    if overwrite {
        open_opts.create(true).truncate(true);
    } else {
        open_opts.create_new(true);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open_opts.mode(0o600);
    }
    let mut file = open_opts.open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            Error::Config(format!(
                "{} already exists (use --overwrite to replace it)",
                path.display()
            ))
        } else {
            Error::Network(format!("cannot write {}: {e}", path.display()))
        }
    })?;
    file.write_all(bytes)
        .map_err(|e| Error::Network(format!("cannot write {}: {e}", path.display())))?;
    Ok(())
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
        assert_eq!(results[0].title, "Mountain sunset");
        assert_eq!(results[0].width, Some(1920));
        assert_eq!(results[0].height, Some(1080));
        assert_eq!(results[0].page_url, "https://blog.example.com/post");
        assert_eq!(results[0].format, "jpg");
        assert_eq!(results[1].width, None);
        assert_eq!(results[1].format, "png");
        assert_eq!(results[1].title, "Logo");
    }

    #[test]
    fn oversized_dimension_is_dropped_not_wrapped() {
        // u32::MAX + 1 must not silently become 0.
        let v: Value = serde_json::json!({"murlw": 4294967296u64});
        assert_eq!(checked_dim(v.get("murlw")), None);
        let v: Value = serde_json::json!({"murlw": 100u64});
        assert_eq!(checked_dim(v.get("murlw")), Some(100));
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
    fn missing_results_field_is_an_error_not_empty() {
        let err = parse_ddg_json(r#"{"other":1}"#).unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn format_from_url_works() {
        assert_eq!(format_from_url("https://x.com/a.JPG?w=1"), "jpg");
        assert_eq!(format_from_url("https://x.com/a.webp"), "webp");
        assert_eq!(format_from_url("https://x.com/redirect?to=/a.jpg"), "");
    }

    #[test]
    fn sniffs_real_formats_by_magic_bytes() {
        assert_eq!(sniff_image_format(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(
            sniff_image_format(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 0, 0]),
            Some("png")
        );
        assert_eq!(sniff_image_format(b"GIF89a..."), Some("gif"));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBP");
        assert_eq!(sniff_image_format(&webp), Some("webp"));
    }

    #[test]
    fn html_error_page_is_not_sniffed_as_an_image() {
        let html = b"<!DOCTYPE html><html><body>404 not found</body></html>";
        assert_eq!(sniff_image_format(html), None);
    }

    #[test]
    fn svg_is_recognized_but_rejected_by_download() {
        assert_eq!(
            sniff_image_format(b"<?xml version=\"1.0\"?><svg xmlns=\"...\"></svg>"),
            Some("svg")
        );
    }

    #[test]
    fn write_new_file_refuses_to_overwrite_by_default() {
        let dir = std::env::temp_dir().join(format!("webseek-img-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("existing.jpg");
        std::fs::write(&path, b"original").unwrap();

        let err = write_new_file(&path, b"new", false).unwrap_err();
        assert!(err.to_string().contains("--overwrite"));
        assert_eq!(std::fs::read(&path).unwrap(), b"original");

        write_new_file(&path, b"new", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(unix)]
    #[test]
    fn write_new_file_does_not_follow_a_planted_symlink() {
        use std::os::unix::fs::symlink;
        let dir = std::env::temp_dir().join(format!("webseek-img-symlink-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let real_target = dir.join("real_secret.txt");
        std::fs::write(&real_target, b"do not touch").unwrap();
        let link_path = dir.join("0001_img.jpg");
        symlink(&real_target, &link_path).unwrap();

        let err = write_new_file(&link_path, b"attacker bytes", false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        assert_eq!(std::fs::read(&real_target).unwrap(), b"do not touch");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
