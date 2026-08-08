//! Small text utilities shared across modules.
//!
//! Pure and dependency-light so every function is unit-testable and the
//! behavior is identical on all platforms (this is where Windows/macOS/Linux
//! filename differences are neutralized).
//!
//! **Everything that reaches a terminal or an agent's context goes through
//! [`sanitize_line`] or [`sanitize_text`] first.** Titles, snippets, URLs and
//! page bodies are attacker-controlled strings: a raw `ESC` or `OSC` sequence
//! can repaint the terminal, rewrite its title, forge a hyperlink or (on some
//! terminals) touch the clipboard. HTML entity decoding can *produce* those
//! bytes (`&#27;`), so sanitizing has to happen after decoding, not before.

use std::borrow::Cow;

/// Longest filename stem we will produce, in bytes. Keeps
/// `NNNN_<stem>.<ext>` inside the 255-byte limit every mainstream filesystem
/// enforces, so a pathological title cannot fail the whole download.
const MAX_STEM_BYTES: usize = 120;

/// True for characters that must never be forwarded verbatim.
fn is_dangerous(c: char) -> bool {
    match c {
        '\n' | '\t' => false,
        // C0 controls (including ESC 0x1B and CR) and DEL.
        c if (c as u32) < 0x20 || c as u32 == 0x7f => true,
        // C1 controls — 8-bit equivalents of ESC-prefixed sequences.
        c if ('\u{80}'..='\u{9f}').contains(&c) => true,
        // Bidi overrides / isolates: they let a URL render back-to-front.
        '\u{200e}' | '\u{200f}' | '\u{2028}' | '\u{2029}' => true,
        c if ('\u{202a}'..='\u{202e}').contains(&c) => true,
        c if ('\u{2066}'..='\u{2069}').contains(&c) => true,
        _ => false,
    }
}

/// Strip control characters, keeping newlines and tabs. For multi-line text.
pub fn sanitize_text(s: &str) -> Cow<'_, str> {
    if !s.chars().any(is_dangerous) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.chars().filter(|c| !is_dangerous(*c)).collect())
}

/// Strip control characters *and* newlines/tabs. For anything rendered on one
/// line: titles, URLs, snippets, engine names.
pub fn sanitize_line(s: &str) -> Cow<'_, str> {
    let bad = |c: char| is_dangerous(c) || c == '\n' || c == '\t';
    if !s.chars().any(bad) {
        return Cow::Borrowed(s);
    }
    Cow::Owned(s.chars().filter(|c| !bad(*c)).collect())
}

/// Sanitize, collapse whitespace to single spaces, and cap at ~300 chars.
pub fn normalize_snippet(s: &str) -> String {
    let clean = sanitize_text(s);
    let one_line = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if one_line.chars().count() > 300 {
        let cut = one_line.chars().take(297).collect::<String>();
        format!("{cut}...")
    } else {
        one_line
    }
}

/// Sanitize and single-line a title coming from an untrusted source.
pub fn clean_title(s: &str) -> String {
    let clean = sanitize_text(s);
    let joined = clean.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.chars().count() > 300 {
        joined.chars().take(300).collect()
    } else {
        joined
    }
}

/// Turn an arbitrary string into a safe, bounded filename stem.
///
/// - Non-alphanumeric characters become `-`
/// - Runs of `-` collapse
/// - Edge dashes are trimmed
/// - The result is capped at [`MAX_STEM_BYTES`] on a char boundary
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
    let trimmed = truncate_bytes(collapsed.trim_matches('-'), MAX_STEM_BYTES);
    let trimmed = trimmed.trim_matches('-').to_string();
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

/// Truncate to at most `max` *bytes*, never splitting a character.
pub fn truncate_bytes(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Clean an HTML fragment into plain single-line text.
///
/// Uses the real HTML parser rather than "decode entities, then delete
/// everything between `<` and `>`": that older order turned an escaped
/// comparison (`1 &lt; 2`) into a literal `<` and then swallowed the rest of
/// the string as if it were a tag.
pub fn strip_html(s: &str) -> String {
    if !s.contains('<') && !s.contains('&') {
        return normalize_snippet(s);
    }
    let fragment = scraper::Html::parse_fragment(s);
    let mut text = String::new();
    // Not a plain `.text()` collect: script/style content is a text *node*
    // in the parsed DOM (that's just how HTML works), so a blanket text
    // walk includes JS/CSS source verbatim. Skip those subtrees explicitly.
    collect_text_skipping(*fragment.root_element(), &mut text);
    normalize_snippet(&text)
}

fn collect_text_skipping(node: ego_tree::NodeRef<'_, scraper::node::Node>, out: &mut String) {
    if let scraper::node::Node::Element(e) = node.value() {
        if matches!(e.name(), "script" | "style") {
            return;
        }
    }
    if let scraper::node::Node::Text(t) = node.value() {
        out.push_str(&t.text);
    }
    for child in node.children() {
        collect_text_skipping(child, out);
    }
}

/// Decode text content that is *not* markup — entity references only.
///
/// Used for feed/API fields that are escaped but contain no tags.
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
            if ent.chars().count() > 10 {
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
                // A numeric entity may decode to ESC/CR/etc.; drop those
                // instead of materializing a control character.
                Some(ch) if !is_dangerous(ch) => out.push(ch),
                Some(_) => {}
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
    fn sanitize_removes_escape_sequences() {
        // CSI clear-screen + OSC window-title + DEL.
        let evil = "safe\u{1b}[2Jtitle\u{1b}]0;pwned\u{7}end\u{7f}";
        let out = sanitize_line(evil);
        assert!(!out.contains('\u{1b}'), "{out:?}");
        assert!(!out.contains('\u{7}'), "{out:?}");
        assert!(!out.contains('\u{7f}'), "{out:?}");
        assert_eq!(out, "safe[2Jtitle]0;pwnedend");
    }

    #[test]
    fn sanitize_removes_c1_and_bidi_overrides() {
        let evil = "a\u{9b}2Kb\u{202e}moc.live\u{2069}";
        let out = sanitize_line(evil);
        assert_eq!(out, "a2Kbmoc.live");
    }

    #[test]
    fn sanitize_text_keeps_newlines_but_not_carriage_returns() {
        let out = sanitize_text("line1\r\nline2\tx\u{1b}y");
        assert_eq!(out, "line1\nline2\txy");
    }

    #[test]
    fn sanitize_borrows_when_clean() {
        assert!(matches!(sanitize_text("plain text"), Cow::Borrowed(_)));
        assert!(matches!(sanitize_line("plain text"), Cow::Borrowed(_)));
    }

    #[test]
    fn numeric_entities_cannot_smuggle_control_characters() {
        // &#27; is ESC, &#x1b; likewise.
        assert_eq!(unescape_entities("a&#27;[2Jb"), "a[2Jb");
        assert_eq!(unescape_entities("a&#x1B;]0;xb"), "a]0;xb");
        assert_eq!(unescape_entities("a&#13;b"), "ab");
        // Ordinary numeric entities still decode.
        assert_eq!(unescape_entities("&#39;&#x41;&lt;"), "'A<");
    }

    #[test]
    fn sanitizes_filenames() {
        assert_eq!(sanitize_name("Mountain  Sunset!"), "Mountain-Sunset");
        assert_eq!(sanitize_name(""), "image");
        assert_eq!(sanitize_name("A/B\\C"), "A-B-C");
        assert_eq!(sanitize_name("---x---"), "x");
    }

    #[test]
    fn filename_stem_is_bounded() {
        let stem = sanitize_name(&"あ".repeat(500));
        assert!(stem.len() <= MAX_STEM_BYTES, "len {}", stem.len());
        assert!(!stem.is_empty());
        let ascii = sanitize_name(&"a".repeat(500));
        assert_eq!(ascii.len(), MAX_STEM_BYTES);
    }

    #[test]
    fn truncates_on_char_boundaries() {
        assert_eq!(truncate_chars("こんにちは", 3), "こんに");
        assert_eq!(truncate_chars("abc", 10), "abc");
        assert_eq!(truncate_bytes("こんにちは", 4), "こ");
        assert_eq!(truncate_bytes("abc", 10), "abc");
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
    fn escaped_comparison_is_not_treated_as_a_tag() {
        // The old decode-then-strip order dropped everything after `<`.
        assert_eq!(strip_html("1 &lt; 2 and 3 &gt; 2"), "1 < 2 and 3 > 2");
        assert_eq!(strip_html("a &lt; b &lt; c"), "a < b < c");
    }

    #[test]
    fn strip_html_drops_script_content() {
        let out = strip_html("hello <script>alert(1)</script> world");
        assert!(!out.contains("alert"), "got: {out}");
        assert!(out.contains("hello"));
    }

    #[test]
    fn unescape_handles_plain_text() {
        assert_eq!(unescape_entities("no entities"), "no entities");
        assert_eq!(unescape_entities("&lt;tag&gt;"), "<tag>");
    }
}
