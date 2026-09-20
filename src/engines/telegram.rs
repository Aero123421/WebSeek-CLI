//! Public Telegram channel posts via `t.me/s/<channel>` (no key, no login).
//!
//! There is no key-free cross-channel Telegram search: the query names a
//! **channel** (`@name`, `name`, or a `t.me/...` link) and the engine returns
//! that channel's latest posts, walking `?before=` while more are needed.
//! Trailing words search *within* the channel (`@name rust async`, sent as
//! `?q=`). Private channels, groups and DMs need authenticated MTProto/API
//! access and are out of scope — an unknown-or-private channel redirects away
//! from `/s/` and surfaces as [`Error::NoResults`]; the two cases are
//! indistinguishable without authentication.

use reqwest::blocking::Client;
use scraper::{Html, Selector};
use url::Url;

use crate::engines::{dedupe_and_truncate, SearchEngine};
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{encode_path_segment, http_url, join_meta, normalize_snippet};

const BASE_URL: &str = "https://t.me";
/// `t.me/s` renders ~20 posts per page (observed, not contractual); cap the
/// `?before=` walk so a large `--count` cannot page forever.
const MAX_PAGES: usize = 4;
/// Longest accepted in-channel keyword string.
const MAX_KEYWORDS_CHARS: usize = 512;

pub struct Telegram {
    base: String,
}

impl Default for Telegram {
    fn default() -> Self {
        Self {
            base: BASE_URL.to_string(),
        }
    }
}

impl Telegram {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

impl SearchEngine for Telegram {
    fn name(&self) -> &'static str {
        "telegram"
    }

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let (channel, keywords) = parse_channel(query)?;
        let mut out = Vec::new();
        let mut before: Option<u64> = None;
        for _ in 0..MAX_PAGES {
            let url = page_url(&self.base, &channel, before, keywords.as_deref())?;
            let resp = opts
                .send(client.get(url))
                .map_err(|e| Error::Network(format!("telegram request failed: {e}")))?;
            crate::http::scrape_status(resp.status().as_u16())?;
            if !resp.url().path().starts_with("/s/") {
                // Unknown and private channels both bounce to `t.me/<name>`.
                // On later pages a redirect just means the history ran out.
                if before.is_none() {
                    return Err(Error::NoResults(format!(
                        "unknown or private channel '{channel}' \
                         (t.me/s/{channel} redirected away)"
                    )));
                }
                break;
            }
            let body = crate::http::response_text(resp)?;
            if crate::reader::max_nesting_depth(&body) > crate::reader::MAX_NESTING_DEPTH {
                return Err(Error::Parse("document nesting too deep".into()));
            }
            if crate::engines::looks_like_challenge(&body) {
                return Err(Error::RateLimited(
                    "telegram served a bot-challenge page instead of results".into(),
                ));
            }
            let page = parse_page(&body, &channel, &self.base);
            if page.results.is_empty() {
                // A first page with no messages and no channel header is not a
                // channel page at all — the layout changed — while later pages
                // may legitimately render without one, so only the first page
                // is judged this strictly.
                if before.is_none() && !page.has_channel {
                    return Err(Error::Parse(
                        "t.me/s layout changed: no messages and no channel header".into(),
                    ));
                }
                break;
            }
            let more_before = page.oldest_id;
            out.extend(page.results);
            if out.len() >= opts.count {
                break;
            }
            match more_before {
                Some(id) if before != Some(id) => before = Some(id),
                _ => break,
            }
        }
        Ok(dedupe_and_truncate(out, opts.count, |r| &r.url))
    }
}

/// One parsed `t.me/s` page.
pub struct ParsedPage {
    pub results: Vec<SearchResult>,
    /// Oldest post id on the page, for the next `?before=` hop.
    pub oldest_id: Option<u64>,
    /// Whether the channel info header rendered at all.
    pub has_channel: bool,
}

/// Accept `@name`, `name`, `t.me/name` and `https://t.me/[s/]name`, with
/// optional trailing keywords for in-channel search (`@name rust async`).
/// Returns the channel plus the keyword string, if any.
pub fn parse_channel(query: &str) -> Result<(String, Option<String>)> {
    // Keywords ride along after the channel token (`@name rust async`).
    let mut parts = query.split_whitespace();
    let head = parts.next().unwrap_or("");
    let keywords = {
        let rest: Vec<&str> = parts.collect();
        if rest.is_empty() {
            None
        } else {
            let joined = rest.join(" ");
            check_keywords_len(&joined)?;
            Some(joined)
        }
    };
    let mut q = head.strip_prefix('@').unwrap_or(head);
    // ASCII-only prefixes, so slicing at `prefix.len()` is always a char
    // boundary: the matched bytes are the same ASCII letters.
    let lower = q.to_ascii_lowercase();
    for prefix in [
        "https://t.me/s/",
        "https://t.me/",
        "http://t.me/s/",
        "http://t.me/",
        "t.me/s/",
        "t.me/",
    ] {
        if lower.starts_with(prefix) {
            q = &q[prefix.len()..];
            break;
        }
    }
    // A `t.me/s/name?q=...` link carries its own keywords.
    let mut q_keywords = None;
    if let Some((path, params)) = q.split_once('?') {
        q = path;
        for pair in params.split('&') {
            if let Some(value) = pair.strip_prefix("q=") {
                let decoded = percent_encoding::percent_decode_str(&value.replace('+', " "))
                    .decode_utf8_lossy()
                    .into_owned();
                let trimmed = decoded.trim();
                if !trimmed.is_empty() {
                    // Pasted links get the same cap as typed keywords: length
                    // must not depend on which door the input came through.
                    check_keywords_len(trimmed)?;
                    q_keywords = Some(trimmed.to_string());
                }
            }
        }
    }
    let name = q.split(['/', '#']).next().unwrap_or("").trim();
    let valid = (5..=32).contains(&name.len())
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_');
    if !valid {
        return Err(Error::Config(format!(
            "telegram query must be a public channel (@name, name, or a t.me/ link), got '{query}'"
        )));
    }
    Ok((name.to_string(), keywords.or(q_keywords)))
}

/// Both keyword doors — trailing words and a pasted `?q=` — share one cap.
fn check_keywords_len(s: &str) -> Result<()> {
    if s.chars().count() > MAX_KEYWORDS_CHARS {
        return Err(Error::Config(format!(
            "telegram search keywords exceed {MAX_KEYWORDS_CHARS} characters"
        )));
    }
    Ok(())
}

fn page_url(base: &str, channel: &str, before: Option<u64>, keywords: Option<&str>) -> Result<Url> {
    let mut url = Url::parse(&format!(
        "{}/s/{}",
        base.trim_end_matches('/'),
        encode_path_segment(channel)
    ))
    .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
    if let Some(kw) = keywords {
        url.query_pairs_mut().append_pair("q", kw);
    }
    if let Some(id) = before {
        url.query_pairs_mut().append_pair("before", &id.to_string());
    }
    Ok(url)
}

/// Pure parser (unit-tested against fixtures): HTML in, posts out. Never
/// assumes a page size — `t.me/s` renders ~20 posts per page by observation,
/// not by contract.
pub fn parse_page(html: &str, channel: &str, base: &str) -> ParsedPage {
    let doc = Html::parse_document(html);
    let wrap_sel =
        Selector::parse("div.tgme_widget_message_wrap").unwrap_or_else(|_| unreachable!("static"));
    let msg_sel =
        Selector::parse(".tgme_widget_message").unwrap_or_else(|_| unreachable!("static"));
    let text_sel =
        Selector::parse(".tgme_widget_message_text").unwrap_or_else(|_| unreachable!("static"));
    let date_sel =
        Selector::parse("a.tgme_widget_message_date").unwrap_or_else(|_| unreachable!("static"));
    let time_sel = Selector::parse("a.tgme_widget_message_date time")
        .unwrap_or_else(|_| unreachable!("static"));
    let views_sel =
        Selector::parse(".tgme_widget_message_views").unwrap_or_else(|_| unreachable!("static"));
    let header_sel =
        Selector::parse(".tgme_channel_info_header").unwrap_or_else(|_| unreachable!("static"));

    let mut results = Vec::new();
    let mut oldest_id: Option<u64> = None;
    for wrap in doc.select(&wrap_sel) {
        let inner = wrap.select(&msg_sel).next();
        // `data-post` lives on the message element; fall back to the wrapper
        // so minor markup moves don't silently drop every post.
        let data_post = wrap
            .value()
            .attr("data-post")
            .or_else(|| inner.and_then(|m| m.value().attr("data-post")));
        let (post_channel, post_id) = match data_post.and_then(split_post_ref) {
            Some(pair) => pair,
            None => continue,
        };
        if let Some(id) = post_id {
            oldest_id = Some(oldest_id.map_or(id, |old| old.min(id)));
        }
        let date_link = wrap.select(&date_sel).next();
        let url = date_link
            .and_then(|a| a.value().attr("href"))
            .and_then(http_url)
            .or_else(|| {
                post_id.map(|id| {
                    format!(
                        "{}/{post_channel}/{id}",
                        base.trim_end_matches('/'),
                        post_channel = encode_path_segment(post_channel)
                    )
                })
            });
        let Some(url) = url else {
            continue;
        };
        // Live markup nests `<time datetime="...">` inside the date link;
        // fall back to the link itself so either shape yields a date.
        let published = wrap
            .select(&time_sel)
            .next()
            .and_then(|t| t.value().attr("datetime"))
            .or_else(|| date_link.and_then(|a| a.value().attr("datetime")))
            .and_then(crate::time::normalize_iso);
        let text = wrap
            .select(&text_sel)
            .next()
            .map(|t| t.text().collect::<String>())
            .unwrap_or_default();
        let views = wrap
            .select(&views_sel)
            .next()
            .map(|v| v.text().collect::<String>().trim().to_string())
            .unwrap_or_default();
        let views_part = if views.is_empty() {
            String::new()
        } else {
            format!("{views} views")
        };
        results.push(SearchResult {
            // The date lives in `published` now; pretty output reattaches it.
            title: format!("@{channel}"),
            url,
            snippet: normalize_snippet(&join_meta(&[text.trim(), &views_part])),
            published,
        });
    }
    ParsedPage {
        results,
        oldest_id,
        has_channel: doc.select(&header_sel).next().is_some(),
    }
}

/// Split `data-post="<channel>/<id>"` into its parts. The id is optional in
/// the return so service rows without one still map when they carry a link.
fn split_post_ref(data_post: &str) -> Option<(&str, Option<u64>)> {
    let (channel, id) = data_post.split_once('/')?;
    if channel.is_empty() {
        return None;
    }
    Some((channel, id.parse::<u64>().ok()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"<html><body>
      <div class="tgme_channel_info_header">
        <span class="tgme_channel_info_header_username">@testchannel</span>
      </div>
      <div class="tgme_widget_message_wrap">
        <div class="tgme_widget_message" data-post="testchannel/99">
          <div class="tgme_widget_message_text">Hello <b>world</b> &amp; friends</div>
          <a class="tgme_widget_message_date" href="https://t.me/testchannel/99"><time datetime="2026-09-20T10:00:00+00:00" class="time">Sep 20</time></a>
          <span class="tgme_widget_message_views">1.2K</span>
        </div>
      </div>
      <div class="tgme_widget_message_wrap">
        <div class="tgme_widget_message" data-post="testchannel/80">
          <div class="tgme_widget_message_text"></div>
          <span class="tgme_widget_message_views">7</span>
        </div>
      </div>
    </body></html>"#;

    #[test]
    fn parses_channel_posts_and_falls_back_to_data_post_url() {
        let page = parse_page(FIXTURE, "testchannel", "https://t.me");
        assert!(page.has_channel);
        assert_eq!(page.oldest_id, Some(80));
        assert_eq!(page.results.len(), 2);
        assert_eq!(page.results[0].title, "@testchannel");
        assert_eq!(page.results[0].url, "https://t.me/testchannel/99");
        assert_eq!(
            page.results[0].snippet,
            "Hello world & friends · 1.2K views"
        );
        assert_eq!(
            page.results[0].published.as_deref(),
            Some("2026-09-20T10:00:00+00:00")
        );
        // No date link: the URL is rebuilt from `data-post`, and the
        // media-only post still carries its view count.
        assert_eq!(page.results[1].url, "https://t.me/testchannel/80");
        assert_eq!(page.results[1].title, "@testchannel");
        assert_eq!(page.results[1].snippet, "7 views");
        assert_eq!(page.results[1].published, None);
    }

    #[test]
    fn empty_page_reports_whether_it_is_a_channel_page() {
        let header_only =
            r#"<html><body><div class="tgme_channel_info_header">x</div></body></html>"#;
        let page = parse_page(header_only, "testchannel", "https://t.me");
        assert!(page.has_channel);
        assert!(page.results.is_empty());
        assert_eq!(page.oldest_id, None);

        let foreign = "<html><body><p>something else entirely</p></body></html>";
        let page = parse_page(foreign, "testchannel", "https://t.me");
        assert!(!page.has_channel);
        assert!(page.results.is_empty());
    }

    #[test]
    fn channel_identifier_forms_are_accepted() {
        for q in [
            "testchannel",
            "@testchannel",
            "  @testchannel  ",
            "t.me/testchannel",
            "t.me/s/testchannel",
            "https://t.me/testchannel",
            "https://t.me/s/testchannel",
            "HTTPS://T.ME/TestChannel",
            "https://t.me/testchannel/99",
        ] {
            let (channel, keywords) = parse_channel(q).unwrap();
            assert_eq!(channel.to_ascii_lowercase(), "testchannel", "for {q:?}");
            assert_eq!(keywords, None, "for {q:?}");
        }
    }

    #[test]
    fn trailing_words_become_in_channel_keywords() {
        let (channel, keywords) = parse_channel("@testchannel rust async").unwrap();
        assert_eq!(channel, "testchannel");
        assert_eq!(keywords.as_deref(), Some("rust async"));
        let (channel, keywords) = parse_channel("testchannel   spaced   out  ").unwrap();
        assert_eq!(channel, "testchannel");
        assert_eq!(keywords.as_deref(), Some("spaced out"));
        // A t.me link can carry its own ?q= keywords.
        let (channel, keywords) =
            parse_channel("https://t.me/s/testchannel?q=rust%20async").unwrap();
        assert_eq!(channel, "testchannel");
        assert_eq!(keywords.as_deref(), Some("rust async"));
        // Trailing keywords win over the link's own ?q=.
        let (_, keywords) = parse_channel("https://t.me/s/testchannel?q=old new words").unwrap();
        assert_eq!(keywords.as_deref(), Some("new words"));
        // Overlong keywords are rejected before any request.
        let long = format!("@testchannel {}", "x".repeat(513));
        assert!(parse_channel(&long).is_err());
    }

    #[test]
    fn overlong_keywords_are_rejected_through_both_doors() {
        let long = "x".repeat(MAX_KEYWORDS_CHARS + 1);
        assert!(parse_channel(&format!("@testchannel {long}")).is_err());
        assert!(parse_channel(&format!("https://t.me/s/testchannel?q={long}")).is_err());
        // Exactly at the cap passes.
        let ok = "x".repeat(MAX_KEYWORDS_CHARS);
        assert!(parse_channel(&format!("@testchannel {ok}")).is_ok());
    }

    #[test]
    fn non_channel_queries_are_usage_errors() {
        for q in [
            "",
            "@",
            "ab",
            "rust lang",
            "no-dashes here",
            "https://evil.com/t.me/testchannel",
            "https://example.com/x",
        ] {
            assert!(parse_channel(q).is_err(), "{q:?} must be rejected");
        }
    }

    #[test]
    fn page_url_carries_keywords_and_pagination() {
        let url = page_url("https://t.me", "testchannel", None, Some("rust async")).unwrap();
        assert_eq!(url.as_str(), "https://t.me/s/testchannel?q=rust+async");
        let url = page_url("https://t.me/", "testchannel", Some(80), Some("x")).unwrap();
        assert_eq!(url.as_str(), "https://t.me/s/testchannel?q=x&before=80");
        let url = page_url("https://t.me", "testchannel", None, None).unwrap();
        assert_eq!(url.as_str(), "https://t.me/s/testchannel");
    }
}
