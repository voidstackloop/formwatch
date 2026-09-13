# Security policy

## Reporting a vulnerability

Please report security issues **privately**, not in a public issue.
Open a [GitHub security advisory](https://github.com/voidstackloop/formwatch/security/advisories/new)
or email the maintainers listed in the repository.

Include, where you can:

- a description of the issue and its impact,
- steps to reproduce (a minimal fixture or URL, the command run),
- affected version/commit, and
- any suggested fix.

We aim to acknowledge reports within a few business days. Please give us a
reasonable window to release a fix before public disclosure; we'll credit
you in the advisory unless you'd rather stay anonymous.

## Scope

formwatch launches a real browser and, with `--submit`, sends a real HTTP
POST. Security-relevant areas include, but aren't limited to:

- the custom-check mechanism (`--checks-dir` runs local JS in the page),
- screenshot/history/audit output that could capture sensitive data,
- the Chrome auto-download and profile-directory handling (`src/browser.rs`),
- webhook delivery and config/env handling,
- any way the tool could be made to access or affect a system beyond the
  URL the operator supplied.

## Dual-use and authorized use

formwatch is an intentionally active tool: it drives a browser against
third-party sites. That is not a vulnerability, but using it without
authorization can be unlawful. See [docs/legal.md](legal.md). Reports that
amount to "this tool can be pointed at a site I don't own" are out of
scope; the operator is responsible for authorization.

## Hardening recommendations

- Run formwatch with the least privilege it needs; it requires no
  privileged access.
- Use `--no-screenshots` where page content may contain personal data, and
  protect the history directory and any reports.
- Only run custom check scripts you trust — they execute in the target
  page's context.
- Pin releases by checksum or verify the published provenance/SBOM.
- Keep dependencies current; `cargo audit` and `cargo deny` run in CI, and
  Dependabot opens update PRs.
