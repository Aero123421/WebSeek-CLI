//! Nominatim (OpenStreetMap) geocoding search (no key, stable JSON).
//!
//! Returns places matching a query. Nominatim's usage policy requires an
//! identifying User-Agent *and* an actual contact (email or URL) — see
//! [`crate::http::build_client`] for the identifying UA; keep request volume
//! low (the shared rate limiter and global `delay` help with that).

use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::http::Http;
use crate::models::{SearchOpts, SearchResult};

const SEARCH_URL: &str = "https://nominatim.openstreetmap.org/search";

pub struct Nominatim {
    base: String,
}

impl Default for Nominatim {
    fn default() -> Self {
        Self {
            base: SEARCH_URL.to_string(),
        }
    }
}

impl Nominatim {
    pub fn with_base(base: impl Into<String>) -> Self {
        Self { base: base.into() }
    }
}

#[derive(Deserialize)]
struct Place {
    #[serde(default)]
    display_name: String,
    #[serde(default)]
    osm_type: String,
    #[serde(default)]
    osm_id: i64,
    #[serde(default, rename = "type")]
    place_type: String,
    #[serde(default)]
    lat: String,
    #[serde(default)]
    lon: String,
}

impl SearchEngine for Nominatim {
    fn name(&self) -> &'static str {
        "nominatim"
    }

    fn search(&self, http: &Http, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![
            ("q", query),
            ("format", "json"),
            ("limit", &limit),
            ("addressdetails", "0"),
        ];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let body = http.get_text(url, crate::http::MAX_API_BODY_BYTES)?;
        parse_results(&body.text)
    }
}

/// Pure parser (unit-tested against fixtures).
///
/// The error probe checks the parsed `Value`'s shape (object vs. array)
/// rather than deserializing straight into an `{ error: ... }` struct:
/// serde's derive also accepts a *sequence* input for a struct (treating
/// elements positionally), so a normal single-place `[{...}]` response would
/// otherwise deserialize its one array element into that struct's one field
/// and be misread as `{"error": <that place object>}`.
pub fn parse_results(body: &str) -> Result<Vec<SearchResult>> {
    let value: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| Error::Parse(format!("invalid Nominatim API response: {e}")))?;
    if let Some(err) = value.as_object().and_then(|m| m.get("error")) {
        let msg = err
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| err.to_string());
        return Err(Error::Parse(format!("Nominatim API error: {msg}")));
    }
    let places: Vec<Place> = serde_json::from_value(value)
        .map_err(|e| Error::Parse(format!("invalid Nominatim API response: {e}")))?;
    Ok(places
        .into_iter()
        .map(|p| {
            let title = if p.display_name.is_empty() {
                p.place_type.clone()
            } else {
                p.display_name
            };
            SearchResult {
                title,
                url: format!("https://www.openstreetmap.org/{}/{}", p.osm_type, p.osm_id),
                snippet: format!("{} · {},{}", p.place_type, p.lat, p.lon),
            }
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &str = r#"[
      {"place_id":1,"osm_type":"relation","osm_id":1543125,"lat":"35.676","lon":"139.763","type":"administrative","class":"boundary","display_name":"Tokyo, Japan"},
      {"place_id":2,"osm_type":"node","osm_id":99,"lat":"1.0","lon":"2.0","type":"city","display_name":"Some City"}
    ]"#;

    #[test]
    fn parses_places_with_osm_links() {
        let r = parse_results(FIXTURE).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Tokyo, Japan");
        assert_eq!(r[0].url, "https://www.openstreetmap.org/relation/1543125");
        assert_eq!(r[0].snippet, "administrative · 35.676,139.763");
        assert_eq!(r[1].url, "https://www.openstreetmap.org/node/99");
    }

    #[test]
    fn malformed_json_is_an_error() {
        let err = parse_results("not json").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn genuinely_empty_array_is_ok() {
        assert!(parse_results("[]").unwrap().is_empty());
    }

    #[test]
    fn api_error_object_is_surfaced() {
        let err = parse_results(r#"{"error":"Something went wrong"}"#).unwrap_err();
        assert!(err.to_string().contains("Something went wrong"));
    }
}
