//! Typed errors with stable exit-code semantics.
//!
//! Exit codes (documented in README):
//! - `0`: success (including "no results" — empty JSON is valid for agents)
//! - `1`: runtime error (network, parse, config, ...)
//! - `2`: CLI usage error (clap)
//!
//! Every variant also carries a stable [`Error::kind`] slug. Batch output
//! embeds it so an agent can branch on the *class* of failure without parsing
//! English prose.

use std::fmt;

#[derive(Debug)]
pub enum Error {
    /// The search engine refused the request (rate limit, CAPTCHA, bot check).
    RateLimited(String),
    /// The response could not be parsed (HTML structure changed upstream).
    Parse(String),
    /// HTTP status outside 2xx.
    Http(u16),
    /// Network / transport failure.
    Network(String),
    /// Bad configuration or CLI value.
    Config(String),
    /// No usable result at all.
    NoResults(String),
    /// Fetching was refused by the site's robots.txt.
    Robots(String),
}

impl Error {
    /// Stable, machine-readable classification. Part of the JSON contract.
    pub fn kind(&self) -> &'static str {
        match self {
            Error::RateLimited(_) => "rate_limited",
            Error::Parse(_) => "parse",
            Error::Http(_) => "http",
            Error::Network(_) => "network",
            Error::Config(_) => "config",
            Error::NoResults(_) => "no_results",
            Error::Robots(_) => "robots",
        }
    }

    /// Is this the kind of failure another engine might succeed at?
    ///
    /// Config errors (an unknown engine, a bad URL) are the user's, not the
    /// upstream's: retrying them elsewhere would only hide the mistake.
    pub fn is_engine_retryable(&self) -> bool {
        matches!(
            self,
            Error::RateLimited(_) | Error::Network(_) | Error::Parse(_) | Error::Http(_)
        )
    }

    /// Standard message for a robots.txt refusal, identical in every code path.
    pub fn robots_blocked(url: &str) -> Error {
        Error::Robots(format!(
            "blocked by robots.txt: {url} (override with --no-respect-robots)"
        ))
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::RateLimited(msg) => write!(f, "rate-limited by search engine: {msg}"),
            Error::Parse(msg) => write!(f, "failed to parse response: {msg}"),
            Error::Http(code) => write!(f, "HTTP {code} from upstream"),
            Error::Network(msg) => write!(f, "network error: {msg}"),
            Error::Config(msg) => write!(f, "configuration error: {msg}"),
            Error::NoResults(msg) => write!(f, "{msg}"),
            Error::Robots(msg) => write!(f, "{msg}"),
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
    fn kinds_are_stable_slugs() {
        assert_eq!(Error::Http(404).kind(), "http");
        assert_eq!(Error::RateLimited("x".into()).kind(), "rate_limited");
        assert_eq!(Error::robots_blocked("https://x").kind(), "robots");
    }

    #[test]
    fn only_upstream_failures_are_worth_retrying_elsewhere() {
        assert!(Error::Http(503).is_engine_retryable());
        assert!(Error::RateLimited("x".into()).is_engine_retryable());
        assert!(Error::Parse("x".into()).is_engine_retryable());
        assert!(Error::Network("x".into()).is_engine_retryable());
        // The user's own mistakes must surface, not silently reroute.
        assert!(!Error::Config("bad engine".into()).is_engine_retryable());
        assert!(!Error::robots_blocked("https://x").is_engine_retryable());
    }

    #[test]
    fn robots_message_names_the_escape_hatch_that_actually_exists() {
        let msg = Error::robots_blocked("https://x/p").to_string();
        assert!(msg.contains("--no-respect-robots"), "got: {msg}");
    }
}
