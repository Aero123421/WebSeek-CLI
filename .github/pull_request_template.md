## What this changes

<!-- One or two sentences. Link the issue if there is one. -->

## Why

<!-- What was wrong, or what this makes possible. -->

## Checklist

- [ ] `cargo fmt --check`
- [ ] `cargo clippy --all-targets --all-features -- -D warnings`
- [ ] `cargo test`
- [ ] Tests cover the change (a bug fix should come with a test that fails
      without it)
- [ ] `CHANGELOG.md` updated under `[Unreleased]`
- [ ] README updated if a user-facing flag, config key or output field changed

## Output contract

- [ ] No JSON field was renamed or removed (adding one is fine)
- [ ] stdout still carries data only; notes go to stderr

## Upstream politeness

<!-- Delete if not applicable. -->

- [ ] No new request is made without going through the shared pacer
- [ ] Official APIs are identified honestly (`SearchOpts::identify`), scraped
      endpoints keep the browser-like agent
