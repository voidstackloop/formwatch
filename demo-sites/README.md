# formwatch demo site

A deliberately varied set of forms for seeing what formwatch does — one
clean baseline, plus a page for **every** built-in check and the known edge
cases (shadow DOM, wizards, CAPTCHAs, RTL, lost input, …). It is served by
`formwatch demo`, which embeds these pages in the binary, so they work after
a plain `cargo install` with nothing to copy.

## Running it

```
formwatch demo                 # serves http://127.0.0.1:8099
formwatch demo --addr 127.0.0.1:9000
```

Then browse the pages, or point formwatch at them:

```
formwatch test    http://127.0.0.1:8099/good-form.html
formwatch test    http://127.0.0.1:8099/lost-input.html --wait 3
formwatch monitor demo-sites/forms.yml          # every demo form at once
formwatch report  --html --out report.html      # browse the results
```

For the optional LLM wording checks:

```
OPENAI_API_KEY=... formwatch --llm test http://127.0.0.1:8099/vague-errors.html
```

## Baseline

| Page | Expected headline |
|---|---|
| `good-form.html` | everything Passes |

## Submission flow

| Page | Expected headline |
|---|---|
| `multi-step.html` | Passes after a 3-step wizard |
| `slow-step.html` | Passes despite a 1.2s transition |
| `keyup-next.html` | Passes (Next enabled on `keyup`) |
| `icon-only-next.html` | Passes via `aria-label` |
| `no-submit-control.html` | **Fail** — no Next/Continue/Submit |
| `shadow-form.html` | Passes — form inside an open shadow root |
| `nested-shadow-form.html` | Validation reaches a doubly-nested shadow field |
| `closed-shadow.html` | **Fail** "No `<form>` found" — a real platform limit |
| `multi-form.html` | Targets the bigger form, not the header search |
| `rtl-arabic.html` | Passes — Arabic Next/Submit wording |

## Accessibility

| Page | Expected headline |
|---|---|
| `missing-labels.html` | **Fail** — unlabeled fields |
| `low-contrast.html` | **Fail** — axe color-contrast |
| `missing-alt.html` | **Fail** — axe image-alt |
| `no-landmarks.html` | **Warn** — moderate axe findings |

## Validation errors

| Page | Expected headline |
|---|---|
| `vague-errors.html` | described but unhelpful wording (LLM demo) |
| `aria-required-only.html` | **Warn** — native validation can't exercise it |
| `no-required-fields.html` | **Warn** — nothing to test |
| `checkbox-required-first.html` | **Fail** — invalid with no accessible message |

## Required documents

| Page | Expected headline |
|---|---|
| `document-upload.html` | **Fail** — no label, no format guidance |
| `no-format-guidance.html` | **Warn** — labeled but no accept/format text |
| `documents-prose-no-upload.html` | **Warn** — documents mentioned, no file input |
| `good-upload.html` | Passes — labeled with `accept` + guidance |

## Autofill hints

| Page | Expected headline |
|---|---|
| `no-autocomplete.html` | **Warn** — name/email/phone/address with no hints |

## Input persistence

| Page | Expected headline |
|---|---|
| `lost-input.html` | **Fail** — the field silently clears |
| `session-timeout.html` | **Warn** — cleared, but explained |
| `no-text-field.html` | **Warn** — nothing to test |

## Mobile usability

| Page | Expected headline |
|---|---|
| `tiny-targets.html` | **Warn** — sub-44px controls |
| `horizontal-overflow.html` | **Warn** — content wider than a phone |

## Bot protection

| Page | Expected headline |
|---|---|
| `captcha.html` | **Warn** — reCAPTCHA widget on a testable form |
| `hcaptcha.html` | **Warn** — hCaptcha widget |
| `turnstile.html` | **Warn** — Cloudflare Turnstile widget |
| `cloudflare-challenge.html` | **Warn** — full-page block, no form reached |
| `generic-botcheck.html` | **Warn** — "prove you're not a robot" wording |

## LLM wording (optional)

| Page | Expected headline |
|---|---|
| `vague-errors.html` | Error wording — "Invalid input." |
| `instructions-unclear.html` | Instructions — vague upload guidance |

Expected results are indicative; real pages vary. The point is to have
something to click around and compare against.

## Benchmarks

`scripts/benchmark.sh` starts this site and times repeated `monitor` runs
against it; see [BENCHMARKS.md](../BENCHMARKS.md).
