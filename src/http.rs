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

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reqwest::blocking::{Client, RequestBuilder, Response};
use reqwest::header::{self, HeaderMap, HeaderValue};

use crate::error::{Error, Result};

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
pub fn build_client(timeout: Duration, user_agent: &str) -> Result<Client> {
    let mut headers = HeaderMap::new();
    headers.insert(
        header::ACCEPT,
        hv("text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8"),
    );
    headers.insert(header::ACCEPT_LANGUAGE, hv("en-US,en;q=0.9"));
    headers.insert(
        "sec-ch-ua",
        hv("\"Google Chrome\";v=\"126\", \"Chromium\";v=\"126\", \"Not/A)Brand\";v=\"8\""),
    );
    headers.insert("sec-ch-ua-mobile", hv("?0"));
    headers.insert("sec-ch-ua-platform", hv("\"Windows\""));
    headers.insert("Upgrade-Insecure-Requests", hv("1"));

    Client::builder()
        .timeout(timeout)
        .user_agent(user_agent)
        .default_headers(headers)
        .build()
        .map_err(|e| Error::Network(format!("failed to build HTTP client: {e}")))
}

/// Send with the default [`RetryPolicy`].
pub fn send_with_retry(rb: &RequestBuilder) -> Result<Response> {
    send_with_retry_policy(rb, &RetryPolicy::default())
}

/// Send a request, retrying transient failures with backoff + jitter.
///
/// Retryable: transport errors and HTTP 202/429/5xx. Other statuses (e.g.
/// 404) are returned immediately so engines can map them to their own errors.
pub fn send_with_retry_policy(rb: &RequestBuilder, policy: &RetryPolicy) -> Result<Response> {
    let attempts = policy.attempts.max(1);
    let mut last_err: Option<Error> = None;

    for attempt in 0..attempts {
        let req = rb
            .try_clone()
            .ok_or_else(|| Error::Network("request body is not cloneable for retry".into()))?;
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
                last_err = Some(Error::Network(e.to_string()));
                if attempt + 1 < attempts {
                    jittered_sleep(attempt, policy);
                }
            }
        }
    }
    Err(last_err.unwrap_or_else(|| Error::Network("request failed after retries".into())))
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
    let secs: u64 = value.trim().parse().ok()?;
    // Never sleep longer than a CLI invocation reasonably should.
    Some(secs.min(60))
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
        assert!(build_client(Duration::from_secs(5), "ua-test").is_ok());
        assert!(build_client(Duration::from_secs(5), "").is_ok());
    }

    #[test]
    fn retry_after_is_parsed_and_bounded() {
        assert_eq!(parse_retry_after("5"), Some(5));
        assert_eq!(parse_retry_after("  12 "), Some(12));
        // A hostile or absurd value must not park the CLI for an hour.
        assert_eq!(parse_retry_after("100000"), Some(60));
        // The HTTP-date form is not honored; we fall back to our own backoff.
        assert_eq!(parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after(""), None);
    }
}
