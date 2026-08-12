//! Nominatim (OpenStreetMap) geocoding search (no key, stable JSON).
//!
//! Returns places matching a query.
//!
//! Nominatim's usage policy requires a User-Agent that **identifies the
//! application**, and explicitly blocks clients impersonating a browser — the
//! opposite of what webseek previously did here. This engine therefore sends
//! the honest `webseek/<version>` agent (plus `contact_email`, when set) via
//! `SearchOpts::identify`. Keep request volume low; the global delay helps.

use reqwest::blocking::Client;
use serde::Deserialize;
use url::Url;

use crate::engines::SearchEngine;
use crate::error::{Error, Result};
use crate::models::{SearchOpts, SearchResult};
use crate::text::{join_meta, normalize_snippet};

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

    fn search(&self, client: &Client, query: &str, opts: &SearchOpts) -> Result<Vec<SearchResult>> {
        let limit = opts.count.clamp(1, 50).to_string();
        let params: Vec<(&str, &str)> = vec![
            ("q", query),
            ("format", "json"),
            ("limit", &limit),
            ("addressdetails", "0"),
        ];
        let url = Url::parse_with_params(&self.base, &params)
            .map_err(|e| Error::Config(format!("bad URL construction: {e}")))?;
        let resp = opts
            .send_api(client.get(url))
            .map_err(|e| Error::Network(format!("nominatim request failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Http(resp.status().as_u16()));
        }
        let body = resp.text().map_err(Error::from)?;
        Ok(parse_results(&body))
    }
}

/// Pure parser (unit-tested against fixtures).
pub fn parse_results(body: &str) -> Vec<SearchResult> {
    let Ok(places) = serde_json::from_str::<Vec<Place>>(body) else {
        return Vec::new();
    };
    places
        .into_iter()
        .map(|p| {
            let title = if p.display_name.is_empty() {
                p.place_type.clone()
            } else {
                p.display_name
            };
            let coords = if p.lat.is_empty() && p.lon.is_empty() {
                String::new()
            } else {
                format!("{},{}", p.lat, p.lon)
            };
            SearchResult {
                title: normalize_snippet(&title),
                url: format!("https://www.openstreetmap.org/{}/{}", p.osm_type, p.osm_id),
                snippet: normalize_snippet(&join_meta(&[&p.place_type, &coords])),
            }
        })
        .collect()
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
        let r = parse_results(FIXTURE);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].title, "Tokyo, Japan");
        assert_eq!(r[0].url, "https://www.openstreetmap.org/relation/1543125");
        assert_eq!(r[0].snippet, "administrative · 35.676,139.763");
        assert_eq!(r[1].url, "https://www.openstreetmap.org/node/99");
    }

    #[test]
    fn bad_json_yields_empty() {
        assert!(parse_results("not json").is_empty());
    }
}
