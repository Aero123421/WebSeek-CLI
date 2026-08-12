# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- One-liner installers: `install.sh` (Linux/macOS) and `install.ps1`
  (Windows) detect the platform, download the latest release, verify the
  SHA-256 checksum, and install `webseek` onto the PATH.
- Tag-driven GitHub Release automation. Pushing a `v*` tag now validates the
  project, builds Linux x86_64, Windows x86_64, macOS Intel (cross-compiled),
  and macOS Apple Silicon binaries, and attaches checksummed archives to a
  GitHub Release.
- **`webseek completions <shell>`** — bash, zsh, fish, PowerShell, elvish.
- **`--no-respect-robots`** to override `respect_robots = true` from the
  config for a single run. The robots error message already told users to do
  this; the flag did not exist.
- **`--array`** on `fetch`, forcing the JSON array shape even for one URL, so
  an agent can parse one shape regardless of how many URLs it passed.
- **`--color auto|always|never`** plus `NO_COLOR` support.
- **`contact_email`** config key. OpenAlex/CrossRef `mailto` and the NCBI
  `email` parameter are sent only when it is set.
- **Character-encoding support.** Bodies are decoded via BOM, `Content-Type`
  charset or `<meta charset>`; Shift_JIS and EUC-JP pages no longer decode to
  replacement characters.
- **`kind` field on batch error items** — a stable slug (`http`, `network`,
  `parse`, `rate_limited`, `robots`, `config`, `no_results`) so agents can
  branch on the failure class without parsing prose.
- **`aliases`, `command` and `fallback` fields** on `webseek engines` output.
- End-to-end CLI test suite (`tests/cli.rs`) covering exit codes, config
  resolution, output shapes, robots override, pipes, pacing and caching — the
  layer that previously had no tests at all.
- Behavioural coverage for the fallback chain itself, the cache-key builders,
  concurrent cache writes, the browser header set on the wire, API
  self-identification and `Retry-After`. Each was verified by mutation: revert
  the fix and the test fails. Two pre-existing tests were doing nothing —
  `keys_differ_by_options` never called the real key builders, and the
  transient-500 retry test passed without any retry occurring, because
  wiremock serves the *first* matching mock and the failure was mounted
  second.
- CI: an MSRV job that builds with the declared `rust-version`, a weekly
  scheduled `cargo-audit` run, and `--locked` on every cargo invocation.
- `SECURITY.md`, issue templates and a pull-request template.

### Fixed

- **`max_chars` and `max_results` in `config.toml` were ignored.** The CLI
  defaults always won, because a clap `default_value_t` cannot be told apart
  from a value the user typed. Both options are now optional.
- **`--quiet` disabled the inter-request delay.** A logging flag silently
  turned off rate-limit protection.
- **The delay was spent after the last request**, adding dead time to every
  invocation without protecting any upstream. Pacing now applies *between*
  requests, and batch workers share one limiter, so `-j 8` no longer issues
  requests eight times faster than `-j 1`.
- **`cache_ttl_secs = 0` disabled the cache** instead of meaning "never
  expire" as documented in both the README and the field's own comment.
- **A closed pipe was reported as an error.** `webseek fetch … | head` now
  exits `0`, and errors print via `Display` instead of a `Debug` dump.
- **Results were truncated before de-duplication**, so duplicates ate into
  `--count` and fewer results were returned than requested (five call sites).
- **Six engines skipped snippet normalisation**, so the documented ~300-char
  single-line snippet bound did not hold for registry descriptions.
- **Empty result sets did not trigger fallback.** A scraper whose selectors
  stop matching returns `Ok([])`, which is the most common way these engines
  break; the headline reliability feature never covered it.
- **Verticals fell back to general web search.** Asking `pubmed` for citations
  could return DuckDuckGo results under a different `engine` value. Only web
  engines are interchangeable now.
- **The search cache never hit after a fallback**, because entries were stored
  under the engine that answered and looked up under the one requested. The
  responding engine is now stored with the entry and reported on a hit.
- **Bing's `ck/a` redirect decoding was broken** for real-world links, which
  are `a1`-prefixed and URL-safe base64; the raw tracking URL leaked into
  results. The unit fixture had never contained a real value.
- **Searching for "captcha" reported a rate limit.** Challenge detection
  scanned body prose for keywords; it is now markup-based with a size gate.
- **Wikipedia and PyPI URLs were built by interpolation.** The article "C#"
  linked to "C", "Who's Next?" grew an empty query, and a PyPI query could
  walk to unrelated paths on pypi.org.
- **Deeply nested HTML could hang the process.** Parsing is quadratic in
  nesting depth and `--timeout` only bounds the request, so a 2 MB crafted
  page stalled webseek for minutes. Over-deep documents are now rejected.
- **Every fetch parsed the HTML twice** — once for the body, once for the
  title.
- **The line cap dropped content while reporting `truncated: false`**, and
  `--html` ignored `--max-chars` entirely. All three caps now report.
- **robots.txt failed open on the strictest input.** `*`/`$` patterns were
  unsupported, so `Disallow: /*` — a common way to say "no bots" — matched
  nothing. Consecutive `User-agent` lines now form one group per RFC 9309,
  and the query string participates in matching.
- **Image downloads made two full requests per file**, applied no pacing,
  buffered entire bodies before checking the size limit, and aborted the whole
  run if one filename was too long for the filesystem.
- **DuckDuckGo ad filtering matched the substring "ad"**, so a class token
  like `shadow` discarded a legitimate result. Safe search sent `p=1` rather
  than DuckDuckGo's `kp`.
- Bing rate limits surfaced as generic HTTP errors while DuckDuckGo's became
  `RateLimited`; Bing results without a link were emitted with an empty `url`.
- `images --jsonl` emitted a bare JSON array line among the objects.
- `search --open N` produced no stdout at all, even with `--json`.
- Single-URL and batch fetches disagreed on robots/cache ordering, message
  wording and exit code for the same condition; they now share one code path.
- Cache warnings bypassed `--quiet`; the cache temp file was not
  process-unique, so concurrent runs could rename a torn file into place.
- `strip_tags` ate text after a bare `<` (`if x < y`); `extract_tag("link")`
  matched `<linkedin>`; metadata joins left dangling `·` separators.
- The default User-Agent advertised `webseek/0.1` regardless of the version.
- **Deeply nested HTML could abort the process.** The nesting guard only
  recognised 48 hard-coded tag names — `<big>`, `<dfn>`, custom elements and
  `<div/>` all slipped past it — and the recursive renderer then overflowed
  the stack. A stack overflow does not unwind, so the batch worker's
  `catch_unwind` could not contain it: one hostile URL killed the whole run.
  The renderer is now iterative, and the guard is a small tokenizer that also
  stops counting `<` inside `<script>` and quoted attribute values (which used
  to *reject* ordinary minified pages).
- **`Crawl-delay` did not end a robots.txt user-agent group**, so a following
  `User-agent: SomeBot` / `Disallow: /` was applied to us and blocked entire
  sites. `Disallow: $` alone also matched every path.
- **Bing redirect payloads containing `+` still leaked the tracking URL**:
  reading the `u` parameter with `query_pairs()` form-decodes `+` to a space.
- **`<meta http-equiv="Content-Type" content="…charset=…">` was ignored**,
  because the word "charset" inside that quoted value matched first and
  yielded a label with a stray quote.
- **A rate limit could be reported as "no results".** When one engine came
  back empty and another failed, the empty answer won and the command exited
  0 with `count: 0`, hiding the failure.
- `--ua` was a no-op for the eleven API-backed engines, which overrode it with
  webseek's own agent.
- The `webseek init` template omitted `contact_email`, `lang` and `region`
  entirely (serde drops unset options), and the README's config block used
  `null`, which is not valid TOML — so neither the generated file nor the
  documented one was a working example.

### Changed

- **Official APIs are no longer sent a fake browser User-Agent.** Wikipedia,
  Hacker News, Stack Exchange, OpenAlex, CrossRef, PubMed, crates.io, npm,
  PyPI, Nominatim and Reddit now receive `webseek/<version>` with a link to
  the repository. Nominatim's usage policy explicitly forbids browser
  impersonation, and the module documented the opposite. Scraped endpoints
  keep the browser-like agent.
- **The placeholder OpenAlex `mailto` (`webseek@example.org`) was removed.**
  Claiming a polite-pool contact that cannot receive mail is worse than
  sending none.
- `--config` now takes precedence over `WEBSEEK_CONFIG` (an explicit flag
  should beat the ambient environment), and a missing explicit path is an
  error instead of a silent fall back to defaults.
- Unknown keys in `config.toml` are rejected rather than ignored.
- `--count` above 50 and `--jobs` outside 1–64 are usage errors instead of
  silent clamps; `--json`/`--jsonl`/`--pretty` conflict instead of one winning.
- `Retry-After` is honoured in preference to the built-in backoff.
- The engine registry is a single table; lookup, config validation and the
  `engines` catalog all derive from it. Catalog entries previously included
  unusable names such as `"bing (images)"`.
- Batch item errors carry `kind`; a panic while parsing one page degrades to
  one error item instead of aborting the batch.
- Declared MSRV corrected to 1.86 and verified in CI. The previous 1.75 was
  unchecked and had not been accurate for some time.
- Expired cache entries are pruned on load; eviction is documented as FIFO
  rather than LRU, which is what it always was.

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

[Unreleased]: https://github.com/Aero123421/WebSeek-CLI/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Aero123421/WebSeek-CLI/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/Aero123421/WebSeek-CLI/releases/tag/v0.1.0
