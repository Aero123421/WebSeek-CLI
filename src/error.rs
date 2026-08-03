//! Typed errors with stable exit-code semantics.
//!
//! Exit codes (documented in README):
//! - `0`: success (including "no results" — empty JSON is valid for agents)
//! - `1`: runtime error (network, parse, config, ...)
//! - `2`: CLI usage error (clap)

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
