//! Timestamps: normalizing upstream dates and filtering by time.
//!
//! Every date webseek keeps is an RFC 3339 UTC string (`published` on
//! [`SearchResult`][crate::models::SearchResult]); the helpers here convert
//! the formats upstreams actually speak (unix seconds, X's `created_at`,
//! RFC 2822 feed dates, ISO dates, bare `YYYY-MM-DD`) and parse `--since` /
//! `--until` bounds (durations like `24h` plus absolute dates).
//!
//! Unparseable input yields `None`, never a panic: a missing date means "the
//! engine could not say", which time filters treat as *not matching*.

use chrono::{DateTime, Datelike, TimeZone, Utc};

use crate::error::{Error, Result};
use crate::models::SearchResult;

/// Earliest plausible web timestamp (1990) and a small future tolerance for
/// clock skew, in seconds. Bounds outside this range are usage errors, not
/// silent empty answers.
const MIN_YEAR: i32 = 1990;
const FUTURE_SKEW_SECS: i64 = 300;

/// Unix seconds (HN `created_at_i`, Stack Exchange `creation_date`) to RFC 3339.
pub fn unix_to_rfc3339(secs: i64) -> Option<String> {
    if secs <= 0 {
        return None;
    }
    Utc.timestamp_opt(secs, 0)
        .single()
        .map(|dt| dt.to_rfc3339())
}

/// X/FxTwitter `created_at`: `Sun Sep 20 07:57:08 +0000 2026`.
pub fn x_created_at(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_str(s.trim(), "%a %b %d %H:%M:%S %z %Y")
        .ok()
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
}

/// RFC 2822 feed dates (`pubDate`): `Sun, 20 Sep 2026 07:57:08 +0000`.
pub fn rfc2822(s: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc2822(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc).to_rfc3339())
}

/// ISO 8601 / RFC 3339 timestamps and bare `YYYY-MM-DD` dates (midnight UTC).
/// Accepts a trailing `Z`, numeric offsets, and fractional seconds. Naive
/// timestamps without an offset (PyPI upload times) are read as UTC.
pub fn normalize_iso(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc).to_rfc3339());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f") {
        return Some(dt.and_utc().to_rfc3339());
    }
    if let Ok(dt) = chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S") {
        return Some(dt.and_utc().to_rfc3339());
    }
    // Bare date: Crossref/PubMed/OpenAlex grade partial dates.
    if let Ok(date) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return Some(date.and_hms_opt(0, 0, 0)?.and_utc().to_rfc3339());
    }
    None
}

/// Feed dates, either shape: RSS `pubDate` (RFC 2822) or Atom
/// `published`/`updated` (ISO 8601).
pub fn feed_date(s: &str) -> Option<String> {
    rfc2822(s).or_else(|| normalize_iso(s))
}

/// PubMed `pubdate`: `2021`, `2021 May`, `2021 May 5` (also `Spring 2021`-style
/// seasonal dates, which resolve to the season's first month).
pub fn pubmed_date(s: &str) -> Option<String> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    let mut parts = s.split_whitespace();
    let first = parts.next()?;
    // Year first: "2021", "2021 May", "2021 May 5". Split manually — chrono
    // cannot build a NaiveDate from partial fields (`%Y %b` dies NotEnough).
    if let Ok(year) = first.parse::<i32>() {
        let month = match parts.next() {
            None => 1,
            Some(name) => month_number(name)?,
        };
        let day: u32 = match parts.next() {
            None => 1,
            Some(d) => d.parse().ok()?,
        };
        if parts.next().is_some() {
            return None;
        }
        return chrono::NaiveDate::from_ymd_opt(year, month, day)?
            .and_hms_opt(0, 0, 0)
            .map(|d| d.and_utc().to_rfc3339());
    }
    // Season first: "Spring 2021", "Fall-Winter 2020".
    let year: i32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    let month = match first.trim_end_matches("-Winter").trim_end_matches("-Fall") {
        "Spring" => 3,
        "Summer" => 6,
        "Fall" | "Autumn" => 9,
        "Winter" => 12,
        _ => return None,
    };
    chrono::NaiveDate::from_ymd_opt(year, month, 1)?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().to_rfc3339())
}

/// English month name to number, tolerant of `Sept` and case.
fn month_number(name: &str) -> Option<u32> {
    match name.get(..3).map(|s| s.to_ascii_lowercase()).as_deref() {
        Some("jan") => Some(1),
        Some("feb") => Some(2),
        Some("mar") => Some(3),
        Some("apr") => Some(4),
        Some("may") => Some(5),
        Some("jun") => Some(6),
        Some("jul") => Some(7),
        Some("aug") => Some(8),
        Some("sep") => Some(9),
        Some("oct") => Some(10),
        Some("nov") => Some(11),
        Some("dec") => Some(12),
        _ => None,
    }
}

/// Crossref `date-parts`: `[year]`, `[year, month]` or `[year, month, day]`.
/// Missing parts default to January 1st.
pub fn date_parts(parts: &[i64]) -> Option<String> {
    let year: i32 = (*parts.first()?).try_into().ok()?;
    let month: u32 = parts.get(1).copied().unwrap_or(1).try_into().ok()?;
    let day: u32 = parts.get(2).copied().unwrap_or(1).try_into().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc().to_rfc3339())
}

/// Parse a `--since`/`--until` bound: a duration (`30m`, `24h`, `7d`, `4w`)
/// relative to now, or an absolute `YYYY-MM-DD` / RFC 3339 timestamp.
pub fn parse_bound(s: &str, flag: &str) -> Result<DateTime<Utc>> {
    let s = s.trim();
    if let Some(dt) = parse_duration(s) {
        return Ok(dt);
    }
    if let Some(dt) = parse_absolute(s) {
        check_plausible(dt, s, flag)?;
        return Ok(dt);
    }
    Err(Error::Config(format!(
        "invalid {flag} '{s}': expected a duration (30m, 24h, 7d, 4w) or a date (2026-09-20, 2026-09-20T15:04:05Z)"
    )))
}

fn parse_duration(s: &str) -> Option<DateTime<Utc>> {
    // Split after the last *char*, not the last byte: `split_at(len - 1)`
    // panics on multibyte input (`--since 24é`).
    let (num, unit) = match s.char_indices().next_back() {
        Some((i, _)) => (&s[..i], &s[i..]),
        None => return None,
    };
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num: i64 = num.parse().ok()?;
    if num <= 0 {
        return None;
    }
    let secs = match unit {
        "m" => num.checked_mul(60)?,
        "h" => num.checked_mul(3_600)?,
        "d" => num.checked_mul(86_400)?,
        "w" => num.checked_mul(604_800)?,
        _ => return None,
    };
    Utc::now().checked_sub_signed(chrono::Duration::seconds(secs))
}

fn parse_absolute(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d")
        .ok()?
        .and_hms_opt(0, 0, 0)
        .map(|d| d.and_utc())
}

/// A parsed `[since, until)` window: each side resolved, or unset.
pub type Window = (Option<DateTime<Utc>>, Option<DateTime<Utc>>);

/// Parse both bounds at once, rejecting an inverted or empty window: no
/// instant satisfies `[since, until)` when `since >= until`, so that is a
/// usage error naming both bounds, not a silent empty answer.
pub fn parse_window(
    since: Option<&str>,
    until: Option<&str>,
    since_flag: &str,
    until_flag: &str,
) -> Result<Window> {
    let since_dt = since.map(|s| parse_bound(s, since_flag)).transpose()?;
    let until_dt = until.map(|s| parse_bound(s, until_flag)).transpose()?;
    if let (Some(s), Some(u)) = (since_dt, until_dt) {
        if s >= u {
            return Err(Error::Config(format!(
                "invalid {} '{}' with {} '{}': the window is empty (since must be before until)",
                since_flag,
                since.unwrap_or(""),
                until_flag,
                until.unwrap_or(""),
            )));
        }
    }
    Ok((since_dt, until_dt))
}

fn check_plausible(dt: DateTime<Utc>, raw: &str, flag: &str) -> Result<()> {
    if dt.year() < MIN_YEAR {
        return Err(Error::Config(format!(
            "invalid {flag} '{raw}': year {} is before {MIN_YEAR} (typo?)",
            dt.year()
        )));
    }
    if dt > Utc::now() + chrono::Duration::seconds(FUTURE_SKEW_SECS) {
        return Err(Error::Config(format!(
            "invalid {flag} '{raw}': date is in the future"
        )));
    }
    Ok(())
}

/// Keep results within `[since, until)`. Undated results never match a set
/// bound — a filter must not pretend unknown dates are recent — and are
/// reported back as the second return value for a verbose note.
pub fn filter_by_time(
    results: Vec<SearchResult>,
    since: Option<DateTime<Utc>>,
    until: Option<DateTime<Utc>>,
) -> (Vec<SearchResult>, usize) {
    if since.is_none() && until.is_none() {
        return (results, 0);
    }
    let mut dropped = 0usize;
    let kept = results
        .into_iter()
        .filter(|r| {
            let Some(date) = r.published.as_deref().and_then(published_instant) else {
                dropped += 1;
                return false;
            };
            if since.is_some_and(|s| date < s) || until.is_some_and(|u| date >= u) {
                return false;
            }
            true
        })
        .collect();
    (kept, dropped)
}

/// Parse a stored `published` value back to an instant.
pub fn published_instant(s: &str) -> Option<DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(s.trim())
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Newest-first ordering key for `combine sort: date`; undated results sink
/// last. Returns `None` only when both are undated (a tie: keep order).
pub fn compare_date_desc(a: &SearchResult, b: &SearchResult) -> Option<std::cmp::Ordering> {
    match (
        a.published.as_deref().and_then(published_instant),
        b.published.as_deref().and_then(published_instant),
    ) {
        (Some(x), Some(y)) => Some(y.cmp(&x)),
        (Some(_), None) => Some(std::cmp::Ordering::Less),
        (None, Some(_)) => Some(std::cmp::Ordering::Greater),
        (None, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_formats_normalize_to_rfc3339() {
        assert_eq!(
            unix_to_rfc3339(1_789_890_908).as_deref(),
            Some("2026-09-20T07:55:08+00:00")
        );
        assert_eq!(unix_to_rfc3339(0), None);
        assert_eq!(unix_to_rfc3339(-5), None);
        assert_eq!(
            x_created_at("Sun Sep 20 07:57:08 +0000 2026").as_deref(),
            Some("2026-09-20T07:57:08+00:00")
        );
        assert_eq!(x_created_at("yesterday"), None);
        assert_eq!(
            rfc2822("Sun, 20 Sep 2026 07:57:08 +0000").as_deref(),
            Some("2026-09-20T07:57:08+00:00")
        );
        assert_eq!(rfc2822("2026-09-20"), None);
        // ISO passthrough normalizes offsets; bare dates mean midnight UTC.
        assert_eq!(
            normalize_iso("2026-09-20T10:00:00+02:00").as_deref(),
            Some("2026-09-20T08:00:00+00:00")
        );
        assert_eq!(
            normalize_iso("2026-09-20").as_deref(),
            Some("2026-09-20T00:00:00+00:00")
        );
        // Naive timestamps (PyPI) read as UTC.
        assert_eq!(
            normalize_iso("2024-01-15T10:30:00.123456").as_deref(),
            Some("2024-01-15T10:30:00.123456+00:00")
        );
        assert_eq!(normalize_iso(""), None);
        assert_eq!(normalize_iso("Sep 20"), None);
    }

    #[test]
    fn partial_academic_dates_resolve_sensibly() {
        assert_eq!(
            pubmed_date("2021 May 5").as_deref(),
            Some("2021-05-05T00:00:00+00:00")
        );
        assert_eq!(
            pubmed_date("2021 May").as_deref(),
            Some("2021-05-01T00:00:00+00:00")
        );
        assert_eq!(
            pubmed_date("2021").as_deref(),
            Some("2021-01-01T00:00:00+00:00")
        );
        assert_eq!(
            pubmed_date("Spring 2021").as_deref(),
            Some("2021-03-01T00:00:00+00:00")
        );
        assert_eq!(pubmed_date("n.d."), None);
        assert_eq!(
            date_parts(&[2020, 5]).as_deref(),
            Some("2020-05-01T00:00:00+00:00")
        );
        assert_eq!(
            date_parts(&[2020]).as_deref(),
            Some("2020-01-01T00:00:00+00:00")
        );
        assert_eq!(date_parts(&[]), None);
        assert_eq!(date_parts(&[2020, 13]), None);
    }

    #[test]
    fn bounds_accept_durations_and_dates() {
        let day_ago = parse_bound("24h", "--since").unwrap();
        let diff = (Utc::now() - day_ago).num_seconds();
        assert!((86_300..=86_500).contains(&diff), "got {diff}");
        assert!(parse_bound("30m", "--since").is_ok());
        assert!(parse_bound("7d", "--since").is_ok());
        assert!(parse_bound("4w", "--since").is_ok());
        assert!(parse_bound("0h", "--since").is_err());
        assert!(parse_bound("24x", "--since").is_err());
        let day = parse_bound("2026-09-20", "--since").unwrap();
        assert_eq!(day.to_rfc3339(), "2026-09-20T00:00:00+00:00");
        // Yesterday, not today: a same-day afternoon timestamp trips the
        // future-date guard when the suite runs in the morning.
        let ts = parse_bound("2026-09-19T15:04:05+02:00", "--until").unwrap();
        assert_eq!(ts.to_rfc3339(), "2026-09-19T13:04:05+00:00");
        assert!(parse_bound("1899-01-01", "--since").is_err());
        assert!(parse_bound("2999-01-01", "--since").is_err());
        assert!(parse_bound("yesterday", "--since").is_err());
    }

    fn dated(published: Option<&str>) -> SearchResult {
        SearchResult {
            title: "t".into(),
            url: "https://x/".into(),
            snippet: String::new(),
            published: published.map(str::to_string),
        }
    }

    #[test]
    fn time_filter_keeps_window_drops_undated() {
        let results = vec![
            dated(Some("2026-09-20T10:00:00+00:00")),
            dated(Some("2026-09-19T10:00:00+00:00")),
            dated(None),
        ];
        let since = parse_bound("2026-09-20", "--since").unwrap();
        let (kept, dropped) = filter_by_time(results, Some(since), None);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 1);
        // No bounds: untouched, nothing reported.
        let results = vec![dated(None)];
        let (kept, dropped) = filter_by_time(results, None, None);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 0);
    }

    #[test]
    fn unicode_bounds_are_errors_never_panics() {
        // Byte-splitting "24é" at len-1 lands inside `é` and panics; the
        // duration parser must split on a char boundary instead.
        for bad in ["24é", "é", "２４h", "h", ""] {
            assert!(parse_bound(bad, "--since").is_err(), "{bad:?}");
        }
    }

    #[test]
    fn windows_reject_inversions_and_equality() {
        let (since, until) =
            parse_window(Some("2026-09-19"), Some("2026-09-20"), "--since", "--until").unwrap();
        assert!(since.is_some() && until.is_some());
        let err = parse_window(Some("2026-09-20"), Some("2026-09-19"), "--since", "--until")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("window is empty"),
            "names both flags and the fault, got: {err}"
        );
        assert!(err.contains("--since") && err.contains("--until"));
        // `[x, x)` matches nothing either.
        assert!(
            parse_window(Some("2026-09-20"), Some("2026-09-20"), "--since", "--until").is_err()
        );
        // One-sided and empty windows are fine.
        assert!(parse_window(Some("24h"), None, "--since", "--until").is_ok());
        assert!(parse_window(None, Some("2026-09-19"), "--since", "--until").is_ok());
        assert!(parse_window(None, None, "--since", "--until").is_ok());
    }

    #[test]
    fn window_edges_are_inclusive_start_exclusive_end() {
        let (since, until) =
            parse_window(Some("2026-09-19"), Some("2026-09-20"), "--since", "--until").unwrap();
        let results = vec![
            dated(Some("2026-09-19T00:00:00+00:00")), // == since: kept
            dated(Some("2026-09-20T00:00:00+00:00")), // == until: dropped
            dated(Some("2026-09-19T12:00:00+00:00")), // inside: kept
        ];
        let (kept, dropped) = filter_by_time(results, since, until);
        assert_eq!(kept.len(), 2);
        assert_eq!(dropped, 0);
        // Until-only: everything before the end passes, undated drops.
        let (_, until) = parse_window(None, Some("2026-09-20"), "--since", "--until").unwrap();
        let results = vec![dated(Some("2020-01-01T00:00:00+00:00")), dated(None)];
        let (kept, dropped) = filter_by_time(results, None, until);
        assert_eq!(kept.len(), 1);
        assert_eq!(dropped, 1);
    }

    #[test]
    fn feed_dates_accept_atom_iso() {
        assert_eq!(
            feed_date("2026-09-20T10:00:00+00:00").as_deref(),
            Some("2026-09-20T10:00:00+00:00")
        );
    }

    #[test]
    fn date_order_is_newest_first_undated_last() {
        let mut results = [
            dated(None),
            dated(Some("2026-09-19T00:00:00+00:00")),
            dated(Some("2026-09-20T00:00:00+00:00")),
        ];
        results.sort_by(|a, b| compare_date_desc(a, b).unwrap_or(std::cmp::Ordering::Equal));
        let dates: Vec<_> = results.iter().map(|r| r.published.clone()).collect();
        assert_eq!(
            dates,
            [
                Some("2026-09-20T00:00:00+00:00".to_string()),
                Some("2026-09-19T00:00:00+00:00".to_string()),
                None
            ]
        );
    }
}
