//! RSS 2.0 / Atom parsing with a real XML parser.
//!
//! Bing and Reddit both hand us feeds, and both used to be parsed by scanning
//! for `"<item>"` / `"<entry>"` substrings and the first `href="` in the block.
//! That breaks on everything real feeds actually do: namespace prefixes
//! (`<atom:entry>`), attributes on the container, `<![CDATA[...]]>`, mixed
//! case, `data-href=` before the real `href`, and `rel="self"` links that are
//! not the entry's target.
//!
//! Element names are matched on their **local name**, so any namespace prefix
//! works, and malformed XML is an explicit [`Error::Parse`] instead of a silent
//! "zero results".

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};

use crate::error::{Error, Result};

/// One feed item, normalized across RSS and Atom.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FeedEntry {
    pub title: String,
    pub link: String,
    /// `description` (RSS) or `summary`/`content` (Atom), still raw markup.
    pub summary: String,
    /// `author/name` (Atom) or `dc:creator`/`author` (RSS).
    pub author: String,
    /// `category@label` (Atom) or `category` text (RSS).
    pub category: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Field {
    Title,
    Link,
    Summary,
    Content,
    Author,
    Category,
}

/// Parse a feed body into entries.
///
/// An empty feed yields `Ok(vec![])`; a body that is not a feed at all, or is
/// malformed XML, is an error — the caller must be able to tell "no matches"
/// from "the endpoint stopped serving a feed".
pub fn parse_entries(body: &str) -> Result<Vec<FeedEntry>> {
    let mut reader = Reader::from_str(body);
    // Do NOT trim text events: with quick-xml >= 0.41 entity references arrive
    // as separate events, so trimming would eat the spaces around them
    // ("Rust &amp; things" -> "Rust&things").
    reader.config_mut().expand_empty_elements = false;

    let mut out = Vec::new();
    let mut saw_feed_root = false;
    let mut depth: usize = 0;
    let mut item_depth: Option<usize> = None;
    let mut current = FeedEntry::default();
    let mut field: Option<(Field, usize)> = None;
    let mut buf = String::new();
    let mut parents: Vec<String> = Vec::new();

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = local_name(&e);
                depth += 1;
                if matches!(name.as_str(), "rss" | "feed" | "channel" | "rdf") {
                    saw_feed_root = true;
                }
                if item_depth.is_none() {
                    if matches!(name.as_str(), "item" | "entry") {
                        saw_feed_root = true;
                        item_depth = Some(depth);
                        current = FeedEntry::default();
                    }
                } else if field.is_none() {
                    if let Some(f) = field_for(&name, &parents) {
                        if f == Field::Link {
                            // Atom `<link href=...>`; RSS puts the URL in text.
                            if let Some(href) = alternate_href(&e) {
                                if current.link.is_empty() {
                                    current.link = href;
                                }
                            } else {
                                field = Some((Field::Link, depth));
                                buf.clear();
                            }
                        } else if f == Field::Category {
                            if let Some(label) = attr(&e, "label") {
                                if current.category.is_empty() {
                                    current.category = label;
                                }
                            } else {
                                field = Some((Field::Category, depth));
                                buf.clear();
                            }
                        } else {
                            field = Some((f, depth));
                            buf.clear();
                        }
                    }
                }
                parents.push(name);
            }
            Ok(Event::Empty(e)) => {
                let name = local_name(&e);
                if item_depth.is_some() {
                    match name.as_str() {
                        "link" => {
                            if let Some(href) = alternate_href(&e) {
                                if current.link.is_empty() {
                                    current.link = href;
                                }
                            }
                        }
                        "category" => {
                            if let Some(label) = attr(&e, "label") {
                                if current.category.is_empty() {
                                    current.category = label;
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if field.is_some() {
                    buf.push_str(&t.xml10_content().map_err(decode_err)?);
                }
            }
            Ok(Event::CData(c)) => {
                if field.is_some() {
                    buf.push_str(&c.decode().map_err(decode_err)?);
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if field.is_some() {
                    match r.resolve_char_ref() {
                        Ok(Some(ch)) => buf.push(ch),
                        // Named entity: resolve the handful XML defines.
                        _ => {
                            let name = r.decode().map_err(decode_err)?;
                            if let Some(ch) = named_entity(&name) {
                                buf.push(ch);
                            }
                        }
                    }
                }
            }
            Ok(Event::End(_)) => {
                if let Some((f, d)) = field {
                    if d == depth {
                        commit(&mut current, f, std::mem::take(&mut buf));
                        field = None;
                    }
                }
                if item_depth == Some(depth) {
                    out.push(std::mem::take(&mut current));
                    item_depth = None;
                }
                parents.pop();
                depth = depth.saturating_sub(1);
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => {
                return Err(Error::Parse(format!("malformed feed XML: {e}")));
            }
        }
    }

    if !saw_feed_root {
        return Err(Error::Parse(
            "response is not an RSS/Atom feed (no <rss>/<feed>/<item>/<entry> element)".into(),
        ));
    }
    Ok(out)
}

fn decode_err(e: impl std::fmt::Display) -> Error {
    Error::Parse(format!("feed text is not decodable: {e}"))
}

fn commit(entry: &mut FeedEntry, field: Field, value: String) {
    let slot = match field {
        Field::Title => &mut entry.title,
        Field::Link => &mut entry.link,
        Field::Summary | Field::Content => &mut entry.summary,
        Field::Author => &mut entry.author,
        Field::Category => &mut entry.category,
    };
    // First non-empty value wins; `summary` set earlier is not clobbered by a
    // later `content` (and vice versa).
    if slot.trim().is_empty() {
        *slot = value;
    }
}

fn field_for(name: &str, parents: &[String]) -> Option<Field> {
    match name {
        "title" => Some(Field::Title),
        "link" => Some(Field::Link),
        "description" | "summary" => Some(Field::Summary),
        "content" | "encoded" => Some(Field::Content),
        "category" => Some(Field::Category),
        // RSS's `<dc:creator>Name</dc:creator>` is a leaf, unambiguous.
        "creator" => Some(Field::Author),
        // Deliberately *not* `"author" => Some(Field::Author)`: Atom's
        // `<author>` is a container with a `<name>` child *and* other
        // siblings like `<uri>`/`<email>`. Treating the container itself as
        // the field start would buffer every descendant's text — including
        // `<uri>`'s — into one blob (`"/u/alicehttps://.../u/alice"` instead
        // of just `"/u/alice"`). Only the `<name>` child is mapped, and only
        // when its parent is `<author>`, so unmapped siblings are ignored.
        "name" if parents.last().map(|p| p == "author").unwrap_or(false) => Some(Field::Author),
        _ => None,
    }
}

/// Local (namespace-stripped), lower-cased element name.
fn local_name(e: &BytesStart<'_>) -> String {
    String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase()
}

fn attr(e: &BytesStart<'_>, want: &str) -> Option<String> {
    for a in e.attributes() {
        let a = a.ok()?;
        let key = String::from_utf8_lossy(a.key.local_name().as_ref()).to_ascii_lowercase();
        if key == want {
            let v = a.normalized_value(XmlVersion::Implicit1_0).ok()?;
            let v = v.trim();
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// `href` of an Atom `<link>`, but only for the entry's own target: `rel`
/// must be absent or `alternate` (never `self`, `next`, `replies`, ...).
fn alternate_href(e: &BytesStart<'_>) -> Option<String> {
    let href = attr(e, "href")?;
    match attr(e, "rel") {
        None => Some(href),
        Some(rel) if rel.eq_ignore_ascii_case("alternate") => Some(href),
        Some(_) => None,
    }
}

fn named_entity(name: &str) -> Option<char> {
    match name {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        "nbsp" => Some(' '),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_items_and_ignores_channel_metadata() {
        let body = r#"<?xml version="1.0" encoding="utf-8"?>
<rss version="2.0"><channel>
  <title>Bing: rust</title>
  <link>http://www.bing.com/search?q=rust</link>
  <item>
    <title>First &amp; result</title>
    <link>https://example.com/1</link>
    <description>Snippet &lt;b&gt;one&lt;/b&gt; here.</description>
  </item>
  <item>
    <title>Second</title>
    <link>https://example.com/2</link>
  </item>
</channel></rss>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].title, "First & result");
        assert_eq!(entries[0].link, "https://example.com/1");
        assert_eq!(entries[0].summary, "Snippet <b>one</b> here.");
        assert_eq!(entries[1].title, "Second");
    }

    #[test]
    fn entity_spacing_is_preserved() {
        let body = "<rss><channel><item><title>Rust &amp; things</title>\
                    <link>https://x/1</link></item></channel></rss>";
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries[0].title, "Rust & things");
    }

    #[test]
    fn handles_cdata_and_attributes_on_containers() {
        let body = r#"<rss><channel>
          <item id="7"><title><![CDATA[Cash & <carry>]]></title>
          <link><![CDATA[https://example.com/cd]]></link></item>
        </channel></rss>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries[0].title, "Cash & <carry>");
        assert_eq!(entries[0].link, "https://example.com/cd");
    }

    #[test]
    fn handles_namespace_prefixes_and_mixed_case() {
        let body = r#"<atom:feed xmlns:atom="http://www.w3.org/2005/Atom">
          <atom:entry>
            <atom:TITLE>Prefixed</atom:TITLE>
            <atom:link href="https://example.com/ns"/>
          </atom:entry>
        </atom:feed>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Prefixed");
        assert_eq!(entries[0].link, "https://example.com/ns");
    }

    #[test]
    fn atom_link_rel_self_is_not_the_entry_url() {
        let body = r#"<feed><entry>
          <link rel="self" href="https://api.example/self"/>
          <link rel="replies" href="https://api.example/replies"/>
          <link href="https://example.com/real"/>
          <title>T</title>
        </entry></feed>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries[0].link, "https://example.com/real");
    }

    #[test]
    fn data_href_does_not_hijack_the_link() {
        let body = r#"<feed><entry>
          <content type="html">&lt;div data-href="https://evil.example"&gt;x&lt;/div&gt;</content>
          <link href="https://example.com/good"/>
          <title>T</title>
        </entry></feed>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries[0].link, "https://example.com/good");
        assert!(entries[0].summary.contains("data-href"));
    }

    #[test]
    fn reads_atom_author_and_category_label() {
        let body = r#"<feed><entry>
          <author><name>/u/alice</name><uri>https://reddit/u/alice</uri></author>
          <category term="rust" label="r/rust"/>
          <title>Post</title>
          <link href="https://example.com/p"/>
        </entry></feed>"#;
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries[0].author, "/u/alice");
        assert_eq!(entries[0].category, "r/rust");
    }

    #[test]
    fn empty_feed_is_ok_but_html_is_an_error() {
        assert!(parse_entries("<rss><channel></channel></rss>")
            .unwrap()
            .is_empty());
        let err = parse_entries("<html><body><p>hi</p></body></html>").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
        let err = parse_entries("").unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn malformed_xml_is_an_error_not_partial_success() {
        // Unclosed </item> mismatch.
        let err = parse_entries("<rss><channel><item><title>a</title></itemx></channel></rss>")
            .unwrap_err();
        assert_eq!(err.code(), "parse_failed");
    }

    #[test]
    fn feed_level_title_is_not_an_entry() {
        let body = "<feed><title>whole feed</title><entry><title>real</title>\
                    <link href=\"https://x/1\"/></entry></feed>";
        let entries = parse_entries(body).unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "real");
    }
}
