# webseek

**API-key-free web search and page reader, designed for AI agents.**

`webseek` searches the web (DuckDuckGo / Bing), fetches pages as clean
boilerplate-free text, and searches/downloads images — **without any API key**.
It is built for agents and scripts: output is JSON when piped, stdout carries
data only, and token cost is bounded by design.

- **Zero keys.** No signup, no tokens, no rate-limit invoices.
- **Machine-first output.** JSON on non-TTY stdout, pretty text on a TTY.
- **Context-friendly.** Snippets are capped, page text is extracted and
  truncatable (`--max-chars`), no banner text pollutes stdout.
- **Cross-platform.** Windows, macOS, Linux (pure Rust, `rustls` TLS).
- **Gentle by default.** Configurable inter-request delay to avoid 429s.

## Install

```sh
cargo install webseek          # from crates.io (once published)
# or from source:
cargo install --path .
```

## Quick start

```sh
# Text search — JSON when piped, pretty when interactive
webseek search "rust async runtime"
webseek search "tokio vs async-std" --count 10 --json

# Fetch a page and get the main content as clean text
webseek fetch https://rust-lang.github.io/async-book/ --max-chars 20000
webseek fetch https://example.com --markdown --jsonl

# Batch fetch: many URLs in parallel, one JSON array, order preserved
webseek fetch https://a.example https://b.example https://c.example -j 4 --json

# Image search + download (no API key)
webseek images "japanese garden" --count 8
webseek images "mountain sunset" --download ./pics --limit 5 --max-bytes 5242880

# Region-aware search (flexible input: jp, en-us, EN_US, ...)
webseek search "ラーメン" --region jp
webseek search "coffee" --region en-us

# Stable sources (official JSON APIs, no scraping) — pick with --engine
webseek search "Tokyo"            --engine wikipedia --lang ja
webseek search "rust"             --engine hackernews
webseek search "rust"             --engine reddit          # public RSS, go easy
webseek search "async runtime"    --engine stackexchange
webseek search "transformers"     --engine openalex
webseek search "immunotherapy"    --engine pubmed
webseek search "async"            --engine crates        # also: npm
webseek search "requests"         --engine pypi          # exact-name lookup
webseek search "Tokyo"            --engine nominatim     # geocoding

# Discover every engine and what it's for (machine-readable)
webseek engines --json

# Honor robots.txt before fetching (opt-in)
webseek fetch https://example.com/private --respect-robots

# Write a default config file
webseek init
```

## Design for agents

### Output contract

| Mode | When |
|---|---|
| `--json` | Force one JSON document on stdout |
| `--jsonl` | One JSON object per line (stream-friendly) |
| `--pretty` | Force human/colored text |
| *(auto)* | JSON when stdout is not a TTY, else pretty text |

**stdout carries data only.** Progress notes go to stderr (`--verbose` to see
them), so piping is always safe:

```sh
webseek search "rust macros" --json | jq .results[].url
```

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success — including *no results* (empty JSON is valid) |
| `1` | Runtime error (network / parse / config / rate-limited) |
| `2` | CLI usage error (clap) |

### JSON shapes

`search`:

```json
{"query":"rust","engine":"duckduckgo","count":5,"results":[
  {"title":"...","url":"https://...","snippet":"..."}]}
```

`fetch`:

```json
{"url":"https://...","title":"...","chars":12034,"truncated":false,"text":"..."}
```

`images`:

```json
{"query":"cats","engine":"bing","count":5,"results":[
  {"title":"...","url":"https://cdn...","page_url":"https://...","width":1920,"height":1080,"format":"jpg"}],
 "downloaded":null}
```

`fetch` (multiple URLs — a JSON **array**, input order preserved; a failed URL
becomes an error item instead of aborting the batch):

```json
[
  {"url":"https://a","title":"...","chars":123,"truncated":false,"text":"..."},
  {"url":"https://b","error":"HTTP 404 from upstream"}
]
```

### Context-saving rules

- Snippets are single-line and capped at ~300 chars.
- `fetch` extracts the main content (readability-lite: strips nav/ads/scripts,
  picks the semantic container or the densest block) and caps text at
  `--max-chars` (default 20 000).
- `--markdown` keeps headings/lists/links; `--html` dumps raw HTML.
- Truncation is reported explicitly (`"truncated": true`) so agents can decide
  to re-fetch with a larger cap instead of trusting partial text.

### Bulk research

For multi-page research, keep the delay modest and raise the caps:

```sh
for url in $(webseek search "2026 LLM survey" --count 8 --json | jq -r .results[].url); do
  webseek fetch "$url" --max-chars 40000 --jsonl >> corpus.jsonl
done
```

Set `delay_ms = 0` in the config for batch speed (be polite: only against
endpoints that tolerate it) or `300`+ for normal use.

## Reliability

- **Cache.** Responses are cached on disk (LRU + TTL, keyed by the full request
  intent) so repeated lookups are instant and gentle on upstreams. Disable per
  run with `--no-cache`, or permanently with `cache_max_entries = 0`. Cache
  files are written atomically and a corrupt cache is discarded, never fatal.
- **Automatic fallback.** If an engine is rate-limited or errors (network /
  parse / HTTP), webseek tries the remaining engines in order until one
  succeeds, and reports which one served the result. Disable with
  `--no-fallback` or `fallback = false`.
- **Resilient transport.** Requests carry browser-like headers
  (Accept / Accept-Language / sec-ch-ua) to avoid tripping anti-bot challenges,
  and transient failures (202/429/5xx/network) are retried with exponential
  backoff + jitter — politely, so retries never amplify load.
- **robots.txt (opt-in).** With `--respect-robots` (or `respect_robots = true`)
  webseek checks the wildcard user-agent group before fetching, using
  RFC 9309 longest-prefix matching. Missing/unreachable robots.txt is treated
  as "allowed" (robots is advisory). Wildcards (`*`, `$`) in patterns are not
  expanded — a documented limitation of this minimal implementation.

## Configuration

`webseek init` writes a documented `config.toml` to the platform config dir:

- Windows: `%APPDATA%\webseek\config.toml`
- Linux: `~/.config/webseek/config.toml`
- macOS: `~/Library/Application Support/webseek/config.toml`

Resolution order: `WEBSEEK_CONFIG` env var > `--config <path>` > platform dir.

```toml
engine = "duckduckgo"     # text engine: duckduckgo | bing
image_engine = "bing"     # image engine: bing | duckduckgo
delay_ms = 300            # pause between upstream requests
timeout_secs = 15
user_agent = "..."        # browser-like by default
safe_search = false
lang = null               # e.g. "ja" (engine-dependent)
region = null             # flexible: "jp", "en-us", "EN_US" (normalized per engine)
max_chars = 20000         # fetch text cap
max_results = 5
cache_ttl_secs = 3600     # response cache TTL (0 = no expiry)
cache_max_entries = 1000  # response cache size (0 = disabled)
fallback = true           # auto-switch engine on rate limit/error
respect_robots = false    # honor robots.txt before fetching
image_max_bytes = 5242880 # skip downloaded images larger than this
```

## Engines & ethics

webseek deliberately mixes two kinds of source. Run `webseek engines` for the
live, machine-readable catalog.

**General web search** (broad, but scraped — can change or block):

- **DuckDuckGo** (`html.duckduckgo.com/html/`): no key, HTML scraping.
- **Bing** (`www.bing.com/search`): no key. Prefers the **RSS output**
  (`&format=rss`) — a stable, structured format that is not behind a bot
  challenge — and falls back to HTML scraping (base64-unwrapping the `ck/a`
  redirect links) if RSS yields nothing.
- **Bing Images** / **DuckDuckGo Images** (`i.js` with a per-request `vqd`
  token): no key.

**Stable sources** (official JSON APIs, no key, no scraping — these are what
keep working when the scraped engines get blocked). Each is a *vertical*: great
for its domain, not a replacement for general web search.

| Engine | Source | Answers |
|---|---|---|
| `wikipedia` | MediaWiki API | Encyclopedia articles (`--lang` = edition) |
| `hackernews` | Algolia HN API | Tech news & discussion |
| `reddit` | Reddit public RSS | Reddit posts (rate-limit-strict) |
| `stackexchange` | Stack Exchange API | Programming Q&A |
| `openalex` | OpenAlex | Scholarly works (all fields) |
| `crossref` | CrossRef | DOI / citation metadata |
| `pubmed` | NCBI E-utilities | Biomedical literature |
| `crates` / `npm` | crates.io / npm registry | Package keyword search |
| `pypi` | PyPI JSON API | Python package lookup (exact name) |
| `nominatim` | OpenStreetMap Nominatim | Geocoding / places |

Notes: PyPI has **no** keyword-search API (its HTML search is bot-protected), so
`pypi` is an exact-name lookup. Nominatim requires a browser-like User-Agent
(webseek sends one) and asks for low volume. All stable sources still honor the
global `delay` and benefit from the cache.

The scraped endpoints can change or block aggressive use. webseek's engine layer
is a small trait precisely so sources can be added or swapped without touching
the CLI. Please respect each site's ToS, keep the default delay, and use
`--safe` when appropriate.

## Development

```sh
cargo test          # unit + wiremock integration tests (no network needed)
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

## Releases

Pushing a version tag (`v*`) starts the release workflow after its quality
gate (format, clippy, and tests). It builds and tests native release binaries
on GitHub-hosted runners for Linux x86_64, Windows x86_64, macOS Intel, and
macOS Apple Silicon. The workflow creates a GitHub Release with the archives
and a `SHA256SUMS.txt` checksum file; it does not publish to crates.io.

```sh
git tag v0.2.0
git push origin v0.2.0
```

Project layout:

```
src/
  cli.rs        clap definitions
  config.rs     config.toml loading / writing
  engines/      SearchEngine + ImageEngine traits; web (DDG/Bing) + stable
                sources (wikipedia, hackernews, reddit, stackexchange,
                academic, packages, nominatim)
  error.rs      typed errors with stable exit-code semantics
  http.rs       browser headers + retry with backoff/jitter (transport layer)
  lib.rs        run() orchestration (cache, fallback, batch wiring)
  models.rs     JSON contract types
  output.rs     JSON / JSONL / pretty writers (data-only stdout)
  reader.rs     fetch + readability-lite text extraction
  batch.rs      parallel multi-URL fetch with per-item error isolation
  cache.rs      on-disk LRU+TTL response cache (SHA-256 keys)
  region.rs     --region normalization (DDG kl / Bing cc)
  robots.rs     minimal robots.txt parser + per-origin checker
  text.rs       shared text helpers (snippet/name/truncate/strip_html)
tests/
  engines.rs    web-engine integration tests against a local mock server
  verticals.rs  stable-source engine integration tests (wiremock)
  batch.rs      batch-fetch + robots.txt integration tests (wiremock)
```

## Roadmap

- [ ] `--sites:` operator to restrict results to a domain
- [ ] CI canary that detects a broken parser (feeds an AI-assisted repair loop)
- [ ] cargo-dist release automation (3-OS binaries + installers)

## License

MIT — see [LICENSE](LICENSE).
