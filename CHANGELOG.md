# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

## [0.3.0] - 2026-08-08

A security- and correctness-focused release driven by an external review
assessing webseek for autonomous/agent use. Several JSON output shapes
changed (see "Changed" below) — that's why this is a minor version bump
rather than a patch. Full detail is in the module-level doc comments of the
files listed; this is a summary.

### Security

- **Network egress policy (SSRF guard).** Every request now goes through a
  shared HTTP layer (`src/net.rs`, `src/http.rs`) that restricts schemes to
  `http`/`https`, rejects embedded credentials, blocks loopback/RFC1918/
  link-local/reserved destinations (including cloud metadata addresses) by
  default, re-validates every redirect hop, and validates the *resolved* IP
  at connect time via a custom DNS resolver (closing the DNS-rebinding gap).
  New `--allow-private` / `allow_private_network` and `--no-proxy` /
  `allow_proxy` escape hatches, with the proxy caveat documented in the
  README. When a proxy env var is set and would otherwise silently narrow
  the guarantee (the proxy resolves the destination, so the IP-based checks
  can't see it), webseek now prints a one-time startup note instead of
  degrading silently. `--lang` is validated against a strict subdomain-label
  shape before Wikipedia splices it into a request host.
- **`--open` scheme restriction.** Opening a search result or a fetched
  page in the browser now rejects non-http(s) schemes (`mailto:`, `file:`,
  custom handlers) unless `--allow-external-schemes` is passed.
- **True streaming byte caps.** Response bodies (API responses, page fetches,
  image downloads) are read through a `max_bytes + 1` cap regardless of
  `Content-Length` — fixing image downloads, which previously fetched the
  whole body into memory *after* a separate, discarded `HEAD`-like GET, so
  the byte limit didn't apply to chunked, mis-declared, or compressed-then-
  inflated responses.
- **Image downloads verify magic bytes**, not just the URL extension or
  `Content-Type` header, and reject SVG by default (active content). A
  download is a single streamed GET (previously two full GETs per image).
- **No silent file overwrite.** Image downloads and the cache file use
  `create_new`/an atomic same-directory rename instead of an unconditional
  `write`, so a symlink planted at a predictable filename can't redirect a
  write to an unintended target; `--overwrite` opts back in explicitly.
  Cache files are written `0600` on Unix.
- **Terminal control-character sanitization.** `--pretty` output strips raw
  C0/C1 control characters and bidi-override characters from titles, URLs,
  and snippets — including ones a numeric HTML entity (`&#27;`) can decode
  to — before they reach a terminal, preventing ANSI/OSC-based screen or
  title-bar spoofing from page content.
- **Installers now perform an atomic same-directory install** (`install.sh`)
  instead of a `mv` that can silently fall back to non-atomic copy+delete
  across filesystems.

### Fixed

- **`max_results`/`max_chars` config values are no longer ignored.** The CLI
  flags are now `Option<usize>`, falling back to config when omitted; they
  used to always default to `5`/`20000` regardless of config.
- **`cache_ttl_secs = 0` now actually means "no expiry"** as documented —
  the condition was inverted, so a TTL-0 entry expired immediately.
- **The response cache is now really LRU**, not FIFO — a cache hit refreshes
  recency. Cache keys are also now schema-versioned and built by
  JSON-encoding the key parts (not joining with a separator byte that user
  input could itself contain), locked across processes during save, and
  bounded by both entry count and total size (`cache_max_bytes`).
- **An engine alias and its canonical name share one cache key**
  (`--engine hn` now hits the same cache entries as `--engine hackernews`).
- **A successful engine fallback is cached under the originally-requested
  engine's key too**, with the answering engine recorded, so the next call
  doesn't repeat the failing request before falling back again.
- **Explicitly requested vertical engines no longer silently fall back to a
  different kind of source.** Automatic fallback is now scoped to engines
  sharing the same `Capability` (general web search, or images); a Wikipedia/
  PubMed/crates.io/Nominatim/... failure is reported as that engine's
  failure, never quietly answered by DuckDuckGo instead.
- **JSON API parsers distinguish "zero results" from "couldn't parse."**
  Wikipedia, Hacker News, Stack Exchange, Nominatim, OpenAlex, CrossRef,
  PubMed, crates.io, npm, and PyPI all now return a typed parse error (or
  surface an API error object) instead of silently returning an empty list
  on a deserialize failure or unexpected shape.
- **`truncated` now reflects every cap that actually fired**, reported as
  `truncation_reasons` (`response_bytes`/`max_chars`/`line_limit`) — it used
  to only track the `max_chars` cap.
- **Fixed the DuckDuckGo→Bing→DuckDuckGo redirect decoding gaps**: results
  are deduped before truncation (not after, which could under-count),
  protocol-relative links get a real scheme, and unsafe/invalid schemes are
  dropped instead of passed through. Bing's redirect-host check is now an
  exact match (`bing.com` or `*.bing.com`), not `ends_with("bing.com")`
  (which also matched `evilbing.com`), and its base64 decoding tries the
  URL-safe alphabet as a fallback.
- **robots.txt**: consecutive `User-agent:` lines are now treated as one
  group per RFC 9309 (previously, `User-agent: *` immediately followed by a
  specific bot's line lost the wildcard rules); *non*-consecutive groups for
  the same token are now merged too (a robots.txt with two separate
  `User-agent: *` blocks — common when one is appended by a plugin — used to
  only apply the first one found, silently ignoring rules in the second);
  inline `#` comments are stripped before parsing; `Allow`/`Disallow` now
  support `*`/`$` wildcards and are evaluated against the request's query
  string too; a group written for webseek's own product token takes priority
  over `*`; robots.txt fetches are capped at 500 KiB; and the per-origin lock
  is no longer held during the network fetch (a slow robots.txt for one host
  no longer stalls every other host).
- **Batch fetch**: duplicate input URLs are now fetched only once
  (singleflight) instead of racing two workers on a cache miss; a corrupt
  cache entry is dropped and refetched instead of surfacing as a batch
  error; robots.txt is now checked before the cache (matching single-URL
  fetch, so a page disallowed after being cached is no longer served stale);
  a panic while processing one URL is caught and reported as that URL's
  error instead of risking the whole batch.
- **`--jobs` is capped** (clap-enforced 1..=64, further clamped to the URL
  count at runtime) instead of accepting an unbounded worker count.
- **Rate limiting moved to *before* each send**, shared across every request
  path (engines, robots.txt, image downloads, batch workers) instead of a
  `sleep` after the whole command finished — which protected nothing, since
  the process exits right after. `Retry-After` (seconds or HTTP-date) is now
  honored by feeding the limiter directly. `--quiet` no longer affects
  network timing (it used to disable the old post-command sleep too).
- **`safe_search`/`respect_robots` can now be forced off from the CLI** even
  when config enables them (`--no-safe`, `--ignore-robots`), and the robots-
  block error message now references a flag that actually exists.
- **Reader/extraction fixes**: the container picker now scores every
  semantic-container candidate and every density-scored block instead of
  taking the first semantic match (so a sidebar `.content` div no longer
  wins over a later `<article>`); a parent's density score no longer counts
  text inside an excluded (ad/comment) child; noise-class detection now
  catches compound classes like `cookie-banner`/`comments-section`, not just
  exact tokens; `<header>` is no longer unconditionally excluded (site-chrome
  headers are now caught by class/id instead, so an in-article header with a
  byline survives); `<pre>` content keeps its original indentation instead
  of being whitespace-collapsed; `strip_html` no longer treats an escaped
  `&lt;`/`&gt;` comparison as the start of a real tag, and drops `<script>`
  content instead of including its source text; page bodies are decoded
  using the actual response charset instead of being assumed UTF-8.
- **`--markdown` now actually keeps lists (`-`), blockquotes (`>`), table
  cells, and code blocks** (previously documented but unimplemented), and
  resolves links to absolute URLs against the final (post-redirect) URL,
  rejecting non-http(s) link targets instead of emitting a followable
  `javascript:`/`data:`/`file:` link.
- **Wikipedia** article URLs are built from proper URL path segments instead
  of string concatenation, so a title containing `/`, `?`, or `#` can no
  longer reshape the request path/query.
- **PyPI** package names are placed as a real URL path segment instead of
  being string-concatenated into the request URL.
- **Images**: `u64` dimensions are dropped (not silently wrapped) when they
  don't fit in `u32`.
- **`ImagesEngine`/`SearchEngine`** now take `&Http` instead of a raw
  `reqwest::Client`, so no engine can bypass the egress policy or rate
  limiter by holding its own client.
- **Unknown `--config`/`WEBSEEK_CONFIG` paths, and directories passed as a
  config path, are now errors** instead of silently falling back to
  defaults; unknown TOML keys are rejected (`deny_unknown_fields`); `webseek
  init` now honors `WEBSEEK_CONFIG` (it used to ignore it and always write to
  the platform default path).
- **Mutually exclusive CLI flags** (`--json`/`--jsonl`/`--pretty`,
  `--verbose`/`--quiet`, `--html`/`--markdown`, `--safe`/`--no-safe`, ...)
  are now rejected by the parser itself (exit code 2), instead of being
  silently resolved by an undocumented priority order at runtime.

### Changed

- **JSON schema changes** (why this is 0.3.0, not 0.2.1):
  - `fetch`'s single `url` field is replaced by `requested_url` and
    `final_url`; new fields `status`, `content_type`, `truncation_reasons`,
    `source_trust`, `fetched_at`.
  - `search`/`images` documents gained `engine_requested`, `engine_used`,
    `fallback`, `cache_hit`.
  - Batch `fetch` items are now tagged (`{"ok":true,"value":{...}}` /
    `{"ok":false,"error":{"url":...,"message":...}}`) instead of an
    untagged union distinguished by shape.
  - `images --jsonl` now emits typed events (`image_result`/`download`/
    `summary`) instead of mixing bare result objects with a raw JSON array
    of downloaded filenames (which was never valid per the "one object per
    line" contract to begin with).
  - `search`/`images --jsonl` now always end with a `{"type":"summary",...}`
    line, so a genuine zero-result run is distinguishable from a crash
    before any output.
- **MSRV raised to 1.89** (from a previously-declared 1.75 that no longer
  matched the locked dependency graph — clap 4.6 alone requires 1.85; 1.89 is
  additionally required by this release's own use of `std::fs::File::lock`).
  Verified in CI against that exact pinned toolchain, not just `stable`.
- **`--region` now parses BCP-47 subtag order** (`language[-script][-region]`,
  e.g. `en-US`, `zh-Hant-TW`), replacing a heuristic that guessed direction
  from a country-code table and mis-parsed inputs like `fr-CA` (a valid
  country code that is coincidentally also a language code).
- Default User-Agent no longer hardcodes a stale `webseek/0.1` suffix,
  and `Accept-Language` follows the configured `lang` instead of always
  being `en-US`.
- `unknown engine`/`unknown image engine` and other CLI-usage-shaped errors
  now exit with code 2 (usage error) instead of 1.
- Engine registration is now a single descriptor list per kind
  (`engines::TEXT_REGISTRY` / `IMAGE_REGISTRY`) generating `engine_by_name`,
  `validate_engine`, the `webseek engines` catalog, and fallback grouping —
  previously spread across four independently-maintained places that could
  (and did) drift, e.g. the catalog listing image engines under a name
  (`"bing (images)"`) that `--engine` didn't actually accept.

### Added

- `webseek cache info` / `webseek cache clear` subcommands.
- `webseek config path` subcommand (prints the resolved config file path).
- On/off pairs for every config-shadowing boolean flag: `--safe`/`--no-safe`,
  `--respect-robots`/`--ignore-robots`, `--fallback`/`--no-fallback`,
  `--cache`/`--no-cache`.
- `--color=always|auto|never` for `--pretty` output; `NO_COLOR` is honored.
- `--fail-on-any-error` / `--fail-if-all-error` for batch `fetch`.
- `--overwrite` for `webseek images --download`.
- Linux ARM64 (`aarch64-unknown-linux-gnu`) added to the Release build
  matrix, matching what the installers and README already claimed to
  support.
- CI: a pinned-toolchain MSRV job (in addition to `stable`); `--locked`
  used consistently in PR/main CI, matching the Release workflow.
- Release workflow: a tag-vs-`Cargo.toml`-version check gate;
  `contents: write` scoped to only the publish job (build/quality are
  `contents: read`); a `--version`/catalog smoke test of each
  non-cross-compiled release binary.
- One-liner installers: `install.sh` (Linux/macOS) and `install.ps1`
  (Windows) detect the platform, download the latest release, verify the
  SHA-256 checksum, and install `webseek` onto the PATH.
- Tag-driven GitHub Release automation. Pushing a `v*` tag now validates the
  project, builds Linux x86_64/ARM64, Windows x86_64, macOS Intel
  (cross-compiled), and macOS Apple Silicon binaries, and attaches
  checksummed archives to a GitHub Release.

## [0.2.0]

### Added

- **Response cache.** On-disk LRU+TTL cache (`cache_ttl_secs`,
  `cache_max_entries`) keyed by request intent (SHA-256); atomic writes,
  corrupt-file safe. Disable with `--no-cache` or `cache_max_entries = 0`.
- **Automatic engine fallback.** When an engine is rate-limited or fails
  (network/parse/HTTP), webseek tries the remaining engines in order until one
  succeeds. Opt out with `--no-fallback` or `fallback = false`.
- **Transport hardening (`http` module).** Browser-like default headers
  (Accept/Accept-Language/sec-ch-ua) to avoid tripping anti-bot challenges,
  plus polite retry with exponential backoff + jitter on 202/429/5xx/network
  errors. Engine-agnostic and dependency-light (no `rand`).
- **Bing via RSS.** Bing text search now prefers the stable, structured RSS
  endpoint (`&format=rss`) — which is not behind a bot challenge — and falls
  back to HTML scraping only if RSS yields nothing.
- **Stable source engines (no key, official JSON APIs).** New `--engine`
  options that don't depend on fragile HTML scraping:
  - `wikipedia` — encyclopedia articles (honors `--lang` for language editions)
  - `hackernews` (alias `hn`) — Hacker News stories/comments
  - `reddit` — Reddit posts via the public RSS/Atom feed (no key;
    rate-limit-strict, best for occasional queries)
  - `stackexchange` (alias `stackoverflow`) — Stack Overflow Q&A
  - `openalex`, `crossref`, `pubmed` — scholarly works / DOI metadata / biomed
  - `crates` (alias `crates.io`), `npm` — package keyword search
  - `pypi` — Python package lookup by exact name (PyPI has no keyword API)
  - `nominatim` (alias `osm`) — OpenStreetMap geocoding
- **`webseek engines` subcommand.** Machine-readable catalog (JSON/JSONL/pretty)
  of every engine with its kind, description, and an example — so an agent can
  discover which source fits a query.
- **Batch fetch.** `webseek fetch URL...` accepts multiple URLs, fetched in
  parallel (`-j/--jobs`), printed as one JSON array preserving input order;
  per-URL failures become error items instead of aborting the batch.
- **robots.txt support (opt-in).** `--respect-robots` / `respect_robots = true`
  checks the wildcard user-agent group (RFC 9309 longest-prefix matching)
  before fetching. Lookup failures are advisory (allowed).
- **`--max-bytes`** for image downloads (default `image_max_bytes`, 5 MiB):
  oversized files are skipped.
- **`--ua`** global flag to override the User-Agent header.
- **Region normalization.** `--region` accepts flexible input (`jp`, `en-us`,
  `EN_US`) and is translated to each engine's expected format (DuckDuckGo
  `kl=us-en`, Bing `cc=us` + `setlang`).
- **JSON contract golden tests** and **batch/robots integration tests**
  (wiremock, no network).
- CI: `cargo-audit` security job.

### Changed

- Text helpers (`normalize_snippet`, `sanitize_name`, `truncate_chars`) moved
  to a shared `text` module.
- Output writers gained `*_to(Write, ...)` variants for testability.

### Fixed

- robots.txt origin now preserves explicit ports (e.g. `http://host:8080`).
- Batch fetch no longer deadlocks after the worker pool finishes.

## [0.1.0]

### Added

- Initial release.
- `search` subcommand: DuckDuckGo and Bing text search (no API key).
- `images` subcommand: Bing Images and DuckDuckGo Images search + download.
- `fetch` subcommand: page fetch with boilerplate-free text extraction
  (readability-lite), optional light-markdown and raw-HTML output.
- `init` subcommand: writes a documented default `config.toml`.
- Machine-first output contract: JSON on non-TTY stdout, pretty text on TTY,
  `--json` / `--jsonl` / `--pretty` overrides, data-only stdout.
- Rate-limit awareness: polite inter-request delay, configurable, plus clear
  errors for 202/403/429 responses.
- CI: 3-OS matrix (Linux/macOS/Windows), fmt + clippy(-D warnings) + tests.
- Test suite: parser unit tests with fixtures, engine integration tests
  against a local mock server (wiremock, no network).
