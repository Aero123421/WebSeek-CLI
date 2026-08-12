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

/// Longest filename stem we will produce.
///
/// Common filesystems cap a single name at 255 *bytes*; image titles routinely
/// run longer than that, and a rejected `write` used to abort the whole
/// download. Non-ASCII titles cost several bytes per character, so the budget
/// is counted in bytes, not chars.
pub const MAX_NAME_BYTES: usize = 120;

use percent_encoding::{utf8_percent_encode, AsciiSet, CONTROLS};

/// Characters that must never survive into a URL path unescaped: they would
/// silently re-interpret the rest of the URL as a query or fragment.
const PATH_UNSAFE: &AsciiSet = &CONTROLS
    .add(b' ')
    .add(b'"')
    .add(b'#')
    .add(b'<')
    .add(b'>')
    .add(b'?')
    .add(b'`')
    .add(b'{')
    .add(b'}')
    .add(b'\\')
    .add(b'%')
    .add(b'^')
    .add(b'|')
    .add(b'[')
    .add(b']');

/// Same, plus `/` — for values that must stay inside a single path segment.
const SEGMENT_UNSAFE: &AsciiSet = &PATH_UNSAFE.add(b'/');

/// Percent-encode a single URL path **segment**, escaping `/` as well.
///
/// Building a URL by string interpolation lets `#` and `?` in the input become
/// a fragment or a query, and lets `../` walk to an unrelated path. Use this
/// wherever the value must stay confined to one segment (PyPI package names).
pub fn encode_path_segment(s: &str) -> String {
    utf8_percent_encode(s, SEGMENT_UNSAFE).to_string()
}

/// Percent-encode a URL path that is *allowed* to contain `/`.
///
/// MediaWiki titles legitimately embed slashes — the article "Async/await"
/// lives at `/wiki/Async/await` — so escaping them would break the link. `#`
/// and `?` are still escaped, which is the bug that mattered ("C#" resolved to
/// the article "C" with an empty fragment).
pub fn encode_path_keep_slashes(s: &str) -> String {
    utf8_percent_encode(s, PATH_UNSAFE).to_string()
}

/// Turn an arbitrary string into a safe, bounded filename stem.
///
/// - Non-alphanumeric characters become `-`
/// - Runs of `-` collapse
/// - Edge dashes are trimmed
/// - Reserved Windows device names are escaped
/// - The result is capped at [`MAX_NAME_BYTES`] on a char boundary
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
    let trimmed = collapsed.trim_matches('-');
    // Truncate on a char boundary so multi-byte titles stay valid UTF-8.
    let mut end = trimmed.len().min(MAX_NAME_BYTES);
    while end > 0 && !trimmed.is_char_boundary(end) {
        end -= 1;
    }
    let bounded = trimmed[..end].trim_matches('-');

    if bounded.is_empty() || bounded.chars().all(|c| c == '.') {
        return "image".into();
    }
    // CON, NUL, LPT1 … are not usable filenames on Windows, even with an
    // extension. `text` is where cross-platform filename differences live.
    const RESERVED: &[&str] = &[
        "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
        "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
    ];
    let stem = bounded.split('.').next().unwrap_or(bounded);
    if RESERVED.contains(&stem.to_ascii_lowercase().as_str()) {
        // Re-trim: the suffix must not push the name back over the budget.
        let room = MAX_NAME_BYTES.saturating_sub("-file".len());
        let mut end = bounded.len().min(room);
        while end > 0 && !bounded.is_char_boundary(end) {
            end -= 1;
        }
        return format!("{}-file", &bounded[..end]);
    }
    bounded.to_string()
}

/// Truncate to at most `max` characters (char-boundary safe for UTF-8).
pub fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// Join metadata fragments with " · ", dropping empty ones.
///
/// Building these with `format!("{a} · {b}")` left a dangling separator
/// whenever a field was missing ("` · v1.0 · 3 downloads`").
pub fn join_meta(parts: &[&str]) -> String {
    parts
        .iter()
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
}

/// Remove HTML/XML tags, leaving their text content.
///
/// A bare `<` is only treated as a tag opener when what follows could actually
/// start one (`<p`, `</p`, `<!--`, `<?`). Otherwise prose and code containing
/// comparisons — `if x < y then` — lost everything up to the next `>`.
pub fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.char_indices().peekable();
    while let Some((_, c)) = chars.next() {
        if c != '<' {
            out.push(c);
            continue;
        }
        let starts_tag = matches!(
            chars.peek().map(|(_, c)| *c),
            Some(c) if c.is_ascii_alphabetic() || c == '/' || c == '!' || c == '?'
        );
        if !starts_tag {
            out.push('<');
            continue;
        }
        // Consume up to and including the closing '>'.
        let mut closed = false;
        for (_, c) in chars.by_ref() {
            if c == '>' {
                closed = true;
                break;
            }
        }
        if !closed {
            break; // unterminated tag: drop the remainder
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
    let close = format!("</{tag}>");
    let mut from = 0usize;

    while let Some(rel) = segment[from..].find(&open) {
        let start = from + rel;
        let after_open = &segment[start + open.len()..];
        // The tag name must end here — otherwise `<link` also matches
        // `<linkedin>` and we return the wrong element's contents.
        let name_ends = matches!(
            after_open.chars().next(),
            Some(c) if c == '>' || c == '/' || c.is_whitespace()
        );
        if !name_ends {
            from = start + open.len();
            continue;
        }
        let Some(gt) = after_open.find('>') else {
            return String::new();
        };
        // A self-closing `<tag/>` has no inner text.
        if after_open[..gt].ends_with('/') {
            return String::new();
        }
        let inner = &after_open[gt + 1..];
        return match inner.find(&close) {
            Some(end) => inner[..end].to_string(),
            None => String::new(),
        };
    }
    String::new()
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
    fn filenames_stay_within_filesystem_limits() {
        // A long image title used to produce a name the OS refuses, which
        // aborted the entire download run.
        let long = sanitize_name(&"word ".repeat(200));
        assert!(long.len() <= MAX_NAME_BYTES, "got {} bytes", long.len());
        assert!(!long.is_empty());

        let jp = sanitize_name(&"日本語のタイトル".repeat(40));
        assert!(jp.len() <= MAX_NAME_BYTES);
        assert!(
            std::str::from_utf8(jp.as_bytes()).is_ok(),
            "must not cut a multi-byte char in half"
        );
    }

    #[test]
    fn dangerous_and_reserved_names_are_neutralised() {
        // No path traversal can survive: separators become dashes, so the
        // result is always a single, harmless path component.
        for hostile in ["../../etc/passwd", "..\\..\\win", "/abs/path", "a/../b"] {
            let name = sanitize_name(hostile);
            assert!(!name.contains('/') && !name.contains('\\'), "{name}");
            assert_eq!(
                std::path::Path::new(&name).components().count(),
                1,
                "{hostile:?} produced a multi-component path: {name}"
            );
        }
        assert_eq!(sanitize_name(".."), "image");
        assert_eq!(sanitize_name("."), "image");
        // Windows device names are not usable even with an extension.
        assert_eq!(sanitize_name("CON"), "CON-file");
        // The suffix must not push the name back over the byte budget.
        let long_reserved = sanitize_name(&format!("con.{}", "x".repeat(400)));
        assert!(
            long_reserved.len() <= MAX_NAME_BYTES,
            "{}",
            long_reserved.len()
        );
        assert_eq!(sanitize_name("nul"), "nul-file");
        assert_eq!(sanitize_name("console"), "console");
    }

    #[test]
    fn path_segments_are_percent_encoded() {
        // "C#" must not become the article "C" with an empty fragment.
        assert_eq!(encode_path_segment("C#"), "C%23");
        assert_eq!(encode_path_segment("Who's_Next?"), "Who's_Next%3F");
        assert_eq!(encode_path_segment("Rust_(lang)"), "Rust_(lang)");
        assert_eq!(encode_path_segment("東京"), "%E6%9D%B1%E4%BA%AC");
        // Strict form confines the value to one segment.
        assert_eq!(encode_path_segment("a/b"), "a%2Fb");
        assert_eq!(encode_path_segment("../../simple"), "..%2F..%2Fsimple");
    }

    #[test]
    fn slash_preserving_encoding_keeps_legitimate_subpaths() {
        // MediaWiki titles may contain '/': "Async/await" is one article.
        assert_eq!(encode_path_keep_slashes("Async/await"), "Async/await");
        assert_eq!(encode_path_keep_slashes("C#"), "C%23");
        assert_eq!(encode_path_keep_slashes("a?b"), "a%3Fb");
    }

    #[test]
    fn strip_tags_keeps_mathematical_comparisons() {
        assert_eq!(strip_tags("if x < y then z"), "if x < y then z");
        assert_eq!(strip_tags("a <b>bold</b> c"), "a bold c");
        assert_eq!(strip_tags("3 < 4 and 5 > 2"), "3 < 4 and 5 > 2");
        assert_eq!(strip_tags("<p>text</p>"), "text");
    }

    #[test]
    fn extract_tag_requires_a_complete_tag_name() {
        // `<link` must not match `<linkedin>`.
        let seg = "<linkedin>nope</linkedin><link>yes</link>";
        assert_eq!(extract_tag(seg, "link"), "yes");
        assert_eq!(extract_tag("<title >spaced</title>", "title"), "spaced");
        assert_eq!(extract_tag("<link/>", "link"), "");
    }

    #[test]
    fn join_meta_never_leaves_a_dangling_separator() {
        assert_eq!(
            join_meta(&["Async runtime", "v1.0", "9 downloads"]),
            "Async runtime · v1.0 · 9 downloads"
        );
        assert_eq!(join_meta(&["", "v1.0"]), "v1.0");
        assert_eq!(join_meta(&["desc", "", ""]), "desc");
        assert_eq!(join_meta(&["", "  ", ""]), "");
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
