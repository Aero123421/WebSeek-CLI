//! Typed errors with stable exit-code semantics and machine-readable codes.
//!
//! Exit codes (documented in README):
//! - `0`: success (including "no results" — empty JSON is valid for agents)
//! - `1`: runtime error (network, parse, config, ...)
//! - `2`: CLI usage error (clap, and any misuse we detect ourselves)
//!
//! Every variant carries a stable `code()` string so JSON consumers can branch
//! on the failure class instead of matching human prose.

use std::fmt;
use std::time::Duration;

#[derive(Debug)]
pub enum Error {
    /// The search engine refused the request (rate limit, CAPTCHA, bot check).
    RateLimited {
        msg: String,
        /// Server-advertised wait, when a `Retry-After` header was present.
        retry_after: Option<Duration>,
    },
    /// The response could not be parsed (HTML structure changed upstream).
    Parse(String),
    /// HTTP status outside 2xx.
    Http(u16),
    /// Network / transport failure.
    Network(String),
    /// Bad configuration value (config file or environment).
    Config(String),
    /// Invalid command-line usage we detect ourselves — exits with code 2 so it
    /// matches clap's own usage errors.
    Usage(String),
    /// The request was refused by the egress policy (SSRF guard).
    Blocked(String),
    /// The response body exceeded the configured byte cap.
    TooLarge { limit: usize },
    /// The response was a type we cannot turn into text.
    UnsupportedContent { content_type: String },
    /// No usable result at all.
    NoResults(String),
}

impl Error {
    /// Convenience constructor for a rate limit without a `Retry-After`.
    pub fn rate_limited(msg: impl Into<String>) -> Self {
        Error::RateLimited {
            msg: msg.into(),
            retry_after: None,
        }
    }

    /// Stable, machine-readable error class.
    pub fn code(&self) -> &'static str {
        match self {
            Error::RateLimited { .. } => "upstream_rate_limited",
            Error::Parse(_) => "parse_failed",
            Error::Http(_) => "http_status",
            Error::Network(_) => "network",
            Error::Config(_) => "config",
            Error::Usage(_) => "usage",
            Error::Blocked(_) => "blocked_by_policy",
            Error::TooLarge { .. } => "response_too_large",
            Error::UnsupportedContent { .. } => "unsupported_content_type",
            Error::NoResults(_) => "no_results",
        }
    }

    /// Process exit code this error should produce.
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Usage(_) => 2,
            _ => 1,
        }
    }

    /// True when trying a different engine (or retrying later) could help.
    pub fn is_retryable(&self) -> bool {
        matches!(
            self,
            Error::RateLimited { .. } | Error::Network(_) | Error::Parse(_) | Error::Http(_)
        )
    }

    /// Server-advertised backoff, when known.
    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Error::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RateLimited { msg, retry_after } => match retry_after {
                Some(d) => write!(
                    f,
                    "rate-limited by search engine: {msg} (retry after {}s)",
                    d.as_secs()
                ),
                None => write!(f, "rate-limited by search engine: {msg}"),
            },
            Error::Parse(msg) => write!(f, "failed to parse response: {msg}"),
            Error::Http(code) => write!(f, "HTTP {code} from upstream"),
            Error::Network(msg) => write!(f, "network error: {msg}"),
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::Usage(msg) => write!(f, "usage error: {msg}"),
            Error::Blocked(msg) => write!(f, "blocked by egress policy: {msg}"),
            Error::TooLarge { limit } => {
                write!(f, "response exceeded the {limit}-byte limit")
            }
            Error::UnsupportedContent { content_type } => write!(
                f,
                "unsupported content type '{content_type}' (webseek extracts text from HTML/plain text)"
            ),
            Error::NoResults(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Network(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usage_errors_exit_with_two() {
        assert_eq!(Error::Usage("x".into()).exit_code(), 2);
        assert_eq!(Error::Config("x".into()).exit_code(), 1);
        assert_eq!(Error::Http(404).exit_code(), 1);
    }

    #[test]
    fn codes_are_stable() {
        assert_eq!(Error::rate_limited("x").code(), "upstream_rate_limited");
        assert_eq!(Error::Blocked("x".into()).code(), "blocked_by_policy");
        assert_eq!(Error::TooLarge { limit: 1 }.code(), "response_too_large");
    }

    #[test]
    fn only_transient_classes_are_retryable() {
        assert!(Error::rate_limited("x").is_retryable());
        assert!(Error::Http(500).is_retryable());
        assert!(!Error::Blocked("x".into()).is_retryable());
        assert!(!Error::Usage("x".into()).is_retryable());
        assert!(!Error::TooLarge { limit: 1 }.is_retryable());
    }
}
