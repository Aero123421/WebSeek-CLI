# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Tag-driven GitHub Release automation. Pushing a `v*` tag now validates the
  project, builds Linux x86_64, Windows x86_64, macOS Intel (cross-compiled),
  and macOS Apple Silicon binaries, and attaches checksummed archives to a
  GitHub Release.

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
