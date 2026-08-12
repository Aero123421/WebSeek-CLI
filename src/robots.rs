//! robots.txt support (opt-in via `--respect-robots`).
//!
//! Scope: the `*` (wildcard) user-agent group, with RFC 9309 matching —
//! longest-pattern-wins, `Allow` beating `Disallow` on a tie, and `*` / `$`
//! wildcards inside path patterns.
//!
//! The wildcards matter more than their obscurity suggests. `Disallow: /*` is
//! a common way to say "no bots here"; treating it as the literal prefix `/*`
//! matches nothing, so the sites that most clearly refuse crawlers would have
//! been the ones webseek happily crawled. A robots parser that fails open on
//! the strictest input is worse than none, because it looks like compliance.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::blocking::Client;
use url::Url;

use crate::error::{Error, Result};
use crate::pace::Pacer;

/// Cap on the robots.txt body we will read (Google's limit is 500 KiB).
const MAX_ROBOTS_BYTES: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Rule {
    pattern: String,
    allow: bool,
}

#[derive(Debug, Default, Clone)]
pub struct RobotsRules {
    rules: Vec<Rule>,
}

impl RobotsRules {
    /// Is fetching `path` permitted?
    ///
    /// RFC 9309 §2.2.2: the most specific (longest) matching pattern wins; on
    /// equal length `Allow` wins. Unmatched paths are allowed.
    pub fn is_allowed(&self, path: &str) -> bool {
        let mut best: Option<(usize, bool)> = None; // (pattern length, allow)
        for rule in &self.rules {
            if !pattern_matches(&rule.pattern, path) {
                continue;
            }
            let len = rule.pattern.len();
            match best {
                // Longer pattern wins; on a tie, Allow wins.
                Some((best_len, best_allow)) => {
                    if len > best_len || (len == best_len && rule.allow && !best_allow) {
                        best = Some((len, rule.allow));
                    }
                }
                None => best = Some((len, rule.allow)),
            }
        }
        best.map(|(_, allow)| allow).unwrap_or(true)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.rules.len()
    }
}

/// RFC 9309 path matching: `*` matches any sequence, `$` anchors the end.
///
/// Implemented as greedy segment matching with backtracking, which is linear
/// in practice for the short patterns robots.txt uses.
fn pattern_matches(pattern: &str, path: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(p) => (p, true),
        None => (pattern, false),
    };

    let segments: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;

    for (i, seg) in segments.iter().enumerate() {
        let first = i == 0;
        let last = i == segments.len() - 1;

        if seg.is_empty() {
            // Trailing "*$" — anything up to the end matches. Guarded on
            // `segments.len() > 1`, because a bare "$" produces one empty
            // segment and must match only the empty path, not everything.
            if last && anchored && segments.len() > 1 {
                return true;
            }
            continue;
        }
        if first {
            // The pattern is anchored at the start of the path.
            if !path[pos..].starts_with(seg) {
                return false;
            }
            pos += seg.len();
        } else if last && anchored {
            // Final literal must sit exactly at the end.
            return path.len() >= pos + seg.len() && path[pos..].ends_with(seg);
        } else {
            match path[pos..].find(seg) {
                Some(idx) => pos += idx + seg.len(),
                None => return false,
            }
        }
    }

    if anchored {
        // Pattern had no trailing wildcard: it must consume the whole path.
        pos == path.len()
    } else {
        true
    }
}

/// Parse a robots.txt body, honoring only the `*` user-agent group.
///
/// Consecutive `User-agent` lines form a single group (RFC 9309 §2.2.1), so
/// `User-agent: *` followed by `User-agent: googlebot` applies to both — the
/// rules that follow still bind us.
pub fn parse_robots(body: &str) -> RobotsRules {
    let mut rules = RobotsRules::default();
    // Are we inside a group whose agent list includes `*`?
    let mut group_matches = false;
    // True while reading a run of consecutive User-agent lines.
    let mut collecting_agents = false;

    for raw_line in body.lines() {
        // Strip trailing comments, which are legal mid-line.
        let line = match raw_line.find('#') {
            Some(i) => &raw_line[..i],
            None => raw_line,
        }
        .trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();

        match key.as_str() {
            "user-agent" => {
                if !collecting_agents {
                    // A User-agent line after rules starts a brand-new group.
                    collecting_agents = true;
                    group_matches = false;
                }
                if value == "*" {
                    group_matches = true;
                }
            }
            "disallow" | "allow" => {
                collecting_agents = false;
                if !group_matches {
                    continue;
                }
                // "Disallow:" with an empty value means "nothing is forbidden"
                // and carries no rule.
                if value.is_empty() {
                    continue;
                }
                rules.rules.push(Rule {
                    pattern: value.to_string(),
                    allow: key == "allow",
                });
            }
            // Any other field ends the run of User-agent lines. `Crawl-delay`
            // is the common case: without this, a following
            // `User-agent: SomeOtherBot` was folded into the `*` group and its
            // `Disallow: /` applied to us, blocking the entire site.
            _ => collecting_agents = false,
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

    /// Returns `Ok(true)` when the URL may be fetched. `Ok(true)` on any lookup
    /// failure (missing robots.txt, network error) — robots is advisory.
    pub fn is_allowed(&mut self, client: &Client, pacer: &Pacer, url: &str) -> Result<bool> {
        let parsed =
            Url::parse(url).map_err(|e| Error::Config(format!("invalid URL '{url}': {e}")))?;
        let host = parsed.host_str().unwrap_or("");
        // Preserve an explicit port so origins like `http://127.0.0.1:8080`
        // resolve their robots.txt on the right endpoint.
        let origin = match parsed.port() {
            Some(p) => format!("{}://{}:{}", parsed.scheme(), host, p),
            None => format!("{}://{}", parsed.scheme(), host),
        };

        // RFC 9309 §2.2.2 matches against path *and* query.
        let mut path = parsed.path().to_string();
        if let Some(q) = parsed.query() {
            path.push('?');
            path.push_str(q);
        }

        let rules = match self.cache.get(&origin) {
            Some(entry) => entry.clone(),
            None => {
                let fetched = self.fetch_rules(client, pacer, &origin);
                self.cache.insert(origin.clone(), fetched.clone());
                fetched
            }
        };
        Ok(rules.map(|r| r.is_allowed(&path)).unwrap_or(true))
    }

    fn fetch_rules(&self, client: &Client, pacer: &Pacer, origin: &str) -> Option<RobotsRules> {
        use std::io::Read as _;

        let url = format!("{origin}/robots.txt");
        pacer.wait();
        let resp = client
            .get(&url)
            .timeout(Duration::from_secs(5))
            .send()
            .ok()?;
        if !resp.status().is_success() {
            return None;
        }
        // Bounded read: a hostile robots.txt should not be able to exhaust
        // memory on a request we make automatically.
        let mut buf = Vec::new();
        resp.take(MAX_ROBOTS_BYTES as u64)
            .read_to_end(&mut buf)
            .ok()?;
        Some(parse_robots(&String::from_utf8_lossy(&buf)))
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
            \n\
            User-agent: *\n\
            Disallow: /private/\n\
            Disallow: /tmp\n\
            Allow: /private/open\n";
        let rules = parse_robots(body);
        assert!(rules.is_allowed("/public"));
        assert!(
            rules.is_allowed("/no-bots"),
            "googlebot's group is not ours"
        );
        assert!(!rules.is_allowed("/private/secret"));
        assert!(!rules.is_allowed("/tmp/x"));
        assert!(rules.is_allowed("/private/open"));
    }

    #[test]
    fn consecutive_user_agent_lines_form_one_group() {
        // RFC 9309 §2.2.1. The old parser let the *last* agent line win, so
        // this ordering silently dropped the rules that bind us.
        let body = "User-agent: *\nUser-agent: googlebot\nDisallow: /x\n";
        let rules = parse_robots(body);
        assert_eq!(rules.len(), 1, "the group applies to us as well");
        assert!(!rules.is_allowed("/x/1"));

        // Reversed order must behave identically.
        let body = "User-agent: googlebot\nUser-agent: *\nDisallow: /x\n";
        assert!(!parse_robots(body).is_allowed("/x/1"));
    }

    #[test]
    fn crawl_delay_ends_the_user_agent_run() {
        // A very common shape. If `Crawl-delay` does not close the run of
        // User-agent lines, the next bot's group is folded into ours and its
        // `Disallow: /` blocks the entire site for us.
        let body = "User-agent: *\nCrawl-delay: 10\n\nUser-agent: SemrushBot\nDisallow: /\n";
        let rules = parse_robots(body);
        assert!(
            rules.is_allowed("/article"),
            "another bot's Disallow was applied to us"
        );
        assert_eq!(rules.len(), 0, "we have no rules in this file");
    }

    #[test]
    fn a_bare_dollar_matches_only_the_empty_path() {
        let rules = parse_robots("User-agent: *\nDisallow: $\n");
        assert!(
            rules.is_allowed("/x"),
            "`Disallow: $` must not block the site"
        );
        assert!(rules.is_allowed("/"));
        assert!(!pattern_matches("$", "/x"));
        assert!(pattern_matches("$", ""));
    }

    #[test]
    fn a_new_group_after_rules_resets_the_match() {
        let body = "User-agent: *\nDisallow: /a\nUser-agent: bingbot\nDisallow: /b\n";
        let rules = parse_robots(body);
        assert!(!rules.is_allowed("/a"));
        assert!(rules.is_allowed("/b"), "bingbot's rules are not ours");
    }

    #[test]
    fn blanket_disallow_wildcard_is_honored() {
        // The failure that mattered: `Disallow: /*` read as a literal prefix
        // matched nothing, so "block everything" meant "crawl everything".
        for body in [
            "User-agent: *\nDisallow: /*\n",
            "User-agent: *\nDisallow: /\n",
        ] {
            let rules = parse_robots(body);
            assert!(!rules.is_allowed("/"), "body: {body:?}");
            assert!(!rules.is_allowed("/anything/at/all"), "body: {body:?}");
        }
    }

    #[test]
    fn wildcards_and_end_anchors() {
        let rules = parse_robots("User-agent: *\nDisallow: /*.pdf$\nDisallow: /a/*/b\n");
        assert!(!rules.is_allowed("/docs/file.pdf"));
        assert!(rules.is_allowed("/docs/file.pdf.html"), "$ anchors the end");
        assert!(!rules.is_allowed("/a/x/b"));
        assert!(!rules.is_allowed("/a/x/y/b"));
        assert!(rules.is_allowed("/a/x/c"));
    }

    #[test]
    fn query_strings_participate_in_matching() {
        let rules = parse_robots("User-agent: *\nDisallow: /search?\n");
        assert!(!rules.is_allowed("/search?q=x"));
        assert!(rules.is_allowed("/search"));
    }

    #[test]
    fn allow_without_disallow_is_unrestricted() {
        let rules = parse_robots("User-agent: *\nAllow: /\n");
        assert!(rules.is_allowed("/anything"));
    }

    #[test]
    fn comments_and_empty_lines_are_ignored() {
        let rules = parse_robots("# comment\n\nUser-agent: *\n# inner\nDisallow: /x  # trailing\n");
        assert!(!rules.is_allowed("/x/1"));
        assert!(rules.is_allowed("/y"));
    }

    #[test]
    fn longest_match_wins_and_allow_breaks_ties() {
        let rules = parse_robots("User-agent: *\nDisallow: /a\nAllow: /a/b\n");
        assert!(!rules.is_allowed("/a/c"));
        assert!(rules.is_allowed("/a/b/c"));

        let tie = parse_robots("User-agent: *\nDisallow: /p\nAllow: /p\n");
        assert!(tie.is_allowed("/p"), "equal length -> Allow wins");
    }

    #[test]
    fn empty_disallow_permits_everything() {
        let rules = parse_robots("User-agent: *\nDisallow:\n");
        assert!(rules.is_allowed("/anything"));
    }

    #[test]
    fn no_rules_means_allowed() {
        assert!(parse_robots("").is_allowed("/"));
    }

    #[test]
    fn sitemap_lines_do_not_break_the_group() {
        let rules =
            parse_robots("User-agent: *\nSitemap: https://x/sitemap.xml\nDisallow: /private\n");
        assert!(!rules.is_allowed("/private/x"));
    }

    #[test]
    fn pattern_matcher_basics() {
        assert!(pattern_matches("/a", "/abc"));
        assert!(!pattern_matches("/b", "/abc"));
        assert!(pattern_matches("/*", "/anything"));
        assert!(pattern_matches("/a*c", "/abbbc"));
        assert!(pattern_matches("/a$", "/a"));
        assert!(!pattern_matches("/a$", "/ab"));
        assert!(pattern_matches("/*$", "/whatever"));
    }
}
