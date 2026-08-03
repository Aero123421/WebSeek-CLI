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

CI runs fmt + clippy(`-D warnings`) + tests on Linux/macOS/Windows, plus a
`cargo-audit` security job. Please keep all four green before opening a PR.

## Testing conventions

- **Parsers are pure functions** (`&str` in, typed data out) so they are
  unit-tested against captured HTML/JSON fixtures — no network.
- **Engines are integration-tested** against a local `wiremock` server
  (`tests/engines.rs`, `tests/batch.rs`).
- **Runtime rule:** the async tokio runtime is used *only* to start wiremock;
  the blocking `reqwest` client and engine/batch calls run in the sync test
  thread. (tokio ≥ 1.53 panics if a runtime is dropped inside an async context
  while a blocking client owns one.) Follow the `setup_runtime()` pattern.
- **JSON shapes are golden-tested** in `output.rs` — if you change an output
  field, update those tests.

## Adding a new engine

1. Implement the `SearchEngine` and/or `ImageEngine` trait in `src/engines/`.
2. Keep the parser a pure function with a fixture-based unit test.
3. Register it in `engine_by_name` / `image_engine_by_name` and in
   `config::validate_engine` / `validate_image_engine`.
4. Add an integration test in `tests/engines.rs` using `with_base(...)` to
   point the engine at the mock server.
5. Document it in the README "Engines & ethics" section and the CHANGELOG.

## Pull requests

- One focused change per PR; reference the issue it solves.
- Update `CHANGELOG.md` under `[Unreleased]`.
- New user-facing flags/config need a README mention and a test.
- Squash is fine; write a concise commit message in the imperative mood.

## Code of conduct

Be kind. We follow the [Rust Code of Conduct](https://www.rust-lang.org/policies/code-of-conduct).
