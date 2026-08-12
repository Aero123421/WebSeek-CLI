//! Transport-layer hardening — the "does not rot" part of stability.
//!
//! Parsers break when upstreams change their HTML; the way we *send* requests
//! does not. This module centralizes two durable, engine-agnostic levers:
//!
//! 1. **Browser-like default headers** ([`build_client`]) so requests don't
//!    look like a naive bot and trip anti-bot challenges.
//! 2. **Polite retry with exponential backoff + jitter** ([`send_with_retry`])
//!    to absorb transient 429/5xx/network errors without hammering upstreams.
//!
//! Jitter is derived from the system clock (no `rand` dependency) — enough to
//! decorrelate retries for a single client while keeping the crate lean.

use std::io::Read;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{self, HeaderMap, HeaderValue};

use crate::error::{Error, Result};
use crate::net::{self, EgressPolicy};

/// Hard cap for search-engine and API response bodies.
pub const MAX_API_BODY_BYTES: usize = 8 * 1024 * 1024;

/// Maximum redirect hops before the request is rejected.
pub const MAX_REDIRECTS: usize = 5;

const DEFAULT_ACCEPT_LANGUAGE: &str = "en-US,en;q=0.9";

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
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_ms: 400,
            max_ms: 8000,
        }
    }
}

/// Build a blocking client with realistic browser headers.
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
        HeaderValue::from_str(accept_language)
            .unwrap_or_else(|_| HeaderValue::from_static(DEFAULT_ACCEPT_LANGUAGE)),
    );
    headers.insert(
        "sec-ch-ua",
        hv("\"Google Chrome\";v=\"126\", \"Chromium\";v=\"126\", \"Not/A)Brand\";v=\"8\""),
    );
    headers.insert("sec-ch-ua-mobile", hv("?0"));
    headers.insert("sec-ch-ua-platform", hv("\"Windows\""));
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

/// Derive a safe browser `Accept-Language` value from the effective search
/// language. Invalid values fall back to English instead of reaching the
/// header parser or allowing extra header syntax through configuration.
pub fn accept_language_for(lang: Option<&str>) -> String {
    let Some(tag) = lang.map(str::trim).filter(|tag| valid_language_tag(tag)) else {
        return DEFAULT_ACCEPT_LANGUAGE.to_string();
    };
    let tag = tag.replace('_', "-");
    let primary = tag.split('-').next().unwrap_or("en");
    if primary.eq_ignore_ascii_case("en") {
        if tag.eq_ignore_ascii_case("en") {
            tag
        } else {
            format!("{tag},en;q=0.9")
        }
    } else {
        format!("{tag},{primary};q=0.9,en;q=0.8")
    }
}

fn valid_language_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 35
        && tag.split(['-', '_']).all(|part| {
            !part.is_empty() && part.len() <= 8 && part.bytes().all(|b| b.is_ascii_alphanumeric())
        })
}

/// Send with the default [`RetryPolicy`].
pub fn send_with_retry(rb: &RequestBuilder) -> Result<Response> {
    send_with_retry_policy_inner(rb, &RetryPolicy::default(), || {})
}

/// Send with retries while applying the shared minimum interval before every
/// actual attempt, including retries.
pub fn send_with_retry_paced(rb: &RequestBuilder, pacer: &crate::pace::Pacer) -> Result<Response> {
    send_with_retry_policy_inner(rb, &RetryPolicy::default(), || pacer.wait())
}

/// Send a request, retrying transient failures with backoff + jitter.
///
/// Retryable: transport errors and HTTP 202/429/5xx. Other statuses (e.g.
/// 404) are returned immediately so engines can map them to their own errors.
pub fn send_with_retry_policy(rb: &RequestBuilder, policy: &RetryPolicy) -> Result<Response> {
    send_with_retry_policy_inner(rb, policy, || {})
}

fn send_with_retry_policy_inner(
    rb: &RequestBuilder,
    policy: &RetryPolicy,
    before_send: impl Fn(),
) -> Result<Response> {
    let attempts = policy.attempts.max(1);
    let mut last_err: Option<Error> = None;

    for attempt in 0..attempts {
        let req = rb
            .try_clone()
            .ok_or_else(|| Error::Network("request body is not cloneable for retry".into()))?;
        before_send();
        match req.send() {
            Ok(resp) => {
                let status = resp.status().as_u16();
                if is_retryable_status(status) && attempt + 1 < attempts {
                    last_err = Some(Error::Http(status));
                    // When the server tells us how long to wait, believe it:
                    // our own backoff is a guess, `Retry-After` is an answer.
                    match retry_after_secs(&resp) {
                        Some(secs) => std::thread::sleep(Duration::from_millis(
                            (secs * 1000).min(policy.max_ms),
                        )),
                        None => jittered_sleep(attempt, policy),
                    }
                    continue;
                }
                return Ok(resp);
            }
            Err(e) => {
                let mapped = classify(&e);
                if matches!(mapped, Error::Blocked(_)) {
                    return Err(mapped);
                }
                last_err = Some(mapped);
                if attempt + 1 < attempts {
                    jittered_sleep(attempt, policy);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Network("request failed after retries".into())))
}

/// Response metadata plus a body read through an actual streaming cap.
#[derive(Debug, Clone)]
pub struct RawBody {
    pub bytes: Vec<u8>,
    pub truncated: bool,
    pub status: u16,
    pub final_url: String,
    pub content_type: Option<String>,
}

/// Read at most `max_bytes`, using one extra byte to distinguish an exact-fit
/// body from a truncated body. `Content-Length` is never trusted.
pub fn read_capped(resp: Response, max_bytes: usize) -> Result<RawBody> {
    let status = resp.status().as_u16();
    let final_url = resp.url().to_string();
    let content_type = resp
        .headers()
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut bytes = Vec::with_capacity(max_bytes.min(64 * 1024));
    resp.take(max_bytes.saturating_add(1) as u64)
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

/// Read a complete text/API response under the global body cap.
pub fn response_text(resp: Response) -> Result<String> {
    let raw = read_capped(resp, MAX_API_BODY_BYTES)?;
    if raw.truncated {
        return Err(Error::TooLarge {
            limit: MAX_API_BODY_BYTES,
        });
    }
    Ok(decode_text(&raw.bytes, raw.content_type.as_deref()))
}

/// Decode using HTTP charset, BOM, an HTML meta charset, then UTF-8.
pub fn decode_text(bytes: &[u8], content_type: Option<&str>) -> String {
    let charset = content_type
        .and_then(|ct| {
            ct.split(';').skip(1).find_map(|p| {
                let (k, v) = p.split_once('=')?;
                k.trim()
                    .eq_ignore_ascii_case("charset")
                    .then(|| v.trim().trim_matches('"'))
            })
        })
        .and_then(|label| encoding_rs::Encoding::for_label(label.as_bytes()))
        .or_else(|| encoding_rs::Encoding::for_bom(bytes).map(|(enc, _)| enc))
        .or_else(|| {
            crate::reader::meta_charset_label(bytes)
                .and_then(|label| encoding_rs::Encoding::for_label(&label))
        })
        .unwrap_or(encoding_rs::UTF_8);
    let (text, _, _) = charset.decode(bytes);
    text.into_owned()
}

/// Recover policy failures wrapped inside reqwest's redirect/DNS errors.
pub fn classify(e: &reqwest::Error) -> Error {
    let mut parts = vec![e.to_string()];
    let mut source = std::error::Error::source(e);
    while let Some(err) = source {
        parts.push(err.to_string());
        source = err.source();
    }
    let chain = parts.join(": ");
    if chain.contains("blocked by egress policy")
        || chain.contains("refusing to connect")
        || chain.contains("too many redirects")
    {
        Error::Blocked(chain)
    } else {
        Error::Network(chain)
    }
}

/// Statuses worth retrying: DDG's 202 "try again", 429 rate limit, and 5xx.
pub fn is_retryable_status(status: u16) -> bool {
    matches!(status, 202 | 429 | 500 | 502 | 503 | 504)
}

/// `Retry-After` in delta-seconds form, if the server sent one.
///
/// Only the numeric form is honored; the HTTP-date form would need a date
/// parser for a header that upstreams here send as seconds anyway.
fn retry_after_secs(resp: &Response) -> Option<u64> {
    parse_retry_after(resp.headers().get(header::RETRY_AFTER)?.to_str().ok()?)
}

/// Pure half of [`retry_after_secs`].
pub fn parse_retry_after(value: &str) -> Option<u64> {
    parse_retry_after_at(value, SystemTime::now())
}

fn parse_retry_after_at(value: &str, now: SystemTime) -> Option<u64> {
    let value = value.trim();
    let secs = match value.parse::<u64>() {
        Ok(secs) => secs,
        Err(_) => parse_http_date(value)?
            .duration_since(now)
            .map(|duration| duration.as_secs())
            .unwrap_or(0),
    };
    // Never sleep longer than a CLI invocation reasonably should.
    Some(secs.min(60))
}

/// Parse the IMF-fixdate form servers are required to use for HTTP dates.
fn parse_http_date(value: &str) -> Option<SystemTime> {
    let rest = value
        .split_once(", ")
        .map(|(_, rest)| rest)
        .unwrap_or(value);
    let mut parts = rest.split_whitespace();
    let day: u64 = parts.next()?.parse().ok()?;
    let month = match parts.next()? {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let year: u64 = parts.next()?.parse().ok()?;
    let mut time = parts.next()?.split(':');
    let hour: u64 = time.next()?.parse().ok()?;
    let minute: u64 = time.next()?.parse().ok()?;
    let second: u64 = time.next()?.parse().ok()?;
    if parts.next()? != "GMT" || parts.next().is_some() || time.next().is_some() {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let month_days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if !(1970..=9999).contains(&year)
        || day == 0
        || day > month_days[month - 1]
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    let cumulative = [0u64, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let previous_year = year - 1;
    let leap_days = |through: u64| through / 4 - through / 100 + through / 400;
    let mut days = 365 * (year - 1970) + leap_days(previous_year) - leap_days(1969);
    days += cumulative[month - 1];
    if leap && month > 2 {
        days += 1;
    }
    days += day - 1;
    Some(UNIX_EPOCH + Duration::from_secs(days * 86_400 + hour * 3_600 + minute * 60 + second))
}

/// Deterministic exponential component: `base * 2^attempt`, capped at `max`.
pub fn exp_delay_ms(attempt: u32, policy: &RetryPolicy) -> u64 {
    let shift = attempt.min(10);
    policy
        .base_ms
        .saturating_mul(1u64 << shift)
        .min(policy.max_ms)
}

/// Sleep for a jittered duration in `[0, exp_delay_ms(attempt)]` (full jitter).
fn jittered_sleep(attempt: u32, policy: &RetryPolicy) {
    let cap = exp_delay_ms(attempt, policy);
    if cap == 0 {
        return;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| u64::from(d.subsec_nanos()))
        .unwrap_or(0);
    let sleep_ms = nanos % (cap + 1);
    std::thread::sleep(Duration::from_millis(sleep_ms));
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
        };
        assert_eq!(exp_delay_ms(0, &p), 400);
        assert_eq!(exp_delay_ms(1, &p), 800);
        assert_eq!(exp_delay_ms(2, &p), 1600);
        assert_eq!(exp_delay_ms(3, &p), 3200);
        assert_eq!(exp_delay_ms(4, &p), 6400);
        assert_eq!(exp_delay_ms(5, &p), 8000); // capped
        assert_eq!(exp_delay_ms(20, &p), 8000); // never overflows / never exceeds cap
    }

    #[test]
    fn retryable_classification() {
        for s in [202, 429, 500, 502, 503, 504] {
            assert!(is_retryable_status(s), "{s} should be retryable");
        }
        for s in [200, 201, 301, 400, 403, 404, 410] {
            assert!(!is_retryable_status(s), "{s} should not be retryable");
        }
    }

    #[test]
    fn build_client_succeeds_with_a_custom_agent() {
        // Header *content* is asserted on the wire by
        // `tests::engines::browser_headers_reach_the_server`; this only
        // guarantees the builder itself is well-formed.
        assert!(build_client(
            Duration::from_secs(5),
            "ua-test",
            DEFAULT_ACCEPT_LANGUAGE,
            EgressPolicy::permissive(),
            true
        )
        .is_ok());
        assert!(build_client(
            Duration::from_secs(5),
            "",
            DEFAULT_ACCEPT_LANGUAGE,
            EgressPolicy::permissive(),
            true
        )
        .is_ok());
    }

    #[test]
    fn accept_language_tracks_a_valid_language_without_accepting_header_syntax() {
        assert_eq!(
            accept_language_for(Some("ja-JP")),
            "ja-JP,ja;q=0.9,en;q=0.8"
        );
        assert_eq!(accept_language_for(Some("en-GB")), "en-GB,en;q=0.9");
        assert_eq!(
            accept_language_for(Some("zh_Hant_TW")),
            "zh-Hant-TW,zh;q=0.9,en;q=0.8"
        );
        assert_eq!(
            accept_language_for(Some("ja, en;q=0.1\r\nx-evil: yes")),
            DEFAULT_ACCEPT_LANGUAGE
        );
    }

    #[test]
    fn retry_after_is_parsed_and_bounded() {
        assert_eq!(parse_retry_after("5"), Some(5));
        assert_eq!(parse_retry_after("  12 "), Some(12));
        // A hostile or absurd value must not park the CLI for an hour.
        assert_eq!(parse_retry_after("100000"), Some(60));
        let now = UNIX_EPOCH + Duration::from_secs(1_445_412_400);
        assert_eq!(
            parse_retry_after_at("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(60)
        );
        assert_eq!(
            parse_retry_after_at("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(0)
        );
        assert_eq!(
            parse_retry_after_at("Fri, 31 Feb 2025 00:00:00 GMT", now),
            None
        );
        assert_eq!(parse_retry_after(""), None);
    }
}
