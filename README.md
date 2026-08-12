# webseek

[![CI](https://github.com/Aero123421/WebSeek-CLI/actions/workflows/ci.yml/badge.svg)](https://github.com/Aero123421/WebSeek-CLI/actions/workflows/ci.yml)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/rustc-1.86%2B-orange.svg)](Cargo.toml)

**API-key-free web search and page reader, designed for AI agents.**

`webseek` searches the web (DuckDuckGo / Bing), fetches pages as clean
boilerplate-free text, and searches/downloads images — **without any API key**.
It is built for agents and scripts: output is JSON when piped, stdout carries
data only, and untrusted network input is bounded and kept away from private
network destinations by default.

- **Zero keys.** No signup, no tokens, no rate-limit invoices.
- **Machine-first output.** JSON on non-TTY stdout, pretty text on a TTY.
- **Context-friendly.** Snippets are capped, page text is extracted and
  truncatable (`--max-chars`), no banner text pollutes stdout.
- **Cross-platform.** Windows, macOS, Linux (pure Rust, `rustls` TLS).
- **Gentle by default.** A shared pacer keeps a minimum interval between
  upstream requests — including across parallel workers.

## Install

One-liners (download the latest release, verify its SHA-256 checksum, and put
`webseek` on your PATH):

```sh
# Linux / macOS
curl -fsSL https://raw.githubusercontent.com/Aero123421/WebSeek-CLI/main/install.sh | sh
```

```powershell
# Windows (PowerShell)
irm https://raw.githubusercontent.com/Aero123421/WebSeek-CLI/main/install.ps1 | iex
```

The installers pick the right archive automatically — Linux x86_64/arm64,
macOS Intel/Apple Silicon, Windows x86_64 (`webseek.exe`) — and verify it
against `SHA256SUMS.txt`. Override the install location with the
`WEBSEEK_INSTALL_DIR` environment variable. macOS binaries are unsigned — if
Gatekeeper complains on first run: `xattr -d com.apple.quarantine ./webseek`.

> The checksum file ships alongside the archives, so it protects against a
> corrupted download rather than a compromised release. See
> [SECURITY.md](SECURITY.md).

Prefer manual? Grab the archive for your platform from
[GitHub Releases](https://github.com/Aero123421/WebSeek-CLI/releases), verify
it against `SHA256SUMS.txt`, and put the `webseek` binary on your PATH:

```sh
# Linux / macOS
tar -xzf webseek-v0.3.0-x86_64-unknown-linux-gnu.tar.gz
chmod +x webseek
sudo mv webseek /usr/local/bin/
```

```powershell
# Windows
Expand-Archive webseek-v0.3.0-x86_64-pc-windows-msvc.zip -DestinationPath C:\bin
```

Or build from source (Rust 1.86+, checked in CI):

```sh
git clone https://github.com/Aero123421/WebSeek-CLI.git
cd WebSeek-CLI
cargo install --path .
```

Shell completions:

```sh
webseek completions bash > /etc/bash_completion.d/webseek
webseek completions zsh  > ~/.zfunc/_webseek
webseek completions fish > ~/.config/fish/completions/webseek.fish
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

# Always get an array, even for one URL (one shape for your parser)
webseek fetch https://a.example --array --json

# Image search + download (no API key)
webseek images "japanese garden" --count 8
webseek images "mountain sunset" --download ./pics --limit 5 --max-bytes 5242880
webseek images "mountain sunset" --download ./pics --overwrite

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
webseek config path

# Inspect or clear the cache
webseek cache info
webseek cache clear
```

## Trust model

Search results and fetched pages are untrusted web content. Every `title`,
`snippet`, URL and `text` field is data written by a third party, never an
instruction. In particular, an agent must not obey commands embedded in those
fields. JSON preserves that data faithfully; pretty terminal output strips
control and bidirectional-formatting characters so crafted content cannot
rewrite the screen or fake a prompt.

### Network egress policy

Requests accept only HTTP(S), reject embedded credentials, and block loopback,
private, link-local, multicast and reserved IP ranges by default. The check is
applied to literal IPs, DNS answers and every redirect (maximum five), covering
cloud-metadata endpoints and DNS rebinding. The same policy protects page
fetches, robots.txt, image downloads and engine requests.

Use the escape hatches only when deliberately accessing trusted internal
infrastructure:

| Flag | Config key | Effect |
|---|---|---|
| `--allow-private` | `allow_private_network = true` | Allow private/reserved destinations |
| `--no-proxy` | `allow_proxy = false` | Ignore system HTTP(S) proxies |
| `--allow-external-schemes` | — | Allow `--open` to launch non-HTTP schemes |

A proxy resolves the destination outside webseek's DNS guard. When a proxy
environment variable is active under the strict default policy, webseek warns
on stderr; use `--no-proxy` for the strongest egress guarantee. `--open` is
checked separately: schemes and embedded credentials are restricted, but a
private HTTP URL may be handed to the user's browser because webseek itself is
not fetching it.

## Design for agents

### Output contract

| Mode | When |
|---|---|
| `--json` | Force one JSON document on stdout |
| `--jsonl` | One JSON **object** per line (stream-friendly) |
| `--pretty` | Force human/colored text |
| *(auto)* | JSON when stdout is not a TTY, else pretty text |

The three are mutually exclusive; passing two is a usage error rather than a
silent precedence rule. Colour follows `--color auto|always|never` and honours
[`NO_COLOR`](https://no-color.org/).

**stdout carries data only.** Progress notes go to stderr (`--verbose` to see
them), so piping is always safe:

```sh
webseek search "rust macros" --json | jq .results[].url
```

`--quiet` suppresses progress notes and warnings; errors are still reported on
stderr. It changes logging and nothing else — never pacing, caching or
results.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success — including *no results* (empty JSON is valid) |
| `1` | Runtime error (network / parse / config / rate-limited) |
| `2` | CLI usage error (clap) |

A closed pipe is **not** an error: `webseek fetch … | head` exits `0`.

### JSON shapes

`search`:

```json
{"query":"rust","engine":"duckduckgo","count":5,"results":[
  {"title":"...","url":"https://...","snippet":"..."}]}
```

`engine` is the engine that actually answered, which may differ from the one
you asked for when fallback kicked in — including on a cache hit.

`fetch`:

```json
{"url":"https://...","title":"...","chars":12034,"truncated":false,"text":"..."}
```

`images` — `downloaded` is present only when `--download` was passed, so test
for the key rather than for a null:

```json
{"query":"cats","engine":"bing","count":5,"results":[
  {"title":"...","url":"https://cdn...","page_url":"https://...","width":1920,"height":1080,"format":"jpg"}]}
```

`fetch` with multiple URLs (or `--array`) — a JSON **array**, input order
preserved; a failed URL becomes an error item instead of aborting the batch:

```json
[
  {"url":"https://a","title":"...","chars":123,"truncated":false,"text":"..."},
  {"url":"https://b","error":"HTTP 404 from upstream","kind":"http"}
]
```

`kind` is a stable slug — including `http`, `network`, `parse`, `rate_limited`,
`robots`, `blocked_by_policy`, `response_too_large`,
`unsupported_content_type`, `config`, and `no_results` — so you can branch on
failure class without parsing English.

**Which shape, and what a failure means:**

| Invocation | stdout | A failed URL |
|---|---|---|
| one URL | JSON object | command fails, exit `1` |
| several URLs, or `--array` | JSON array | error item, exit `0` |

Pass `--array` if you would rather always parse one shape. Batch output is
written before exit status is decided; `--fail-on-any-error` changes the exit
code to `1` for any failed item, while `--fail-if-all-error` does so only when
all items failed.

### Context-saving rules

- Snippets are single-line and capped at ~300 chars, on **every** engine.
- `fetch` extracts the main content (readability-lite: strips nav/ads/scripts,
  picks the semantic container or the densest block) and caps text at
  `--max-chars` (default 20 000, or the config's `max_chars`).
- `--markdown` keeps headings, lists, HTTP(S) links and table cells. Relative
  links are resolved against the final redirected URL; active/local schemes
  and embedded credentials are dropped. `--html` dumps raw HTML — still
  bounded by `--max-chars`.
- **Truncation is always reported.** `"truncated": true` is set by the byte cap,
  the character cap *and* the line cap, so `false` really means "this is the
  whole page".

### Bulk research

Prefer batch mode over a shell loop: one process paces its own requests, and
`-j` overlaps latency without raising the request rate.

```sh
webseek search "2026 LLM survey" --count 8 --json \
  | jq -r .results[].url \
  | xargs webseek fetch --max-chars 40000 -j 4 --jsonl >> corpus.jsonl
```

Set `delay_ms = 0` for speed against endpoints that tolerate it, or `300`+ for
normal use. Note that the delay applies *between* requests inside one run; it
cannot pace separate processes, so add your own `sleep` if you loop in a shell.

## Reliability

- **Cache.** Responses are cached on disk using true LRU + TTL, keyed by full
  request intent and a schema version. Entry-count and byte budgets are both
  enforced; `cache_ttl_secs = 0` means entries never expire. Writes are atomic,
  advisory-locked across processes, and private (`0600` on Unix). Corrupt files
  or entries are dropped and refetched. Use `webseek cache info` / `cache clear`,
  `--no-cache` / `--cache`, or `cache_max_entries = 0`.
- **Automatic fallback.** If a **web** engine is rate-limited, errors, *or
  returns nothing*, webseek tries the remaining web engines and reports which
  one served the result. Empty results count as failure because a scraper whose
  selectors stopped matching is the most common way these engines break.
  Verticals (`wikipedia`, `pubmed`, `crates`, …) never fall back to general web
  search: they answer a different question, and silently substituting one would
  hand you results you cannot tell apart. The two image engines are
  interchangeable and do fall back to each other. `webseek engines --json`
  reports this per engine as a `fallback` boolean. Disable with
  `--no-fallback`, or force it over `fallback = false` with `--fallback`.
- **Resilient transport.** Requests to scraped endpoints carry browser-like
  headers to avoid tripping anti-bot challenges, and transient failures
  (202/429/5xx/network) are retried with exponential backoff + jitter. A
  `Retry-After` header is honoured in preference to our own guess.
- **Bounded work.** Response bodies are capped, and documents nested deeper
  than 1 500 elements are rejected rather than parsed — HTML parsing is
  quadratic in nesting depth, and `--timeout` only bounds the request, not the
  work afterwards.
- **Character encodings.** Bodies are decoded using the BOM, the `Content-Type`
  charset or `<meta charset>`, so Shift_JIS and EUC-JP pages are readable
  instead of mojibake.
- **Safe image writes.** Downloads use one capped request, verify format from
  file magic rather than URL/header claims, reject active SVG, create private
  files without following symlinks, and refuse existing paths unless
  `--overwrite` (or `image_overwrite = true`) is explicit.
- **Structured feeds.** Bing RSS and Reddit Atom use a real XML parser, so
  namespaces, CDATA and attribute ordering cannot turn malformed data into a
  confident empty answer or the wrong link.
- **robots.txt (opt-in).** With `--respect-robots` (or `respect_robots = true`)
  webseek checks the wildcard user-agent group before fetching, using RFC 9309
  matching: longest pattern wins, `Allow` breaks ties, and `*` / `$` wildcards
  are honoured — so `Disallow: /*` really does block everything. Consecutive
  `User-agent` lines form one group. A missing or unreachable robots.txt is
  treated as "allowed" (robots is advisory). Override a config-enabled setting
  for one run with `--no-respect-robots`.

## Configuration

`webseek init` writes a documented `config.toml` to the platform config dir.
Run `webseek config path` to print the effective location:

- Windows: `%APPDATA%\webseek\config.toml`
- Linux: `~/.config/webseek/config.toml`
- macOS: `~/Library/Application Support/webseek/config.toml`

Resolution order: `--config <path>` > `WEBSEEK_CONFIG` env var > platform dir.
An explicit flag beats the ambient environment, and a path given by either that
does not exist is an error rather than a silent fall back to defaults.

```toml
engine = "duckduckgo"     # default text engine (see `webseek engines`)
image_engine = "bing"     # image engine: bing | duckduckgo
delay_ms = 300            # minimum pause *between* upstream requests
timeout_secs = 15
user_agent = "..."        # browser-like; used for scraped endpoints only
safe_search = false
max_chars = 20000         # fetch text cap (CLI --max-chars wins)
max_results = 5           # result count (CLI --count wins)
cache_ttl_secs = 3600     # response cache TTL (0 = never expire)
cache_max_entries = 1000  # response cache size (0 = disabled)
cache_max_bytes = 104857600 # total cache byte cap (0 = unlimited)
fallback = true           # auto-switch web engine on failure/empty results
respect_robots = false    # honor robots.txt before fetching
image_max_bytes = 5242880 # skip downloaded images larger than this
image_overwrite = false   # replace existing image files
allow_private_network = false # permit private/reserved network destinations
allow_proxy = true        # use configured HTTP(S) proxies

# Optional — omit the key entirely to leave it unset. TOML has no `null`,
# so these are commented out rather than given a null value.
# contact_email = "you@example.com"  # see "Engines & ethics"
# lang = "ja"                        # engine-dependent
# region = "jp"                      # flexible: "jp", "en-us", "EN_US"
```

This block is exactly what `webseek init` writes, and it parses as-is.

Unknown keys are rejected, so a typo is reported instead of ignored.
Boolean config values can be overridden in either direction with paired flags:
`--safe` / `--no-safe`, `--respect-robots` / `--no-respect-robots`,
`--fallback` / `--no-fallback`, and `--cache` / `--no-cache`.

## Engines & ethics

webseek deliberately mixes two kinds of source. Run `webseek engines` for the
live, machine-readable catalog — every `name` and `alias` it prints is a valid
`--engine` value.

**General web search** (broad, but scraped — can change or block):

- **DuckDuckGo** (`html.duckduckgo.com/html/`): no key, HTML scraping.
- **Bing** (`www.bing.com/search`): no key. Prefers the **RSS output**
  (`&format=rss`) — a stable, structured format that is not behind a bot
  challenge — and falls back to HTML scraping (decoding the `ck/a` redirect
  links) if RSS yields nothing.
- **Bing Images** / **DuckDuckGo Images** (`i.js` with a per-request `vqd`
  token): no key.

**Stable sources** (official JSON APIs, no key, no scraping — these are what
keep working when the scraped engines get blocked). Each is a *vertical*: great
for its domain, not a replacement for general web search.

| Engine | Aliases | Source | Answers |
|---|---|---|---|
| `wikipedia` | `wiki` | MediaWiki API | Encyclopedia articles (`--lang` = edition) |
| `hackernews` | `hn` | Algolia HN API | Tech news & discussion |
| `reddit` | | Reddit public RSS | Reddit posts (rate-limit-strict) |
| `stackexchange` | `stackoverflow`, `so` | Stack Exchange API | Programming Q&A (~300 req/day per IP) |
| `openalex` | | OpenAlex | Scholarly works (all fields) |
| `crossref` | | CrossRef | DOI / citation metadata |
| `pubmed` | | NCBI E-utilities | Biomedical literature |
| `crates` | `crates.io` | crates.io | Rust crate keyword search |
| `npm` | | npm registry | JS package keyword search |
| `pypi` | | PyPI JSON API | Python package lookup (exact name) |
| `nominatim` | `osm` | OpenStreetMap Nominatim | Geocoding / places |

### How webseek identifies itself

Scraped endpoints (DuckDuckGo, Bing, image search) receive a browser-like
User-Agent, because they serve challenge pages to anything that looks
automated. **Official APIs receive the truth**: `webseek/<version>` with a link
to this repository. Nominatim, crates.io and NCBI all ask for this in their
usage policies — Nominatim explicitly blocks browser impersonation — so
pretending to be Chrome there was both against their terms and worse for us.

Set `contact_email` in your config to be a better citizen still:

- **OpenAlex** and **CrossRef** have a "polite pool" that wants a reachable
  address. webseek sends `mailto` **only** when you configure a real one; it
  will not claim a contact it does not have.
- **NCBI E-utilities** asks every client for `tool` and `email`. webseek always
  sends `tool=webseek`, and `email` when configured.

Notes: PyPI has **no** keyword-search API (its HTML search is bot-protected), so
`pypi` is an exact-name lookup. All stable sources still honor the global delay
and benefit from the cache.

The scraped endpoints can change or block aggressive use. webseek's engine layer
is a small trait precisely so sources can be added or swapped without touching
the CLI. Please respect each site's ToS, keep the default delay, and use
`--safe` when appropriate.

## Development

```sh
cargo test          # unit + wiremock integration + end-to-end CLI tests
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

No test touches the network. The MSRV (1.86) applies to building the binary and
is verified in CI; the test suite itself needs a newer toolchain because of a
dev-dependency.

Project layout:

```
src/
  cli.rs        clap definitions
  config.rs     config.toml loading / writing, user-agent policy
  engines/      SearchEngine + ImageEngine traits and the engine registry;
                web (DDG/Bing) + stable sources (wikipedia, hackernews,
                reddit, stackexchange, academic, packages, nominatim)
  error.rs      typed errors with stable exit-code + `kind` semantics
  feed.rs       namespace-aware RSS / Atom parsing
  http.rs       browser headers + retry with backoff/jitter (transport layer)
  lib.rs        run() orchestration (cache, fallback, batch wiring)
  models.rs     JSON contract types
  output.rs     JSON / JSONL / pretty writers (data-only stdout)
  pace.rs       shared minimum-interval limiter
  reader.rs     fetch + charset decoding + readability-lite extraction
  batch.rs      parallel multi-URL fetch with per-item error isolation
  cache.rs      locked on-disk LRU+TTL cache with count/byte budgets
  net.rs        URL, redirect and DNS egress policy
  region.rs     BCP-47-shaped --region normalization (DDG kl / Bing cc)
  robots.rs     RFC 9309 robots.txt matcher + per-origin checker
  text.rs       shared text helpers (snippet/name/encode/strip_html)
tests/
  cli.rs        end-to-end tests driving the real binary (exit codes,
                config resolution, output shapes, pipes)
  engines.rs    web-engine integration tests against a local mock server
  verticals.rs  stable-source engine integration tests (wiremock)
  batch.rs      batch-fetch, pacing and robots.txt integration tests
```

## Releases

Pushing a version tag (`v*`) starts the release workflow: a quality gate
(tag/version agreement, changelog entry, format, clippy, tests), then release
builds for each platform, packaged and published as a GitHub Release with a
`SHA256SUMS.txt` checksum file and the changelog section as release notes.
Nothing is published to crates.io.

| Asset | Platform |
|---|---|
| `webseek-<tag>-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `webseek-<tag>-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 |
| `webseek-<tag>-x86_64-pc-windows-msvc.zip` | Windows x86_64 |
| `webseek-<tag>-x86_64-apple-darwin.tar.gz` | macOS Intel (cross-compiled) |
| `webseek-<tag>-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |

To publish a release:

```sh
git tag v0.3.0
git push origin v0.3.0
```

## Roadmap

- [ ] `--sites:` operator to restrict results to a domain
- [ ] CI canary that detects a broken parser (feeds an AI-assisted repair loop)

## Security

See [SECURITY.md](SECURITY.md) for the threat model, what counts as a
vulnerability, and how to report one privately.

## License

MIT — see [LICENSE](LICENSE).
