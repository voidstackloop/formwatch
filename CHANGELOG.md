# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/); this
project doesn't have a release yet, so everything below is grouped under
`[Unreleased]`.

## [Unreleased]

### Added

- `formwatch init`, `formwatch test`, `formwatch monitor`, `formwatch report`
  (plain-text, `--html`, `--json`) — the whole CLI.
- Seven built-in checks: submission flow (including multi-step wizard
  walking), accessibility (via vendored axe-core), mobile usability,
  validation errors, required documents, autofill hints, input persistence.
- `--checks-dir`: run custom `*.js` checks through the same
  `page.evaluate()` mechanism the accessibility check uses, no forking
  required.
- `--history-dir`, `--json`, `--wait`, `--submit`, `--headful`, `--name`.
- A community registry/dashboard layer (`community-forms/`,
  `.github/workflows/monitor.yml`, `index.html`) — scaffolded and
  locally verified, not yet deployed.
- CI (`fmt`, `clippy -D warnings`, `test`, a build-only check on
  macOS/Windows, `cargo audit`) and a release build matrix
  (Linux/macOS/Windows binaries on a version tag).
- Full rustdoc coverage of the public API, enforced going forward via
  `#![warn(missing_docs)]`.
- Every check that comes back Fail or Warn (including a custom check)
  now carries a full-page screenshot of the moment it finished, embedded
  in `--html`/`--json` output — a report reader can see what the check
  actually saw without re-running formwatch against a possibly-
  already-changed page. A Pass carries no screenshot: it needs no
  evidence, and capturing one for every check on every run would just
  bloat history for no benefit.

### Fixed

Found via code review and two independent review agents during
development; every fix below was verified with a fixture reproducing
the bug, not just reasoned about:

- Date fields were filled with a hardcoded past date, tripping a form's
  own `min`/`max` constraints for no real reason.
- A session-timeout detector matched bare "timed out" anywhere on the
  page, false-positiving on unrelated copy like "connection timed out".
- Forcing one required field invalid used `.value = ''`, a silent no-op
  for checkboxes/radios — a required checkbox as the first field never
  actually got tested.
- With `--submit`, every check after the real submission silently ran
  against whatever page the site's confirmation landed on, not the form.
- Any single check erroring (or, later, hanging) lost every other
  check's result for that form instead of being isolated.
- An unreachable page (DNS failure, connection refused, blocked port)
  wasn't detected — Chrome's own error interstitial was silently
  treated as a real page, and the full check suite ran against it.
- URLs differing only by a hyphen vs. underscore collapsed to the same
  history directory, silently merging two different forms' history.
- Two runs completing within the same wall-clock second could clobber
  each other's history file.
- The plain-text `formwatch report` never showed "changed since
  previous run", unlike `--html`.
- `monitor.yml`'s exit-code contract meant it would never actually
  publish results, since finding a real Fail is the normal case, not a
  workflow failure.
- Every check-engine selector was unscoped, so a page with more than one
  `<form>` (e.g. a header search box) could be filled, validated, or —
  with `--submit` — actually clicked on the wrong form entirely.
- Fields marked only `aria-required` made validation checking a silent
  no-op that always reported a false Pass.
- Icon-only Next/Submit buttons (aria-label, no text) were invisible to
  the button classifier.
- A quote in an element's `id` (legal HTML) crashed a hand-built CSS
  selector instead of just not matching.
- `check_input_persistence` had the same unscoped-selector bug as
  above, missed in the first pass — a false negative, not just a false
  positive: it could report "input retained" while the real form's
  input was actually lost.
- `monitor` exited 0 on a missing or broken YAML config, contradicting
  its documented CI-safety contract.
- `formwatch init subdir/file.yml` failed if `subdir/` didn't exist yet.
- An unmaintained `async-std` runtime was compiled in for no reason —
  a transitive default feature of an unrelated dependency.
- No timeout existed anywhere in the check engine; a hung page or
  custom check would block a run (and, with concurrency, a whole
  concurrency slot) forever.
- Two formwatch processes downloading Chrome-for-Testing at the same
  time on a machine with a cold cache could corrupt each other's
  extraction ("corrupt deflate stream"). Each caller now downloads into
  its own private temp directory and atomically renames it into place;
  whichever caller loses that race discards its own copy instead of
  colliding with the winner's.
- A form (or field, or label) rendered inside an *open* shadow root —
  common in modern government-site design systems built on web
  components — was completely invisible to every check, which reported
  "No `<form>` element found" as if the page had no form at all. Every
  selector-based lookup now pierces open shadow roots. Closed shadow
  roots and cross-origin `<iframe>`s remain genuinely out of reach (real
  platform limits, not gaps), and are documented as such.

### Changed

- Wizard-step button recognition now covers English, Spanish, French,
  German, Portuguese, Arabic, and Chinese wording (not full i18n, but
  meaningfully wider than English-only), and the word list was
  de-duplicated into one source of truth used everywhere.
- Field filling now dispatches `keydown`/`keyup` in addition to
  `input`/`change`, so a Next button gated on typing (not just value
  changes) is recognized correctly.
- The fixed 700ms wait after clicking a wizard's Next button was
  replaced with polling — faster for the common case (most transitions
  are near-instant) and more tolerant of a genuinely slower one.
- `monitor` checks up to 4 forms concurrently instead of one at a time.
