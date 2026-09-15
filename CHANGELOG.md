# Changelog

Format follows [Keep a Changelog](https://keepachangelog.com/); this
project doesn't have a release yet, so everything below is grouped under
`[Unreleased]`.

## [Unreleased]

### Security

- An authorized-use notice now ships with the tool and is surfaced
  everywhere it matters: `formwatch legal` prints it, `docs/legal.md`
  carries the long form, `NOTICE` and the README cover it, and the HTML
  report and community dashboard carry a footer. `test`/`monitor` print a
  one-line reminder when the notice hasn't been acknowledged, and
  `--submit` (a real POST to a live system) now **refuses to run** until
  acknowledged via `--accept-terms`, `accept_terms: true` in config, or
  `FORMWATCH_ACCEPT_TERMS=1`.
- New privacy and network controls: `--no-screenshots` (omit captured
  page content, which can contain personal data), `--proxy` and
  `--insecure` for corporate proxies/internal test hosts, `--no-sandbox`
  for containers, and per-host request pacing (`--per-host-delay-ms`)
  plus an append-only, screenshot-free JSON audit log (`--audit-log`).

### Performance

- **`prune` no longer opens or parses any run file.** Same anti-pattern
  as `all_known_forms` below, in the one command whose entire job is
  handling a *lot* of accumulated history: `history::prune` read and
  fully deserialized every run's JSON just to sort by `run.timestamp`,
  which `save_run` already writes into the filename. Retention
  decisions are now made from filenames alone — `prune` never touches a
  run's content at all, so a deployment that's been running `monitor`
  nightly for months prunes exactly as fast as one just getting
  started.
- **`all_known_forms` no longer parses every historical run to find the
  newest one.** `history::all_known_forms` — behind `formwatch report`,
  `serve`'s `/metrics`/`/api/forms`, and `baseline --write` — picked each
  form's newest run by fully parsing *every* run file in that form's
  directory and keeping the max timestamp, discarding everything else.
  Since `save_run` already encodes the timestamp in the filename
  (`{ts}.json` / `{ts}-{suffix}.json`), the winner can be picked by
  comparing filenames and only that one file needs to be read and
  parsed — a form directory with months of accumulated history now costs
  the same as one with a single run. Also fixed a latent robustness gap
  this exposed: a single corrupted/truncated old run file used to fail
  `all_known_forms` (and therefore `report`/`serve`/`baseline`) for that
  entire form; it's now simply never read. `report::build` also had its
  own copy of the `previous_run` find-predecessor snippet — missed in
  the fix below — now uses the shared helper too.
- **History loaded once per invocation, not three times per form.**
  `print_run` (the CLI diff/flakiness display), `notify::regressions_from_history`
  (`--webhook-url`), and `issues::events_from_history`
  (`--github-issues-repo`) each independently called `history::load_runs`
  for the same URL — a full directory scan plus a parse of every
  historical run, screenshots included. With both integrations enabled
  (the README's own documented full CI setup), that was 3 full history
  reloads per form, growing with however much history has accumulated.
  `main::load_prior_by_url` now loads each URL's history once per
  invocation and shares it; `notify::regressions_from_runs` and
  `issues::events_from_runs` (renamed, since they no longer touch disk
  themselves) take that pre-loaded map instead. Also collapsed the
  `prior.iter().rev().find(|r| r.timestamp < run.timestamp)`
  "find the predecessor" snippet — copy-pasted identically in all three
  places — into one `history::previous_run` helper.
- **LLM client reuse across a whole `monitor` run.** `--llm` semantic
  checks used to build a brand-new HTTP client (`LlmClient::new`) for
  every single form, discarding its connection pool immediately after —
  so a registry of N forms paid N separate DNS/TCP/TLS handshakes to the
  same provider host (openai.com, anthropic.com, ...) even though every
  form talks to the identical endpoint. The client is now built once per
  `test`/`monitor` invocation and passed by reference into every
  concurrent form task (`runner::run_one_with`'s new `llm_client`
  parameter), so the connection pool is actually shared. Client-build
  failure (rare) is now logged once instead of once per form, and still
  degrades every form's LLM checks to a `Warn`, never a dropped run.

### Added

- **GitHub Issues auto-tracking** (`--github-issues-repo owner/repo`,
  `github_issues.*` config, `FORMWATCH_GITHUB_ISSUES_REPO`/
  `FORMWATCH_GITHUB_ISSUES_TOKEN`): a check going to Fail opens a
  tracking issue, a check recovering to Pass comments and closes it, and
  Warn never touches issue lifecycle either way. Reuses the same
  `history::diff` primitive as `--webhook-url` regression notifications
  rather than a second notion of "what changed." A stable HTML-comment
  marker (form URL + check name) embedded in the issue body is used to
  find the same issue again, so repeated `monitor` runs never duplicate
  it. The token falls back through `github_issues.token` →
  `GITHUB_TOKEN` (set automatically in GitHub Actions) → `GH_TOKEN` (the
  `gh` CLI's convention) → a skip-with-warning if none is found, and the
  integration is never fatal to the run itself. Live testing against a
  real repository surfaced a genuine bug unit tests couldn't have caught:
  GitHub's issue-search index is eventually consistent, so a lookup
  moments after creating an issue can miss it and create a duplicate;
  fixed with a bounded retry-with-backoff before falling back to create.
- A reusable **GitHub Action** (`action.yml`): any repository can add
  `uses: voidstackloop/formwatch@vX` to its own CI and run `test` or
  `monitor` with no Rust toolchain — it downloads the matching prebuilt
  release binary for the runner's OS (Linux/macOS x86_64+ARM64/Windows),
  reflects formwatch's own exit code, and always writes a full JSON
  report (exposed as the `report-json` output) regardless of that
  outcome. `scripts/test-action-locally.sh` exercises the action's own
  download/extract/invoke logic against a locally-built binary, so
  changes to `action.yml` don't need a real release to verify.
- A ninth check, **Duplicate field names**: flags form controls that
  share a `name` attribute with something other than a legitimate
  radio/checkbox group. Standard form encoding keeps only one value (or
  silently merges them) for a repeated key, so two different pieces of
  information submitted under one name means one vanishes with no
  client-side signal at all — not an accessibility issue (axe-core has
  no concept of submission semantics) and not covered by any other
  check. Always a `Fail`: unlike Bot protection, this is a real defect
  in the form itself.
- A tenth check, **Required-indicator mismatch**: flags a field whose
  own label visually promises it's required (a `*`, or the word
  "required") but isn't actually marked `required` or
  `aria-required="true"`. The visual promise and the real enforcement
  silently disagree — native validation and screen readers both treat
  the field as optional — so a user who skips it can submit incomplete
  data with no error at all. Not what axe-core's label rule checks
  (that's about a label *existing*, not about a required-looking one
  being honored), and not covered by any other check. Always a `Fail`:
  the field's own label contradicts its own enforcement.
- An eleventh check, **Input type mismatch**: guesses a field's
  semantic purpose from its label the same way Autofill hints does, but
  checks the `type` attribute instead of `autocomplete` — a field
  labeled like an email address or phone number left as `type="text"`
  (or untyped) gets no native format validation, and the wrong virtual
  keyboard on mobile (a full alphabetic layout instead of an
  email-optimized or numeric-telephone one). Deliberately scoped to
  email/phone only: both have a well-supported dedicated `type` and an
  unambiguous label pattern, unlike e.g. a ZIP code, where
  `type="number"` would actively be the wrong call (it strips a leading
  zero and adds spinner arrows). A heuristic guess from label wording,
  like Autofill hints, so a mismatch is a `Warn`, not a `Fail`.
- **Optional LLM semantic checks** (`--llm` / `llm:` config): two
  provider-agnostic checks — "Error wording (LLM)" and
  "Instructions (LLM)" — that score the clarity of validation error
  messages and instructions/required-document guidance, addressing the
  heuristic wording limitation called out in the README. Providers are
  OpenAI (and any OpenAI-compatible endpoint via `--llm-base-url`),
  Anthropic, and a no-network `mock`. The feature is off by default,
  never fatal (missing key / provider error / timeout / unparseable reply
  degrade to a single `Warn`), redacts obvious PII, truncates input, and
  caches verdicts on disk. A low score is a `Warn` unless `--llm-fail` is
  set; `--llm-threshold` sets the pass bar.
- LLM resilience and safety hardening: transient provider failures (HTTP
  429/5xx, network) are retried with exponential backoff honoring
  `Retry-After`; the two semantic checks run concurrently; page text is
  wrapped as untrusted data with delimiter neutralization and an explicit
  instruction not to follow embedded directions (prompt-injection
  resistance); provider token usage is logged at debug level; and
  `formwatch doctor --probe-llm` verifies provider connectivity and
  credentials.
- **Baseline / allow-list of accepted findings** (`--baseline FILE`,
  `baseline:` config, `FORMWATCH_BASELINE`, and `formwatch baseline
  [--write]`): a finding is suppressed only when a baseline entry for the
  same URL and check is at least as severe as the current status, so a
  baselined `WARN` never absorbs a `FAIL`. Baseline-aware behavior covers
  `test`/`monitor` exit codes, a plain-text `[BASELINED]` annotation, and
  regression notifications; stale entries (improved or disappeared) are
  reported. This lets CI fail on *new* problems instead of failing on day
  one and being ignored.
- Deterministic **sharding** (`monitor --shard INDEX/TOTAL`, `shard:`
  config, `FORMWATCH_SHARD`): checks only a stable slice of the configured
  forms, so a large registry can be split across several CI jobs or
  runners with no coordination — the union of all shards is the whole set
  and each form lands in exactly one shard every run.
- **Baseline-aware JUnit and SARIF**: with `--baseline`, accepted findings
  render as `<skipped>` in JUnit (so an allow-listed `FAIL` doesn't fail
  CI) and as `baselineState: "unchanged"` with a SARIF `suppressions`
  entry, so code-scanning UIs treat them as pre-existing rather than new.
- `test` and `monitor` accept `--junit` / `--sarif` (with `--out`), so a
  single CI job can run the checks and emit results in one step.
- `report --prometheus` renders Prometheus text-exposition metrics
  (`formwatch_forms`, `formwatch_checks_failing`,
  `formwatch_checks_warning`, `formwatch_check_last_run_timestamp_seconds`,
  and per-check `formwatch_check_status`) for a scrape endpoint or a
  node_exporter textfile-collector directory.
- A read-only **service mode** (`formwatch serve --addr HOST:PORT`, config
  `serve_addr`, or `FORMWATCH_SERVE_ADDR`): serves `/healthz`, `/readyz`,
  `/metrics` (Prometheus), and `/api/forms` (JSON) over the accumulated
  history, binding to `127.0.0.1:8080` by default and running until
  Ctrl-C. Deliberately read-only — no endpoint accepts a URL, triggers a
  run, or submits anything, so exposing it can't make formwatch touch a
  third-party site.
- History **retention**: `formwatch prune (--keep-last N | --keep-days D)
  [--dry-run]` (defaults via `keep_last`/`keep_days` config or
  `FORMWATCH_KEEP_LAST`/`FORMWATCH_KEEP_DAYS`) trims append-only history
  per form, touching only `*.json` run files.
- **Streak/trend reporting**: the report now computes each check's
  unbroken run of its current status. The plain-text report adds a
  `↳ FAIL for the last N run(s), since <date>` line for chronic
  non-Pass checks, and the HTML report adds a streak badge plus a compact
  coloured trend strip of recent runs.
- The HTML report gained a client-side toolbar: search by name/URL, filter
  by status (failing / warnings / flaky / passing), sort by recency,
  severity, or name, and a live "shown/total" count — all self-contained,
  no external assets.
- A bundled **demo site** (`formwatch demo`, pages under `demo-sites/`)
  with 35 deliberately varied forms covering **every** built-in check and
  known edge case: a clean baseline; missing labels / low contrast /
  missing alt / no landmarks; small tap targets and horizontal overflow;
  validation variants (vague errors, aria-required-only, no required
  fields, a required checkbox); document-upload variants; autofill;
  lost input vs. explained session loss; multi-step wizards including
  slow, keyup-gated, and icon-only-next variants; open, nested, and closed
  shadow roots; multiple forms on a page; RTL Arabic; reCAPTCHA, hCaptcha,
  Turnstile, and a full-page Cloudflare challenge; and the LLM-wording
  demos. Pages are compiled into the binary, so `cargo install` gets them;
  nothing third-party is contacted.
- `scripts/benchmark.sh` + [BENCHMARKS.md](BENCHMARKS.md): a reproducible
  benchmark over the demo site (9 forms / 72 checks, median 4.4s at
  `--wait 1`), with methodology and caveats.
- Structured diagnostics via `tracing`, written to **stderr** so
  machine-readable stdout is never corrupted: `-v`/`-vv`/`--quiet` and
  `--log-format json`.
- Configuration file and environment-variable support
  (`src/config.rs`): a `formwatch.yml` (or `--config PATH`, or the user
  config dir) plus `FORMWATCH_*` variables, with CLI > env > file >
  default precedence.
- New machine-readable report formats: `report --junit` (JUnit XML for
  CI dashboards; `FAIL` becomes `<failure>`, `WARN` becomes `<skipped>`)
  and `report --sarif` (SARIF 2.1.0 for code-scanning UIs).
- Regression webhooks: `--webhook-url` on `test`/`monitor` posts to a
  Slack incoming webhook or a generic JSON endpoint when a check
  regresses to `FAIL` (`notify.on: always` to notify every run).
- `formwatch completions <shell>` emits shell completions; a man page
  lives in `man/formwatch.1`.
- Every persisted/JSON run now carries a `schema_version`
  (`history::SCHEMA_VERSION`), so downstream consumers can detect a
  breaking shape change instead of silently misreading newer data.
- A typed `formwatch::error::Error` (`thiserror`) alongside the existing
  `anyhow` code, so embedders can match on failure cause; new modules are
  written against it and `anyhow::Error` interops via one variant.
- Container and supply-chain packaging: a multi-stage `Dockerfile`,
  `deny.toml` (cargo-deny policy), a Homebrew formula template, a
  Dependabot config, and release checksums, a CycloneDX SBOM, and build
  provenance attestation in the release workflow.
- CI now also runs an MSRV check, `cargo deny`, coverage
  (`cargo-llvm-cov`), and CodeQL.
- `formwatch doctor [--json]`: checks the local environment (Chrome
  availability, writable history/audit directories, config, legal
  acknowledgement) without running a form, and exits non-zero if
  something a run depends on is broken.
- `formwatch notify-test [--webhook-url URL]`: sends one synthetic
  regression so a webhook can be verified before relying on it.
- `--fail-on fail|warn` (also `fail_on` in config and
  `FORMWATCH_FAIL_ON`): choose whether only `FAIL`, or also `WARN`,
  makes the process exit non-zero. Default is unchanged (`fail`).
- A `SECURITY.md` vulnerability-disclosure policy.

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
- Flakiness detection: `formwatch report` (plain-text and `--html`) now
  flags a check whose status has flip-flopped across its *entire*
  recorded history (2+ transitions), not just the single immediately-
  previous run `diff` compares against. A check that changed once and
  stayed changed is a genuine regression or fix, not flakiness — only
  the unstable, back-and-forth case gets the "Flaky" badge.
- `monitor --delay-ms MS`: paces how fast new forms start (default 0 —
  unchanged behavior), so a large `forms.yml` against real third-party
  sites can spread its load out instead of firing up to
  `MAX_CONCURRENT_FORMS` requests at once.
- Page loads now retry up to 3 times with backoff (500ms, then 1s)
  before reporting "Page load: Fail" — a transient DNS blip or dropped
  connection against a real site no longer gets mistaken for the form
  itself being down.
- An eighth check, **Bot protection**: detects reCAPTCHA, hCaptcha,
  Cloudflare Turnstile, or a generic "verify you're human" interstitial.
  Never a Fail — none of this is evidence the form itself is broken, a
  real person sails through a CAPTCHA fine — but without it, a page
  stuck behind a challenge just looked like "No `<form>` element
  found," a real finding with the wrong explanation.

### Fixed

- Validation reported a false `FAIL` for a correctly-described field whose
  `aria-describedby` (or `aria-errormessage`) listed more than one id: the
  check resolved the whole space-separated list with a single
  `getElementById`, which returns null for a list. Each id is now
  resolved, and either attribute counts as a described field.
- LLM verdict parsing clamped an out-of-range score up to a passing 5,
  contradicting the module's own contract that an out-of-range score is a
  parse failure and letting a model-injected `{"score": 99}` force a
  `Pass`. It is now rejected (the check degrades to `Warn`), and verdict
  extraction is brace-balanced, so prose containing `{}` no longer
  discards a valid verdict.
- LLM retries now apply to 429/5xx responses whose body isn't JSON
  (routine for gateway/proxy errors): the status is checked before the
  body is parsed, instead of the failed parse turning a retryable status
  into a permanent failure.
- LLM `Retry-After` is honored up to its documented 60s cap instead of
  being silently re-capped to 8s. `LlmOptions`' hand-written `Debug`
  redacts the API key; the verdict cache key includes the resolved base
  URL (so switching `--llm-base-url` no longer serves the previous
  backend's verdicts); and the Anthropic response parser finds the first
  `text` content block rather than assuming the first block is text.
- `serve`'s `/readyz` reported ready for an existing but read-only history
  directory — `create_dir_all` returns `Ok` for any existing directory —
  so it now probes writability, and the server bounds concurrent
  connections and caps header-read time.
- `formwatch baseline --write` (`Baseline::from_runs`) recorded the
  *first* status seen for a repeated URL instead of the newest.
- `formwatch doctor` probed the audit log's parent directory rather than
  an existing (possibly read-only) log file, so it could report writable
  while every append failed.
- `--per-host-delay-ms` collapsed every IPv6 literal onto one pacing
  bucket: `host_of` split `[::1]:8080` on `:` and returned `"["`.
- `--check-timeout-secs` (via the wizard timeout) and the Chrome-open
  retry backoff could panic on overflow; both saturate now.
- Webhook auto-detection treated any URL *containing* `hooks.slack.com`
  (even in a query string or a look-alike host) as a Slack webhook; it
  now matches the host exactly.
- `report --prometheus`'s `formwatch_forms_total` was a gauge carrying the
  reserved counter `_total` suffix, which `promtool` rejects; renamed to
  `formwatch_forms`.
- JUnit XML output also drops the XML-1.0-forbidden U+FFFE/U+FFFF
  characters, and history loads same-second runs in a stable filename
  order so "the previous run" can't vary between invocations.
- The declared MSRV was wrong: the code (edition-2024 let-chains) and
  several transitive dependencies (`home`, `icu_*` need 1.88;
  `idna_adapter` needs 1.86) require Rust **1.88**, not 1.85, so the MSRV
  CI job failed. `rust-version`, the CI MSRV toolchain, and the Docker
  builder are now 1.88.
- Mobile usability never reported horizontal overflow: the check compared
  `document.documentElement.scrollWidth` against `window.innerWidth`, but
  under mobile emulation `innerWidth` already reflects the overflowed
  width, so the comparison was never true even on a page far wider than a
  phone. It now compares against the layout viewport
  (`documentElement.clientWidth`). Found while building the demo site, and
  now covered by a fixture + regression test.
- The validation check counted the `<form>` element itself as an unlabeled
  invalid field: the invalid-field query used a bare `:invalid` scoped at
  the form, and `form:invalid` matches whenever any control inside is
  invalid. A form whose *actual* invalid field had a proper
  `aria-describedby` was therefore failed. Found by building the demo
  site; fixed to query `input:invalid, select:invalid, textarea:invalid,
  [aria-invalid=true]`, with a regression fixture and test.
- A truly **cold** Chrome cache failed to populate: the concurrent-safe
  download created the cache's parent directory but never the private
  temp directory the fetcher writes its archive into, so the very first
  run on a machine with no system Chrome — or an empty cache — died with
  a bare `Failed to create archive file: No such file or directory`.
  Found while exercising the release test suite in a fresh environment;
  the temp directory is now created before the download.
- All emitted errors/diagnostics now go through the logging pipeline, so
  `--log-format json` produces valid newline-delimited JSON on stderr
  instead of being interleaved with plain-text `eprintln!` warnings.

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
- Every `browser::launch()` shared a single fixed Chrome profile
  directory by default (a chromiumoxide default, not something formwatch
  ever set). Harmless launching one at a time, but two concurrent
  launches collide on Chrome's own SingletonLock for that shared
  profile and one fails outright — invisible in local runs, but hit
  reliably the first time `cargo test` ran on GitHub Actions' faster,
  more-parallel runner. Each launch now gets its own unique profile
  directory.
- `formwatch report --out some/new/dir/file` (plain `--json` or
  `--html`) failed with a bare "No such file or directory" if that
  directory didn't exist yet — the same bug already fixed once for
  `formwatch init subdir/file.yml`, in a command that fix never reached.
  Found by the community-monitor workflow's own first real run, writing
  to a `results/` directory nothing had created yet.
- `fixtures/input-vanishes-form.html`'s test was flaky on a loaded CI
  runner: its `setTimeout` counted from page load, not from when
  formwatch actually got around to writing its marker value, so a
  slower machine could clear the field *before* the marker was ever
  set — silently defeating the whole scenario instead of testing it.
  Now waits for the field's own `input` event (which formwatch's own
  marker-set dispatches) before scheduling the clear.

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
