# Contributing

## Building

```
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
cargo fmt
```

CI runs `fmt --check`, `clippy -D warnings`, and `test` on Linux, a
build-only check on macOS/Windows, and `cargo audit` against advertised
security advisories. Run the four commands above (plus `cargo audit` if
you touched `Cargo.toml`) before opening a PR so CI doesn't surprise you.

When adding a dependency, set `default-features = false` unless you
specifically want its defaults — chromiumoxide's defaults used to pull in
an entire unmaintained async-std runtime alongside the tokio one this
project actually uses, purely because nothing disabled it.

The unit tests (in `history.rs` and `checks.rs`) are pure logic and don't
need Chrome. `tests/checks_test.rs` and `tests/runner_test.rs` launch a
real headless Chrome and assert on the results — that's what `cargo test`
running slower than instant is; it's exercising a browser, not hanging.

If you don't have system Chrome/Chromium installed, run something once to
warm formwatch's own download cache (`cargo run -- test
"file://$PWD/fixtures/test-form.html"`) *before* running `cargo test` —
`cargo test`'s default parallelism means multiple tests can otherwise race
to populate a cold cache at once and corrupt each other's download (see
the `ponytail:` note on `fetch_chrome` in `browser.rs`). This doesn't
affect real `formwatch` runs, which only ever launch the browser once per
process.

When you add or change a check, extend a fixture (or add a new one) with
a case that would only pass/fail correctly if your change works, and
assert on it in `tests/checks_test.rs` — don't just eyeball
`cargo run -- test "file://$PWD/fixtures/..."` output and move on.
`fixtures/test-form.html` already has several deliberate bugs (missing
`alt`, an unlabeled field, tiny tap targets, an unlabeled file upload);
`fixtures/multi-step-form.html` is a 3-step wizard for the submission-flow
walk logic.

## Adding a built-in check

Built-in checks live in `checks.rs` as `async fn check_*(page: &Page) ->
Result<CheckResult>`, added to `run_all`. If what you need is specific to
one form or organization rather than broadly useful, prefer a
[custom check](README.md#custom-checks) instead — it needs no PR at all.

## Documentation

Every `pub` item in `src/` needs a `///` doc comment — `lib.rs` sets
`#![warn(missing_docs)]`, and `clippy -D warnings` (already required
above) turns that into a hard error, so an undocumented public item
fails CI, not just a style nit. `cargo doc --no-deps --open` to read it
rendered. If your change is user-visible, add a line to
[CHANGELOG.md](CHANGELOG.md)'s `[Unreleased]` section too.

## Scope

See [docs/community-design.md](docs/community-design.md) for where this
is headed (prebuilt binaries, a shared form registry, a public dashboard)
and the trade-offs behind those decisions. The short version: this is an
unfunded civic-tech tool, so PRs that add operational burden (a server, a
database, a new required paid service) are a harder sell than ones that
don't.
