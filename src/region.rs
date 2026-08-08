//! BCP-47-shaped locale parsing for `--region`.
//!
//! `--region` accepts a language tag in standard BCP-47 subtag *order* —
//! `language[-script][-region]`, e.g. `en-US`, `zh-Hant-TW`, `sr-Latn-RS` —
//! plus a lone code, which is treated as a region/country (`jp`, `US`).
//!
//! This replaces a heuristic that tried to *guess* whether a two-segment
//! input meant `language-region` or `region-language` by checking a small
//! built-in country table. That guess was wrong for exactly the inputs a
//! French/German/Portuguese/Norwegian/... user would type: `fr`, `de`, `pt`,
//! `no` etc. are valid ISO-3166 country codes *and* valid ISO-639 language
//! codes, so `fr-CA` ("French, Canada") was decoded as "France, no
//! language" — the opposite of BCP-47's actual, unambiguous rule that the
//! first subtag is always the language. Once multiple subtags are present
//! there is nothing left to guess: BCP-47 defines the order.
//!
//! This is a *structural* parser (subtag shape only), not a validating one —
//! it does not check `language` against ISO-639 or `region` against
//! ISO-3166/UN M49. An engine that gets a well-formed but nonsensical code
//! simply omits the parameter rather than sending something malformed; it
//! never fabricates a value. `--region` and `--lang` remain independent CLI
//! flags — this module does not read or set `--lang`.

/// A small set of ISO-3166-1 alpha-2 codes, used only to disambiguate a
/// **lone** subtag (no dash): `--region jp` should mean "Japan", not treat
/// "jp" as a two-letter language code. Not exhaustive by design — an
/// unrecognized lone code still parses as a region when it has region shape
/// (see [`parse_region`]).
const KNOWN_REGIONS: &[&str] = &[
    "us", "gb", "uk", "ca", "au", "nz", "ie", "jp", "cn", "hk", "tw", "kr", "in", "sg", "my", "th",
    "vn", "id", "ph", "de", "fr", "es", "it", "pt", "nl", "be", "ch", "at", "se", "no", "fi", "dk",
    "pl", "cz", "gr", "tr", "ru", "ua", "br", "mx", "ar", "cl", "co", "za", "eg", "sa", "ae", "il",
];

/// A parsed locale: language, script and region subtags, present only when
/// the input contained a well-formed subtag of that shape.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Locale {
    /// 2-3 letter language subtag, lower-cased (e.g. "en", "zh").
    pub language: Option<String>,
    /// 4-letter script subtag, title-cased (e.g. "Hant", "Latn").
    pub script: Option<String>,
    /// 2-letter or 3-digit region subtag, upper-cased (e.g. "US", "419").
    pub region: Option<String>,
}

fn normalize(region: &str) -> String {
    region
        .trim()
        .to_ascii_lowercase()
        .chars()
        .map(|c| if c == '_' { '-' } else { c })
        .collect()
}

fn is_language_subtag(s: &str) -> bool {
    let n = s.chars().count();
    (2..=3).contains(&n) && s.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_script_subtag(s: &str) -> bool {
    s.chars().count() == 4 && s.chars().all(|c| c.is_ascii_alphabetic())
}

fn is_region_subtag(s: &str) -> bool {
    let n = s.chars().count();
    (n == 2 && s.chars().all(|c| c.is_ascii_alphabetic()))
        || (n == 3 && s.chars().all(|c| c.is_ascii_digit()))
}

fn is_known_region(s: &str) -> bool {
    KNOWN_REGIONS.contains(&s)
}

fn title_case(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().chain(chars).collect(),
        None => String::new(),
    }
}

/// Parse a flexible region string into a [`Locale`].
///
/// - A lone subtag is a region if it is a known country code or has region
///   shape (2 alpha / 3 digit); otherwise, if it has language shape, it is
///   treated as a language (e.g. `--region en` with no country in mind).
/// - Two or more subtags always follow BCP-47 order: `language[-script][-region]`.
///   The first subtag is the language whenever it has language shape (which
///   covers effectively all real input); trailing subtags are matched by
///   shape (4-alpha = script, 2-alpha/3-digit = region) and extra subtags
///   (variants/extensions) are ignored.
pub fn parse_region(region: &str) -> Locale {
    let cleaned = normalize(region);
    let parts: Vec<&str> = cleaned.split('-').filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        [] => Locale::default(),
        [only] => {
            if is_known_region(only) || (is_region_subtag(only) && !is_language_subtag(only)) {
                Locale {
                    region: Some(only.to_ascii_uppercase()),
                    ..Default::default()
                }
            } else if is_language_subtag(only) {
                Locale {
                    language: Some(only.to_string()),
                    ..Default::default()
                }
            } else {
                Locale::default()
            }
        }
        [lang, rest @ ..] if is_language_subtag(lang) => {
            let mut loc = Locale {
                language: Some(lang.to_string()),
                ..Default::default()
            };
            let mut idx = 0;
            if let Some(s) = rest.first() {
                if is_script_subtag(s) {
                    loc.script = Some(title_case(s));
                    idx = 1;
                }
            }
            if let Some(r) = rest.get(idx) {
                if is_region_subtag(r) {
                    loc.region = Some(r.to_ascii_uppercase());
                }
            }
            loc
        }
        // First subtag isn't language-shaped (digits, wrong length, ...):
        // still surface a region if any subtag has region shape, so a stray
        // trailing country code is not silently lost.
        parts => {
            let mut loc = Locale::default();
            for p in parts {
                if loc.region.is_none() && is_region_subtag(p) {
                    loc.region = Some(p.to_ascii_uppercase());
                }
            }
            loc
        }
    }
}

/// DuckDuckGo `kl` value: `<country>-<language>` (e.g. `us-en`, `jp-jp`).
/// `None` when no usable region is present, so the parameter is omitted
/// rather than sent malformed.
pub fn ddg_kl(region: &str) -> Option<String> {
    let loc = parse_region(region);
    let r = loc.region?.to_ascii_lowercase();
    match loc.language {
        Some(l) => Some(format!("{r}-{l}")),
        None => Some(format!("{r}-{r}")),
    }
}

/// Bing `cc` value: the 2-letter (or 3-digit) region code, if any.
pub fn bing_cc(region: &str) -> Option<String> {
    parse_region(region).region.map(|r| r.to_ascii_lowercase())
}

/// Bing `setlang` value derived from the region's language subtag, if any.
pub fn bing_language(region: &str) -> Option<String> {
    parse_region(region).language
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loc(language: Option<&str>, script: Option<&str>, region: Option<&str>) -> Locale {
        Locale {
            language: language.map(String::from),
            script: script.map(String::from),
            region: region.map(String::from),
        }
    }

    #[test]
    fn language_region_pairs_from_the_review_all_parse_correctly() {
        // Each of these previously broke because `fr`/`de`/`pt` are also
        // valid country codes, so the old heuristic picked the wrong order.
        assert_eq!(parse_region("fr-CA"), loc(Some("fr"), None, Some("CA")));
        assert_eq!(parse_region("de-CH"), loc(Some("de"), None, Some("CH")));
        assert_eq!(parse_region("pt-BR"), loc(Some("pt"), None, Some("BR")));
    }

    #[test]
    fn script_subtags_are_recognized_and_do_not_consume_the_region() {
        assert_eq!(
            parse_region("zh-Hant-TW"),
            loc(Some("zh"), Some("Hant"), Some("TW"))
        );
        assert_eq!(
            parse_region("sr-Latn-RS"),
            loc(Some("sr"), Some("Latn"), Some("RS"))
        );
        // Case-insensitive on input, normalized on output.
        assert_eq!(
            parse_region("SR-latn-rs"),
            loc(Some("sr"), Some("Latn"), Some("RS"))
        );
    }

    #[test]
    fn country_only_duplicates_for_ddg() {
        assert_eq!(ddg_kl("jp"), Some("jp-jp".into()));
        assert_eq!(ddg_kl("US"), Some("us-us".into()));
        assert_eq!(bing_cc("jp"), Some("jp".into()));
    }

    #[test]
    fn language_country_is_reordered_for_ddg() {
        // `en-us` (lang-country, the BCP-47 order) becomes DDG's `us-en`.
        assert_eq!(ddg_kl("en-us"), Some("us-en".into()));
        assert_eq!(bing_cc("en-us"), Some("us".into()));
        assert_eq!(bing_language("en-us"), Some("en".into()));
    }

    #[test]
    fn two_subtag_input_is_always_read_as_language_then_region() {
        // There is no more direction-guessing: the first subtag is always
        // the language. `us-en` therefore means "language `us`, region EN",
        // not "country us, language en" as the old heuristic assumed.
        assert_eq!(ddg_kl("us-en"), Some("en-us".into()));
        assert_eq!(ddg_kl("jp-jp"), Some("jp-jp".into())); // symmetric either way
        assert_eq!(ddg_kl("xx-yy"), Some("yy-xx".into()));
        assert_eq!(bing_cc("xx-yy"), Some("yy".into()));
    }

    #[test]
    fn underscore_and_case_are_normalized() {
        assert_eq!(ddg_kl("EN_US"), Some("us-en".into()));
        assert_eq!(bing_cc("En_Us"), Some("us".into()));
    }

    #[test]
    fn language_only_has_no_region() {
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
    fn three_digit_region_is_recognized() {
        // UN M49 region code (e.g. 419 = Latin America), not a country.
        assert_eq!(parse_region("es-419"), loc(Some("es"), None, Some("419")));
    }

    #[test]
    fn extra_trailing_subtags_are_ignored_not_fatal() {
        // Variant/extension subtags after region are simply dropped.
        assert_eq!(
            parse_region("de-DE-1996"),
            loc(Some("de"), None, Some("DE"))
        );
    }

    #[test]
    fn lang_and_region_flags_stay_independent() {
        // This module never reads or infers `--lang`; only `--region` text
        // is parsed here. Engines combine an explicit `--lang` with this
        // module's output themselves (see bing.rs: `lang.or(region_lang)`).
        assert_eq!(bing_language("jp"), None); // lone region, no language
        assert_eq!(parse_region("jp").language, None);
    }
}
