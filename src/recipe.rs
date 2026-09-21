//! YAML recipes: multi-step search/fetch flows in one invocation.
//!
//! A recipe is a small Hurl-style script — steps run top to bottom, results
//! are merged, and one combined list comes out:
//!
//! ```yaml
//! version: 1
//! vars:
//!   theme: "rust async"
//! steps:
//!   - search: {engine: fxtwitter, query: "${theme}", count: 10}
//!   - for_each: {items: ["@telegram", "@rustlang"], do: {search: {engine: telegram, query: "${item}"}}}
//! combine: {dedupe_by: url, limit: 20}
//! output: {format: jsonl}
//! ```
//!
//! Design rules (v1):
//!
//! - Steps are `search` and `fetch` only. A fetch result joins the combined
//!   list as a digest item (`title` + first ~300 chars as the snippet), so the
//!   output contract stays one shape: a [`SearchResult`] list.
//! - `${var}` interpolation works in every string field, with `$$` as the
//!   escape. A placeholder standing alone (`count: "${n}"`) splices the raw
//!   value, so numbers keep working in numeric fields.
//! - `${item}` exists only inside `for_each`. Nested `for_each` is rejected.
//! - Unknown keys are rejected (like `config.toml`), and every step is
//!   validated before the first request is sent.
//! - Runtime failures are per-step warnings: the run continues, and only an
//!   all-steps-failed run exits non-zero.

use std::collections::HashMap;
use std::path::PathBuf;

use serde::Deserialize;

use crate::error::{Error, Result};
use crate::models::{FetchResult, SearchResult};

/// Largest recipe file accepted, stdin included. This bounds the raw input;
/// serde_yaml itself rejects deep nesting (recursion limit), alias bombs
/// (repetition limit) and duplicate keys with graceful errors, all observed
/// against serde_yaml 0.9.34.
pub const MAX_RECIPE_BYTES: usize = 256 * 1024;
/// Largest `for_each` item list and largest unrolled step count. One recipe
/// must not fan out into an unbounded number of upstream requests.
pub const MAX_FOR_EACH_ITEMS: usize = 1024;
pub const MAX_UNROLLED_STEPS: usize = 1024;
/// Largest `combine.limit`. Combined runs legitimately exceed `--count`'s 50.
pub const COMBINE_LIMIT_MAX: usize = 200;
/// The only recipe schema version this webseek understands.
const SUPPORTED_VERSION: u32 = 1;

/// A parsed, validated, fully unrolled recipe.
#[derive(Debug, Clone)]
pub struct Recipe {
    pub steps: Vec<Step>,
    pub combine: Option<Combine>,
    pub output: Option<Output>,
}

/// One executable step. `for_each` is unrolled into these during parsing.
/// `step_no` is the origin recipe step number (1-based), so runtime warnings
/// name the same step the user wrote even after unrolling.
#[derive(Debug, Clone)]
pub enum Step {
    Search {
        step_no: usize,
        engine: String,
        query: String,
        count: Option<usize>,
        lang: Option<String>,
        region: Option<String>,
        safe: Option<bool>,
        since: Option<String>,
        until: Option<String>,
        feed: Option<String>,
    },
    Fetch {
        step_no: usize,
        urls: Vec<String>,
        max_chars: Option<usize>,
        jobs: Option<usize>,
    },
}

/// How step results are merged. Absent: concatenate in step order, untouched.
#[derive(Debug, Clone, Default)]
pub struct Combine {
    /// Deduplicate by URL (the only v1 key; defaults to true when `combine:`
    /// is present at all).
    pub dedupe: bool,
    pub sort: Option<SortKey>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortKey {
    Url,
    Title,
    /// Newest first by `published`; undated results sink last, stable.
    Date,
}

/// Output sink. Absent: recipe-format-or-auto to stdout.
#[derive(Debug, Clone, Default)]
pub struct Output {
    pub format: Option<Format>,
    pub file: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Json,
    Jsonl,
    Pretty,
}

impl Format {
    pub fn mode(self) -> crate::output::Mode {
        match self {
            Format::Json => crate::output::Mode::Json,
            Format::Jsonl => crate::output::Mode::Jsonl,
            Format::Pretty => crate::output::Mode::Pretty,
        }
    }
}

// ---------------------------------------------------------------------------
// Raw schema (what serde sees after substitution)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCombine {
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    dedupe_by: Option<String>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    sort: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOutput {
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    format: Option<String>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    file: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
enum SingleStep {
    #[serde(rename = "search")]
    Search(RawSearch),
    #[serde(rename = "fetch")]
    Fetch(RawFetch),
}

/// String fields accept YAML scalars: a whole-spliced number/bool var
/// stringifies, matching interpolation semantics, so `vars: {n: 10}` feeds
/// both `query: "${n}"` and (via the raw splice) numeric fields.
fn de_coerce_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<String, D::Error> {
    match serde_yaml::Value::deserialize(d)? {
        serde_yaml::Value::String(s) => Ok(s),
        serde_yaml::Value::Number(n) => Ok(n.to_string()),
        serde_yaml::Value::Bool(b) => Ok(b.to_string()),
        other => Err(serde::de::Error::invalid_type(
            serde::de::Unexpected::Other(&format!("{other:?}")),
            &"a string, number or boolean",
        )),
    }
}

fn de_opt_coerce_string<'de, D: serde::Deserializer<'de>>(
    d: D,
) -> std::result::Result<Option<String>, D::Error> {
    match Option::<serde_yaml::Value>::deserialize(d)? {
        None | Some(serde_yaml::Value::Null) => Ok(None),
        Some(serde_yaml::Value::String(s)) => Ok(Some(s)),
        Some(serde_yaml::Value::Number(n)) => Ok(Some(n.to_string())),
        Some(serde_yaml::Value::Bool(b)) => Ok(Some(b.to_string())),
        Some(other) => Err(serde::de::Error::invalid_type(
            serde::de::Unexpected::Other(&format!("{other:?}")),
            &"a string, number or boolean",
        )),
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSearch {
    #[serde(deserialize_with = "de_coerce_string")]
    engine: String,
    #[serde(deserialize_with = "de_coerce_string")]
    query: String,
    #[serde(default)]
    count: Option<usize>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    lang: Option<String>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    region: Option<String>,
    #[serde(default)]
    safe: Option<bool>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    since: Option<String>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    until: Option<String>,
    #[serde(default, deserialize_with = "de_opt_coerce_string")]
    feed: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFetch {
    urls: Vec<String>,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    jobs: Option<usize>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawForEach {
    items: Vec<serde_yaml::Value>,
    #[serde(rename = "do")]
    inner: serde_yaml::Value,
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// Read a recipe from `path`, or from stdin when `path` is `-`.
/// Input over [`MAX_RECIPE_BYTES`] is rejected before YAML parsing.
pub fn read_recipe(path: &str) -> Result<String> {
    let mut buf = Vec::new();
    if path == "-" {
        read_capped(&mut std::io::stdin().lock(), &mut buf, "stdin")?;
    } else {
        let mut file = std::fs::File::open(path)
            .map_err(|e| Error::Config(format!("cannot read recipe '{path}': {e}")))?;
        read_capped(&mut file, &mut buf, path)?;
    }
    String::from_utf8(buf).map_err(|_| Error::Config(format!("recipe '{path}' is not valid UTF-8")))
}

fn read_capped(r: &mut impl std::io::Read, buf: &mut Vec<u8>, what: &str) -> Result<()> {
    use std::io::Read as _;
    r.take(MAX_RECIPE_BYTES as u64 + 1)
        .read_to_end(buf)
        .map_err(|e| Error::Config(format!("cannot read recipe '{what}': {e}")))?;
    if buf.len() > MAX_RECIPE_BYTES {
        return Err(Error::Config(format!(
            "recipe '{what}' exceeds the {MAX_RECIPE_BYTES}-byte limit"
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

/// Parse and validate recipe `text`: schema, versions, variables, `for_each`
/// unrolling, per-step ranges. Every failure names the step or key at fault.
pub fn parse(text: &str) -> Result<Recipe> {
    let doc: serde_yaml::Value =
        serde_yaml::from_str(text).map_err(|e| Error::Config(format!("invalid recipe: {e}")))?;
    let mapping = doc
        .as_mapping()
        .ok_or_else(|| Error::Config("invalid recipe: top level must be a mapping".into()))?;
    for key in mapping.keys() {
        match key.as_str() {
            Some("version" | "vars" | "steps" | "combine" | "output") => {}
            _ => {
                return Err(Error::Config(format!(
                    "invalid recipe: unknown key '{key:?}' (expected version, vars, steps, combine, output)"
                )));
            }
        }
    }

    let version = mapping
        .get(serde_yaml::Value::from("version"))
        .and_then(serde_yaml::Value::as_u64)
        .ok_or_else(|| Error::Config("invalid recipe: 'version' must be the number 1".into()))?;
    if version != u64::from(SUPPORTED_VERSION) {
        return Err(Error::Config(format!(
            "unsupported recipe version {version} (this webseek supports version {SUPPORTED_VERSION})"
        )));
    }

    let vars = extract_vars(mapping)?;
    let raw_steps = mapping
        .get(serde_yaml::Value::from("steps"))
        .and_then(|v| v.as_sequence())
        .ok_or_else(|| Error::Config("invalid recipe: 'steps' must be a non-empty list".into()))?;
    if raw_steps.is_empty() {
        return Err(Error::Config(
            "invalid recipe: 'steps' must be a non-empty list".into(),
        ));
    }

    // Unroll structurally first (no substitution yet), remembering each
    // clone's `${item}` context and origin step; then substitute each clone in
    // one pass so `$$` escapes can never collide with a later rescan.
    let mut unrolled: Vec<(serde_yaml::Value, Option<String>, usize)> = Vec::new();
    for (i, raw) in raw_steps.iter().enumerate() {
        let step_no = i + 1;
        let single = step_kind(raw, step_no)?;
        if single == "for_each" {
            let body = raw
                .as_mapping()
                .and_then(|m| m.values().next())
                .expect("step_kind verified a single-key mapping");
            let each: RawForEach = serde_yaml::from_value(body.clone())
                .map_err(|e| Error::Config(format!("invalid recipe step {step_no}: {e}")))?;
            if each.items.is_empty() {
                return Err(Error::Config(format!(
                    "invalid recipe step {step_no}: for_each items must not be empty"
                )));
            }
            if step_kind(&each.inner, step_no)? == "for_each" {
                return Err(Error::Config(format!(
                    "invalid recipe step {step_no}: nested for_each is not supported"
                )));
            }
            if each.items.len() > MAX_FOR_EACH_ITEMS {
                return Err(Error::Config(format!(
                    "invalid recipe step {step_no}: for_each items exceed {MAX_FOR_EACH_ITEMS} (got {})",
                    each.items.len()
                )));
            }
            for item in &each.items {
                let item = substitute(item, &vars, None).map_err(|e| {
                    // `${item}` is the loop variable being defined here, so the
                    // generic "only inside for_each" wording would confuse.
                    if e.contains("${item}") {
                        Error::Config(format!(
                            "invalid recipe step {step_no}: '${{item}}' is the loop variable and cannot appear in the items list itself"
                        ))
                    } else {
                        Error::Config(format!("invalid recipe step {step_no}: {e}"))
                    }
                })?;
                let item = match &item {
                    serde_yaml::Value::String(s) => s.clone(),
                    serde_yaml::Value::Number(n) => n.to_string(),
                    serde_yaml::Value::Bool(b) => b.to_string(),
                    _ => {
                        return Err(Error::Config(format!(
                            "invalid recipe step {step_no}: for_each items must be strings, numbers or booleans"
                        )));
                    }
                };
                unrolled.push((each.inner.clone(), Some(item), step_no));
            }
        } else {
            unrolled.push((raw.clone(), None, step_no));
        }
    }

    if unrolled.len() > MAX_UNROLLED_STEPS {
        return Err(Error::Config(format!(
            "invalid recipe: {} unrolled steps exceed {MAX_UNROLLED_STEPS} (split the recipe)",
            unrolled.len()
        )));
    }

    let mut steps = Vec::with_capacity(unrolled.len());
    for (raw, item, step_no) in unrolled.iter() {
        let substituted = substitute(raw, &vars, item.as_deref())
            .map_err(|e| Error::Config(format!("invalid recipe step {step_no}: {e}")))?;
        let single = parse_single(&substituted, *step_no)?;
        steps.push(validate_step(single, *step_no)?);
    }

    let combine = parse_combine(mapping, &vars)?;
    let output = parse_output(mapping, &vars)?;
    Ok(Recipe {
        steps,
        combine,
        output,
    })
}

/// Parse one `search`/`fetch` step value. Dispatched by hand because
/// `serde_yaml::from_value` cannot deserialize an externally-tagged enum from
/// a plain mapping (only `!tag`ged values); this also yields a better error
/// for unknown step kinds than serde's variant list.
fn parse_single(value: &serde_yaml::Value, step_no: usize) -> Result<SingleStep> {
    let kind = step_kind(value, step_no)?;
    let inner = value
        .as_mapping()
        .and_then(|m| m.values().next())
        .expect("step_kind verified a single-key mapping");
    match kind.as_str() {
        "search" => {
            let raw: RawSearch = serde_yaml::from_value(inner.clone())
                .map_err(|e| Error::Config(format!("invalid recipe step {step_no}: {e}")))?;
            Ok(SingleStep::Search(raw))
        }
        "fetch" => {
            let raw: RawFetch = serde_yaml::from_value(inner.clone())
                .map_err(|e| Error::Config(format!("invalid recipe step {step_no}: {e}")))?;
            Ok(SingleStep::Fetch(raw))
        }
        other => Err(Error::Config(format!(
            "invalid recipe step {step_no}: unknown step '{other}' (expected 'search', 'fetch' or 'for_each')"
        ))),
    }
}

/// The single mapping key of a step (`search`, `fetch`, `for_each`).
fn step_kind(raw: &serde_yaml::Value, step_no: usize) -> Result<String> {
    let mapping = raw.as_mapping().ok_or_else(|| {
        Error::Config(format!("invalid recipe step {step_no}: must be a mapping"))
    })?;
    if mapping.len() != 1 {
        return Err(Error::Config(format!(
            "invalid recipe step {step_no}: must have exactly one of 'search', 'fetch', 'for_each'"
        )));
    }
    mapping
        .keys()
        .next()
        .and_then(|k| k.as_str())
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Config(format!(
                "invalid recipe step {step_no}: step kind must be a string"
            ))
        })
}

fn extract_vars(mapping: &serde_yaml::Mapping) -> Result<HashMap<String, serde_yaml::Value>> {
    let Some(vars) = mapping.get(serde_yaml::Value::from("vars")) else {
        return Ok(HashMap::new());
    };
    let vars = vars
        .as_mapping()
        .ok_or_else(|| Error::Config("invalid recipe: 'vars' must be a mapping".into()))?;
    let mut out = HashMap::with_capacity(vars.len());
    for (k, v) in vars {
        let name = k
            .as_str()
            .ok_or_else(|| Error::Config("invalid recipe: vars keys must be strings".into()))?;
        if !is_var_name(name) {
            return Err(Error::Config(format!(
                "invalid recipe: bad vars key '{name}' (use letters, digits, _)"
            )));
        }
        if name == "item" {
            return Err(Error::Config(
                "invalid recipe: vars key 'item' is reserved for for_each".into(),
            ));
        }
        if !is_scalar(v) {
            return Err(Error::Config(format!(
                "invalid recipe: var '{name}' must be a string, number or boolean"
            )));
        }
        out.insert(name.to_string(), v.clone());
    }
    // One expansion pass so vars may reference other vars; single-pass keeps
    // self-references terminating (they resolve to themselves, then stop).
    let raw = out.clone();
    for (name, value) in out.iter_mut() {
        *value = substitute(value, &raw, None)
            .map_err(|e| Error::Config(format!("invalid recipe: var '{name}': {e}")))?;
    }
    Ok(out)
}

fn is_var_name(name: &str) -> bool {
    !name.is_empty() && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

fn is_scalar(v: &serde_yaml::Value) -> bool {
    v.is_string() || v.is_number() || v.is_bool()
}

/// Substitute `${var}` placeholders in `value`. `item` is the `for_each`
/// element when inside one (`None` elsewhere, where `${item}` is an error).
/// A placeholder standing alone splices the raw value so numbers survive in
/// numeric fields; `$$` is a literal `$`.
fn substitute(
    value: &serde_yaml::Value,
    vars: &HashMap<String, serde_yaml::Value>,
    item: Option<&str>,
) -> std::result::Result<serde_yaml::Value, String> {
    match value {
        serde_yaml::Value::Mapping(m) => {
            // Values only: rewriting keys would let `"${k}"` smuggle in step
            // kinds and section names the structural checks never saw.
            let mut out = serde_yaml::Mapping::with_capacity(m.len());
            for (k, v) in m {
                out.insert(k.clone(), substitute(v, vars, item)?);
            }
            Ok(serde_yaml::Value::Mapping(out))
        }
        serde_yaml::Value::Sequence(s) => {
            let mut out = Vec::with_capacity(s.len());
            for v in s {
                out.push(substitute(v, vars, item)?);
            }
            Ok(serde_yaml::Value::Sequence(out))
        }
        serde_yaml::Value::String(s) => substitute_string(s, vars, item).map(|r| match r {
            Spliced::Raw(v) => v,
            Spliced::Text(t) => serde_yaml::Value::String(t),
        }),
        other => Ok(other.clone()),
    }
}

enum Spliced {
    Raw(serde_yaml::Value),
    Text(String),
}

fn substitute_string(
    s: &str,
    vars: &HashMap<String, serde_yaml::Value>,
    item: Option<&str>,
) -> std::result::Result<Spliced, String> {
    // Whole-string placeholder: splice the raw value, type intact.
    if let Some(name) = whole_placeholder(s) {
        return Ok(Spliced::Raw(lookup_var(name, vars, item)?));
    }
    let bytes = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if bytes[i] == b'$' && bytes.get(i + 1) == Some(&b'{') {
            let rest = &s[i + 2..];
            let Some(end) = rest.find('}') else {
                return Err("unterminated '${' (missing '}')".into());
            };
            let name = &rest[..end];
            if !is_var_name(name) {
                return Err(format!("invalid variable name '${{{name}}}'"));
            }
            out.push_str(&stringify_var(&lookup_var(name, vars, item)?));
            i += 2 + end + 1;
            continue;
        }
        out.push(s[i..].chars().next().expect("char boundary"));
        i += s[i..].chars().next().expect("char boundary").len_utf8();
    }
    Ok(Spliced::Text(out))
}

/// `Some(name)` when `s` is exactly one `${name}` placeholder.
fn whole_placeholder(s: &str) -> Option<&str> {
    let inner = s.strip_prefix("${")?.strip_suffix('}')?;
    if inner.contains('$') || inner.contains('{') || inner.contains('}') {
        return None;
    }
    if !is_var_name(inner) {
        return None;
    }
    Some(inner)
}

fn lookup_var(
    name: &str,
    vars: &HashMap<String, serde_yaml::Value>,
    item: Option<&str>,
) -> std::result::Result<serde_yaml::Value, String> {
    if name == "item" {
        return item
            .map(|i| serde_yaml::Value::String(i.to_string()))
            .ok_or_else(|| "'${item}' may only be used inside for_each".to_string());
    }
    vars.get(name).cloned().ok_or_else(|| {
        if vars.is_empty() {
            format!("unknown variable '${{{name}}}' (no vars are defined)")
        } else {
            let mut known: Vec<&str> = vars.keys().map(String::as_str).collect();
            known.sort_unstable();
            format!(
                "unknown variable '${{{name}}}' (defined: {})",
                known.join(", ")
            )
        }
    })
}

fn stringify_var(v: &serde_yaml::Value) -> String {
    match v {
        serde_yaml::Value::String(s) => s.clone(),
        serde_yaml::Value::Number(n) => n.to_string(),
        serde_yaml::Value::Bool(b) => b.to_string(),
        // Null/sequences/mappings cannot occur: vars are validated scalar-only
        // and `item` is always a string.
        _ => String::new(),
    }
}

fn validate_step(step: SingleStep, step_no: usize) -> Result<Step> {
    match step {
        SingleStep::Search(s) => {
            crate::config::validate_engine(&s.engine).map_err(|_| {
                Error::Config(format!(
                    "invalid recipe step {step_no}: {}",
                    unknown_engine_msg(&s.engine)
                ))
            })?;
            if let Some(count) = s.count {
                if !(1..=crate::cli::MAX_RESULTS).contains(&count) {
                    return Err(Error::Config(format!(
                        "invalid recipe step {step_no}: count must be 1..={} (got {count})",
                        crate::cli::MAX_RESULTS
                    )));
                }
            }
            // Time bounds and feed fail here — before the first request —
            // not as per-step runtime warnings.
            crate::time::parse_window(s.since.as_deref(), s.until.as_deref(), "since", "until")
                .map_err(|e| {
                    Error::Config(format!("invalid recipe step {step_no}: {}", config_msg(e)))
                })?;
            if let Some(feed) = s.feed.as_deref() {
                crate::engines::fxtwitter::checked_feed(Some(feed)).map_err(|e| {
                    Error::Config(format!("invalid recipe step {step_no}: {}", config_msg(e)))
                })?;
            }
            Ok(Step::Search {
                step_no,
                engine: s.engine,
                query: s.query,
                count: s.count,
                lang: s.lang,
                region: s.region,
                safe: s.safe,
                since: s.since,
                until: s.until,
                feed: s.feed,
            })
        }
        SingleStep::Fetch(f) => {
            if f.urls.is_empty() {
                return Err(Error::Config(format!(
                    "invalid recipe step {step_no}: fetch urls must not be empty"
                )));
            }
            if let Some(max_chars) = f.max_chars {
                if !(1..=crate::cli::MAX_CHARS).contains(&max_chars) {
                    return Err(Error::Config(format!(
                        "invalid recipe step {step_no}: max_chars must be 1..={} (got {max_chars})",
                        crate::cli::MAX_CHARS
                    )));
                }
            }
            if let Some(jobs) = f.jobs {
                if !(1..=crate::cli::MAX_JOBS).contains(&jobs) {
                    return Err(Error::Config(format!(
                        "invalid recipe step {step_no}: jobs must be 1..={} (got {jobs})",
                        crate::cli::MAX_JOBS
                    )));
                }
            }
            Ok(Step::Fetch {
                step_no,
                urls: f.urls,
                max_chars: f.max_chars,
                jobs: f.jobs,
            })
        }
    }
}

fn unknown_engine_msg(name: &str) -> String {
    format!("unknown engine '{name}' (run `webseek engines` to list all)")
}

/// The message inside an [`Error::Config`], without its Display prefix — for
/// re-wrapping with step context instead of doubling "configuration error".
fn config_msg(e: Error) -> String {
    match e {
        Error::Config(m) => m,
        other => other.to_string(),
    }
}

fn parse_combine(
    mapping: &serde_yaml::Mapping,
    vars: &HashMap<String, serde_yaml::Value>,
) -> Result<Option<Combine>> {
    let Some(raw) = mapping.get(serde_yaml::Value::from("combine")) else {
        return Ok(None);
    };
    // `combine:` with a null body means "defaults".
    if raw.is_null() {
        return Ok(Some(Combine {
            dedupe: true,
            sort: None,
            limit: None,
        }));
    }
    let raw = substitute(raw, vars, None)
        .map_err(|e| Error::Config(format!("invalid recipe combine: {e}")))?;
    let raw: RawCombine = serde_yaml::from_value(raw)
        .map_err(|e| Error::Config(format!("invalid recipe combine: {e}")))?;
    if let Some(key) = &raw.dedupe_by {
        if key != "url" {
            return Err(Error::Config(format!(
                "invalid recipe combine: dedupe_by must be 'url' in v1 (got '{key}')"
            )));
        }
    }
    let sort = raw
        .sort
        .as_deref()
        .map(|s| match s {
            "url" => Ok(SortKey::Url),
            "title" => Ok(SortKey::Title),
            "date" => Ok(SortKey::Date),
            other => Err(Error::Config(format!(
                "invalid recipe combine: sort must be 'url', 'title' or 'date' (got '{other}')"
            ))),
        })
        .transpose()?;
    if let Some(limit) = raw.limit {
        if !(1..=COMBINE_LIMIT_MAX).contains(&limit) {
            return Err(Error::Config(format!(
                "invalid recipe combine: limit must be 1..={COMBINE_LIMIT_MAX} (got {limit})"
            )));
        }
    }
    Ok(Some(Combine {
        dedupe: true,
        sort,
        limit: raw.limit,
    }))
}

fn parse_output(
    mapping: &serde_yaml::Mapping,
    vars: &HashMap<String, serde_yaml::Value>,
) -> Result<Option<Output>> {
    let Some(raw) = mapping.get(serde_yaml::Value::from("output")) else {
        return Ok(None);
    };
    if raw.is_null() {
        return Ok(Some(Output::default()));
    }
    let raw = substitute(raw, vars, None)
        .map_err(|e| Error::Config(format!("invalid recipe output: {e}")))?;
    let raw: RawOutput = serde_yaml::from_value(raw)
        .map_err(|e| Error::Config(format!("invalid recipe output: {e}")))?;
    let format = raw
        .format
        .as_deref()
        .map(|f| match f {
            "json" => Ok(Format::Json),
            "jsonl" => Ok(Format::Jsonl),
            "pretty" => Ok(Format::Pretty),
            other => Err(Error::Config(format!(
                "invalid recipe output: format must be 'json', 'jsonl' or 'pretty' (got '{other}')"
            ))),
        })
        .transpose()?;
    Ok(Some(Output {
        format,
        file: raw.file.map(PathBuf::from),
    }))
}

// ---------------------------------------------------------------------------
// Combining
// ---------------------------------------------------------------------------

/// Merge step results: dedupe by URL, then stable-sort, then truncate.
/// `None` (no `combine:` section) concatenates in step order, untouched.
pub fn combine_results(
    mut results: Vec<SearchResult>,
    combine: &Option<Combine>,
) -> Vec<SearchResult> {
    let Some(combine) = combine else {
        return results;
    };
    if combine.dedupe {
        results = crate::engines::dedupe_by_url(results, |r| &r.url);
    }
    match combine.sort {
        Some(SortKey::Url) => results.sort_by(|a, b| a.url.cmp(&b.url)),
        Some(SortKey::Title) => results.sort_by(|a, b| a.title.cmp(&b.title)),
        // Stable: ties (including undated) keep step order.
        Some(SortKey::Date) => results.sort_by(|a, b| {
            crate::time::compare_date_desc(a, b).unwrap_or(std::cmp::Ordering::Equal)
        }),
        None => {}
    }
    if let Some(limit) = combine.limit {
        results.truncate(limit);
    }
    results
}

/// Refuse an `output.file` that would write through a symlink. Overwriting a
/// regular file is allowed (repeated digest runs need it) and documented;
/// a symlink hop is never what a recipe means.
pub fn check_sink_writable(path: &std::path::Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(Error::Config(format!(
            "cannot write {}: {e}",
            path.display()
        ))),
        Ok(meta) if meta.file_type().is_symlink() => Err(Error::Config(format!(
            "refusing to write through symlink {}",
            path.display()
        ))),
        Ok(_) => Ok(()),
    }
}

/// A fetched page as a digest item, so fetch steps join the same combined
/// list as search steps. Full text stays available through `fetch` itself.
pub fn map_fetch(fetch: &FetchResult) -> SearchResult {
    SearchResult {
        title: crate::text::normalize_snippet(fetch.title.as_deref().unwrap_or(fetch.url.as_str())),
        url: fetch.url.clone(),
        snippet: crate::text::normalize_snippet(&fetch.text),
        // A fetched page carries no publication instant of its own.
        published: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
version: 1
vars:
  theme: rust
  n: 10
steps:
  - search: {engine: fxtwitter, query: "${theme} async", count: "${n}"}
  - for_each: {items: ["@a", "@b"], do: {search: {engine: telegram, query: "${item}"}}}
  - fetch: {urls: ["https://example.com"], max_chars: 5000, jobs: 2}
combine: {dedupe_by: url, sort: title, limit: 20}
output: {format: jsonl, file: ./out.jsonl}
"#;

    #[test]
    fn parses_and_unrolls_a_full_recipe() {
        let r = parse(FULL).unwrap();
        assert_eq!(r.steps.len(), 4);
        match &r.steps[0] {
            Step::Search {
                engine,
                query,
                count,
                ..
            } => {
                assert_eq!(engine, "fxtwitter");
                assert_eq!(query, "rust async");
                assert_eq!(*count, Some(10));
            }
            _ => panic!("step 1 must be a search"),
        }
        match &r.steps[1] {
            Step::Search { engine, query, .. } => {
                assert_eq!(engine, "telegram");
                assert_eq!(query, "@a");
            }
            _ => panic!("step 2 must be the first unrolled telegram search"),
        }
        match &r.steps[2] {
            Step::Search { query, .. } => assert_eq!(query, "@b"),
            _ => panic!("step 3 must be the second unrolled search"),
        }
        match &r.steps[3] {
            Step::Fetch { urls, jobs, .. } => {
                assert_eq!(urls, &["https://example.com".to_string()]);
                assert_eq!(*jobs, Some(2));
            }
            _ => panic!("step 4 must be a fetch"),
        }
        let combine = r.combine.unwrap();
        assert!(combine.dedupe);
        assert_eq!(combine.sort, Some(SortKey::Title));
        assert_eq!(combine.limit, Some(20));
        let output = r.output.unwrap();
        assert_eq!(output.format, Some(Format::Jsonl));
        assert_eq!(output.file, Some(PathBuf::from("./out.jsonl")));
    }

    #[test]
    fn minimal_recipe_parses_with_defaults() {
        let r = parse("version: 1\nsteps:\n  - search: {engine: reddit, query: x}\n").unwrap();
        assert_eq!(r.steps.len(), 1);
        assert!(r.combine.is_none());
        assert!(r.output.is_none());
    }

    #[test]
    fn schema_violations_name_the_fault() {
        // Missing version.
        assert!(parse("steps:\n  - search: {engine: x, query: y}\n").is_err());
        // Wrong version.
        assert!(
            parse("version: 2\nsteps:\n  - search: {engine: x, query: y}\n")
                .unwrap_err()
                .to_string()
                .contains("version 2")
        );
        // Top level must be a mapping.
        assert!(parse("- just\n- a\n- list\n").is_err());
        // Empty steps.
        assert!(parse("version: 1\nsteps: []\n").is_err());
        // Unknown step kind.
        let err = parse("version: 1\nsteps:\n  - crawl: {x: 1}\n").unwrap_err();
        assert!(err.to_string().contains("step 1"), "got: {err}");
        // Step with two keys.
        assert!(parse(
            "version: 1\nsteps:\n  - {search: {engine: x, query: y}, fetch: {urls: [z]}}\n"
        )
        .is_err());
        // Unknown keys rejected.
        assert!(
            parse("version: 1\nsteps:\n  - search: {engine: x, query: y, bogus: 1}\n").is_err()
        );
        assert!(parse(
            "version: 1\ntoplevel_bogus: 1\nsteps:\n  - search: {engine: x, query: y}\n"
        )
        .is_err());
        // Not YAML at all.
        assert!(parse("version: [unclosed").is_err());
    }

    #[test]
    fn step_ranges_are_validated_up_front() {
        let bad_engine = "version: 1\nsteps:\n  - search: {engine: nope, query: x}\n";
        assert!(parse(bad_engine)
            .unwrap_err()
            .to_string()
            .contains("unknown engine"));
        let bad_count = "version: 1\nsteps:\n  - search: {engine: reddit, query: x, count: 51}\n";
        assert!(parse(bad_count).unwrap_err().to_string().contains("count"));
        let bad_count_zero =
            "version: 1\nsteps:\n  - search: {engine: reddit, query: x, count: 0}\n";
        assert!(parse(bad_count_zero).is_err());
        let no_urls = "version: 1\nsteps:\n  - fetch: {urls: []}\n";
        assert!(parse(no_urls).unwrap_err().to_string().contains("urls"));
        let bad_jobs = "version: 1\nsteps:\n  - fetch: {urls: [https://x], jobs: 65}\n";
        assert!(parse(bad_jobs).unwrap_err().to_string().contains("jobs"));
        let bad_limit =
            "version: 1\nsteps:\n  - search: {engine: reddit, query: y}\ncombine: {limit: 201}\n";
        assert!(parse(bad_limit).unwrap_err().to_string().contains("limit"));
        let bad_sort =
            "version: 1\nsteps:\n  - search: {engine: reddit, query: y}\ncombine: {sort: bogus}\n";
        assert!(parse(bad_sort).unwrap_err().to_string().contains("sort"));
        let bad_dedupe = "version: 1\nsteps:\n  - search: {engine: reddit, query: y}\ncombine: {dedupe_by: title}\n";
        assert!(parse(bad_dedupe)
            .unwrap_err()
            .to_string()
            .contains("dedupe_by"));
        let bad_format =
            "version: 1\nsteps:\n  - search: {engine: reddit, query: y}\noutput: {format: yaml}\n";
        assert!(parse(bad_format)
            .unwrap_err()
            .to_string()
            .contains("format"));
    }

    #[test]
    fn variables_interpolate_splice_and_escape() {
        // Unknown variable names the known ones.
        let err = parse(
            "version: 1\nvars: {a: x}\nsteps:\n  - search: {engine: reddit, query: \"${b}\"}\n",
        )
        .unwrap_err();
        assert!(
            err.to_string().contains("unknown variable '${b}'"),
            "got: {err}"
        );
        assert!(err.to_string().contains("defined: a"), "got: {err}");
        // Unterminated placeholder.
        assert!(
            parse("version: 1\nsteps:\n  - search: {engine: reddit, query: \"${oops\"}\n").is_err()
        );
        // $$ is a literal $.
        let r =
            parse("version: 1\nsteps:\n  - search: {engine: reddit, query: \"$${not_a_var}\"}\n")
                .unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "${not_a_var}"),
            _ => panic!("must be a search"),
        }
        // Reserved + invalid vars keys and non-scalar values.
        assert!(
            parse("version: 1\nvars: {item: x}\nsteps:\n  - search: {engine: x, query: y}\n")
                .is_err()
        );
        assert!(parse(
            "version: 1\nvars: {\"has-dash\": x}\nsteps:\n  - search: {engine: x, query: y}\n"
        )
        .is_err());
        assert!(
            parse("version: 1\nvars: {a: [1]}\nsteps:\n  - search: {engine: x, query: y}\n")
                .is_err()
        );
        // Booleans stringify in interpolation.
        let r = parse("version: 1\nvars: {flag: true}\nsteps:\n  - search: {engine: reddit, query: \"v=${flag}\"}\n").unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "v=true"),
            _ => panic!("must be a search"),
        }
    }

    #[test]
    fn item_only_exists_inside_for_each() {
        // Outside for_each it is an error, not an unknown variable.
        let err = parse("version: 1\nsteps:\n  - search: {engine: reddit, query: \"${item}\"}\n")
            .unwrap_err();
        assert!(
            err.to_string().contains("only be used inside for_each"),
            "got: {err}"
        );
        // Nested for_each is rejected structurally.
        let nested = "version: 1\nsteps:\n  - for_each: {items: [a], do: {for_each: {items: [b], do: {search: {engine: x, query: y}}}}}\n";
        assert!(parse(nested).unwrap_err().to_string().contains("nested"));
        // Empty items rejected.
        assert!(parse(
            "version: 1\nsteps:\n  - for_each: {items: [], do: {search: {engine: x, query: y}}}\n"
        )
        .is_err());
        // Non-scalar items rejected (numbers/bools stringify, covered above).
        assert!(parse(
            "version: 1\nsteps:\n  - for_each: {items: [[1]], do: {search: {engine: x, query: y}}}\n"
        )
        .is_err());
        // Escaped $${item} outside for_each is a literal, not an error.
        let r = parse("version: 1\nsteps:\n  - search: {engine: reddit, query: \"$${item}\"}\n")
            .unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "${item}"),
            _ => panic!("must be a search"),
        }
    }

    #[test]
    fn combine_defaults_and_null_sections() {
        // Bare `combine:` dedupes with no sort or limit.
        let r = parse("version: 1\nsteps:\n  - search: {engine: x, query: y}\ncombine:\n").unwrap();
        let combine = r.combine.unwrap();
        assert!(combine.dedupe);
        assert_eq!(combine.sort, None);
        assert_eq!(combine.limit, None);
        // Bare `output:` changes nothing.
        let r = parse("version: 1\nsteps:\n  - search: {engine: x, query: y}\noutput:\n").unwrap();
        let output = r.output.unwrap();
        assert_eq!(output.format, None);
        assert_eq!(output.file, None);
    }

    fn hit(title: &str, url: &str) -> SearchResult {
        SearchResult {
            title: title.into(),
            url: url.into(),
            snippet: String::new(),
            published: None,
        }
    }

    #[test]
    fn search_steps_accept_time_bounds_and_feed() {
        // Both bounds are relative: an absolute `until` is fixed in wall-clock
        // time while `since: 24h` keeps sliding, so any absolute date becomes
        // an empty window once it falls behind now-24h.
        let r = parse(
            "version: 1\nsteps:\n  - search: {engine: fxtwitter, query: y, since: 24h, until: 1h, feed: top}\n",
        )
        .unwrap();
        match &r.steps[0] {
            Step::Search {
                since, until, feed, ..
            } => {
                assert_eq!(since.as_deref(), Some("24h"));
                assert_eq!(until.as_deref(), Some("1h"));
                assert_eq!(feed.as_deref(), Some("top"));
            }
            _ => panic!("must be a search"),
        }
        // Bad bounds and feed fail at parse time, naming the step.
        for bad in [
            "version: 1\nsteps:\n  - search: {engine: x, query: y, since: someday}\n",
            "version: 1\nsteps:\n  - search: {engine: x, query: y, until: 24x}\n",
            "version: 1\nsteps:\n  - search: {engine: x, query: y, feed: hot}\n",
            "version: 1\nsteps:\n  - search: {engine: x, query: y, since: 2026-09-20, until: 2026-09-19}\n",
        ] {
            let err = parse(bad).unwrap_err().to_string();
            assert!(err.contains("step 1"), "got: {err}");
            assert_eq!(
                err.matches("configuration error").count(),
                1,
                "kind prefix must not double, got: {err}"
            );
        }
    }

    #[test]
    fn sort_date_orders_newest_first_with_undated_last() {
        let dated = |url: &str, published: Option<&str>| SearchResult {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
            published: published.map(str::to_string),
        };
        let results = vec![
            dated("https://x/old", Some("2020-01-01T00:00:00+00:00")),
            dated("https://x/none", None),
            dated("https://x/new", Some("2026-09-20T00:00:00+00:00")),
        ];
        let combined = combine_results(
            results,
            &Some(Combine {
                dedupe: false,
                sort: Some(SortKey::Date),
                limit: None,
            }),
        );
        let urls: Vec<_> = combined.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(urls, ["https://x/new", "https://x/old", "https://x/none"]);
    }

    #[test]
    fn sort_date_is_stable_on_ties() {
        let dated = |url: &str, published: Option<&str>| SearchResult {
            title: url.into(),
            url: url.into(),
            snippet: String::new(),
            published: published.map(str::to_string),
        };
        // Same instant twice plus an undated twin: step order survives.
        let results = vec![
            dated("https://x/a", Some("2026-09-20T00:00:00+00:00")),
            dated("https://x/b", Some("2026-09-20T00:00:00+00:00")),
            dated("https://x/u1", None),
            dated("https://x/u2", None),
        ];
        let combined = combine_results(
            results,
            &Some(Combine {
                dedupe: false,
                sort: Some(SortKey::Date),
                limit: None,
            }),
        );
        let urls: Vec<_> = combined.iter().map(|r| r.url.as_str()).collect();
        assert_eq!(
            urls,
            ["https://x/a", "https://x/b", "https://x/u1", "https://x/u2"]
        );
    }

    #[test]
    fn combining_dedupes_sorts_then_truncates() {
        let results = vec![
            hit("b", "https://x/2"),
            hit("a", "https://x/1"),
            hit("b-dup", "https://x/2"),
        ];
        // No combine section: untouched.
        assert_eq!(combine_results(results.clone(), &None).len(), 3);
        let combined = combine_results(
            results,
            &Some(Combine {
                dedupe: true,
                sort: Some(SortKey::Title),
                limit: Some(1),
            }),
        );
        // Dedupe drops the dup, sort puts "a" first, limit keeps one.
        assert_eq!(combined.len(), 1);
        assert_eq!(combined[0].title, "a");
    }

    #[test]
    fn fetch_results_map_to_digest_items() {
        let mapped = map_fetch(&FetchResult {
            url: "https://example.com/p".into(),
            title: Some("A page".into()),
            chars: 9,
            truncated: false,
            text: "hello world".into(),
        });
        assert_eq!(mapped.title, "A page");
        assert_eq!(mapped.url, "https://example.com/p");
        assert_eq!(mapped.snippet, "hello world");
        // Missing title falls back to the URL.
        let mapped = map_fetch(&FetchResult {
            url: "https://example.com/q".into(),
            title: None,
            chars: 1,
            truncated: false,
            text: "x".into(),
        });
        assert_eq!(mapped.title, "https://example.com/q");
    }

    #[test]
    fn number_vars_feed_string_fields_but_not_numeric_ones() {
        // A numeric var stringifies in string position ...
        let r = parse(
            "version: 1\nvars: {n: 10}\nsteps:\n  - search: {engine: reddit, query: \"${n}\"}\n",
        )
        .unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "10"),
            _ => panic!("must be a search"),
        }
        // ... while numeric fields need numeric vars (write n: 10 unquoted).
        let err = parse(
            "version: 1\nvars: {s: \"10\"}\nsteps:\n  - search: {engine: reddit, query: x, count: \"${s}\"}\n",
        )
        .unwrap_err();
        assert!(err.to_string().contains("step 1"), "got: {err}");
    }

    #[test]
    fn vars_may_reference_other_vars_single_level() {
        let r = parse(
            "version: 1\nvars: {base: rust, q: \"${base} async\"}\nsteps:\n  - search: {engine: reddit, query: \"${q}\"}\n",
        )
        .unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "rust async"),
            _ => panic!("must be a search"),
        }
        // A self-reference resolves to itself and terminates.
        let r = parse(
            "version: 1\nvars: {a: \"${a}\"}\nsteps:\n  - search: {engine: reddit, query: \"v=${a}\"}\n",
        )
        .unwrap();
        match &r.steps[0] {
            Step::Search { query, .. } => assert_eq!(query, "v=${a}"),
            _ => panic!("must be a search"),
        }
    }

    #[test]
    fn substitution_leaves_mapping_keys_untouched() {
        // `"${k}"` as a step kind stays literal and is rejected as unknown.
        let err =
            parse("version: 1\nvars: {k: search}\nsteps:\n  - \"${k}\": {engine: x, query: y}\n")
                .unwrap_err();
        assert!(err.to_string().contains("unknown step"), "got: {err}");
    }

    #[test]
    fn fetch_steps_unroll_inside_for_each_with_numeric_items() {
        let r = parse(
            "version: 1\nsteps:\n  - for_each: {items: [1, 2], do: {fetch: {urls: [\"https://x/${item}\"]}}}\n",
        )
        .unwrap();
        assert_eq!(r.steps.len(), 2);
        for (step, want) in r.steps.iter().zip(["https://x/1", "https://x/2"]) {
            match step {
                Step::Fetch { urls, step_no, .. } => {
                    assert_eq!(urls, &[want.to_string()]);
                    assert_eq!(*step_no, 1, "origin step, not unrolled position");
                }
                _ => panic!("must be a fetch"),
            }
        }
    }

    #[test]
    fn unknown_keys_rejected_in_every_section() {
        assert!(parse("version: 1\nsteps:\n  - for_each: {items: [a], do: {search: {engine: x, query: y}}, extra: 1}\n").is_err());
        assert!(parse(
            "version: 1\nsteps:\n  - search: {engine: x, query: y}\ncombine: {bogus: 1}\n"
        )
        .is_err());
        assert!(parse(
            "version: 1\nsteps:\n  - search: {engine: x, query: y}\noutput: {bogus: 1}\n"
        )
        .is_err());
        assert!(parse("version: 1\nsteps:\n  - fetch: {urls: [https://x], bogus: 1}\n").is_err());
    }

    #[test]
    fn max_chars_mirrors_the_cli_ceiling() {
        let ok = "version: 1\nsteps:\n  - fetch: {urls: [https://x], max_chars: 10000000}\n";
        assert!(parse(ok).is_ok());
        let over = "version: 1\nsteps:\n  - fetch: {urls: [https://x], max_chars: 10000001}\n";
        assert!(parse(over).unwrap_err().to_string().contains("max_chars"));
    }

    #[test]
    fn fan_out_is_capped() {
        let many_items = (0..MAX_FOR_EACH_ITEMS + 1)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let doc = format!(
            "version: 1\nsteps:\n  - for_each: {{items: [{many_items}], do: {{search: {{engine: x, query: y}}}}}}\n"
        );
        assert!(parse(&doc).unwrap_err().to_string().contains("exceed"));
        let many_steps = (0..MAX_UNROLLED_STEPS + 1)
            .map(|_| "  - search: {engine: x, query: y}".to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let doc = format!("version: 1\nsteps:\n{many_steps}\n");
        assert!(parse(&doc).unwrap_err().to_string().contains("exceed"));
    }

    #[test]
    fn item_in_the_items_list_gets_its_own_error() {
        let err = parse("version: 1\nsteps:\n  - for_each: {items: [\"${item}\"], do: {search: {engine: x, query: y}}}\n")
            .unwrap_err();
        assert!(err.to_string().contains("loop variable"), "got: {err}");
    }

    #[test]
    fn sink_guard_allows_plain_paths() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.jsonl");
        assert!(check_sink_writable(&missing).is_ok());
        let plain = dir.path().join("out.jsonl");
        std::fs::write(&plain, "x").unwrap();
        assert!(check_sink_writable(&plain).is_ok(), "overwrite is allowed");
    }

    #[cfg(unix)]
    #[test]
    fn sink_guard_refuses_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.txt");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let err = check_sink_writable(&link).unwrap_err();
        assert!(err.to_string().contains("symlink"), "got: {err}");
    }
}
