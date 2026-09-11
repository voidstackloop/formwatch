# System design: formwatch as a community tool

The CLI works today for one person checking one form on their own machine.
"The whole community will use it" is a different problem: people who can't
build Rust need a binary; people who want to check *other* forms want to
add them without forking; and the real payoff of civic monitoring is
comparing notes over time, not everyone running the same check alone. This
document designs that jump.

**Status:** Phase 1 (init, `--checks-dir` custom checks, `--json`, exit
codes, release build matrix, CI) is done. Phase 2's pieces are scaffolded
— `community-forms/`, `.github/workflows/monitor.yml`, and the static
`index.html` dashboard all exist and the dashboard has been verified
end-to-end against real generated data — but nothing is deployed: no
repo is pushed anywhere yet, GitHub Pages isn't enabled, and the
scheduled workflow has never actually run on GitHub's infrastructure
(only inspected for correctness; GitHub Actions can't be run locally).

## 1. Requirements

**Functional**
- FR1 — Install without a Rust toolchain (prebuilt binaries + `cargo install`).
- FR2 — `formwatch init` scaffolds a working `forms.yml` on first use.
- FR3 — Extend the check set without forking/recompiling the binary.
- FR4 — A community-curated list of public-service forms, added by pull
  request, organized by jurisdiction/category.
- FR5 — That list is checked on a recurring schedule automatically —
  nobody has to remember to run it, and history accumulates on its own.
- FR6 — The accumulated history is browsable by anyone, not just people
  running the CLI: a public status dashboard with trend over time.
- FR7 — A regression (Pass→Fail) can notify someone without polling the
  dashboard (webhook/Slack/Discord).
- FR8 — Machine-readable output and a meaningful exit code, so anyone's
  own automation can consume formwatch without depending on the hosted
  dashboard.

**Non-functional**
- NFR1 — Zero required paid infrastructure. This is an unfunded civic
  project; the design must not need the maintainer to run or pay for a
  server or database.
- NFR2 — Cross-platform installers (Linux/macOS/Windows).
- NFR3 — Adding a form to the registry must be a lightweight PR (YAML),
  not a code change — keeps review burden low for a small maintainer team.
- NFR4 — Published results never contain anything beyond what formwatch
  already generates (dummy field values, short heuristic strings) — no
  accidental PII, no raw page dumps.
- NFR5 — Scale is civic, not hyperscale: low hundreds to a few thousand
  tracked forms, checked at most a few times a day each. The design
  should not carry operational complexity sized for a bigger problem.

**Constraints**
- Small/volunteer maintainer team, no budget for hosting.
- The existing Rust CLI (checks + history + report) is a given, not up
  for a rewrite.
- Phase 1 (usability) must not require or block on Phase 2 (community
  registry) — they should ship independently.

## 2. High-level design

**Phase 1 — usable by one person, no new infrastructure**

```
                 ┌──────────────────────────┐
  binary/PATH    │         formwatch          │
 ───────────────►│  test / monitor / report   │
                 │  init            (new)     │
                 └─────────────┬──────────────┘
                                │ page.evaluate()
                        ┌───────┴────────┐
                        │  Check engine   │
                        │  - built-in Rust checks (unchanged)
                        │  - custom checks: any .js under
                        │    --checks-dir, run through the
                        │    SAME evaluate() path axe-core
                        │    already uses
                        └────────────────┘
```

Distribution: GitHub Releases with prebuilt binaries (3-target matrix:
linux-x86_64, macos-universal, windows-x86_64) plus `cargo install
formwatch` for Rust users. Skip package-manager taps (Homebrew etc.) until
there's real demand — cheap to add later, not worth building speculatively.

**Phase 2 — a shared registry and dashboard, still no servers**

```
 community-forms/            (a folder in this repo, or a separate
   us-federal/*.yml            "formwatch-registry" repo — either works)
   us-state-ca/*.yml
   ...
        │  PR-reviewed by maintainers — it's YAML, not code
        ▼
 GitHub Actions (scheduled cron)
        │  runs: formwatch monitor community-forms/**/*.yml --json
        ▼
 results/ (append-only JSON per run; pushed to an orphan `data`
            branch so history doesn't bloat the main branch's diffs)
        │
        ▼
 GitHub Pages static site
   - fetches results/*.json client-side — no backend
   - renders latest status + trend per form
   - a step in the same Action posts to a Slack/Discord webhook URL
     (repo secret) whenever a form flips Pass → Fail
```

Data flow: a PR adds a form → the next scheduled Action run picks it up
automatically (no separate "register with the dashboard" step) → formwatch
runs, results get committed as JSON → the static site re-renders on next
visit (a fetch, not a server round trip) → a regression fires the webhook.

## 3. Deep dive

**Data model.** Reuse `RunResult` from `history.rs` verbatim as the
registry's per-run schema — don't invent a second shape:

```json
{
  "name": "Business License Renewal",
  "url": "https://city.gov/business-license",
  "jurisdiction": "us-state-ca",
  "timestamp": 1234567890,
  "checks": [
    { "name": "Accessibility", "status": "Fail", "detail": "..." }
  ]
}
```

One file per run, `results/<slug>/<timestamp>.json` — the same layout
`.formwatch/history/` already uses locally, so the existing
`load_runs`/`diff` code works unmodified against a checked-out registry.

**"API" contract.** There isn't a live API — the contract is "a directory
of JSON files behind a CDN" (GitHub Pages). That's a deliberate
simplification, not an oversight (see trade-offs).

**Custom-check plugin contract (Phase 1).** A `--checks-dir DIR` flag runs
every `*.js` file in it through the same `page.evaluate()` mechanism
`check_accessibility` already uses for axe-core; each script must resolve
to `{ name, status: "Pass"|"Warn"|"Fail", detail }`. This reuses
infrastructure that already exists instead of building a Rust
trait-object plugin system or `dlopen`-based dynamic loading — those carry
real ABI/versioning pain across three OSes for no benefit here, since
everything a check needs (DOM access, a verdict) is already exactly what
JS-in-the-page does.

**Error handling.** A single form failing to load must not abort the
whole community run — `monitor` already isolates per-entry errors
(`main.rs`). Phase 2 only needs the Action step to not fail the whole
workflow on one bad form, and `monitor`'s exit code should mean "did
formwatch run," not "did every form pass" — the dashboard, not the exit
code, is where pass/fail-per-form lives.

## 4. Scale and reliability

- **Load:** even 2,000 tracked forms checked daily is ~2,000 headless-Chrome
  page loads/day — comfortably inside GitHub Actions' free public-repo
  minutes, sequential or lightly sharded.
- **Failover:** none needed. A missed scheduled run self-heals on the next
  cron tick; the dashboard just shows "last checked N days ago" rather than
  breaking.
- **Monitoring:** the thing being monitored is civic infrastructure, not
  formwatch's own — "is our monitoring healthy" is just "did the Action
  succeed," which GitHub already surfaces.

## 5. Trade-offs

| Decision | Chosen because | Cost | Revisit when |
|---|---|---|---|
| Static JSON + GitHub Pages, not a backend+DB | Zero cost/ops for a volunteer project | No ad-hoc querying, update latency = once per scheduled run | Registry outgrows a few thousand forms or people want live search — add one serverless query function, not a backend |
| JS-injected custom checks, not a compiled plugin ABI | Reuses axe-core's exact mechanism; no cross-platform ABI risk | Checks are sandboxed to what `page.evaluate()` can see (DOM only, no filesystem/network) | A check genuinely needs to correlate multiple pages — that's the signal to revisit the boundary |
| PR-reviewed YAML registry, not self-service submission | Keeps moderation light; a public "submit any URL" button is an abuse vector against third-party sites | Slower to add forms than self-service | Maintainer bandwidth grows enough for a review queue or approval bot |

## 6. What to revisit as it grows

- Registry exceeds a few thousand forms, or people want live filtering:
  add a small serverless function reading the same JSON — don't
  preemptively stand up a backend.
- `monitor` noise from forms that change layout often becomes a problem:
  add a "flaky" status requiring N consecutive failures before a
  regression fires on the dashboard — don't build this ahead of the actual
  noise.
- Custom checks outgrow `page.evaluate()`'s reach (e.g. need to correlate
  multiple pages/requests): reconsider the plugin boundary then, not now.
