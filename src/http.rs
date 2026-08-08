//! Transport layer — the "does not rot" part of stability.
//!
//! Parsers break when upstreams change their HTML; the way we *send* requests
//! does not. Everything that touches the network goes through [`Http`], which
//! owns four engine-agnostic guarantees:
//!
//! 1. **Egress policy** — the client is built with a guarded DNS resolver and a
//!    redirect policy that re-validates each hop (see [`crate::net`]).
//! 2. **Rate limiting before the send**, shared across engines, robots.txt,
//!    image downloads and batch workers (see [`crate::ratelimit`]).
//! 3. **Polite retry** with exponential backoff + jitter for *transient*
//!    failures only, honoring `Retry-After`.
//! 4. **Bounded bodies** — every response is read through a cap, so a hostile
//!    or broken upstream cannot exhaust memory regardless of `Content-Length`.
//!
//! Jitter is derived from the system clock (no `rand` dependency) — enough to
//! decorrelate retries for a single client while keeping the crate lean.

use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{self, HeaderMap, HeaderValue};
use url::Url;

use crate::error::{Error, Result};
use crate::net::{self, EgressPolicy};
use crate::ratelimit::{self, RateLimiter};

/// Cap for API/search-engine responses. Generous for JSON and HTML result
/// pages, small enough that a hostile endpoint cannot exhaust memory.
pub const MAX_API_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Cap for robots.txt bodies — Google's own documented limit.
pub const MAX_ROBOTS_BYTES: usize = 500 * 1024;

/// Maximum redirect hops before we give up.
pub const MAX_REDIRECTS: usize = 5;

/// Retry tuning. Defaults are deliberately gentle: few attempts, short base,
/// capped growth — enough to ride out hiccups, never enough to amplify load.
#[derive(Debug, Clone, Copy)]
pub struct RetryPolicy {
    /// Total attempts (1 = no retry).
    pub attempts: u32,
    /// Base delay for the exponential backoff.
    pub base_ms: u64,
    /// Upper bound on any single backoff sleep.
    pub max_ms: u64,
    /// Lower bound, so "full jitter" can never pick 0 ms right after a 429.
    pub floor_ms: u64,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_ms: 400,
            max_ms: 8000,
            floor_ms: 100,
        }
    }
}

/// Whether a request may be replayed automatically.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetrySafety {
    /// Idempotent (GET/HEAD): safe to resend.
    Idempotent,
    /// Anything else: never resent automatically.
    Once,
}

/// Shared HTTP context. `&Http` is passed to every engine, so all requests
/// inherit the same limiter, retry policy and egress guards.
pub struct Http {
    client: Client,
    limiter: Arc<RateLimiter>,
    retry: RetryPolicy,
    policy: EgressPolicy,
}

impl Http {
    pub fn new(client: Client, limiter: Arc<RateLimiter>, policy: EgressPolicy) -> Self {
        Self {
            client,
            limiter,
            retry: RetryPolicy::default(),
            policy,
        }
    }

    /// Zero-delay, permissive context for tests and local mock servers.
    pub fn for_tests(client: Client) -> Self {
        Self {
            client,
            limiter: Arc::new(RateLimiter::unlimited()),
            retry: RetryPolicy::default(),
            policy: EgressPolicy::permissive(),
        }
    }

    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    pub fn client(&self) -> &Client {
        &self.client
    }

    pub fn policy(&self) -> EgressPolicy {
        self.policy
    }

    pub fn limiter(&self) -> &RateLimiter {
        &self.limiter
    }

    /// Validate, rate-limit and GET a URL, retrying transient failures.
    pub fn get(&self, url: Url) -> Result<Response> {
        net::check_url(&url, self.policy)?;
        let origin = ratelimit::origin_of(&url);
        self.send(self.client.get(url), &origin, RetrySafety::Idempotent)
    }

    /// Like [`Http::get`] but with a per-request timeout override.
    pub fn get_with_timeout(&self, url: Url, timeout: Duration) -> Result<Response> {
        net::check_url(&url, self.policy)?;
        let origin = ratelimit::origin_of(&url);
        self.send(
            self.client.get(url).timeout(timeout),
            &origin,
            RetrySafety::Idempotent,
        )
    }

    /// GET a URL and read at most `max_bytes` of the body as decoded text.
    pub fn get_text(&self, url: Url, max_bytes: usize) -> Result<TextBody> {
        let resp = self.get(url)?;
        let status = resp.status().as_u16();
        if !resp.status().is_success() {
            return Err(self.status_error(&resp, status));
        }
        text_capped(resp, max_bytes)
    }

    /// Map a non-2xx response to a typed error, capturing `Retry-After`.
    pub fn status_error(&self, resp: &Response, status: u16) -> Error {
        if status == 429 {
            let wait = retry_after_of(resp);
            if let Some(w) = wait {
                self.limiter.penalize(&ratelimit::origin_of(resp.url()), w);
            }
            return Error::RateLimited {
                msg: format!("upstream answered HTTP {status}"),
                retry_after: wait,
            };
        }
        Error::Http(status)
    }

    /// Send a prepared request through the limiter and retry loop.
    pub fn send(&self, rb: RequestBuilder, origin: &str, safety: RetrySafety) -> Result<Response> {
        let attempts = if safety == RetrySafety::Idempotent {
            self.retry.attempts.max(1)
        } else {
            1
        };
        let mut last_err: Option<Error> = None;

        for attempt in 0..attempts {
            let req = rb
                .try_clone()
                .ok_or_else(|| Error::Network("request body is not cloneable for retry".into()))?;
            // Rate limit *here*: immediately before the bytes leave.
            let _slot = self.limiter.acquire(origin);
            match req.send() {
                Ok(resp) => {
                    let status = resp.status().as_u16();
                    if !is_retryable_status(status) {
                        return Ok(resp);
                    }
                    let advised = retry_after_of(&resp);
                    if let Some(w) = advised {
                        self.limiter.penalize(origin, w);
                    }
                    if attempt + 1 >= attempts {
                        return Ok(resp);
                    }
                    last_err = Some(Error::Http(status));
                    drop(_slot);
                    match advised {
                        // The limiter already holds the penalty; a short local
                        // sleep keeps the retry loop from spinning.
                        Some(_) => {}
                        None => self.backoff(attempt),
                    }
                }
                Err(e) => {
                    let mapped = classify(&e);
                    let retryable = is_retryable_transport(&e) && mapped.is_retryable();
                    if !retryable {
                        return Err(mapped);
                    }
                    last_err = Some(mapped);
                    if attempt + 1 < attempts {
                        drop(_slot);
                        self.backoff(attempt);
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| Error::Network("request failed after retries".into())))
    }

    fn backoff(&self, attempt: u32) {
        let d = jittered_delay(attempt, &self.retry);
        if !d.is_zero() {
            std::thread::sleep(d);
        }
    }
}

/// Build a blocking client with browser-ish headers and the egress guards.
///
/// `accept_language` follows `--lang` so the header cannot contradict the
/// requested language (it used to be hard-coded `en-US` even for `--lang ja`).
pub fn build_client(
    timeout: Duration,
    user_agent: &str,
    accept_language: &str,
    policy: EgressPolicy,
    allow_proxy: bool,
) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        hv("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"),
    );
    headers.insert(
        header::ACCEPT_LANGUAGE,
        HeaderValue::from_str(accept_language).unwrap_or_else(|_| hv("en-US,en;q=0.9")),
    );
    headers.insert("Upgrade-Insecure-Requests", hv("1"));

    let mut builder = Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .default_headers(headers)
        .redirect(net::redirect_policy(policy, MAX_REDIRECTS))
        .dns_resolver(net::GuardedResolver::new(policy));
    if !allow_proxy {
        builder = builder.no_proxy();
    }
    builder
        .build()
        .map_err(|e| Error::Network(format!("failed to build HTTP client: {e}")))
}

/// `Accept-Language` value for a language code, or the neutral default.
pub fn accept_language_for(lang: Option<&str>) -> String {
    match lang {
        Some(l) if !l.is_empty() => {
            let primary = l.split(['-', '_']).next().unwrap_or(l);
            if primary.eq_ignore_ascii_case("en") {
                "en-US,en;q=0.9".to_string()
            } else {
                format!("{l},{primary};q=0.9,en;q=0.8")
            }
        }
        _ => "en-US,en;q=0.9".to_string(),
    }
}

/// A body read through a byte cap.
#[derive(Debug, Clone)]
pub struct RawBody {
    pub bytes: Vec<u8>,
    /// True when the upstream had more bytes than the cap allowed.
    pub truncated: bool,
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
}

/// A decoded text body plus its response metadata.
#[derive(Debug, Clone)]
pub struct TextBody {
    pub text: String,
    pub truncated: bool,
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
}

impl TextBody {
    /// Fail when the cap was hit — for endpoints whose output is only useful
    /// complete (JSON APIs).
    pub fn require_complete(self, limit: usize) -> Result<Self> {
        if self.truncated {
            return Err(Error::TooLarge { limit });
        }
        Ok(self)
    }
}

/// Read at most `max_bytes` of a response body, flagging over-cap responses.
///
/// Reads `max_bytes + 1` so "exactly at the cap" and "one byte too many" are
/// distinguishable; `Content-Length` is never trusted.
pub fn read_capped(resp: Response, max_bytes: usize) -> Result<RawBody> {
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);

    let limit = max_bytes.saturating_add(1) as u64;
    let mut reader = resp.take(limit);
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    reader
        .read_to_end(&mut bytes)
        .map_err(|e| Error::Network(format!("read body failed for {final_url}: {e}")))?;

    let truncated = bytes.len() > max_bytes;
    if truncated {
        bytes.truncate(max_bytes);
    }
    Ok(RawBody {
        bytes,
        truncated,
        status,
        final_url,
        content_type,
    })
}

/// [`read_capped`] plus charset-aware decoding.
pub fn text_capped(resp: Response, max_bytes: usize) -> Result<TextBody> {
    let raw = read_capped(resp, max_bytes)?;
    let text = decode_text(&raw.bytes, raw.content_type.as_deref());
    Ok(TextBody {
        text,
        truncated: raw.truncated,
        status: raw.status,
        final_url: raw.final_url,
        content_type: raw.content_type,
    })
}

/// Decode bytes to a `String`, honoring (in order) the `Content-Type` charset,
/// a BOM, an HTML `<meta charset>`, then UTF-8.
///
/// This is what makes Shift_JIS / EUC-JP / Windows-1252 pages readable instead
/// of mojibake — a hard requirement for a tool that advertises Japanese pages.
pub fn decode_text(bytes: &[u8], content_type: Option<&str>) -> String {
    let enc = charset_from_content_type(content_type)
        .or_else(|| encoding_rs::Encoding::for_bom(bytes).map(|(e, _)| e))
        .or_else(|| charset_from_meta(bytes))
        .unwrap_or(encoding_rs::UTF_8);
    let (cow, _, _) = enc.decode(bytes);
    cow.into_owned()
}

fn charset_from_content_type(ct: Option<&str>) -> Option<&'static encoding_rs::Encoding> {
    let ct = ct?;
    let label = ct.split(';').skip(1).find_map(|p| {
        let (k, v) = p.split_once('=')?;
        k.trim().eq_ignore_ascii_case("charset").then_some(v)
    })?;
    let label = label.trim().trim_matches('"');
    encoding_rs::Encoding::for_label(label.as_bytes())
}

/// Look for `<meta charset=...>` / `<meta http-equiv=content-type ...>` in the
/// first 4 KiB, the window browsers use for pre-scanning.
fn charset_from_meta(bytes: &[u8]) -> Option<&'static encoding_rs::Encoding> {
    let window = &bytes[..bytes.len().min(4096)];
    let head = String::from_utf8_lossy(window).to_ascii_lowercase();
    let mut rest = head.as_str();
    while let Some(idx) = rest.find("<meta") {
        rest = &rest[idx + 5..];
        let tag_end = rest.find('>').unwrap_or(rest.len());
        let tag = &rest[..tag_end];
        if let Some(label) = attr_value(tag, "charset") {
            if let Some(e) = encoding_rs::Encoding::for_label(label.as_bytes()) {
                return Some(e);
            }
        }
        if let Some(content) = attr_value(tag, "content") {
            if let Some(e) = charset_from_content_type(Some(&format!("x;{content}"))) {
                return Some(e);
            }
        }
        rest = &rest[tag_end..];
    }
    None
}

/// Read `name=value` from a lower-cased tag body, tolerating both quote styles.
fn attr_value(tag: &str, name: &str) -> Option<String> {
    let mut rest = tag;
    loop {
        let idx = rest.find(name)?;
        let after = &rest[idx + name.len()..];
        let after_trim = after.trim_start();
        // Guard against matching a longer attribute name (e.g. `charsetx=`).
        let is_boundary = idx == 0
            || !rest[..idx]
                .chars()
                .next_back()
                .map(|c| c.is_alphanumeric() || c == '-' || c == '_')
                .unwrap_or(false);
        if is_boundary {
            if let Some(v) = after_trim.strip_prefix('=') {
                let v = v.trim_start();
                let value = if let Some(q) = v.strip_prefix('"') {
                    q.split('"').next().unwrap_or("")
                } else if let Some(q) = v.strip_prefix('\'') {
                    q.split('\'').next().unwrap_or("")
                } else {
                    v.split_whitespace().next().unwrap_or("")
                };
                if !value.is_empty() {
                    return Some(value.to_string());
                }
            }
        }
        rest = after;
    }
}

/// Statuses worth retrying at the transport layer: 429 and 5xx.
///
/// 202 is deliberately **not** here: it is a legitimate success status, and
/// treating it as "try again" was a DuckDuckGo-specific quirk that belongs in
/// that engine (which maps it to a rate-limit error).
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 429 | 500 | 502 | 503 | 504)
}

/// Transport errors worth retrying: timeouts and connect failures.
///
/// Builder mistakes, redirect-policy refusals and decode errors are permanent —
/// resending them only wastes requests.
pub fn is_retryable_transport(e: &reqwest::Error) -> bool {
    if e.is_builder() || e.is_redirect() || e.is_decode() {
        return false;
    }
    e.is_timeout() || e.is_connect()
}

/// Turn a reqwest error into a typed one, recovering policy refusals that
/// surface as connect/redirect failures.
pub fn classify(e: &reqwest::Error) -> Error {
    let chain = error_chain(e);
    if chain.contains("blocked by egress policy")
        || chain.contains("refusing to connect")
        || chain.contains("too many redirects")
    {
        return Error::Blocked(chain);
    }
    if e.is_timeout() {
        return Error::Network(format!("timed out: {e}"));
    }
    Error::Network(chain)
}

fn error_chain(e: &reqwest::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        parts.push(s.to_string());
        src = s.source();
    }
    parts.join(": ")
}

fn retry_after_of(resp: &Response) -> Option<Duration> {
    let raw = resp.headers().get(header::RETRY_AFTER)?.to_str().ok()?;
    ratelimit::parse_retry_after(raw, SystemTime::now())
}

/// Deterministic exponential component: `base * 2^attempt`, capped at `max`.
pub fn exp_delay_ms(attempt: u32, policy: &RetryPolicy) -> u64 {
    let shift = attempt.min(10);
    policy
        .base_ms
        .saturating_mul(1u64 << shift)
        .min(policy.max_ms)
}

/// Full jitter in `[floor, exp_delay_ms(attempt)]`.
pub fn jittered_delay(attempt: u32, policy: &RetryPolicy) -> Duration {
    let cap = exp_delay_ms(attempt, policy);
    if cap == 0 {
        return Duration::ZERO;
    }
    let floor = policy.floor_ms.min(cap);
    let span = cap - floor + 1;
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    Duration::from_millis(floor + nanos % span)
}

/// Parse a static header value; panics only on a compile-time-constant bug.
fn hv(s: &'static str) -> HeaderValue {
    HeaderValue::from_static(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exponential_backoff_grows_then_caps() {
        let p = RetryPolicy {
            attempts: 5,
            base_ms: 400,
            max_ms: 8000,
            floor_ms: 100,
        };
        assert_eq!(exp_delay_ms(0, &p), 400);
        assert_eq!(exp_delay_ms(1, &p), 800);
        assert_eq!(exp_delay_ms(2, &p), 1600);
        assert_eq!(exp_delay_ms(3, &p), 3200);
        assert_eq!(exp_delay_ms(4, &p), 6400);
        assert_eq!(exp_delay_ms(5, &p), 8000); // capped
        assert_eq!(exp_delay_ms(20, &p), 8000); // never overflows
    }

    #[test]
    fn jitter_never_returns_zero_when_a_floor_is_set() {
        let p = RetryPolicy::default();
        for attempt in 0..4 {
            let d = jittered_delay(attempt, &p);
            assert!(d >= Duration::from_millis(p.floor_ms), "{d:?}");
            assert!(d <= Duration::from_millis(exp_delay_ms(attempt, &p)));
        }
    }

    #[test]
    fn retryable_classification_excludes_202() {
        for s in [429, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [200, 201, 202, 301, 400, 403, 404, 410] {
            assert!(!is_retryable_status(s), "{s} should not be retryable");
        }
    }

    #[test]
    fn charset_comes_from_content_type_first() {
        // Shift_JIS bytes for "日本語".
        let sjis = [0x93, 0xfa, 0x96, 0x7b, 0x8c, 0xea];
        let decoded = decode_text(&sjis, Some("text/html; charset=Shift_JIS"));
        assert_eq!(decoded, "日本語");
        // Quoted charset parameter.
        let decoded = decode_text(&sjis, Some("text/html; charset=\"shift_jis\""));
        assert_eq!(decoded, "日本語");
    }

    #[test]
    fn charset_falls_back_to_meta_tag() {
        let mut body = b"<html><head><meta charset=euc-jp></head><body>".to_vec();
        // EUC-JP bytes for "日本".
        body.extend_from_slice(&[0xc6, 0xfc, 0xcb, 0xdc]);
        body.extend_from_slice(b"</body></html>");
        let decoded = decode_text(&body, None);
        assert!(decoded.contains("日本"), "got: {decoded}");
    }

    #[test]
    fn charset_honors_http_equiv_meta() {
        let mut body =
            b"<html><head><meta http-equiv=\"Content-Type\" content=\"text/html; charset=windows-1252\">"
                .to_vec();
        body.push(0x92); // right single quote in cp1252
        let decoded = decode_text(&body, None);
        assert!(decoded.contains('\u{2019}'), "got: {decoded:?}");
    }

    #[test]
    fn bom_wins_over_missing_headers() {
        let mut body = vec![0xef, 0xbb, 0xbf];
        body.extend_from_slice("hello".as_bytes());
        assert_eq!(decode_text(&body, None), "hello");
    }

    #[test]
    fn utf8_is_the_default() {
        assert_eq!(decode_text("héllo".as_bytes(), None), "héllo");
        assert_eq!(decode_text(b"plain", Some("text/html")), "plain");
    }

    #[test]
    fn accept_language_follows_lang() {
        assert_eq!(accept_language_for(None), "en-US,en;q=0.9");
        assert_eq!(accept_language_for(Some("en")), "en-US,en;q=0.9");
        assert_eq!(accept_language_for(Some("ja")), "ja,ja;q=0.9,en;q=0.8");
        assert_eq!(
            accept_language_for(Some("pt-BR")),
            "pt-BR,pt;q=0.9,en;q=0.8"
        );
    }

    #[test]
    fn attr_value_requires_a_name_boundary() {
        assert_eq!(
            attr_value("meta data-charset=\"x\" charset=\"utf-8\"", "charset").as_deref(),
            Some("utf-8")
        );
        assert_eq!(attr_value("meta charsetx=\"utf-8\"", "charset"), None);
    }
}
