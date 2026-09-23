//! Query-focused passage selection for `fetch --query`.
//!
//! A fetched page is often 20 000 characters of which an agent needs three
//! paragraphs. This module splits extracted text into passages, scores each
//! against the query with Okapi BM25, and keeps the best few **in document
//! order**. No model, no network, no key: plain lexical ranking, so the same
//! page and query always select the same passages.
//!
//! Tokenization is dictionary-free and works for both kinds of script:
//!
//! - Space-delimited scripts (Latin, Cyrillic, ...) become lowercase word
//!   tokens; a short list of English function words is dropped.
//! - CJK runs (Han, kana, Hangul) have no word spaces, so they become
//!   overlapping character bigrams — the standard approach for search without
//!   a morphological dictionary. "非同期処理" matches "非同期" and "処理".
//! - Full-width ASCII is folded first, so "ＲＵＳＴ" matches "rust".

use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Serialize};

/// Passages aim for about this many characters before a new one starts.
pub const TARGET_CHARS: usize = 600;
/// A single line longer than this is split at sentence ends (or hard-cut).
pub const MAX_CHARS: usize = 1_200;
/// Default and ceiling for `--passages`.
pub const DEFAULT_PASSAGES: usize = 5;
pub const MAX_PASSAGES: usize = 50;

/// BM25 parameters (the usual defaults).
const K1: f64 = 1.2;
const B: f64 = 0.75;

/// Words that carry no topic on their own. Kept deliberately short: IDF
/// already discounts common terms, this only stops "how to" queries from
/// rewarding every passage that contains "to".
const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "by", "for", "from", "how", "in", "is", "it", "of",
    "on", "or", "that", "the", "this", "to", "was", "what", "when", "where", "which", "who", "why",
    "with",
];

/// Where a `fetch --query` result came from: the query, how many passages
/// the page had, and which ones were kept (document order) with their score.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Focus {
    pub query: String,
    /// Passages the page was split into.
    pub total: usize,
    /// Kept passages, in document order.
    pub passages: Vec<PassageHit>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PassageHit {
    /// 0-based position among the page's passages.
    pub index: usize,
    /// BM25 score, rounded to 3 decimals. Only positive scores are kept.
    pub score: f64,
}

/// Result of [`select`]: the joined text plus its provenance.
#[derive(Debug, Clone, PartialEq)]
pub struct Selection {
    /// Kept passages joined by a blank line, in document order.
    pub text: String,
    pub focus: Focus,
}

impl Selection {
    /// Were any passages left out? (Feeds `truncated`.)
    pub fn dropped_any(&self) -> bool {
        self.focus.passages.len() < self.focus.total
    }
}

/// Keep the `k` passages of `text` that best match `query`.
///
/// Passages with no query term at all are never kept, so a page that does not
/// mention the topic yields empty text rather than an unrelated lead
/// paragraph dressed up as an answer.
pub fn select(text: &str, query: &str, k: usize) -> Selection {
    let passages = split_passages(text);
    let terms = query_terms(query);
    let scores = bm25(&passages, &terms);

    let mut ranked: Vec<usize> = (0..passages.len()).filter(|&i| scores[i] > 0.0).collect();
    // Highest score first; earlier passage wins a tie so output is stable.
    ranked.sort_by(|&a, &b| scores[b].total_cmp(&scores[a]).then(a.cmp(&b)));
    ranked.truncate(k.max(1));
    ranked.sort_unstable();

    let text = ranked
        .iter()
        .map(|&i| passages[i].as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let hits = ranked
        .iter()
        .map(|&i| PassageHit {
            index: i,
            score: (scores[i] * 1000.0).round() / 1000.0,
        })
        .collect();
    Selection {
        text,
        focus: Focus {
            query: query.to_string(),
            total: passages.len(),
            passages: hits,
        },
    }
}

/// Distinct searchable terms of a query. Empty means the query cannot match
/// anything (only punctuation or stopwords), which callers reject up front.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    tokenize(query)
        .into_iter()
        .filter(|t| seen.insert(t.clone()))
        .collect()
}

/// Split extracted text (one block per line) into passages of roughly
/// [`TARGET_CHARS`]. Markdown headings start a new passage so a section's
/// heading stays with its body.
pub fn split_passages(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut cur_chars = 0usize;

    let flush = |cur: &mut String, cur_chars: &mut usize, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(std::mem::take(cur));
        }
        *cur_chars = 0;
    };

    // True while `cur` holds only headings: a heading must never be flushed
    // alone, away from the body it introduces.
    let mut heading_only = false;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let heading = line.starts_with('#');
        if heading && !heading_only {
            flush(&mut cur, &mut cur_chars, &mut out);
        }
        for piece in split_long(line) {
            let n = piece.chars().count();
            if cur_chars > 0 && cur_chars + n > TARGET_CHARS && !heading_only {
                flush(&mut cur, &mut cur_chars, &mut out);
            }
            heading_only = heading && (heading_only || cur.is_empty());
            if !cur.is_empty() {
                cur.push('\n');
                cur_chars += 1;
            }
            cur.push_str(&piece);
            cur_chars += n;
        }
    }
    flush(&mut cur, &mut cur_chars, &mut out);
    out
}

/// Break an over-long line at sentence ends once a piece reaches
/// [`TARGET_CHARS`], hard-cutting at [`MAX_CHARS`] when no sentence ends.
fn split_long(line: &str) -> Vec<String> {
    if line.chars().count() <= MAX_CHARS {
        return vec![line.to_string()];
    }
    let mut out = Vec::new();
    let mut buf = String::new();
    let mut n = 0usize;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        buf.push(c);
        n += 1;
        let sentence_end = matches!(c, '。' | '！' | '？' | '．')
            || (matches!(c, '.' | '!' | '?') && chars.peek().is_none_or(|n| n.is_whitespace()));
        if (sentence_end && n >= TARGET_CHARS) || n >= MAX_CHARS {
            out.push(buf.trim().to_string());
            buf.clear();
            n = 0;
        }
    }
    if !buf.trim().is_empty() {
        out.push(buf.trim().to_string());
    }
    out
}

/// Tokens of `text`: lowercase words for spaced scripts, bigrams for CJK.
pub fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut cjk: Vec<char> = Vec::new();

    fn flush_word(word: &mut String, out: &mut Vec<String>) {
        if !word.is_empty() && !STOPWORDS.contains(&word.as_str()) {
            out.push(word.clone());
        }
        word.clear();
    }
    fn flush_cjk(run: &mut Vec<char>, out: &mut Vec<String>) {
        match run.len() {
            0 => {}
            1 => out.push(run[0].to_string()),
            _ => out.extend(run.windows(2).map(|w| w.iter().collect::<String>())),
        }
        run.clear();
    }

    for c in text.chars().map(fold_width) {
        if is_cjk(c) {
            flush_word(&mut word, &mut out);
            cjk.push(c);
        } else if c.is_alphanumeric() {
            flush_cjk(&mut cjk, &mut out);
            word.extend(c.to_lowercase());
        } else {
            flush_word(&mut word, &mut out);
            flush_cjk(&mut cjk, &mut out);
        }
    }
    flush_word(&mut word, &mut out);
    flush_cjk(&mut cjk, &mut out);
    out
}

/// Fold full-width ASCII (U+FF01..U+FF5E) and the ideographic space to their
/// half-width forms, so "ＲＵＳＴ　１" tokenizes like "RUST 1".
fn fold_width(c: char) -> char {
    match c as u32 {
        0xFF01..=0xFF5E => char::from_u32(c as u32 - 0xFEE0).unwrap_or(c),
        0x3000 => ' ',
        _ => c,
    }
}

/// Scripts written without spaces between words.
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x3005..=0x3007          // 々 〆 〇
        | 0x3040..=0x30FF        // Hiragana, Katakana (incl. ー)
        | 0x31F0..=0x31FF        // Katakana phonetic extensions
        | 0x3400..=0x4DBF        // CJK Extension A
        | 0x4E00..=0x9FFF        // CJK Unified Ideographs
        | 0xF900..=0xFAFF        // CJK Compatibility Ideographs
        | 0xFF66..=0xFF9F        // Half-width Katakana
        | 0x1100..=0x11FF        // Hangul Jamo
        | 0x3130..=0x318F        // Hangul Compatibility Jamo
        | 0xAC00..=0xD7AF        // Hangul Syllables
        | 0x20000..=0x2FFFF      // CJK Extensions B..
    )
}

/// Okapi BM25 score of every passage against `terms`.
fn bm25(passages: &[String], terms: &[String]) -> Vec<f64> {
    if passages.is_empty() || terms.is_empty() {
        return vec![0.0; passages.len()];
    }
    let docs: Vec<(HashMap<String, usize>, usize)> = passages
        .iter()
        .map(|p| {
            let tokens = tokenize(p);
            let len = tokens.len();
            let mut tf = HashMap::new();
            for t in tokens {
                *tf.entry(t).or_insert(0) += 1;
            }
            (tf, len)
        })
        .collect();
    let n = docs.len() as f64;
    let avgdl = (docs.iter().map(|(_, len)| *len).sum::<usize>() as f64 / n).max(1.0);

    let idf: Vec<f64> = terms
        .iter()
        .map(|t| {
            let df = docs.iter().filter(|(tf, _)| tf.contains_key(t)).count() as f64;
            (1.0 + (n - df + 0.5) / (df + 0.5)).ln()
        })
        .collect();

    docs.iter()
        .map(|(tf, len)| {
            let norm = K1 * (1.0 - B + B * (*len as f64) / avgdl);
            terms
                .iter()
                .zip(&idf)
                .map(|(t, idf)| {
                    let f = *tf.get(t).unwrap_or(&0) as f64;
                    idf * f * (K1 + 1.0) / (f + norm)
                })
                .sum()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latin_words_are_lowercased_and_stopwords_dropped() {
        assert_eq!(
            tokenize("How to use the Tokio runtime?"),
            vec!["use", "tokio", "runtime"]
        );
    }

    #[test]
    fn cjk_runs_become_bigrams_and_mixed_scripts_split() {
        assert_eq!(tokenize("非同期処理"), vec!["非同", "同期", "期処", "処理"]);
        assert_eq!(
            tokenize("Rustの非同期"),
            vec!["rust", "の非", "非同", "同期"]
        );
        // A lone CJK character still counts.
        assert_eq!(tokenize("猫 cat"), vec!["猫", "cat"]);
        assert_eq!(tokenize("ラーメン"), vec!["ラー", "ーメ", "メン"]);
    }

    #[test]
    fn full_width_ascii_folds_to_half_width() {
        assert_eq!(tokenize("ＲＵＳＴ　１．８６"), vec!["rust", "1", "86"]);
    }

    #[test]
    fn query_terms_are_distinct_and_can_be_empty() {
        assert_eq!(query_terms("rust Rust RUST"), vec!["rust"]);
        assert!(query_terms("the of ?!").is_empty());
    }

    #[test]
    fn headings_start_a_passage_and_long_blocks_split() {
        let text = "# Intro\nshort\n# Usage\nbody";
        assert_eq!(
            split_passages(text),
            vec!["# Intro\nshort", "# Usage\nbody"]
        );

        let long = "Sentence number one is here. ".repeat(100);
        let parts = split_passages(&long);
        assert!(parts.len() > 1);
        assert!(parts.iter().all(|p| p.chars().count() <= MAX_CHARS));
        // Nothing is lost when splitting.
        let words: usize = parts.iter().map(|p| p.split_whitespace().count()).sum();
        assert_eq!(words, long.split_whitespace().count());
    }

    #[test]
    fn a_heading_is_never_split_from_a_long_body() {
        let body = "word ".repeat(200); // ~1000 chars, over TARGET_CHARS
        let text = format!("# A\n## A.1\n{body}\n# B\nshort");
        let parts = split_passages(&text);
        assert_eq!(parts.len(), 2, "{parts:?}");
        assert!(parts[0].starts_with("# A\n## A.1\nword"));
        assert_eq!(parts[1], "# B\nshort");
    }

    #[test]
    fn a_line_without_sentence_ends_is_hard_cut() {
        let line = "あ".repeat(MAX_CHARS * 2 + 10);
        let parts = split_long(&line);
        assert_eq!(parts.len(), 3);
        assert_eq!(parts[0].chars().count(), MAX_CHARS);
    }

    fn page() -> String {
        let filler = "Lorem ipsum dolor sit amet consectetur adipiscing elit. ".repeat(12);
        format!(
            "# Cooking\n{filler}\n# Tokio runtime\nThe tokio runtime schedules async tasks on worker threads.\n\
             # Gardening\n{filler}\n# 非同期処理\nRustの非同期処理はランタイムが必要です。\n# Footer\n{filler}"
        )
    }

    #[test]
    fn best_passages_are_kept_in_document_order() {
        let sel = select(&page(), "tokio async runtime", 3);
        assert_eq!(sel.focus.total, 5);
        assert_eq!(
            sel.focus.passages.len(),
            1,
            "only matching passages are kept"
        );
        assert_eq!(sel.focus.passages[0].index, 1);
        assert!(sel.text.starts_with("# Tokio runtime"));

        // Two matches from different sections come back in page order, not
        // score order.
        let sel = select(&page(), "tokio 非同期", 5);
        let idx: Vec<usize> = sel.focus.passages.iter().map(|p| p.index).collect();
        assert_eq!(idx, vec![1, 3]);
        assert!(sel.text.find("Tokio").unwrap() < sel.text.find("非同期").unwrap());
    }

    #[test]
    fn japanese_queries_match_japanese_passages() {
        let sel = select(&page(), "非同期 ランタイム", 5);
        assert_eq!(sel.focus.passages.len(), 1);
        assert_eq!(sel.focus.passages[0].index, 3);
        assert!(sel.text.contains("ランタイムが必要"));
        assert!(sel.dropped_any());
    }

    #[test]
    fn k_limits_and_highest_score_wins() {
        let sel = select(&page(), "tokio 非同期", 1);
        assert_eq!(sel.focus.passages.len(), 1);
        assert!(sel.focus.passages[0].score > 0.0);
    }

    #[test]
    fn nothing_matching_yields_empty_text() {
        let sel = select(&page(), "kubernetes", 5);
        assert!(sel.text.is_empty());
        assert!(sel.focus.passages.is_empty());
        assert_eq!(sel.focus.total, 5);
        assert!(sel.dropped_any());
    }

    #[test]
    fn a_page_where_everything_matches_drops_nothing() {
        let sel = select("rust one\n\n# b\nrust two", "rust", 5);
        assert_eq!(sel.focus.total, 2);
        assert!(!sel.dropped_any());
        assert_eq!(sel.text, "rust one\n\n# b\nrust two");
    }
}
