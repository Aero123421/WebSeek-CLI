//! Small text utilities shared across modules.
//!
//! Kept dependency-free and pure so every function is unit-testable and the
//! behavior is identical on all platforms (this is where Windows/macOS/Linux
//! filename differences are neutralized).

/// Collapse whitespace to single spaces and cap at ~300 chars.
pub fn normalize_snippet(s: &str) -> String {
    let one_line = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 300 {
        let cut = one_line.chars().take(297).collect::<String>();
        format!("{cut}...")
    } else {
        one_line
    }
}

/// Turn an arbitrary string into a safe, unique-ish filename stem.
///
/// - Non-alphanumeric characters become `-`
/// - Runs of `-` collapse
/// - Edge dashes are trimmed
/// - Empty results become `"image"`
pub fn sanitize_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let mut collapsed = String::new();
    let mut prev_dash = false;
    for c in cleaned.chars() {
        if c == '-' {
            if !prev_dash {
                collapsed.push('-');
            }
            prev_dash = true;
        } else {
            collapsed.push(c);
            prev_dash = false;
        }
    }
    let trimmed = collapsed.trim_matches('-').to_string();
    if trimmed.is_empty() {
        "image".into()
    } else {
        trimmed
    }
}

/// Truncate to at most `max` characters (char-boundary safe for UTF-8).
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Remove HTML/XML tags, leaving their text content.
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

/// Decode common HTML/XML entities: `&amp; &lt; &gt; &quot; &apos; &#39; &#x41;`.
pub fn unescape_entities(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '&' {
            out.push(c);
            continue;
        }
        let mut ent = String::new();
        let mut terminated = false;
        for nc in chars.by_ref() {
            if nc == ';' {
                terminated = true;
                break;
            }
            ent.push(nc);
            if ent.len() > 10 {
                break;
            }
        }
        if !terminated {
            out.push('&');
            out.push_str(&ent);
            continue;
        }
        match ent.as_str() {
            "amp" => out.push('&'),
            "lt" => out.push('<'),
            "gt" => out.push('>'),
            "quot" => out.push('"'),
            "apos" => out.push('\''),
            "nbsp" => out.push(' '),
            _ => match decode_numeric_entity(&ent) {
                Some(ch) => out.push(ch),
                None => {
                    out.push('&');
                    out.push_str(&ent);
                    out.push(';');
                }
            },
        }
    }
    out
}

fn decode_numeric_entity(ent: &str) -> Option<char> {
    let n = if let Some(hex) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
        u32::from_str_radix(hex, 16).ok()?
    } else {
        let dec = ent.strip_prefix('#')?;
        dec.parse::<u32>().ok()?
    };
    char::from_u32(n)
}

/// Clean an HTML fragment into plain single-line text: unescape entities,
/// strip tags, collapse whitespace. Used for snippets returned by JSON APIs
/// (Wikipedia, Stack Exchange, ...) that embed light markup.
pub fn strip_html(s: &str) -> String {
    let unescaped = unescape_entities(s);
    let no_tags = strip_tags(&unescaped);
    no_tags.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Inner text of the first `<tag ...>...</tag>` in `segment`. Tolerates
/// attributes on the opening tag (e.g. `<content type="html">`). Only matches
/// real tags: escaped markup in feed content (`&lt;tag&gt;`) is ignored
/// because its `<` is not a literal `<`.
pub fn extract_tag(segment: &str, tag: &str) -> String {
    let open = format!("<{tag}");
    let Some(start) = segment.find(&open) else {
        return String::new();
    };
    let after_open = &segment[start + open.len()..];
    // Skip any attributes to the end of the opening tag.
    let Some(gt) = after_open.find('>') else {
        return String::new();
    };
    let inner = &after_open[gt + 1..];
    let close = format!("</{tag}>");
    let Some(end) = inner.find(&close) else {
        return String::new();
    };
    inner[..end].to_string()
}

/// Value of the first `attr="..."` in `segment` (e.g. `href`, `label`).
pub fn extract_attr(segment: &str, attr: &str) -> String {
    let needle = format!("{attr}=\"");
    let Some(start) = segment.find(&needle) else {
        return String::new();
    };
    let rest = &segment[start + needle.len()..];
    let Some(end) = rest.find('"') else {
        return String::new();
    };
    rest[..end].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_is_single_line_and_capped() {
        let long = "word ".repeat(200);
        let s = normalize_snippet(&long);
        assert!(s.ends_with("..."));
        assert!(s.chars().count() <= 301);
        let short = normalize_snippet("  a\n b  c ");
        assert_eq!(short, "a b c");
    }

    #[test]
    fn sanitizes_filenames() {
        assert_eq!(sanitize_name("Mountain  Sunset!"), "Mountain-Sunset");
        assert_eq!(sanitize_name(""), "image");
        assert_eq!(sanitize_name("A/B\\C"), "A-B-C");
        assert_eq!(sanitize_name("---x---"), "x");
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("こんにちは", 3), "こんに");
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    #[test]
    fn strip_html_cleans_markup_and_entities() {
        assert_eq!(
            strip_html("the <span class=\"x\">async</span> runtime &amp; more"),
            "the async runtime & more"
        );
        assert_eq!(strip_html("a &#39; b &#x41;"), "a ' b A");
        assert_eq!(strip_html("  multi\n  line "), "multi line");
    }

    #[test]
    fn unescape_handles_plain_text() {
        assert_eq!(unescape_entities("no entities"), "no entities");
        assert_eq!(unescape_entities("&lt;tag&gt;"), "<tag>");
    }

    #[test]
    fn extract_tag_tolerates_attributes_and_ignores_escaped() {
        let seg = r#"<content type="html">&lt;p&gt;hi&lt;/p&gt;</content><title>A &amp; B</title>"#;
        assert_eq!(extract_tag(seg, "content"), "&lt;p&gt;hi&lt;/p&gt;");
        assert_eq!(extract_tag(seg, "title"), "A &amp; B");
        // Escaped <title> inside content is NOT matched as a tag.
        assert_eq!(
            extract_tag("<x>&lt;title&gt;nope&lt;/title&gt;</x>", "title"),
            ""
        );
    }

    #[test]
    fn extract_attr_reads_quoted_value() {
        assert_eq!(
            extract_attr(r#"<link href="https://x/y"/>"#, "href"),
            "https://x/y"
        );
        assert_eq!(
            extract_attr(r#"<category label="r/rust"/>"#, "label"),
            "r/rust"
        );
        assert_eq!(extract_attr("<x/>", "href"), "");
    }
}
