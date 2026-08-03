//! Minimal robots.txt support (opt-in via `--respect-robots`).
//!
//! Scope: the `*` (wildcard) user-agent group only, with RFC 9309-style
//! longest-prefix matching for `Allow` / `Disallow`. Wildcards (`*`, `$`)
//! inside path patterns are **not** expanded — that is deliberately
//! documented as a limitation of this minimal implementation.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::blocking::Client;
use url::Url;

use crate::error::{Error, Result};

#[derive(Debug, Default, Clone)]
pub struct RobotsRules {
    /// `Disallow` prefixes for user-agent `*`.
    disallow: Vec<String>,
    /// `Allow` prefixes for user-agent `*`.
    allow: Vec<String>,
}

impl RobotsRules {
    /// Is fetching `path` permitted?
    pub fn is_allowed(&self, path: &str) -> bool {
        // Longest-matching directive wins; Allow beats Disallow on a tie
        // (RFC 9309 §2.2.2.A.2).
        let allow_len = self
            .allow
            .iter()
            .filter(|a| path.starts_with(a.as_str()))
            .map(|a| a.len())
            .max();
        let disallow_len = self
            .disallow
            .iter()
            .filter(|d| path.starts_with(d.as_str()))
            .map(|d| d.len())
            .max();
        match (allow_len, disallow_len) {
            (Some(al), Some(dl)) => al >= dl,
            (Some(_), None) => true,
            (None, Some(_)) => false,
            (None, None) => true, // not mentioned -> allowed
        }
    }
}

/// Parse a robots.txt body, honoring only the `*` user-agent group.
/// Pure and testable.
pub fn parse_robots(body: &str) -> RobotsRules {
    let mut rules = RobotsRules::default();
    let mut in_wildcard_group = false;

    for raw_line in body.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_ascii_lowercase();
            let value = value.trim().to_string();
            match key.as_str() {
                "user-agent" => {
                    // A new group starts; only the `*` group interests us.
                    in_wildcard_group = value == "*" || value.eq_ignore_ascii_case("all");
                }
                "disallow" if in_wildcard_group && !value.is_empty() => {
                    rules.disallow.push(value);
                }
                "allow" if in_wildcard_group && !value.is_empty() => {
                    rules.allow.push(value);
                }
                _ => {}
            }
        }
    }
    rules
}

/// Per-origin cache shared across a run (fetch may hit many URLs).
#[derive(Default)]
pub struct RobotsChecker {
    cache: HashMap<String, Option<RobotsRules>>,
}

impl RobotsChecker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns `Ok(true)` when the URL may be fetched. `Ok` on any lookup
    /// failure (missing robots.txt, network error) — robots is advisory.
    pub fn is_allowed(&mut self, client: &Client, url: &str) -> Result<bool> {
        let parsed =
            Url::parse(url).map_err(|e| Error::Config(format!("invalid URL '{url}': {e}")))?;
        let host = parsed.host_str().unwrap_or("");
        // Preserve an explicit port so origins like `http://127.0.0.1:8080`
        // resolve their robots.txt on the right endpoint.
        let origin = match parsed.port() {
            Some(p) => format!("{}://{}:{}", parsed.scheme(), host, p),
            None => format!("{}://{}", parsed.scheme(), host),
        };
        let path = parsed.path();

        let rules = match self.cache.get(&origin) {
            Some(entry) => entry.clone(),
            None => {
                let fetched = self.fetch_rules(client, &origin);
                self.cache.insert(origin.clone(), fetched.clone());
                fetched
            }
        };
        Ok(rules.map(|r| r.is_allowed(path)).unwrap_or(true))
    }

    fn fetch_rules(&self, client: &Client, origin: &str) -> Option<RobotsRules> {
        let url = format!("{origin}/robots.txt");
        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        let body = resp.text().ok()?;
        Some(parse_robots(&body))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcard_group_only() {
        let body = "\
            User-agent: googlebot\n\
            Disallow: /no-bots\n\
            User-agent: *\n\
            Disallow: /private/\n\
            Disallow: /tmp\n\
            Allow: /private/open\n";
        let rules = parse_robots(body);
        assert!(rules.is_allowed("/public"));
        assert!(!rules.is_allowed("/private/secret"));
        assert!(!rules.is_allowed("/tmp/x"));
        assert!(rules.is_allowed("/private/open")); // allow beats disallow
    }

    #[test]
    fn allow_without_disallow_is_unrestricted() {
        let rules = parse_robots("User-agent: *\nAllow: /\n");
        assert!(rules.is_allowed("/anything"));
    }

    #[test]
    fn comments_and_empty_lines_are_ignored() {
        let rules = parse_robots("# comment\n\nUser-agent: *\n# inner\nDisallow: /x\n");
        assert!(!rules.is_allowed("/x/1"));
        assert!(rules.is_allowed("/y"));
    }

    #[test]
    fn longest_match_wins() {
        let rules = parse_robots("User-agent: *\nDisallow: /a\nAllow: /a/b\n");
        assert!(!rules.is_allowed("/a/c"));
        assert!(rules.is_allowed("/a/b/c"));
    }

    #[test]
    fn no_rules_means_allowed() {
        let rules = parse_robots("");
        assert!(rules.is_allowed("/"));
    }
}
