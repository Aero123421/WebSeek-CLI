//! Per-origin rate limiting, enforced *before* each HTTP send.
//!
//! The old design slept after the command had already finished, which protected
//! nothing: the process exited immediately afterwards and multi-request engines
//! (vqd → i.js, esearch → esummary, RSS → HTML, engine → fallback, image
//! downloads) sent back-to-back requests with no pause at all.
//!
//! [`RateLimiter`] is shared by every request path — engines, robots.txt,
//! image downloads, batch workers — so `-j 10` can no longer fan out ten
//! simultaneous requests at one host. It is independent of `--quiet`, which is
//! an output flag and must never change network behavior.
//!
//! `Retry-After` is honored through [`RateLimiter::penalize`]: the next request
//! to that origin waits at least that long.

use std::collections::HashMap;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

/// Never sleep longer than this for a single acquisition, even if an upstream
/// asks for more — an agent waiting 10 minutes is worse than a clear error.
pub const MAX_WAIT: Duration = Duration::from_secs(60);

#[derive(Debug, Default)]
struct OriginState {
    /// Earliest instant at which the next request may be sent.
    next_allowed: Option<Instant>,
    /// True while a request for this origin holds the slot.
    busy: bool,
}

/// Serializes requests per origin and keeps `min_interval` between them.
pub struct RateLimiter {
    min_interval: Duration,
    state: Mutex<HashMap<String, OriginState>>,
    idle: Condvar,
}

impl RateLimiter {
    pub fn new(min_interval: Duration) -> Self {
        Self {
            min_interval,
            state: Mutex::new(HashMap::new()),
            idle: Condvar::new(),
        }
    }

    /// A limiter that never waits (tests, `delay_ms = 0`).
    pub fn unlimited() -> Self {
        Self::new(Duration::ZERO)
    }

    pub fn min_interval(&self) -> Duration {
        self.min_interval
    }

    /// Block until a request to `origin` may be sent, then reserve the slot.
    ///
    /// Returns a guard; dropping it releases the origin and arms the next
    /// earliest-send instant.
    pub fn acquire(&self, origin: &str) -> Slot<'_> {
        loop {
            let wait = {
                let mut guard = match self.state.lock() {
                    Ok(g) => g,
                    // A poisoned lock must not turn into "no rate limiting".
                    Err(p) => p.into_inner(),
                };
                loop {
                    let entry = guard.entry(origin.to_string()).or_default();
                    if !entry.busy {
                        break;
                    }
                    guard = match self.idle.wait(guard) {
                        Ok(g) => g,
                        Err(p) => p.into_inner(),
                    };
                }
                let entry = guard.entry(origin.to_string()).or_default();
                let now = Instant::now();
                let wait = entry
                    .next_allowed
                    .filter(|t| *t > now)
                    .map(|t| (t - now).min(MAX_WAIT))
                    .unwrap_or(Duration::ZERO);
                if wait.is_zero() {
                    entry.busy = true;
                }
                wait
            };
            if wait.is_zero() {
                return Slot {
                    limiter: self,
                    origin: origin.to_string(),
                };
            }
            std::thread::sleep(wait);
        }
    }

    /// Record a server-requested backoff for `origin` (e.g. `Retry-After`).
    pub fn penalize(&self, origin: &str, wait: Duration) {
        let wait = wait.min(MAX_WAIT);
        let mut guard = match self.state.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        let entry = guard.entry(origin.to_string()).or_default();
        let target = Instant::now() + wait;
        if entry.next_allowed.map(|t| target > t).unwrap_or(true) {
            entry.next_allowed = Some(target);
        }
    }

    fn release(&self, origin: &str) {
        {
            let mut guard = match self.state.lock() {
                Ok(g) => g,
                Err(p) => p.into_inner(),
            };
            let interval = self.min_interval;
            let entry = guard.entry(origin.to_string()).or_default();
            entry.busy = false;
            let target = Instant::now() + interval;
            if entry.next_allowed.map(|t| target > t).unwrap_or(true) {
                entry.next_allowed = Some(target);
            }
        }
        self.idle.notify_all();
    }
}

impl Default for RateLimiter {
    fn default() -> Self {
        Self::unlimited()
    }
}

/// Held for the duration of one request to an origin.
pub struct Slot<'a> {
    limiter: &'a RateLimiter,
    origin: String,
}

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        self.limiter.release(&self.origin);
    }
}

/// Scheme + host + port key, built with the `Url` API so IPv6 hosts keep their
/// brackets (`http://[::1]:8080`) instead of being string-concatenated.
pub fn origin_of(url: &url::Url) -> String {
    let mut origin = url.clone();
    origin.set_path("");
    origin.set_query(None);
    origin.set_fragment(None);
    let _ = origin.set_username("");
    let _ = origin.set_password(None);
    origin.as_str().trim_end_matches('/').to_string()
}

/// Parse a `Retry-After` header: delay-seconds or an HTTP-date.
pub fn parse_retry_after(value: &str, now: std::time::SystemTime) -> Option<Duration> {
    let v = value.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs));
    }
    let at = parse_http_date(v)?;
    at.duration_since(now).ok().or(Some(Duration::ZERO))
}

/// Minimal IMF-fixdate parser (`Wed, 21 Oct 2015 07:28:00 GMT`), the only
/// format servers are required to send. Returns a `SystemTime`.
fn parse_http_date(s: &str) -> Option<std::time::SystemTime> {
    let rest = s.split_once(", ").map(|(_, r)| r).unwrap_or(s).trim();
    let mut it = rest.split_whitespace();
    let day: u64 = it.next()?.parse().ok()?;
    let month = match it.next()? {
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
    let year: u64 = it.next()?.parse().ok()?;
    let time = it.next()?;
    let mut tp = time.split(':');
    let hh: u64 = tp.next()?.parse().ok()?;
    let mm: u64 = tp.next()?.parse().ok()?;
    let ss: u64 = tp.next()?.parse().ok()?;
    if year < 1970 || hh > 23 || mm > 59 || ss > 60 || day == 0 || day > 31 {
        return None;
    }
    let days = days_from_civil(year, month, day)?;
    let secs = days * 86_400 + hh * 3_600 + mm * 60 + ss;
    Some(std::time::UNIX_EPOCH + Duration::from_secs(secs))
}

/// Days since 1970-01-01 for a proleptic Gregorian date.
// `is_multiple_of` reads better but is a newer stdlib addition than this
// crate's declared MSRV can be verified against; plain `%` works everywhere.
#[allow(clippy::manual_is_multiple_of)]
fn days_from_civil(year: u64, month: u64, day: u64) -> Option<u64> {
    const CUM: [u64; 12] = [0, 31, 59, 90, 120, 151, 181, 212, 243, 273, 304, 334];
    let leaps = |y: u64| y / 4 - y / 100 + y / 400;
    let prev = year - 1;
    let mut days = 365 * (year - 1970) + leaps(prev) - leaps(1969);
    days += CUM[(month - 1) as usize];
    let is_leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    if is_leap && month > 2 {
        days += 1;
    }
    Some(days + day - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origin_keeps_ipv6_brackets_and_port() {
        let u = url::Url::parse("http://[::1]:8080/robots.txt?x=1").unwrap();
        assert_eq!(origin_of(&u), "http://[::1]:8080");
        let u = url::Url::parse("https://example.com/a/b").unwrap();
        assert_eq!(origin_of(&u), "https://example.com");
        let u = url::Url::parse("https://example.com:8443/a").unwrap();
        assert_eq!(origin_of(&u), "https://example.com:8443");
    }

    #[test]
    fn origin_ignores_credentials() {
        let u = url::Url::parse("https://u:p@example.com/a").unwrap();
        assert_eq!(origin_of(&u), "https://example.com");
    }

    #[test]
    fn retry_after_seconds_and_dates() {
        let now = std::time::UNIX_EPOCH + Duration::from_secs(1_445_412_400);
        assert_eq!(parse_retry_after("30", now), Some(Duration::from_secs(30)));
        // 2015-10-21T07:28:00Z == 1445412480
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:28:00 GMT", now),
            Some(Duration::from_secs(80))
        );
        // A date in the past clamps to zero rather than failing.
        assert_eq!(
            parse_retry_after("Wed, 21 Oct 2015 07:00:00 GMT", now),
            Some(Duration::ZERO)
        );
        assert_eq!(parse_retry_after("soon", now), None);
    }

    #[test]
    fn spacing_is_enforced_between_requests_to_one_origin() {
        let rl = RateLimiter::new(Duration::from_millis(60));
        let start = Instant::now();
        for _ in 0..3 {
            let _slot = rl.acquire("https://example.com");
        }
        // Two gaps of 60ms between three requests.
        assert!(
            start.elapsed() >= Duration::from_millis(110),
            "elapsed {:?}",
            start.elapsed()
        );
    }

    #[test]
    fn different_origins_do_not_block_each_other() {
        let rl = RateLimiter::new(Duration::from_millis(200));
        let _a = rl.acquire("https://a.example");
        let start = Instant::now();
        let _b = rl.acquire("https://b.example");
        assert!(start.elapsed() < Duration::from_millis(150));
    }

    #[test]
    fn penalize_delays_the_next_acquire() {
        let rl = RateLimiter::new(Duration::ZERO);
        rl.penalize("https://x.example", Duration::from_millis(120));
        let start = Instant::now();
        let _slot = rl.acquire("https://x.example");
        assert!(start.elapsed() >= Duration::from_millis(100));
    }

    #[test]
    fn parallel_workers_share_one_origin_slot() {
        let rl = RateLimiter::new(Duration::from_millis(40));
        let start = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..4 {
                s.spawn(|| {
                    let _slot = rl.acquire("https://example.com");
                });
            }
        });
        // 4 requests, 3 enforced gaps.
        assert!(
            start.elapsed() >= Duration::from_millis(110),
            "elapsed {:?}",
            start.elapsed()
        );
    }
}
