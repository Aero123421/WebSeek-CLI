//! BCP-47-shaped locale parsing for `--region`.
//!
//! `--region` accepts a language tag in standard BCP-47 subtag order —
//! `language[-script][-region]`, such as `en-US` or `zh-Hant-TW` — plus a
//! lone country code such as `jp` or `US`.
//!
//! Multiple subtags are never direction-guessed: the first is the language,
//! followed by an optional script and country. This matters for inputs such as
//! `fr-CA` and `de-CH`, where both segments can look like country codes.

/// Common ISO 3166-1 alpha-2 country codes, used only to distinguish a lone
/// country (`jp`) from a lone language (`en`). Multi-part tags are parsed by
/// BCP-47 shape instead.
const COUNTRIES: &[&str] = &[
    "us", "gb", "uk", "ca", "au", "nz", "ie", "jp", "cn", "hk", "tw", "kr", "in", "sg", "my", "th",
    "vn", "id", "ph", "de", "fr", "es", "it", "pt", "nl", "be", "ch", "at", "se", "no", "fi", "dk",
    "pl", "cz", "gr", "tr", "ru", "ua", "br", "mx", "ar", "cl", "co", "za", "eg", "sa", "ae", "il",
];

fn is_country(seg: &str) -> bool {
    COUNTRIES.contains(&seg)
}

/// A parsed locale. `country` is retained for API compatibility even though
/// BCP-47 calls the same component a region subtag.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locale {
    pub country: Option<String>,
    pub language: Option<String>,
    pub script: Option<String>,
}

fn normalize(region: &str) -> String {
    region
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '_' { '-' } else { c })
        .collect()
}

fn is_language_subtag(value: &str) -> bool {
    (2..=3).contains(&value.len()) && value.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_script_subtag(value: &str) -> bool {
    value.len() == 4 && value.bytes().all(|b| b.is_ascii_alphabetic())
}

fn is_country_subtag(value: &str) -> bool {
    (value.len() == 2 && value.bytes().all(|b| b.is_ascii_alphabetic()))
        || (value.len() == 3 && value.bytes().all(|b| b.is_ascii_digit()))
}

fn title_case(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_ascii_uppercase().to_string() + chars.as_str(),
        None => String::new(),
    }
}

/// Parse a flexible region string into a [`Locale`].
///
/// Accepts a lone country or language, and BCP-47-shaped
/// `language[-script][-country]` input. `_` is accepted as a separator.
pub fn parse_region(region: &str) -> Locale {
    let cleaned = normalize(region);
    let parts: Vec<&str> = cleaned.split('-').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => Locale::default(),
        [only] => {
            if is_country(only) {
                Locale {
                    country: Some((*only).to_string()),
                    ..Default::default()
                }
            } else if is_language_subtag(only) {
                Locale {
                    country: None,
                    language: Some((*only).to_string()),
                    script: None,
                }
            } else {
                Locale::default()
            }
        }
        [language, rest @ ..] if is_language_subtag(language) => {
            let mut locale = Locale {
                language: Some((*language).to_string()),
                ..Default::default()
            };
            let mut index = 0;
            if let Some(script) = rest.first().filter(|s| is_script_subtag(s)) {
                locale.script = Some(title_case(script));
                index = 1;
            }
            if let Some(country) = rest.get(index).filter(|s| is_country_subtag(s)) {
                locale.country = Some((*country).to_string());
            }
            locale
        }
        rest => {
            // Malformed leading subtags do not become engine parameters, but
            // retain a later structurally valid country when one exists.
            Locale {
                country: rest
                    .iter()
                    .copied()
                    .find(|part| is_country_subtag(part))
                    .map(str::to_string),
                language: None,
                script: None,
            }
        }
    }
}

/// DuckDuckGo's locale parameter uses several legacy language codes rather
/// than blindly repeating the country code. Unknown country-only input is
/// omitted instead of fabricating a likely-invalid value.
fn ddg_default_language(country: &str) -> Option<&'static str> {
    match country {
        "us" | "gb" | "uk" | "ca" | "au" | "nz" | "ie" | "in" | "sg" | "za" | "ph" => Some("en"),
        "jp" => Some("jp"),
        "cn" => Some("zh"),
        "hk" | "tw" => Some("tzh"),
        "kr" => Some("kr"),
        "br" => Some("pt"),
        "mx" | "ar" | "cl" | "co" => Some("es"),
        "de" | "at" | "ch" => Some("de"),
        "fr" | "be" => Some("fr"),
        "es" => Some("es"),
        "it" => Some("it"),
        "pt" => Some("pt"),
        "nl" => Some("nl"),
        "se" => Some("sv"),
        "no" => Some("no"),
        "fi" => Some("fi"),
        "dk" => Some("da"),
        "pl" => Some("pl"),
        "cz" => Some("cs"),
        "gr" => Some("el"),
        "tr" => Some("tr"),
        "ru" => Some("ru"),
        "ua" => Some("uk"),
        "th" => Some("th"),
        "vn" => Some("vi"),
        "id" => Some("id"),
        "my" => Some("ms"),
        "il" => Some("he"),
        "sa" | "ae" | "eg" => Some("ar"),
        _ => None,
    }
}

fn ddg_country(country: &str) -> &str {
    if country == "gb" {
        "uk"
    } else {
        country
    }
}

fn ddg_language(language: &str, script: Option<&str>, country: &str) -> String {
    match language {
        // DuckDuckGo's `kl` values predate BCP-47 and retain these labels.
        "ja" => "jp".to_string(),
        "ko" => "kr".to_string(),
        "zh" if matches!(country, "hk" | "tw") || script == Some("Hant") => "tzh".to_string(),
        _ => language.to_string(),
    }
}

/// DuckDuckGo `kl` value, e.g. `us-en` or `jp-jp`. `None` when no usable
/// country is present (so the parameter is simply omitted).
pub fn ddg_kl(region: &str) -> Option<String> {
    let loc = parse_region(region);
    let script = loc.script.as_deref();
    match (loc.country, loc.language) {
        (Some(country), Some(language)) => {
            let language = ddg_language(&language, script, &country);
            Some(format!("{}-{language}", ddg_country(&country)))
        }
        (Some(country), None) => ddg_default_language(&country)
            .map(|language| format!("{}-{language}", ddg_country(&country))),
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

    fn locale(language: Option<&str>, script: Option<&str>, country: Option<&str>) -> Locale {
        Locale {
            country: country.map(str::to_string),
            language: language.map(str::to_string),
            script: script.map(str::to_string),
        }
    }

    #[test]
    fn country_only_uses_duckduckgo_locale_codes() {
        assert_eq!(ddg_kl("jp"), Some("jp-jp".into()));
        assert_eq!(ddg_kl("US"), Some("us-en".into()));
        assert_eq!(ddg_kl("br"), Some("br-pt".into()));
        assert_eq!(ddg_kl("GB"), Some("uk-en".into()));
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
    fn multiple_subtags_follow_bcp47_order() {
        assert_eq!(ddg_kl("fr-CA"), Some("ca-fr".into()));
        assert_eq!(ddg_kl("de-CH"), Some("ch-de".into()));
        assert_eq!(ddg_kl("pt-BR"), Some("br-pt".into()));
        assert_eq!(ddg_kl("us-en"), Some("en-us".into()));
        assert_eq!(ddg_kl("jp-jp"), Some("jp-jp".into()));
        assert_eq!(ddg_kl("ja-JP"), Some("jp-jp".into()));
        assert_eq!(ddg_kl("ko-KR"), Some("kr-kr".into()));
        assert_eq!(ddg_kl("zh-Hant-TW"), Some("tw-tzh".into()));
    }

    #[test]
    fn script_subtag_does_not_consume_country() {
        assert_eq!(
            parse_region("zh-Hant-TW"),
            locale(Some("zh"), Some("Hant"), Some("tw"))
        );
        assert_eq!(
            parse_region("sr-Latn-RS"),
            locale(Some("sr"), Some("Latn"), Some("rs"))
        );
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
        assert_eq!(parse_region("-"), Locale::default());
    }

    #[test]
    fn malformed_or_unknown_country_only_input_is_not_fabricated() {
        assert_eq!(ddg_kl("not-a-locale"), None);
        assert_eq!(ddg_kl("xyz"), None);
    }

    #[test]
    fn numeric_region_is_supported() {
        assert_eq!(
            parse_region("es-419"),
            locale(Some("es"), None, Some("419"))
        );
    }
}
