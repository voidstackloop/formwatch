# ADR 0002: Enterprise / authorized-use hardening

**Status:** Accepted.

## Context

formwatch began as a single-person CLI: one operator, one machine, a
handful of forms. Everything it gained after that — the check engine,
history and diffing, custom checks, a scheduled community dashboard — was
built to the standard of "careful, well-tested, honest about limits," but
assumed a solo, cooperative user. "Enterprise level" changes the
assumptions:

- Deployments run it **unattended** (CI, cron, containers), so stdout
  must be machine-parseable and failures must be structured.
- Operators need **configuration** without editing a command line in a
  job definition: files and environment variables.
- Downstream systems (dashboards, code scanning, chat) need **stable,
  versioned, conventional output**, not just colored text.
- Security teams ask about **supply chain** (licenses, advisories,
  provenance) and **data governance** (what gets stored).
- Most importantly, a tool that drives an automated browser against live
  third-party sites — and can really POST — is **easy to misuse
  unlawfully**. A responsible release must say so, prominently and in the
  product, not bury it in a README.

## Decision

Harden the existing binary along five axes, **additively**, keeping every
existing CLI flag, output shape, and on-disk format backward compatible.
New behavior is opt-in or defaulted to today's behavior.

1. **Authorized-use notice.** A canonical notice (`src/legal.rs`, long
   form `docs/legal.md`) is printed by `formwatch legal`, summarized in
   the README/`NOTICE` and HTML footers, surfaced as a reminder on
   `test`/`monitor`, and enforced: `--submit` refuses to run (exit 2)
   until acknowledged via `--accept-terms`, config, or
   `FORMWATCH_ACCEPT_TERMS=1`.
2. **Observability.** `tracing` to **stderr** (never corrupting stdout
   JSON), `-v`/`--quiet`/`--log-format json`, and an append-only,
   screenshot-free JSON audit log.
3. **Configuration.** A `formwatch.yml` file plus `FORMWATCH_*` variables,
   with CLI > env > file > default precedence.
4. **Contracts and integrations.** A `schema_version` on every record,
   `--junit` and `--sarif` report formats, regression webhooks, shell
   completions, and a man page.
5. **Supply chain and CI.** `Dockerfile`, `deny.toml`, a Homebrew
   template, Dependabot, and CI jobs for MSRV, `cargo deny`, coverage,
   and CodeQL; release checksums, a CycloneDX SBOM, and build-provenance
   attestation.

## Consequences

- The binary is meaningfully more deployable and auditable, at the cost
  of a larger dependency set (`tracing`, `reqwest`, `clap_complete`,
  `dirs`, `thiserror`).
- Errors are becoming structured (`formwatch::error::Error`) without a
  risky rewrite; existing `anyhow` code interops through one variant, and
  new modules use the typed error.
- `--submit` is now gated. This is a deliberate, user-requested behavior
  change: it is the one place where "additive only" must yield to a
  safety requirement.
- The legal notice is intentionally prominent and repeated. If users find
  it noisy, the escape hatch is an explicit acknowledgement, not removing
  the warning.

## Alternatives considered

- **A server / control plane.** Rejected: contradicts the project's
  zero-infrastructure, unfunded-civic-tech constraint (see
  ADR 0001 and `docs/community-design.md`).
- **Typed errors everywhere, in one sweep.** Rejected as unnecessarily
  risky; introduced alongside `anyhow` and adopted in new code.
- **A compiled plugin ABI for checks.** Still rejected for the same
  cross-platform ABI reasons as ADR 0001; custom checks remain JS.
- **Silent `--submit` with a README warning.** Rejected: a warning nobody
  reads doesn't prevent the harm.
