//! Region / locale normalization.
//!
//! `--region` accepts flexible human input (`jp`, `JP`, `jp-jp`, `en-us`,
//! `en_US`) and each engine receives the exact format it expects:
//!
//! - **DuckDuckGo** `kl` parameter: `<country>-<language>` (e.g. `us-en`,
//!   `jp-jp`). Unknown regions are omitted rather than sent malformed.
//! - **Bing** `cc` parameter: a 2-letter country code (e.g. `us`); the
//!   language (if any) feeds `setlang`.
//!
//! Disambiguation between `ll-CC` and `CC-ll` uses a small built-in table of
//! common ISO 3166-1 alpha-2 country codes. Codes outside the table fall back
//! to a positional heuristic (first segment = country), which is documented as
//! a best-effort limitation.

/// Common ISO 3166-1 alpha-2 country codes. Not exhaustive by design; see
/// module docs for the fallback behavior.
const COUNTRIES: &[&str] = &[
    "us", "gb", "uk", "ca", "au", "nz", "ie", "jp", "cn", "hk", "tw", "kr", "in", "sg", "my", "th",
    "vn", "id", "ph", "de", "fr", "es", "it", "pt", "nl", "be", "ch", "at", "se", "no", "fi", "dk",
    "pl", "cz", "gr", "tr", "ru", "ua", "br", "mx", "ar", "cl", "co", "za", "eg", "sa", "ae", "il",
];

fn is_country(seg: &str) -> bool {
    COUNTRIES.contains(&seg)
}

/// A parsed locale: an optional country and an optional language, both
/// lower-cased 2-letter codes.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locale {
    pub country: Option<String>,
    pub language: Option<String>,
}

/// Parse a flexible region string into a [`Locale`].
///
/// Accepts `CC`, `cc-cc`, `ll-CC`, `CC-ll`, and `_` as a separator.
pub fn parse_region(region: &str) -> Locale {
    let cleaned: String = region
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '_' { '-' } else { c })
        .collect();
    let parts: Vec<&str> = cleaned.split('-').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => Locale::default(),
        [only] => {
            let seg = only.to_string();
            if is_country(&seg) {
                Locale {
                    country: Some(seg),
                    language: None,
                }
            } else {
                // A lone non-country token is treated as a language.
                Locale {
                    country: None,
                    language: Some(seg),
                }
            }
        }
        [a, b, ..] => {
            if is_country(a) {
                // `CC-ll` (or `CC-CC` like `jp-jp`).
                let language = if *a == *b || !is_country(b) {
                    Some(b.to_string())
                } else {
                    None
                };
                Locale {
                    country: Some(a.to_string()),
                    language,
                }
            } else if is_country(b) {
                // `ll-CC`.
                Locale {
                    country: Some(b.to_string()),
                    language: Some(a.to_string()),
                }
            } else {
                // Unknown codes: positional fallback (first = country).
                Locale {
                    country: Some(a.to_string()),
                    language: Some(b.to_string()),
                }
            }
        }
    }
}

/// DuckDuckGo `kl` value, e.g. `us-en` or `jp-jp`. `None` when no usable
/// country is present (so the parameter is simply omitted).
pub fn ddg_kl(region: &str) -> Option<String> {
    let loc = parse_region(region);
    match (loc.country, loc.language) {
        (Some(c), Some(l)) => Some(format!("{c}-{l}")),
        (Some(c), None) => Some(format!("{c}-{c}")),
        (None, _) => None,
    }
}

/// Bing `cc` value: the 2-letter country code, if any.
pub fn bing_cc(region: &str) -> Option<String> {
    parse_region(region).country
}

/// Bing `setlang` value derived from the region's language, if any.
pub fn bing_language(region: &str) -> Option<String> {
    parse_region(region).language
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn country_only_duplicates_for_ddg() {
        assert_eq!(ddg_kl("jp"), Some("jp-jp".into()));
        assert_eq!(ddg_kl("US"), Some("us-us".into()));
        assert_eq!(bing_cc("jp"), Some("jp".into()));
    }

    #[test]
    fn language_country_is_reordered_for_ddg() {
        // `en-us` (lang-country) must become DDG's `us-en` (country-lang).
        assert_eq!(ddg_kl("en-us"), Some("us-en".into()));
        assert_eq!(bing_cc("en-us"), Some("us".into()));
        assert_eq!(bing_language("en-us"), Some("en".into()));
    }

    #[test]
    fn country_language_is_kept_for_ddg() {
        assert_eq!(ddg_kl("us-en"), Some("us-en".into()));
        assert_eq!(ddg_kl("jp-jp"), Some("jp-jp".into()));
    }

    #[test]
    fn underscore_and_case_are_normalized() {
        assert_eq!(ddg_kl("EN_US"), Some("us-en".into()));
        assert_eq!(bing_cc("En_Us"), Some("us".into()));
    }

    #[test]
    fn language_only_has_no_country() {
        assert_eq!(ddg_kl("en"), None);
        assert_eq!(bing_cc("en"), None);
        assert_eq!(bing_language("en"), Some("en".into()));
    }

    #[test]
    fn empty_and_blank_yield_nothing() {
        assert_eq!(ddg_kl(""), None);
        assert_eq!(bing_cc("   "), None);
    }

    #[test]
    fn unknown_codes_use_positional_fallback() {
        // Neither segment is a known country -> first treated as country.
        assert_eq!(ddg_kl("xx-yy"), Some("xx-yy".into()));
        assert_eq!(bing_cc("xx-yy"), Some("xx".into()));
    }
}
