//! Image search backends (API-key-free) plus a download helper.
//!
//! - **Bing Images**: scrapes `bing.com/images/search`. Full-size URLs are
//!   embedded as JSON in the `m` attribute of `a.iusc` elements.
//! - **DuckDuckGo Images**: fetches a one-time `vqd` token from the image
//!   page, then queries the JSON endpoint `duckduckgo.com/i.js`.
//!
//! Both parsers are pure functions, unit-tested with fixtures.

use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use serde_json::Value;
use url::Url;

use crate::engines::{dedupe_and_truncate, ImageEngine};
use crate::error::{Error, Result};
use crate::models::{ImageOpts, ImageResult};
use crate::net::EgressPolicy;
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

    fn search(&self, client: &Client, query: &str, opts: &ImageOpts) -> Result<Vec<ImageResult>> {
        let mut params = vec![("q", query), ("form", "HDRSC2")];
        if opts.safe {
            params.push(("adlt", "strict"));
        }
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send(client.get(url))
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
        let body = crate::http::response_text(resp)?;
        // Without this the text engine reported challenges but the image
        // engine silently returned zero results, so fallback never fired.
        if crate::engines::looks_like_challenge(&body) {
            return Err(Error::RateLimited(
                "bing images served a bot-challenge page instead of results".into(),
            ));
        }
        Ok(dedupe_and_truncate(
            parse_bing_html(&body),
            opts.count,
            |r| &r.url,
        ))
    }
}

impl ImageEngine for DuckDuckGoImages {
    fn name(&self) -> &'static str {
        "duckduckgo"
    }

    fn search(&self, client: &Client, query: &str, opts: &ImageOpts) -> Result<Vec<ImageResult>> {
        // Two upstream requests: the token page, then the JSON endpoint. Both
        // are paced, which is exactly why the pacer lives in the opts.
        let vqd = fetch_vqd(client, query, opts, &self.page_base)?;
        let mut params = vec![("q", query), ("o", "json"), ("vqd", vqd.as_str())];
        if opts.safe {
            params.push(("kp", "1"));
        }
        let url = Url::parse_with_params(&self.json_base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;

        let resp = opts
            .send(client.get(url))
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
        let body = crate::http::response_text(resp)?;
        Ok(dedupe_and_truncate(
            parse_ddg_json(&body)?,
            opts.count,
            |r| &r.url,
        ))
    }
}

/// Grab the one-time `vqd` token DDG requires for its image JSON API.
fn fetch_vqd(client: &Client, query: &str, opts: &ImageOpts, page_base: &str) -> Result<String> {
    let mut params = vec![("q", query), ("iax", "images"), ("ia", "images")];
    if opts.safe {
        params.push(("kp", "1"));
    }
    let url = Url::parse_with_params(page_base, &params)
        .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
    let resp = opts
        .send(client.get(url))
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
    let body = crate::http::response_text(resp)?;
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
        .ok_or_else(|| Error::Parse("i.js response omitted results array".into()))?;
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

fn checked_dim(value: Option<&Value>) -> Option<u32> {
    u32::try_from(value?.as_u64()?).ok()
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
    /// Network destinations permitted for untrusted result URLs.
    pub policy: EgressPolicy,
    /// Whether an existing generated path may be replaced.
    pub overwrite: bool,
}

/// Download image results into `dir`, at most `ctx.limit` of them, skipping
/// anything larger than `ctx.max_bytes`. Returns saved paths.
///
/// Notes on the request pattern, which used to be neither gentle nor safe:
/// - **One request per image.** The streaming cap makes a separate HEAD/GET
///   preflight unnecessary.
/// - **Bounded in memory.** The body is streamed with a hard cap instead of
///   being buffered in full and measured afterwards.
/// - **Content is verified.** Extensions and Content-Type are hints; magic
///   bytes decide whether the response is really an image.
/// - **No implicit overwrite.** `create_new` refuses existing files and
///   symlinks unless the caller explicitly opts in.
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

        let url = match crate::net::parse_checked(&img.url, ctx.policy) {
            Ok(url) => url,
            Err(e) => {
                crate::output::warn(&format!("skipping {} ({e})", img.url));
                continue;
            }
        };

        let resp = match crate::http::send_with_retry_paced(&ctx.client.get(url), ctx.pacer) {
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

        let body = match crate::http::read_capped(resp, ctx.max_bytes) {
            Ok(body) => body,
            Err(e) => {
                crate::output::warn(&format!("skipping {} ({e})", img.url));
                continue;
            }
        };
        if body.truncated {
            crate::output::warn(&format!(
                "skipping {} (larger than the {} byte limit)",
                img.url, ctx.max_bytes
            ));
            continue;
        }
        let Some(ext) = sniff_image_format(&body.bytes) else {
            let content_type = body.content_type.as_deref().unwrap_or("unknown");
            crate::output::warn(&format!(
                "skipping {} (response is not a supported image; Content-Type: {content_type})",
                img.url
            ));
            continue;
        };
        if ext == "svg" {
            crate::output::warn(&format!(
                "skipping {} (SVG may contain active content)",
                img.url
            ));
            continue;
        }

        let name = sanitize_name(&img.title);
        let filename = dir.join(format!("{:04}_{name}.{ext}", i + 1));
        if let Err(e) = write_image_file(&filename, &body.bytes, ctx.overwrite) {
            crate::output::warn(&e.to_string());
            continue;
        }
        saved.push(filename.display().to_string());
    }
    Ok(saved)
}

/// Identify an image by magic bytes. A URL suffix or header can claim JPEG
/// while the response is an HTML error page, so neither is authoritative.
fn sniff_image_format(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        return Some("jpg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    if bytes.starts_with(&[0, 0, 1, 0]) {
        return Some("ico");
    }
    if bytes.len() >= 12 && &bytes[4..8] == b"ftyp" && matches!(&bytes[8..12], b"avif" | b"avis") {
        return Some("avif");
    }
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start_matches('\u{feff}').trim_start();
    if trimmed.starts_with("<svg") || (trimmed.starts_with("<?xml") && head.contains("<svg")) {
        return Some("svg");
    }
    None
}

fn write_image_file(path: &Path, bytes: &[u8], overwrite: bool) -> Result<()> {
    if overwrite {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let mut tmp = tempfile::Builder::new()
            .prefix(".webseek-image-")
            .suffix(".tmp")
            .tempfile_in(parent)
            .map_err(|e| Error::Network(format!("cannot stage {}: {e}", path.display())))?;
        tmp.write_all(bytes)
            .map_err(|e| Error::Network(format!("cannot write {}: {e}", path.display())))?;
        tmp.flush()
            .map_err(|e| Error::Network(format!("cannot flush {}: {e}", path.display())))?;
        tmp.persist(path).map_err(|e| {
            Error::Network(format!("cannot replace {}: {}", path.display(), e.error))
        })?;
        return Ok(());
    }

    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| {
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
        .map_err(|e| Error::Network(format!("cannot write {}: {e}", path.display())))
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
    fn oversized_dimensions_are_dropped_instead_of_wrapping() {
        let value = serde_json::json!(u64::from(u32::MAX) + 1);
        assert_eq!(checked_dim(Some(&value)), None);
        let value = serde_json::json!(800);
        assert_eq!(checked_dim(Some(&value)), Some(800));
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
    fn ddg_api_error_object_is_not_reported_as_zero_results() {
        assert!(parse_ddg_json(r#"{"error":"rate limit"}"#).is_err());
    }

    #[test]
    fn format_from_url_works() {
        assert_eq!(format_from_url("https://x.com/a.JPG?w=1"), "jpg");
        assert_eq!(format_from_url("https://x.com/a.webp"), "webp");
        assert_eq!(format_from_url("https://x.com/redirect?to=/a.jpg"), "");
    }

    #[test]
    fn known_image_mimes_map_to_conventional_extensions() {
        // Redirector URLs carry no extension; ".img" opens in nothing.
        assert_eq!(extension_for_mime("image/jpeg"), Some("jpg"));
        assert_eq!(extension_for_mime("image/webp"), Some("webp"));
        assert_eq!(extension_for_mime("image/svg+xml"), Some("svg"));
        assert_eq!(extension_for_mime("text/html"), None);
    }

    #[test]
    fn magic_bytes_override_untrusted_names_and_headers() {
        assert_eq!(sniff_image_format(&[0xff, 0xd8, 0xff, 0xe0]), Some("jpg"));
        assert_eq!(
            sniff_image_format(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]),
            Some("png")
        );
        assert_eq!(sniff_image_format(b"GIF89a..."), Some("gif"));
        assert_eq!(
            sniff_image_format(b"<!doctype html><title>404</title>"),
            None
        );
        assert_eq!(sniff_image_format(b"<svg xmlns='x'></svg>"), Some("svg"));
    }

    #[test]
    fn image_files_do_not_overwrite_by_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("0001_image.jpg");
        std::fs::write(&path, b"original").unwrap();

        let err = write_image_file(&path, b"replacement", false).unwrap_err();
        assert!(err.to_string().contains("--overwrite"));
        assert_eq!(std::fs::read(&path).unwrap(), b"original");

        write_image_file(&path, b"replacement", true).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }

    #[cfg(unix)]
    #[test]
    fn image_files_do_not_follow_existing_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target");
        let link = dir.path().join("0001_image.jpg");
        std::fs::write(&target, b"secret").unwrap();
        symlink(&target, &link).unwrap();

        assert!(write_image_file(&link, b"attacker", false).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");

        write_image_file(&link, b"replacement", true).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"secret");
        assert_eq!(std::fs::read(&link).unwrap(), b"replacement");
    }
}
