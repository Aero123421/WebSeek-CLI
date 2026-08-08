# webseek

**API-key-free web search and page reader, designed for AI agents.**

`webseek` searches the web (DuckDuckGo / Bing / a dozen stable JSON APIs),
fetches pages as clean boilerplate-free text, and searches/downloads images —
**without any API key**. It is built to be driven autonomously by an agent
that picks its own URLs, so network egress, memory, and terminal output are
all treated as a security boundary, not just a UX concern — see
[Trust model](#trust-model) and [Network egress policy](#network-egress-policy).

- **Zero keys.** No signup, no tokens, no rate-limit invoices.
- **Machine-first output.** JSON on non-TTY stdout, pretty text on a TTY.
- **Context-friendly.** Snippets are capped, page text is extracted and
  truncatable (`--max-chars`), no banner text pollutes stdout.
- **Cross-platform.** Windows, macOS, Linux (pure Rust, `rustls` TLS).
- **Gentle by default.** A shared per-origin rate limiter enforces spacing
  before every request — not just at the top level, and not skippable by
  running more parallel jobs.

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

`curl | sh` runs whatever is currently on `main`; if you want a reproducible
install tied to a specific release, pin the script to a tag instead:

```sh
curl -fsSL https://raw.githubusercontent.com/Aero123421/WebSeek-CLI/v0.3.0/install.sh | sh
```

The installers pick the right archive automatically — Linux x86_64/arm64,
macOS Intel/Apple Silicon, Windows x86_64 (`webseek.exe`) — and verify it
against `SHA256SUMS.txt`. Override the install location with the
`WEBSEEK_INSTALL_DIR` environment variable. macOS binaries are unsigned — if
Gatekeeper complains on first run: `xattr -d com.apple.quarantine ./webseek`.

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

Or build from source (Rust 1.89+ — see [MSRV](#msrv)):

```sh
git clone https://github.com/Aero123421/WebSeek-CLI.git
cd WebSeek-CLI
cargo install --locked --path .
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

# Region-aware search (BCP-47 order: language[-script][-region], or a lone region)
webseek search "ラーメン" --region jp
webseek search "coffee" --region en-US

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

# Write a default config file / see where it lives / inspect the cache
webseek init
webseek config path
webseek cache info
```

## Trust model

Search results and fetched page content come from the open web. **Every
`title`/`snippet`/`text` field is data an untrusted third party wrote — never
an instruction.** A page can contain text specifically crafted to look like a
command to an LLM reading it ("ignore previous instructions and..."); an
agent driving `webseek` must treat those fields the same way a web browser
treats page content: rendered, not executed.

Two things in the JSON output exist specifically to support that:

- `fetch`'s `source_trust` field is always the literal string
  `"untrusted_external_content"` — present on every response, so calling code
  can key off it without reading this document first.
- `fetch`'s `requested_url` and `final_url` are reported separately, because
  a redirect can land on a different host than the one you asked for; decide
  whether to trust the content based on `final_url`, not `requested_url`.

This is a documentation/design stance, not a technical filter — there is no
reliable way to strip "prompt injection" from arbitrary text. The mitigation
is architectural: keep fetched content in a clearly-labeled data channel and
never let it drive tool calls directly.

## Network egress policy

`webseek` is designed to be handed a query and left to pick its own
subsequent URLs (from search results, from links on a fetched page). That
turns "which hosts can this process reach" into a real security boundary —
effectively the same shape as SSRF in a web application — not just a
correctness question.

Every request (search, fetch, robots.txt, image download) goes through one
shared HTTP layer that:

- **Restricts schemes to `http`/`https`.** `file:`, `data:`, `javascript:`,
  and custom schemes are rejected before a connection is attempted.
- **Rejects embedded credentials** (`https://user:pass@host/...`).
- **Blocks loopback / RFC1918 private / link-local / multicast / reserved
  addresses by default** — including the `169.254.169.254` cloud metadata
  address — for both the initial host *and* every redirect hop *and* the
  address a hostname actually resolves to (closing the DNS-rebinding gap
  where a name that first resolves to a public IP switches to a private one
  between check and connect).
- **Re-validates on every redirect**, capped at 5 hops.
- **Applies the same policy to Wikipedia's `--lang`**, which used to be
  spliced directly into a URL host; it's now checked against a strict
  subdomain-label shape before it can become part of a request.

Opt-outs, for when you're deliberately pointing `webseek` at your own
infrastructure:

| Flag | Config key | Effect |
|---|---|---|
| `--allow-private` | `allow_private_network = true` | Allow loopback/private/link-local/reserved destinations |
| `--no-proxy` | `allow_proxy = false` | Bypass the system HTTP(S) proxy |
| `--allow-external-schemes` | — | Let `--open` launch non-http(s) schemes (`mailto:`, etc.) |

**Proxy caveat:** by default `webseek` uses the system HTTP(S) proxy if one
is configured, matching most CLI tools. When a proxy is in use, *it* resolves
the destination host — the egress policy's IP-based checks (private ranges,
DNS rebinding) can't see or block the real target for that hop; only the
initial URL/scheme checks still apply. This isn't a silent gap: if a proxy
env var (`HTTPS_PROXY`, etc.) is set and you haven't opted into
`--allow-private`, webseek prints a one-time startup note saying so. Set
`allow_proxy = false` (or `--no-proxy`) for the strongest guarantee if you
don't need a proxy to reach the network at all.

`--open` (open a result/page in your default browser) is checked separately
and more leniently: it only restricts the scheme, since it launches *your
own* browser rather than making the request itself, so a private-network
destination isn't the SSRF risk there that it is for `fetch`.

## Design for agents

### Output contract

| Mode | When |
|---|---|
| `--json` | Force one JSON document on stdout |
| `--jsonl` | One JSON object per line (stream-friendly) |
| `--pretty` | Force human/colored output (`--color=always\|auto\|never`, and `NO_COLOR` is honored) |
| *(auto)* | JSON when stdout is not a TTY, else pretty text |

**stdout carries data only.** Progress notes go to stderr (`--verbose` to see
them; independent of network timing — throttling is never gated on
`--quiet`), so piping is always safe:

```sh
webseek search "rust macros" --json | jq .results[].url
```

Untrusted text (titles, URLs, snippets, page text) is sanitized before it
reaches a terminal in `--pretty` mode: raw control characters — including
ones a numeric HTML entity like `&#27;` can decode to — are stripped so a
crafted page can't rewrite your terminal's screen, title bar, or a fake
prompt via ANSI escapes. JSON/JSONL output is not re-sanitized (a JSON string
is data, not a terminal command), which is exactly why the
[trust model](#trust-model) above matters regardless of output mode.

A broken output pipe (e.g. `webseek search foo --json | head -1`) exits `0`,
matching Unix convention — the reader chose to stop, that isn't a
`webseek` error.

### Exit codes

| Code | Meaning |
|---|---|
| `0` | Success — including *no results* (empty JSON is valid), and a broken output pipe |
| `1` | Runtime error (network / parse / config / blocked by egress policy) |
| `2` | CLI usage error — clap's own errors, mutually exclusive flags, and an unknown `--engine` |

In `--json`/`--jsonl` mode, a failure also gets a structured line on stdout
so it's parseable without scraping stderr:

```json
{"error": {"code": "upstream_rate_limited", "message": "...", "retry_after_ms": 30000}}
```

`error.code` is a stable machine-readable string (`blocked_by_policy`,
`response_too_large`, `unsupported_content_type`, `parse_failed`, `usage`,
...) — see `src/error.rs` for the full set.

### JSON shapes

`search` / `images` carry engine-outcome metadata alongside results, so an
agent can tell "PubMed answered" from "PubMed failed and DuckDuckGo answered
instead" — automatic fallback only ever happens within the same capability
(see [Automatic fallback](#reliability)), but it's still a materially
different answer than what was asked for:

```json
{"query":"rust","engine_requested":"duckduckgo","engine_used":"duckduckgo",
 "fallback":false,"cache_hit":false,"count":5,"results":[
  {"title":"...","url":"https://...","snippet":"..."}]}
```

`fetch`:

```json
{
  "requested_url": "https://example.com",
  "final_url": "https://example.com/",
  "status": 200,
  "content_type": "text/html",
  "title": "...",
  "chars": 12034,
  "truncated": false,
  "truncation_reasons": [],
  "source_trust": "untrusted_external_content",
  "fetched_at": 1754640000,
  "text": "..."
}
```

`requested_url`/`final_url` are reported separately (a redirect can land on a
different host). `truncated` is `true` exactly when `truncation_reasons` is
non-empty; possible reasons are `response_bytes` (the download hit its byte
cap), `max_chars` (extracted text was cut), and `line_limit` (a pathological
page's line count was capped) — reported individually because they imply
different remedies (raise `--max-chars` vs. re-fetch with `--html`).
Unsupported content (PDF, images, JSON, archives — anything that isn't
HTML/XHTML/plain text) is a typed error (`unsupported_content_type`) instead
of being lossily decoded as if it were a web page.

`images`:

```json
{"query":"cats","engine_requested":"bing","engine_used":"bing","fallback":false,
 "cache_hit":false,"count":5,"results":[
  {"title":"...","url":"https://cdn...","page_url":"https://...","width":1920,"height":1080,"format":"jpg"}],
 "downloaded":[{"url":"https://cdn.../x.jpg","ok":true,"path":"./pics/0001_x.jpg"}]}
```

Each download's outcome is reported individually (`ok`/`path`/`error`) — one
failed image never hides whether the others succeeded. Downloads are a
single streamed request per image (capped at the byte limit regardless of
`Content-Length`), and the saved format is verified against the file's own
magic bytes, not trusted from the URL extension or `Content-Type` header —
an HTML error page served at a `.jpg` URL is rejected, not saved as one.
`webseek images --download` never overwrites an existing file unless you
pass `--overwrite`.

In `--jsonl` mode, `images` emits one **tagged event** per line —
`{"type":"image_result","result":{...}}`, `{"type":"download","record":{...}}`,
`{"type":"summary",...}` — rather than mixing bare result objects with a raw
JSON array of filenames.

`fetch` (multiple URLs — a JSON **array**, input order preserved; each item
is tagged so a future `FetchResult` field can never be confused with the
error shape):

```json
[
  {"ok":true,"value":{"requested_url":"https://a","final_url":"https://a/","status":200,"...":"..."}},
  {"ok":false,"error":{"url":"https://b","message":"HTTP 404 from upstream"}}
]
```

By default the batch command exits `0` even if some URLs failed (per-URL
errors are already data in the output); pass `--fail-on-any-error` or
`--fail-if-all-error` if your pipeline needs a non-zero exit instead.

### Context-saving rules

- Snippets are single-line and capped at ~300 chars.
- `fetch` extracts the main content (readability-lite: strips nav/ads/scripts,
  scores every semantic-container candidate and every density-scored block
  and picks the best — not just the first match) and caps text at
  `--max-chars` (default 20 000, or the config's `max_chars` when the flag is
  omitted).
- `--markdown` keeps headings, lists (`-`), blockquotes (`>`), tables,
  fenced code blocks (with original indentation preserved — plain-text mode
  preserves `<pre>` indentation too), and bold/italic emphasis; links are
  resolved to absolute URLs against the page's *final* URL and only kept
  when they're http(s) (`javascript:`/`data:`/`file:` links become plain
  text, never a followable link). `--html` dumps raw HTML instead.
- Truncation is reported explicitly and by reason (see JSON shapes above) so
  agents can decide to re-fetch with a larger cap instead of trusting
  partial text.
- Page text is decoded using the response's actual charset (`Content-Type`
  header → BOM → `<meta charset>` → UTF-8 fallback), not assumed to be UTF-8
  — Shift_JIS/EUC-JP/Windows-1252 pages come back readable instead of mojibake.

### Bulk research

For multi-page research, keep the delay modest and raise the caps:

```sh
for url in $(webseek search "2026 LLM survey" --count 8 --json | jq -r .results[].url); do
  webseek fetch "$url" --max-chars 40000 --jsonl >> corpus.jsonl
done
```

Set `delay_ms = 0` in the config for batch speed (be polite: only against
endpoints that tolerate it) or `300`+ for normal use. The delay is enforced
*per origin*, immediately before each request is sent — including between
workers in a `-j N` batch fetch sharing a host, and between the internal
requests some engines make (DuckDuckGo's token fetch → JSON query, PubMed's
esearch → esummary, an engine failing over to the next one in its fallback
group).

## Reliability

- **Cache.** Responses are cached on disk (true LRU + TTL, keyed by request
  intent plus a schema version so an extraction/format change can't return
  stale-shaped data) so repeated lookups are instant and gentle on upstreams.
  `cache_ttl_secs = 0` means no expiry (matching its documented meaning — a
  previous version had this inverted). Both an entry-count cap
  (`cache_max_entries`) and a total-size cap (`cache_max_bytes`, default
  100 MiB) are enforced; a corrupt cache file or entry is discarded and
  refetched, never fatal. Writes are atomic (unique temp file + rename) and
  advisory-locked across processes so two `webseek` runs can't silently drop
  each other's updates. An engine alias (`--engine hn`) and its canonical
  name (`hackernews`) share one cache key. Inspect or clear it with
  `webseek cache info` / `webseek cache clear`. Disable per run with
  `--no-cache` (or force it on with `--cache`), or permanently with
  `cache_max_entries = 0`.
- **Automatic fallback.** If an engine is rate-limited or errors (network /
  parse / HTTP), webseek tries another engine **with the same capability** —
  DuckDuckGo ⇄ Bing for general web search, Bing ⇄ DuckDuckGo for images —
  until one succeeds, and reports both `engine_requested` and `engine_used`
  so the substitution is never silent. Vertical sources (Wikipedia, PubMed,
  crates.io, Nominatim, ...) each occupy their own capability, so a PubMed
  query that fails is reported as a PubMed failure, never quietly answered
  by a general web search instead. A successful fallback is cached under
  *both* the originally-requested and the answering engine's key, so the
  next call for the original engine doesn't repeat the failing request
  before falling back again. Disable with `--no-fallback` (or force it on
  with `--fallback`) or `fallback = false`.
- **Resilient transport.** Requests carry browser-like headers to avoid
  tripping anti-bot challenges, and transient failures (429/5xx/timeouts/
  connect errors) are retried with exponential backoff + jitter — a
  server's `Retry-After` header (seconds or an HTTP-date) is honored via the
  shared rate limiter rather than raced against. Retries never amplify load:
  a persistent failure surfaces as an error rather than looping forever, and
  only genuinely transient failure classes are retried (a certificate error
  or a malformed-request bug is not).
- **robots.txt (opt-in).** With `--respect-robots` (or `respect_robots =
  true`; `--ignore-robots` forces it back off even if config enables it)
  webseek evaluates the RFC 9309 longest-match `Allow`/`Disallow` rules,
  including `*`/`$` wildcards and the request's query string, and prefers a
  group written specifically for `webseek`'s own product token over `*` when
  both exist. Consecutive `User-agent:` lines are treated as one group per
  RFC 9309 (a wildcard group followed immediately by a specific bot's line no
  longer loses its rules), and separate groups for the same token anywhere
  else in the file are merged rather than only the first one being applied.
  Missing/unreachable robots.txt is treated as "allowed" (robots is advisory).

## Configuration

`webseek init` writes a documented `config.toml` to the platform config dir
(run `webseek config path` to see exactly where, honoring the same
resolution order as everything else below):

- Windows: `%APPDATA%\webseek\config.toml`
- Linux: `~/.config/webseek/config.toml`
- macOS: `~/Library/Application Support/webseek/config.toml`

Resolution order: `WEBSEEK_CONFIG` env var > `--config <path>` > platform
dir. An explicitly given source (env var or flag) must exist and be a
regular file — it's an error, not a silent fallback to defaults, if it
doesn't check out (a mistyped path used to be ignored quietly). Unknown keys
in the TOML file are also rejected, so a typo like `max_result` fails loudly
instead of quietly keeping the default.

```toml
engine = "duckduckgo"     # text engine: run `webseek engines` for the full list
image_engine = "bing"     # image engine: bing | duckduckgo
delay_ms = 300            # minimum pause between requests to the same host
timeout_secs = 15
user_agent = "..."        # browser-like by default
safe_search = false
lang = null               # e.g. "ja" (engine-dependent)
region = null             # BCP-47 order: "jp", "en-US", "zh-Hant-TW", ...
max_chars = 20000         # fetch text cap
max_results = 5
cache_ttl_secs = 3600     # response cache TTL (0 = no expiry)
cache_max_entries = 1000  # response cache entry cap (0 = disabled)
cache_max_bytes = 104857600  # response cache size cap in bytes (100 MiB)
fallback = true           # auto-switch to a same-capability engine on rate limit/error
respect_robots = false    # honor robots.txt before fetching
image_max_bytes = 5242880 # skip downloaded images larger than this
image_overwrite = false   # overwrite an existing file when downloading images
allow_private_network = false  # allow loopback/private/link-local/reserved destinations
allow_proxy = true        # use the system HTTP(S) proxy if configured
```

Every CLI flag that shadows a boolean config value has an explicit opposite
so you can override either direction at the command line, not just turn a
config default on:

| On | Off |
|---|---|
| `--safe` | `--no-safe` |
| `--respect-robots` | `--ignore-robots` |
| `--fallback` | `--no-fallback` |
| `--cache` | `--no-cache` |

`--json`/`--jsonl`/`--pretty`, `--verbose`/`--quiet`, and `--html`/`--markdown`
are mutually exclusive and rejected by the CLI parser (exit code 2) if
combined — there is no silent priority order to be surprised by.

## Engines & ethics

webseek deliberately mixes two kinds of source. Run `webseek engines` for the
live, machine-readable catalog.

**General web search** (broad, but scraped — can change or block):

- **DuckDuckGo** (`html.duckduckgo.com/html/`): no key, HTML scraping.
- **Bing** (`www.bing.com/search`): no key. Prefers the **RSS output**
  (`&format=rss`, parsed with a real XML parser) — a stable, structured
  format that is not behind a bot challenge — and falls back to HTML
  scraping (base64-unwrapping the `ck/a` redirect links, standard or
  URL-safe) if RSS yields nothing.
- **Bing Images** / **DuckDuckGo Images** (`i.js` with a per-request `vqd`
  token): no key.

**Stable sources** (official JSON APIs, no key, no scraping — these are what
keep working when the scraped engines get blocked). Each is a *vertical*:
great for its domain, not a replacement for general web search, and — per
the fallback design above — never silently substituted for one.

| Engine | Source | Answers |
|---|---|---|
| `wikipedia` | MediaWiki API | Encyclopedia articles (`--lang` = edition) |
| `hackernews` | Algolia HN API | Tech news & discussion (stories *and* comment hits) |
| `reddit` | Reddit public RSS | Reddit posts (rate-limit-strict) |
| `stackexchange` | Stack Exchange API | Programming Q&A |
| `openalex` | OpenAlex | Scholarly works (all fields) |
| `crossref` | CrossRef | DOI / citation metadata |
| `pubmed` | NCBI E-utilities | Biomedical literature |
| `crates` / `npm` | crates.io / npm registry | Package keyword search |
| `pypi` | PyPI JSON API | Python package lookup (exact name) |
| `nominatim` | OpenStreetMap Nominatim | Geocoding / places |

Notes: PyPI has **no** keyword-search API (its HTML search is bot-protected), so
`pypi` is an exact-name lookup, safe against path-injection (the name is a
proper URL path segment, not string-concatenated). Nominatim requires a
browser-like User-Agent (webseek sends one) and asks for low volume. OpenAlex
and PubMed will use a real contact address (`mailto`/`email`, their
"polite pool" mechanism) if you set `WEBSEEK_CONTACT_EMAIL`; otherwise it's
omitted rather than sending a fabricated one. All stable sources still honor
the global rate limiter and benefit from the cache.

The scraped endpoints can change or block aggressive use. webseek's engine
layer is a small trait plus a single descriptor registry
(`engines::TEXT_REGISTRY` / `IMAGE_REGISTRY`) precisely so sources can be
added or swapped without touching the CLI layer or drifting out of sync with
`webseek engines`'s catalog. Please respect each site's ToS, keep the
default delay, and use `--safe` when appropriate.

## MSRV

Minimum Supported Rust Version: **1.89** (`rust-version` in `Cargo.toml`),
verified in CI by checking against that exact pinned toolchain — not just
`stable` — on every push. If you need a specific pin locally:

```sh
rustup toolchain install 1.89.0
cargo +1.89.0 check --locked
```

## Development

```sh
cargo test --all-features                                     # unit + wiremock integration tests (no network needed)
cargo clippy --locked --all-targets --all-features -- -D warnings
cargo fmt --check
```

Project layout:

```
src/
  cli.rs        clap definitions (conflicts_with groups, on/off flag pairs)
  config.rs     config.toml loading / writing / path resolution
  engines/      SearchEngine + ImageEngine traits, the descriptor registry
                (mod.rs), web (DDG/Bing) + stable sources (wikipedia,
                hackernews, reddit, stackexchange, academic, packages,
                nominatim) + images (search & download)
  error.rs      typed errors with stable exit codes and machine-readable codes
  feed.rs       RSS/Atom parsing (quick-xml) shared by Bing and Reddit
  http.rs       transport layer: egress policy + rate limiter + retry +
                charset-aware decoding, wrapping every outbound request
  lib.rs        run() orchestration (cache, fallback, batch wiring, exit codes)
  models.rs     JSON contract types
  net.rs        the SSRF egress policy (scheme/IP/redirect/DNS-rebinding guards)
  output.rs     JSON / JSONL / pretty writers (data-only, sanitized stdout)
  ratelimit.rs  shared per-origin rate limiter, enforced before every send
  reader.rs     fetch + readability-lite text extraction (+ light markdown)
  batch.rs      parallel multi-URL fetch, singleflight dedup, panic-isolated
  cache.rs      on-disk LRU+TTL+byte-budget response cache (SHA-256 keys)
  region.rs     --region parsing (BCP-47 subtag order)
  robots.rs     RFC 9309 robots.txt parser + per-origin checker
  text.rs       shared text helpers (sanitize/snippet/name/truncate/strip_html)
tests/
  engines.rs    web-engine integration tests against a local mock server
  verticals.rs  stable-source engine integration tests (wiremock)
  batch.rs      batch-fetch + robots.txt integration tests (wiremock)
```

## Releases

Pushing a version tag (`v*`) starts the release workflow: a quality gate
(tag-vs-`Cargo.toml` version check, format, clippy, tests, all `--locked`),
then release builds for each platform (each binary is smoke-tested where it
isn't cross-compiled), packaged and published as a GitHub Release with a
`SHA256SUMS.txt` checksum file. Nothing is published to crates.io.

| Asset | Platform |
|---|---|
| `webseek-<tag>-x86_64-unknown-linux-gnu.tar.gz` | Linux x86_64 |
| `webseek-<tag>-aarch64-unknown-linux-gnu.tar.gz` | Linux ARM64 (cross-compiled) |
| `webseek-<tag>-x86_64-pc-windows-msvc.zip` | Windows x86_64 |
| `webseek-<tag>-x86_64-apple-darwin.tar.gz` | macOS Intel (cross-compiled) |
| `webseek-<tag>-aarch64-apple-darwin.tar.gz` | macOS Apple Silicon |

To publish a release:

```sh
git tag v0.3.0
git push origin v0.3.0
```

### Known limitations / roadmap

- **Supply-chain hardening beyond checksums** (Sigstore/cosign signing, SLSA
  provenance, SBOM generation) is not yet in place — `SHA256SUMS.txt`
  detects transport corruption but not a compromised release pipeline.
- **GitHub Actions are pinned by tag, not commit SHA.** Reasonable trust in
  `actions/*`/`dtolnay/*`/`Swatinem/*`, but not the strongest supply-chain
  posture available.
- **`cargo machete`/`cargo deny`** aren't wired into CI yet for catching
  unused or newly-risky dependencies automatically; run them locally
  occasionally.
- `--sites:` operator to restrict results to a domain.

## License

MIT — see [LICENSE](LICENSE).
