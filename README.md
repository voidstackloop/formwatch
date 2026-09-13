# formwatch

[![CI](https://github.com/voidstackloop/formwatch/actions/workflows/ci.yml/badge.svg)](https://github.com/voidstackloop/formwatch/actions/workflows/ci.yml)

A CLI that tests public-service forms (permit applications, benefits forms,
license renewals) for the things that quietly break them: broken submission
flows, inaccessible markup, unusable mobile layouts, lost input, and unclear
validation errors.

```
formwatch init
formwatch test https://city.gov/apply
formwatch monitor forms.yml
formwatch report --html
```

See [CHANGELOG.md](CHANGELOG.md) for what's shipped so far, and
[docs/adr/0001-formwatch-architecture.md](docs/adr/0001-formwatch-architecture.md)
for why it's built the way it is.

## What it checks

- **Page load** — if the form doesn't load at all (unreachable, DNS
  failure, connection refused), that's reported as a single clear Fail
  rather than running — and being misled by — the rest of the checks
  against Chrome's own error page. A load is retried a couple of times
  with backoff first, so a transient network blip against a real
  third-party site doesn't get misreported as the form itself being
  down — every attempt still has to fail for that to be reported.
  Likewise, if any individual check hits a transient problem, it's
  reported as a Warn on just that check; one check failing never loses
  the results of the others.
- **Submission flow** — the form exists and is reachable. Fields are
  filled with plausible dummy data on each screen; if a step has a
  Next/Continue control (multi-step wizards are common for permit/license
  applications) formwatch advances through it and repeats, up to 8 steps,
  flagging any step that doesn't visibly change when clicked. On the final
  step, native HTML5 validation is exercised. **The real submit is never
  clicked unless you pass `--submit`** — this tool should never silently
  POST fake data into a live government system by default.
- **Accessibility** — runs [axe-core](https://github.com/dequelabs/axe-core)
  (vendored unmodified in `assets/axe.min.js`, © Deque Systems, licensed
  under MPL 2.0 — see that file's header) against the rendered page:
  labels, contrast, ARIA, landmarks.
- **Mobile usability** — emulates a 375px-wide phone viewport, flags
  horizontal overflow and tap targets under the WCAG-recommended 44x44px.
- **Validation errors** — triggers native browser validation and checks
  that invalid fields have a screen-reader-visible error message
  (`aria-describedby` pointing at real text), not just a color change.
  Custom JS-driven validation messages are only exercised with `--submit`.
- **Required documents** — checks every file-upload field has an
  accessible label and either an `accept` attribute or nearby text naming
  the expected format/size, and flags pages that mention required
  documents in prose with no matching upload field.
- **Autofill hints** — flags text fields that look like name/email/
  phone/address (by their label) but have no `autocomplete` attribute.
  axe-core checks a *present* `autocomplete` is valid; this catches the
  more common gap of a missing one, which matters for anyone leaning on
  a password manager or browser autofill (WCAG 1.3.5).
- **Input persistence** — fills a field, waits (`--wait`, default 5s), and
  checks the value is still there and whether the page started showing
  session-timeout wording. A real 15-30 minute timeout isn't waited out by
  default; pass a longer `--wait` to test that for real.

## Commands

- `formwatch init [PATH]` — scaffold a starter `forms.yml` (default:
  `./forms.yml`) plus `checks/example.js`, a working example of the
  custom-check mechanism below.
- `formwatch test <url> [--name NAME] [--submit] [--wait SECS] [--headful]
  [--checks-dir DIR] [--json]` — run every check once against a single form.
- `formwatch monitor <forms.yml>... [--checks-dir DIR] [--json]
  [--delay-ms MS]` — run `test` against every form across one or more
  YAML configs (a shell glob like `community-forms/**/*.yml` works — the
  shell expands it to multiple arguments). Checks up to 4 forms at once
  (each gets its own browser page — they can't interfere with each
  other) and prints results in config order regardless of which one
  finishes first. `--delay-ms` (default 0) is a courtesy knob for a
  large `forms.yml` against real third-party sites you don't control —
  it paces how fast new forms start, spreading the load out instead of
  firing up to 4 requests at once:
  ```yaml
  forms:
    - name: Business License Renewal
      url: https://city.gov/business-license
  ```
- `formwatch report [--html | --json] [--out FILE]` — print (or render to
  HTML/JSON) the latest run of every form formwatch has recorded, including
  what changed since each form's previous run.

Every run is stored under `.formwatch/history/` in the current directory so
`monitor` and `report` can diff against history. Override that location
with the global `--history-dir DIR` flag (e.g. to point `monitor` and
`report` at the same non-default directory, useful in CI where you want
history committed somewhere other than the gitignored default).

`test` and `monitor` exit with status 1 if any check came back FAIL (0
otherwise) — safe to gate a CI job or a cron alert on. `--json` prints the
run(s) as JSON instead of colored text for anything scripting against
formwatch's output.

Any check that comes back WARN or FAIL carries a full-page screenshot of
the moment it finished, embedded directly in `--html`/`--json` output (a
PASS carries none — it needs no evidence). The plain-text/colored output
just notes that one was captured, since a terminal can't render it.

`formwatch report` also looks across a form's *entire* recorded history
(not just the single previous run) and flags any check whose status has
flip-flopped rather than settling — a `[FLAKY]` note in plain text, a
"⚠ Flaky" badge in `--html`. A check that changed once and stayed changed
is a genuine regression or fix, not flakiness; only a check that's
genuinely unstable across several runs gets flagged.

## Custom checks

Anything specific to your forms that the built-in checks don't cover — a
required-field convention your agency uses, a locale-specific rule — can be
added without forking formwatch. Point `--checks-dir` at a folder and every
`*.js` file in it runs against the page through the same `page.evaluate()`
mechanism the built-in accessibility check uses to run axe-core:

```js
// checks/example.js
(() => {
    const hasLang = document.documentElement.hasAttribute('lang');
    return {
        status: hasLang ? 'Pass' : 'Warn',
        detail: hasLang ? 'ok' : 'No lang attribute on <html>.',
    };
})()
```

The script must evaluate — directly, or via a `Promise` (an `async () =>
{...}` IIFE works) — to `{ status: "Pass"|"Warn"|"Fail", detail }`. The
check's name in reports comes from the filename, not anything the script
returns. `formwatch init` writes a working copy of the example above.
A script that never resolves (or any built-in check that hangs on an
unusual page) times out after 20s and reports a Warn rather than
blocking the rest of the run.

## Requirements

Just a Chrome or Chromium install. If none is found on `PATH`, formwatch
downloads a headless Chrome build into `~/.cache/formwatch/chrome` on first
run — no `apt`/root step needed.

## Community dashboard

Beyond running formwatch yourself, forms listed in
[`community-forms/`](community-forms/README.md) get checked automatically
by [`.github/workflows/monitor.yml`](.github/workflows/monitor.yml) and
published to a static dashboard (`index.html`, reading
`results/index.json` — no backend, no database). See
[docs/community-design.md](docs/community-design.md) for the design and
why it's built this way. This part needs a maintainer to enable GitHub
Pages and push to a real repo before it does anything; it's scaffolded,
not deployed.

## Known limitations (v1)

- Validation-clarity and required-document checks are heuristic (field
  labeling, ARIA associations, keyword matching), not a judgment of
  whether error or instruction *wording* is actually understandable —
  that needs a human or an LLM pass, not pattern matching.
- Long real-world session timeouts aren't exercised by default (see above).
- The wizard-step classifier (Next/Continue/Submit detection) matches a
  fixed word list — English plus Spanish, French, German, Portuguese,
  Arabic, and Chinese, not full i18n coverage. A form whose step
  controls are labeled in some other language won't be walked past the
  first step — it'll report "found no Next/Continue or submit button"
  even though the form works fine for a real user in that language.
- A form rendered inside an *open* shadow root (common in modern
  government-site design systems built on web components) is found and
  checked normally — every selector-based lookup pierces open shadow
  roots. A *closed* shadow root remains genuinely invisible (that's the
  platform's own encapsulation working as designed, not a formwatch
  gap — a closed root can't be inspected from outside by any tool), and
  a cross-origin `<iframe>` remains out of reach (a separate browsing
  context this page's JS has no access to at all). Both show up as a
  loud "No `<form>` element found" rather than a misleading pass.
