# Contributing to webseek

Thanks for your interest! webseek is a small, focused CLI and we keep the bar
high but the surface area small. This guide gets you productive fast.

## Ground rules

- **API-key-free is a hard constraint.** Anything that requires a signup,
  token, or paid tier does not belong here.
- **stdout carries data only.** Never print progress/banners to stdout; use
  stderr (`output::note`). Agents parse stdout directly.
- **The JSON contract is stable.** Adding a field is fine; renaming or
  removing one is a breaking change (bump the major/minor version and update
  the golden tests + CHANGELOG).
- **Be gentle upstream.** These are scraped endpoints, not public APIs. Keep
  the default delay, don't hammer, and prefer opt-in for anything aggressive.

## Development setup

```sh
git clone <repo> && cd webseek
cargo build
cargo test                 # unit + wiremock integration tests (no network)
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check
```

CI runs fmt + clippy(`-D warnings`) + tests on Linux/macOS/Windows, an MSRV
build at the `rust-version` declared in `Cargo.toml`, and a `cargo-audit`
security job. Please keep them green before opening a PR.

Note that the declared MSRV covers building the library and binary. The test
suite needs a newer toolchain because of a dev-dependency, which is why the
MSRV job runs `cargo build` rather than `cargo test`.

## Testing conventions

- **Parsers are pure functions** (`&str` in, typed data out) so they are
  unit-tested against captured HTML/JSON fixtures — no network.
- **Engines are integration-tested** against a local `wiremock` server
  (`tests/engines.rs`, `tests/verticals.rs`, `tests/batch.rs`).
- **The CLI itself is end-to-end tested** in `tests/cli.rs`, which runs the
  real binary. Exit codes, config resolution, output shape and pacing live in
  the wiring between components, so unit tests cannot see them — that gap is
  where most of the bugs fixed in the last release came from. A change to any
  of those belongs there.
- **Runtime rule:** the async tokio runtime is used *only* to start wiremock;
  the blocking `reqwest` client and engine/batch calls run in the sync test
  thread. (tokio ≥ 1.53 panics if a runtime is dropped inside an async context
  while a blocking client owns one.) Follow the `setup_runtime()` pattern.
- **JSON shapes are golden-tested** in `output.rs` — if you change an output
  field, update those tests.

## Adding a new engine

1. Implement the `SearchEngine` and/or `ImageEngine` trait in `src/engines/`.
2. Keep the parser a pure function with a fixture-based unit test. **Use a
   real captured response as the fixture**, not a hand-written approximation:
   Bing's redirect decoder passed its tests for a year while being broken in
   production, because the fixture omitted the prefix live Bing actually sends.
3. Add one entry to `TEXT_REGISTRY` (or `IMAGE_REGISTRY`) in
   `src/engines/mod.rs`. That is the *only* registration step — lookup,
   `config::validate_engine` and the `webseek engines` catalog all derive from
   it, and a test asserts every advertised name is a usable `--engine` value.
4. Add an integration test in `tests/engines.rs` (web) or `tests/verticals.rs`
   (stable sources) using `with_base(...)` to point the engine at the mock
   server.
5. Document it in the README "Engines & ethics" table and the CHANGELOG.

### Conventions every engine must follow

- **Snippets go through `text::normalize_snippet`.** Upstream descriptions are
  arbitrary user text; the ~300-char single-line bound is a promise webseek
  makes to agents about token cost.
- **De-duplicate before truncating** (`engines::dedupe_and_truncate`), or
  duplicates eat into the caller's `--count`.
- **Identify honestly to official APIs** with `opts.identify(...)`. Only the
  scraped endpoints get the browser-like User-Agent, and only because they
  serve challenge pages otherwise. If an API's policy asks for a contact
  address, send `contact_email` — and send nothing when it is unset rather
  than inventing a placeholder.
- **An empty result set is treated as engine failure** by the fallback chain.
  Return a typed error for a real failure so the class is preserved; do not
  paper over a broken parser with `Ok(vec![])`.
- **Build URLs with `Url::parse_with_params` or `text::encode_path_segment`**,
  never by interpolating user input into a path.
- **Every request goes through the shared `Pacer`.** Politeness is a property
  of the request stream, not of any one call site.

## Pull requests

- One focused change per PR; reference the issue it solves.
- Update `CHANGELOG.md` under `[Unreleased]`.
- New user-facing flags/config need a README mention and a test.
- Squash is fine; write a concise commit message in the imperative mood.

## Code of conduct

Be kind. We follow the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).
