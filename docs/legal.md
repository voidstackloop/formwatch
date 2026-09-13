# Legal and ethical use

formwatch is a tool for testing public-service forms. It does that by
driving a real headless browser against a real, live website. With
`--submit` it goes further and sends a real HTTP POST — with dummy data,
but a real POST — to a live system.

That makes formwatch powerful and also easy to misuse. This document is
the authoritative, long-form authorized-use notice. The short version the
CLI prints is in `src/legal.rs`; `formwatch legal` prints it on demand.

**If you are unsure whether you are allowed to test a given site, you are
not. Get written permission first.**

## 1. What formwatch actually does to a target

- Loads the URL in headless Chrome and lets the page's JavaScript run.
- Fills visible fields with obvious dummy values (`Formwatch Test`,
  `formwatch-test@example.com`, `5555550123`, and so on).
- Exercises native HTML5 validation.
- Captures full-page screenshots of non-Pass checks.
- Optionally, with `--submit`, clicks the final submit control and
  performs a real POST.
- Makes no attempt to solve CAPTCHAs or bypass any access control. If a
  challenge is detected, formwatch reports it and moves on.

## 2. You must be authorized

Only run formwatch against forms that you:

- own or operate, **or**
- have explicit, written permission to test, **or**
- are testing as part of an engagement whose scope covers exactly that
  system.

"It is a public website" is **not** authorization. A form being reachable
without a login does not make automated interaction with it lawful or
permitted.

## 3. Legal exposure

Unauthorized automated access can violate computer-misuse and
unauthorized-access statutes, including but not limited to:

- The U.S. **Computer Fraud and Abuse Act**, 18 U.S.C. § 1030.
- The U.K. **Computer Misuse Act 1990**.
- Equivalent provisions in the EU member states, Canada, Australia, and
  elsewhere.

Depending on jurisdiction and conduct, it can also amount to breach of
contract and/or terms of service, trespass to chattels, and privacy-law
violations. Criminal liability does not require intent to damage
anything — unauthorized access alone can be enough.

Automated access is also near-universally governed by the target's
**terms of service** and **acceptable-use policy**. Even where no statute
is implicated, violating those can get accounts, IPs, or organizations
banned and can expose you to civil claims.

## 4. Prohibited uses

Do not use formwatch for:

- Denial-of-service, "stress testing", or any attempt to overwhelm a host.
- Credential stuffing, password spraying, or authentication attacks.
- Scraping, data harvesting, or bulk data extraction.
- Bypassing authentication, CAPTCHAs, paywalls, or rate limits.
- Sending data you do not have the right to send to a system you do not
  administer (including under `--submit`).
- Any activity intended to access systems or data you are not permitted
  to access.

## 5. Be a good citizen

- Respect `robots.txt`, the site's terms, and any stated rate limits.
- Pace your requests. Use `--delay-ms` and `--per-host-delay-ms`, and
  keep `--max-concurrent` conservative when checking third-party sites.
- Prefer off-peak schedules and avoid repeatedly re-checking the same
  form far more often than it can change.
- Stop if asked to. If a site owner objects to your traffic, stop and
  discuss it.

## 6. `--submit` specifically

Because `--submit` performs a real write:

- It requires an explicit acknowledgement (`--accept-terms` or
  `FORMWATCH_ACCEPT_TERMS=1`) before it will run.
- It should only ever be used against a form you are authorized to submit
  to — ideally a staging/test instance, not a production system whose
  intake staff must process a fake application.
- The data it submits is dummy data, but it is submitted for real.

Leave `--submit` off by default. It is off unless you ask for it.

## 7. Data protection and privacy

- Screenshots captured on non-Pass checks can contain anything visible on
  the page, including **personal data**. Where that is a concern, disable
  capture with `--no-screenshots` / `screenshots: false` in config, and
  delete any stored history and reports when no longer needed.
- Custom check scripts can read anything in the page's DOM. Only run
  scripts you trust, and only against pages you are authorized to
  inspect.
- History and reports (`--json`, `--html`, JUnit, SARIF, the audit log)
  may be portable and are your responsibility to secure. The audit log
  intentionally records URLs and verdicts but no captured page content,
  so it is safer to retain than a screenshot-bearing report.
- Comply with applicable privacy law, including the GDPR and CCPA, for
  any personal data that passes through the tool.

## 8. No warranty, no authorization from the authors

formwatch is distributed under the MIT License, **as is, without warranty
of any kind**. The authors do not authorize, and specifically disclaim any
authorization for, any use of this software that would be unlawful or
that breaches a third party's rights. You are solely responsible for your
own use of the tool.

If a law-enforcement or platform inquiry reaches you, your own
authorization records — scope documents, permission emails, your
organization's testing policy — are what stand between an authorized
test and an unauthorized access claim. Keep them.

## 9. LLM providers send page text to a third party

The optional LLM semantic checks (`--llm`) extract text from the page
under test — validation error messages, labels, and instructions — and
send it to the configured provider (OpenAI, Anthropic, or any
OpenAI-compatible endpoint you point `--llm-base-url` at).

That is a data-transfer decision with legal consequences:

- **You are the data controller** for that transfer. Ensure you have a
  lawful basis to send the extracted text to the provider, under the
  provider's terms and applicable law (including GDPR/CCPA).
- **Redaction is best-effort, not a guarantee.** formwatch scrubs obvious
  emails, phone numbers, and long digit runs before sending, but it cannot
  know what else on a page is personal or confidential. Do not enable the
  LLM checks on pages that display sensitive data unless you have
  assessed and accepted the risk.
- **Caching writes verdicts to disk.** Verdicts (the model's score and
  short issue list, not the raw page text) are cached under the user cache
  directory to avoid re-sending unchanged pages. Disable with
  `--llm-no-cache` if that is not acceptable.
- **Self-hosting avoids the transfer entirely.** `--llm-base-url` can
  point at a model you run yourself (Ollama, vLLM, ...), keeping page text
  on infrastructure you control.

The LLM checks are off unless you explicitly enable them. If in doubt,
leave them off.
