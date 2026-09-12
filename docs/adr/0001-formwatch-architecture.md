# ADR-0001: formwatch architecture

**Status:** Accepted and implemented. This records the decisions *made*
at the start of the project — it isn't kept in sync with everything
that's shipped since (new checks, timeout handling, concurrency,
i18n coverage, etc.). For what's actually in the tool today, see
[CHANGELOG.md](../../CHANGELOG.md) and the main [README](../../README.md).
**Date:** 2026-09-11
**Deciders:** salihyilboga13@gmail.com

## Context

`formwatch` is a new open-source Rust CLI that tests public-service forms (e.g. `city.gov/apply`) for the things that quietly break civic services: broken submission flows, inaccessible markup, unusable mobile layouts, lost input on timeout, and confusing validation. Three commands are needed:

```
formwatch test https://city.gov/apply
formwatch monitor forms.yml
formwatch report --html
```

Constraints: single Rust binary, no required external services (no separate `chromedriver`/`geckodriver` process to install), and the check logic should lean on established tools (axe-core) rather than reinventing WCAG rule sets.

## Decision

Build on **`chromiumoxide`** (async, pure-Rust Chrome DevTools Protocol client) driving a real headless Chrome. It gives one dependency instead of a CLI + driver-binary + client triangle, and CDP exposes everything the checks need directly: DOM, network, input events, device-metrics emulation, and JS evaluation for injecting **axe-core** (vendored JS asset) for accessibility rules instead of hand-rolling WCAG logic.

Each check category is a function that takes a `chromiumoxide::Page` and returns a `CheckResult { name, status: Pass|Fail|Warn, detail }`. `test` runs all categories once and prints them; `monitor` runs `test` over every entry in a YAML config and persists results; `report --html` renders the last N persisted runs (with diffs) to a static HTML file.

## Options Considered

### Option A: chromiumoxide (CDP, pure Rust)
| Dimension | Assessment |
|-----------|------------|
| Complexity | Medium — async, but one dependency |
| Cost | Free, headless Chrome only |
| Scalability | Good — CDP sessions are cheap, can run pages concurrently |
| Team familiarity | New crate, but CDP is well-documented |

**Pros:** No external driver process; direct CDP access (network, input, emulation, JS eval) covers every check we need; async fits `monitor` running many forms concurrently.
**Cons:** Async/tokio required throughout; Chrome itself must be present (or downloaded on first run).

### Option B: fantoccini (WebDriver)
| Dimension | Assessment |
|-----------|------------|
| Complexity | Low API, but requires a running `chromedriver`/`geckodriver` |
| Cost | Free |
| Scalability | Fine |
| Team familiarity | WebDriver is a known standard |

**Pros:** Simple, standard protocol, works with any WebDriver-compatible browser.
**Cons:** Ships/launches a second binary the user must have on PATH — worse "just run the CLI" experience for a distributable tool; less direct access to CDP-only features (device metrics override, precise input dispatch) needed for mobile/keyboard checks.

### Option C: headless_chrome (sync CDP wrapper)
| Dimension | Assessment |
|-----------|------------|
| Complexity | Low — sync API, simplest to write |
| Cost | Free |
| Scalability | Poor — sync, one page at a time without manual threading |
| Team familiarity | N/A |

**Pros:** Simplest call sites, no async.
**Cons:** `monitor` needs to check many forms; sync-only makes that either slow (serial) or requires bolting threads on top anyway. Less actively maintained than chromiumoxide.

## Trade-off Analysis

The real fork is CDP vs. WebDriver. WebDriver (Option B) is the "industry standard" choice but fails the single-binary distribution goal this tool needs — a public-service auditing CLI should be `cargo install formwatch` and go, not "also install chromedriver and make sure it's on PATH." CDP options (A, C) both avoid that. Between them, async (A) costs some complexity now but avoids rewriting `monitor`'s concurrency later, so it's the one-way door worth taking upfront rather than the one that's expensive to undo.

For accessibility specifically: hand-rolling WCAG checks (contrast ratios, ARIA rules, landmark structure) is a large, easy-to-get-subtly-wrong surface. Injecting axe-core via `Page.evaluate` gets a maintained, industry-standard rule engine for free and keeps `formwatch`'s own code limited to orchestration (navigate, fill, submit, emulate, diff) — the part that's actually specific to this tool.

## Consequences

- Easier: single binary, `cargo install formwatch` works with just Chrome present; accessibility rules stay current by updating one vendored JS file, not Rust logic.
- Harder: entire check pipeline is async (tokio), so all check functions and the CLI's main loop take that on; first-run UX needs to detect/point to a Chrome install (or use `chromiumoxide`'s fetcher to download one).
- Revisit later: "unclear validation errors" and "required documents" checks start as heuristics (empty/invalid submit → check error text is non-empty, field-associated via `aria-describedby`/`aria-invalid`, and not color-only) rather than NLP-graded clarity — that's a v2 concern, not blocking v1.
- Revisit later: true session-timeout testing (waiting out a real 15–30 min server timeout) is impractical for a fast CLI check; v1 tests input persistence across reload/back-navigation and flags any `<meta http-equiv="refresh">` / JS-driven timeout warnings, with a configurable `--wait` for anyone who wants the real thing.

## Crate choices (v1)

| Concern | Crate | Why |
|---|---|---|
| CLI parsing | `clap` (derive) | Standard, zero-debate |
| Async runtime | `tokio` | Required by chromiumoxide |
| Browser control | `chromiumoxide` | See above |
| Accessibility rules | vendored `axe-core` JS, run via CDP `evaluate` | Don't reimplement WCAG |
| Config (`forms.yml`) | `serde` + `serde_yaml` | Standard serde pattern |
| Run history / diff | `serde_json`, plain struct comparison under `.formwatch/history/` | No diff library needed — structured data, `derive(PartialEq)` is enough |
| HTML report | `askama` (compile-time templates) | Report has tables/diff-highlighting; plain `format!` strings would get unreadable fast |
| Errors | `anyhow` | Binary crate, no custom error types needed |
| Terminal output | `owo-colors` or plain ANSI (decide at implementation time — not worth an ADR line) | |

## Action Items

1. [ ] Scaffold `cargo new formwatch`, add crates above
2. [ ] `formwatch test <url>`: navigate, run check categories (submission flow, a11y via axe-core, mobile emulation, input-persistence, validation-error heuristics), print pass/fail table
3. [ ] Persist run results as JSON under `.formwatch/history/<host+path-hash>/<timestamp>.json`
4. [ ] `formwatch monitor forms.yml`: parse YAML list of `{name, url}`, run `test` on each, diff against previous run's JSON, print "changed since last run"
5. [ ] `formwatch report --html`: render latest run(s) + diffs via askama to a static HTML file
6. [ ] README documenting the check categories and their known limitations (heuristic checks, no real long-timeout testing)
