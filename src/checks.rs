use anyhow::{Context, Result};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    Pass,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    pub name: String,
    pub status: Status,
    pub detail: String,
}

fn result(name: &str, status: Status, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        detail: detail.into(),
    }
}

impl Status {
    pub fn label(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }

    pub fn css_class(&self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Warn => "warn",
            Status::Fail => "fail",
        }
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Resolves to "the form under test": the `<form>` with the most input/
/// select/textarea descendants, not just the first one in document order.
/// A real page often has an incidental header/footer search box as its
/// own tiny `<form>` — blindly using `document.querySelector('form')`
/// (the original approach) would silently fill, click, and validate
/// *that* instead of the actual application form. Picked once per page
/// session and cached on `window` so every check agrees on the same
/// form, and so "the form disappeared" means specifically this form is
/// gone, not merely that some other form on the page still exists.
const TARGET_FORM_JS: &str = "(() => {
    if (window.__formwatchFormPicked) {
        return document.contains(window.__formwatchForm) ? window.__formwatchForm : null;
    }
    window.__formwatchFormPicked = true;
    const forms = Array.from(document.querySelectorAll('form'));
    forms.sort((a, b) => b.querySelectorAll('input, select, textarea').length - a.querySelectorAll('input, select, textarea').length);
    window.__formwatchForm = forms[0] ?? null;
    return window.__formwatchForm;
})()";

/// Fills every visible input/select/textarea on the target form with
/// plausible dummy data so the later checks (validation, persistence)
/// have something to work with. Returns the number of fields filled.
async fn fill_form_fields(page: &Page) -> Result<usize> {
    let filled: i64 = page
        .evaluate(format!(
            r#"(() => {{
                const form = {TARGET_FORM_JS};
                const fields = form ? form.querySelectorAll('input, textarea, select') : [];
                let count = 0;
                for (const el of fields) {{
                    if (el.disabled || el.type === 'hidden' || el.type === 'submit' || el.type === 'button' || el.type === 'file') continue;
                    if (el.offsetParent === null) continue; // not visible
                    if (el.tagName === 'SELECT') {{
                        if (el.options.length > 1) el.selectedIndex = 1;
                    }} else if (el.type === 'checkbox' || el.type === 'radio') {{
                        el.checked = true;
                    }} else if (el.type === 'email') {{
                        el.value = 'formwatch-test@example.com';
                    }} else if (el.type === 'tel') {{
                        el.value = '5555550123';
                    }} else if (el.type === 'number' || el.type === 'range') {{
                        el.value = el.min || '1';
                    }} else if (el.type === 'date') {{
                        // Respect min/max — a hardcoded date can violate a
                        // form's own constraints (e.g. an appointment
                        // scheduler requiring a future date) and report a
                        // false "invalid" that has nothing to do with the
                        // site itself.
                        const today = new Date().toISOString().slice(0, 10);
                        el.value = el.min || (el.max && el.max < today ? el.max : today);
                    }} else {{
                        el.value = 'Formwatch Test';
                    }}
                    el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                    el.dispatchEvent(new Event('change', {{ bubbles: true }}));
                    // A Next/submit button gated on keydown/keyup (a
                    // char-counter, an "enable on type" listener) rather
                    // than input/change would otherwise look stuck even
                    // though the form works fine for a real user typing.
                    el.dispatchEvent(new KeyboardEvent('keydown', {{ bubbles: true }}));
                    el.dispatchEvent(new KeyboardEvent('keyup', {{ bubbles: true }}));
                    count += 1;
                }}
                return count;
            }})()"#
        ))
        .await?
        .into_value()?;
    Ok(filled as usize)
}

const MAX_WIZARD_STEPS: usize = 8;

/// Only buttons the user could actually see and click, scoped to the
/// target form — a hidden earlier step's "Next" stays in the DOM after
/// `hidden` is set on it, so without the visibility filter a wizard would
/// loop forever re-clicking a stale control; without the form scope, a
/// button in some other `<form>` on the page (e.g. a header search box)
/// could get misclassified as this form's Next/submit control.
fn visible_buttons_js() -> String {
    format!(
        "(() => {{ const f = {TARGET_FORM_JS}; return f ? Array.from(f.querySelectorAll('button, input[type=submit], input[type=button]')).filter(b => b.offsetParent !== null) : []; }})()"
    )
}

const LABEL_JS: &str = "b => (b.textContent.trim() || b.value || b.getAttribute('aria-label') || b.title || '').trim()";

/// Classifies the visible action on the current step: "next" (Next/Continue
/// — advance a multi-step wizard without submitting), "final" (Submit/
/// Apply/Finish/... or a bare type=submit), or "none".
fn classify_action_js() -> String {
    let buttons = visible_buttons_js();
    format!(
        r#"(() => {{
            // textContent must be trimmed *before* the `||` chain, not just
            // at the end — an icon-only button with whitespace/newlines
            // around its <svg> child has non-empty (truthy) whitespace-only
            // textContent, which would short-circuit past aria-label/title
            // otherwise.
            const label = {LABEL_JS};
            const buttons = {buttons};
            if (buttons.some(b => /\b(next|continue)\b/i.test(label(b)))) return 'next';
            if (buttons.some(b => b.type === 'submit' || /\b(submit|apply|finish|send|complete)\b/i.test(label(b)))) return 'final';
            return 'none';
        }})()"#
    )
}

fn click_matching_js(pattern: &str) -> String {
    let buttons = visible_buttons_js();
    format!(
        r#"(() => {{
            // textContent must be trimmed *before* the `||` chain, not just
            // at the end — an icon-only button with whitespace/newlines
            // around its <svg> child has non-empty (truthy) whitespace-only
            // textContent, which would short-circuit past aria-label/title
            // otherwise.
            const label = {LABEL_JS};
            const buttons = {buttons};
            const target = buttons.find(b => {pattern});
            if (target) {{ target.click(); return true; }}
            return false;
        }})()"#
    )
}

async fn looks_like_success(page: &Page) -> Result<bool> {
    let body_text: String = page
        .evaluate("document.body.innerText.slice(0, 2000).toLowerCase()")
        .await?
        .into_value()?;
    Ok([
        "thank you",
        "success",
        "received",
        "confirmation",
        "submitted",
    ]
    .iter()
    .any(|kw| body_text.contains(kw)))
}

/// Walks a form (or multi-step wizard) to completion. At each step it fills
/// the visible fields and either advances past a Next/Continue control or,
/// on reaching the final submit, checks native HTML5 validation is wired
/// up. The real submit is never clicked unless `allow_submit` is true —
/// this tool should not silently POST fake data into a live public-service
/// form.
pub async fn check_submission_flow(page: &Page, allow_submit: bool) -> Result<CheckResult> {
    let mut steps = 0usize;
    let mut total_filled = 0usize;

    loop {
        let target_form_exists: bool = page
            .evaluate(format!("({TARGET_FORM_JS}) !== null"))
            .await?
            .into_value()?;
        if !target_form_exists {
            if steps == 0 {
                return Ok(result(
                    "Submission flow",
                    Status::Fail,
                    "No <form> element found on the page.",
                ));
            }
            let status = if looks_like_success(page).await? {
                Status::Pass
            } else {
                Status::Warn
            };
            return Ok(result(
                "Submission flow",
                status,
                format!(
                    "Advanced through {steps} step(s), filling {total_filled} field(s) total, then the form disappeared with no further submit control."
                ),
            ));
        }

        total_filled += fill_form_fields(page).await?;
        let action: String = page.evaluate(classify_action_js()).await?.into_value()?;

        match action.as_str() {
            "next" => {
                steps += 1;
                if steps > MAX_WIZARD_STEPS {
                    return Ok(result(
                        "Submission flow",
                        Status::Warn,
                        format!(
                            "Gave up after {MAX_WIZARD_STEPS} steps without reaching a final submit — the flow may be longer than expected, or stuck advancing in place."
                        ),
                    ));
                }
                let before: String = page
                    .evaluate("document.body.innerText")
                    .await?
                    .into_value()?;
                page.evaluate(click_matching_js(r"/\b(next|continue)\b/i.test(label(b))"))
                    .await?;
                tokio::time::sleep(Duration::from_millis(700)).await;
                let after: String = page
                    .evaluate("document.body.innerText")
                    .await?
                    .into_value()?;
                if before == after {
                    return Ok(result(
                        "Submission flow",
                        Status::Fail,
                        format!(
                            "Step {steps}: clicked \"Next\"/\"Continue\" but the page didn't change — that step appears broken."
                        ),
                    ));
                }
            }
            "final" => {
                let valid: bool = page
                    .evaluate(format!(
                        "(() => {{ const f = {TARGET_FORM_JS}; return !f || !f.checkValidity ? true : f.checkValidity(); }})()"
                    ))
                    .await?
                    .into_value()?;

                if !allow_submit {
                    return Ok(result(
                        "Submission flow",
                        Status::Pass,
                        format!(
                            "{}filled {total_filled} field(s) total; native validation on the final step reports {}. \
                             Real submission skipped (pass --submit to test the live POST).",
                            if steps > 0 {
                                format!("Advanced through {steps} step(s), ")
                            } else {
                                String::new()
                            },
                            if valid { "valid" } else { "invalid" }
                        ),
                    ));
                }

                let before_url: String = page.evaluate("location.href").await?.into_value()?;
                page.evaluate(click_matching_js(
                    r"b.type === 'submit' || /\b(submit|apply|finish|send|complete)\b/i.test(label(b))",
                ))
                .await?;
                tokio::time::sleep(Duration::from_secs(2)).await;
                let after_url: String = page.evaluate("location.href").await?.into_value()?;
                let succeeded = looks_like_success(page).await?;
                let body_text: String = page
                    .evaluate("document.body.innerText.slice(0, 2000).toLowerCase()")
                    .await?
                    .into_value()?;
                let looks_failed = ["error", "failed", "try again", "something went wrong"]
                    .iter()
                    .any(|kw| body_text.contains(kw));

                let status = if looks_failed {
                    Status::Fail
                } else if succeeded || before_url != after_url {
                    Status::Pass
                } else {
                    Status::Warn
                };
                return Ok(result(
                    "Submission flow",
                    status,
                    format!(
                        "Submitted after {steps} step(s). URL changed: {}. Success/failure keywords seen: success={succeeded} failed={looks_failed}.",
                        before_url != after_url
                    ),
                ));
            }
            _ => {
                return Ok(result(
                    "Submission flow",
                    Status::Fail,
                    format!(
                        "After {steps} step(s), filled {total_filled} field(s) but found no Next/Continue or submit button."
                    ),
                ));
            }
        }
    }
}

/// Runs the vendored axe-core engine against the page for WCAG-based
/// accessibility issues (labels, contrast, landmarks, ARIA, keyboard traps
/// axe can detect statically).
pub async fn check_accessibility(page: &Page) -> Result<CheckResult> {
    const AXE_SRC: &str = include_str!("../assets/axe.min.js");
    page.evaluate(AXE_SRC).await?;

    let violations: serde_json::Value = page
        .evaluate("axe.run().then(r => JSON.stringify(r.violations))")
        .await?
        .into_value::<String>()
        .map(|s| serde_json::from_str(&s).unwrap_or(serde_json::Value::Array(vec![])))?;

    let Some(items) = violations.as_array() else {
        return Ok(result(
            "Accessibility",
            Status::Warn,
            "axe-core returned no parseable result.",
        ));
    };
    if items.is_empty() {
        return Ok(result(
            "Accessibility",
            Status::Pass,
            "axe-core found no violations.",
        ));
    }

    let critical = items.iter().filter(|v| v["impact"] == "critical").count();
    let serious = items.iter().filter(|v| v["impact"] == "serious").count();
    let summary: Vec<String> = items
        .iter()
        .take(5)
        .filter_map(|v| {
            v["id"]
                .as_str()
                .map(|id| format!("{id} ({})", v["impact"].as_str().unwrap_or("?")))
        })
        .collect();

    let status = if critical > 0 || serious > 0 {
        Status::Fail
    } else {
        Status::Warn
    };
    Ok(result(
        "Accessibility",
        status,
        format!(
            "{} violation(s) [{critical} critical, {serious} serious]: {}",
            items.len(),
            summary.join(", ")
        ),
    ))
}

/// Emulates a phone viewport and checks for horizontal overflow and
/// undersized tap targets (WCAG 2.5.5 recommends >=44x44 CSS px).
pub async fn check_mobile_usability(page: &Page) -> Result<CheckResult> {
    page.execute(
        SetDeviceMetricsOverrideParams::builder()
            .width(375)
            .height(667)
            .device_scale_factor(2.0)
            .mobile(true)
            .build()
            .map_err(|e| anyhow::anyhow!(e))?,
    )
    .await?;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Measure inside its own block so the reset below always runs even
    // if a measurement fails — otherwise an early `?` return would skip
    // the reset entirely, leaving the viewport stuck in mobile mode for
    // every check that runs after this one in run_all's fixed order.
    let measured: Result<(bool, i64)> = async {
        let overflow: bool = page
            .evaluate("document.documentElement.scrollWidth > window.innerWidth + 1")
            .await?
            .into_value()?;

        let small_targets: i64 = page
            .evaluate(
                r#"(() => {
                    const els = document.querySelectorAll('a, button, input, select, textarea');
                    let small = 0;
                    for (const el of els) {
                        const r = el.getBoundingClientRect();
                        if (r.width === 0 && r.height === 0) continue;
                        if (r.width < 44 || r.height < 44) small += 1;
                    }
                    return small;
                })()"#,
            )
            .await?
            .into_value()?;

        Ok((overflow, small_targets))
    }
    .await;

    // Reset to a normal desktop viewport for whatever check runs next.
    // Best-effort: measured results (if any) are already computed and
    // valid, so a failure here shouldn't discard them via `?` — that
    // would report "Mobile usability: check did not complete" for a
    // check that in fact completed just fine.
    if let Ok(reset) = SetDeviceMetricsOverrideParams::builder()
        .width(1280)
        .height(800)
        .device_scale_factor(1.0)
        .mobile(false)
        .build()
    {
        let _ = page.execute(reset).await;
    }

    let (overflow, small_targets) = measured?;
    let status = if overflow || small_targets > 0 {
        Status::Warn
    } else {
        Status::Pass
    };
    Ok(result(
        "Mobile usability",
        status,
        format!(
            "Horizontal overflow at 375px width: {overflow}. Tap targets under 44x44px: {small_targets}."
        ),
    ))
}

/// Fills a distinctive value into the first text field, waits, then checks
/// whether it's still there and whether anything on the page started
/// warning about a session timeout. A full real-world timeout (often
/// 15-30 minutes) isn't practical to wait out by default; `wait_secs` lets
/// callers opt into a longer wait.
pub async fn check_input_persistence(page: &Page, wait_secs: u64) -> Result<CheckResult> {
    let marker = "formwatch-persistence-check";
    let set: bool = page
        .evaluate(format!(
            r#"(() => {{
                const el = document.querySelector('input[type=text], input:not([type]), textarea');
                if (!el) return false;
                el.value = '{marker}';
                el.dispatchEvent(new Event('input', {{ bubbles: true }}));
                return true;
            }})()"#
        ))
        .await?
        .into_value()?;

    if !set {
        return Ok(result(
            "Input persistence",
            Status::Warn,
            "No text field found to test.",
        ));
    }

    tokio::time::sleep(Duration::from_secs(wait_secs)).await;

    let still_there: bool = page
        .evaluate(format!(
            "document.querySelector('input[type=text], input:not([type]), textarea')?.value === '{marker}'"
        ))
        .await?
        .into_value()?;

    // Phrase list, not a bare "timed out"/"timeout" match — unrelated copy
    // ("connection timed out", "request timeout: 30s") would otherwise
    // false-positive on any page that mentions a network timeout.
    let timeout_warning: bool = page
        .evaluate(
            r#"(() => {
                const text = document.body.innerText.toLowerCase();
                return [
                    'session expired', 'session has expired', 'your session has expired',
                    'session timed out', 'your session timed out',
                    'please log in again', 'please sign in again',
                ].some((kw) => text.includes(kw));
            })()"#,
        )
        .await?
        .into_value()?;

    let status = if !still_there && !timeout_warning {
        Status::Fail // input silently vanished with no explanation
    } else if timeout_warning {
        Status::Warn
    } else {
        Status::Pass
    };
    Ok(result(
        "Input persistence",
        status,
        format!(
            "After {wait_secs}s idle: input retained={still_there}, session-timeout wording seen={timeout_warning}."
        ),
    ))
}

/// Triggers native browser constraint validation (no real submit) and
/// checks that required fields and error messages are surfaced in a way
/// assistive tech can pick up (aria-invalid / aria-describedby / visible
/// error text), not just implied by color.
pub async fn check_validation_errors(page: &Page) -> Result<CheckResult> {
    let required_count: i64 = page
        .evaluate(format!(
            "(() => {{ const f = {TARGET_FORM_JS}; return f ? f.querySelectorAll('[required], [aria-required=true]').length : 0; }})()"
        ))
        .await?
        .into_value()?;

    // Fields using only aria-required (no native `required` attribute)
    // are invisible to reportValidity()/:invalid — the browser's native
    // constraint validation was never actually exercising anything for
    // them. Tracked separately so an all-aria-required form doesn't get
    // a false Pass just because nothing native-invalid was found.
    let native_required_count: i64 = page
        .evaluate(format!("(() => {{ const f = {TARGET_FORM_JS}; return f ? f.querySelectorAll('[required]').length : 0; }})()"))
        .await?
        .into_value()?;

    // Invalidate one required field, then ask the browser to validate.
    // Checkboxes/radios are governed by .checked, not .value — setting
    // .value on one is a silent no-op that leaves it valid, so a form
    // whose first required field is a checkbox needs a different reset.
    let triggered: bool = page
        .evaluate(format!(
            r#"(() => {{
                const f = {TARGET_FORM_JS};
                if (!f) return false;
                const req = f.querySelector('[required], [aria-required=true]');
                if (req) {{
                    if (req.type === 'checkbox' || req.type === 'radio') {{
                        req.checked = false;
                    }} else if (req.tagName === 'SELECT') {{
                        req.selectedIndex = -1;
                    }} else {{
                        req.value = '';
                    }}
                }}
                if (!f.reportValidity) return false;
                return !f.reportValidity();
            }})()"#
        ))
        .await?
        .into_value()?;

    let unlabeled_invalid: i64 = page
        .evaluate(format!(
            r#"(() => {{
                const f = {TARGET_FORM_JS};
                if (!f) return 0;
                const invalid = f.querySelectorAll(':invalid, [aria-invalid=true]');
                let unlabeled = 0;
                for (const el of invalid) {{
                    const describedBy = el.getAttribute('aria-describedby');
                    const hasDescribedText = describedBy && document.getElementById(describedBy)?.textContent.trim();
                    if (!hasDescribedText) unlabeled += 1;
                }}
                return unlabeled;
            }})()"#
        ))
        .await?
        .into_value()?;

    if required_count == 0 {
        return Ok(result(
            "Validation errors",
            Status::Warn,
            "No fields marked required/aria-required; couldn't exercise validation. \
             Custom JS-driven validation errors aren't checked here — pass --submit to also test those.",
        ));
    }

    let status = if unlabeled_invalid > 0 {
        Status::Fail
    } else if native_required_count == 0 {
        // Every required-looking field is aria-required-only, so native
        // validation genuinely could not have exercised anything — a
        // Pass here would be false confidence, not a real result.
        Status::Warn
    } else {
        Status::Pass
    };
    let aria_only_note = if native_required_count == 0 {
        " All of them use aria-required without the native required attribute, so native validation couldn't exercise them — pass --submit to test the site's real (custom JS) validation."
    } else {
        ""
    };
    Ok(result(
        "Validation errors",
        status,
        format!(
            "{required_count} required field(s). Native validation blocked submit: {triggered}. \
             Invalid field(s) without a screen-reader-visible error message: {unlabeled_invalid}.{aria_only_note}"
        ),
    ))
}

/// Looks for a mismatch between "you'll need to upload X" prose and the
/// actual upload mechanism: file inputs without an accessible label or
/// without format/size guidance (either an `accept` attribute or nearby
/// text naming a format), and document-related copy with no upload field
/// anywhere on the page to match it.
pub async fn check_required_documents(page: &Page) -> Result<CheckResult> {
    let mentions_documents: bool = page
        .evaluate("/required document|upload (a |your )?(copy|scan|photo|file)|attach(ment)? of|supporting document/i.test(document.body.innerText)")
        .await?
        .into_value()?;

    let file_inputs: i64 = page
        .evaluate(format!(
            "(() => {{ const f = {TARGET_FORM_JS}; return f ? f.querySelectorAll('input[type=file]').length : 0; }})()"
        ))
        .await?
        .into_value()?;

    if file_inputs == 0 {
        return Ok(if mentions_documents {
            result(
                "Required documents",
                Status::Warn,
                "Page text mentions required/uploaded documents but no file upload field was found.",
            )
        } else {
            result(
                "Required documents",
                Status::Pass,
                "No document upload required.",
            )
        });
    }

    let unlabeled: i64 = page
        .evaluate(format!(
            r#"(() => {{
                const f = {TARGET_FORM_JS};
                const inputs = f ? f.querySelectorAll('input[type=file]') : [];
                let unlabeled = 0;
                for (const el of inputs) {{
                    // Not a `label[for="${{el.id}}"]` selector: an id containing a
                    // double quote (legal in HTML) breaks that selector with a
                    // DOMException instead of just not matching.
                    const byFor = el.id && Array.from(document.querySelectorAll('label')).find((l) => l.htmlFor === el.id)?.textContent.trim();
                    const byWrap = el.closest('label')?.textContent.trim();
                    const byAria = el.getAttribute('aria-label');
                    if (!byFor && !byWrap && !byAria) unlabeled += 1;
                }}
                return unlabeled;
            }})()"#
        ))
        .await?
        .into_value()?;

    let without_format_guidance: i64 = page
        .evaluate(format!(
            r#"(() => {{
                const f = {TARGET_FORM_JS};
                const inputs = f ? f.querySelectorAll('input[type=file]') : [];
                let missing = 0;
                for (const el of inputs) {{
                    if (el.hasAttribute('accept')) continue;
                    const nearbyText = el.closest('label, div, li, p, fieldset')?.textContent.toLowerCase() ?? '';
                    if (!/pdf|jpg|jpeg|png|doc|mb|size|format/.test(nearbyText)) missing += 1;
                }}
                return missing;
            }})()"#
        ))
        .await?
        .into_value()?;

    let status = if unlabeled > 0 {
        Status::Fail
    } else if without_format_guidance > 0 {
        Status::Warn
    } else {
        Status::Pass
    };
    Ok(result(
        "Required documents",
        status,
        format!(
            "{file_inputs} upload field(s). Without an accessible label: {unlabeled}. \
             Without format/size guidance (accept attribute or nearby text): {without_format_guidance}."
        ),
    ))
}

/// axe-core's `autocomplete-valid` rule checks that a *present*
/// `autocomplete` attribute is well-formed, but never flags a field that
/// looks like name/email/phone/address and has no `autocomplete` at all —
/// a real gap, since autofill is a genuine accessibility aid (WCAG 1.3.5)
/// for anyone with a motor or cognitive impairment, and civic forms are
/// exactly the kind of form people fill out repeatedly.
pub async fn check_autofill_hints(page: &Page) -> Result<CheckResult> {
    let missing: i64 = page
        .evaluate(format!(
            r#"(() => {{
                const guesses = [/name/i, /e-?mail/i, /tel|phone/i, /postal|zip/i, /address/i];
                const f = {TARGET_FORM_JS};
                const fields = f ? f.querySelectorAll(
                    'input[type=text], input[type=email], input[type=tel], input:not([type])'
                ) : [];
                let missing = 0;
                for (const el of fields) {{
                    if (el.disabled || el.offsetParent === null || el.hasAttribute('autocomplete')) continue;
                    const label = (
                        Array.from(document.querySelectorAll('label')).find((l) => l.htmlFor === el.id)?.textContent
                        || el.name
                        || el.id
                        || ''
                    ).toLowerCase();
                    if (guesses.some((re) => re.test(label))) missing += 1;
                }}
                return missing;
            }})()"#
        ))
        .await?
        .into_value()?;

    let status = if missing > 0 {
        Status::Warn
    } else {
        Status::Pass
    };
    Ok(result(
        "Autofill hints",
        status,
        format!(
            "{missing} field(s) that look like name/email/phone/address have no autocomplete \
             attribute — browsers and password managers can't autofill them."
        ),
    ))
}

/// Runs one check's future to completion, converting an Err into a Warn
/// result instead of letting it propagate. `run_all` checks many
/// real-world pages unattended (nightly `monitor` runs); one check
/// hitting a transient CDP hiccup or an unusual page structure shouldn't
/// erase every other check's result for that form.
async fn run_safely(
    name: &str,
    fut: impl std::future::Future<Output = Result<CheckResult>>,
) -> CheckResult {
    match fut.await {
        Ok(check) => check,
        Err(e) => result(name, Status::Warn, format!("check did not complete: {e:#}")),
    }
}

pub async fn run_all(page: &Page, allow_submit: bool, wait_secs: u64) -> Vec<CheckResult> {
    // check_submission_flow runs first deliberately: the later checks
    // (especially validation errors, which breaks one already-valid
    // field to see if just that one gets caught) want a filled form, not
    // a pristine one. But with --submit, a real submission can navigate
    // away or replace the form's markup with a confirmation message —
    // and every check after this one would then silently be inspecting
    // the confirmation page instead of the form. Re-navigate and re-fill
    // so the rest of the checks are always about the form itself. This
    // (like everything else here) is best-effort: a failure here just
    // means the rest of the checks run against whatever page is current,
    // not a lost report for the whole form.
    let original_url = if allow_submit {
        page.evaluate("location.href")
            .await
            .ok()
            .and_then(|r| r.into_value::<String>().ok())
    } else {
        None
    };

    let submission = run_safely("Submission flow", check_submission_flow(page, allow_submit)).await;

    if let Some(url) = original_url {
        let _ = page.goto(&url).await;
        let _ = page.wait_for_navigation().await;
        let _ = fill_form_fields(page).await;
    }

    vec![
        submission,
        run_safely("Accessibility", check_accessibility(page)).await,
        run_safely("Mobile usability", check_mobile_usability(page)).await,
        run_safely("Validation errors", check_validation_errors(page)).await,
        run_safely("Required documents", check_required_documents(page)).await,
        run_safely("Autofill hints", check_autofill_hints(page)).await,
        run_safely(
            "Input persistence",
            check_input_persistence(page, wait_secs),
        )
        .await,
    ]
}

/// Runs every `*.js` file in `dir` against the page through the same
/// `page.evaluate()` mechanism `check_accessibility` uses for axe-core —
/// no Rust plugin ABI needed, since a check only ever needs DOM access
/// and a verdict, and JS-in-the-page already does that. Each script must
/// evaluate (directly, or via a Promise) to `{ status, detail }` with
/// status one of "Pass"/"Warn"/"Fail"; the check's name comes from the
/// filename so scripts can't spoof a built-in check's name.
pub async fn run_custom_checks(page: &Page, dir: &Path) -> Result<Vec<CheckResult>> {
    let mut paths: Vec<_> = std::fs::read_dir(dir)
        .with_context(|| format!("reading --checks-dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("js"))
        .collect();
    paths.sort();

    let mut results = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("custom check")
            .to_string();
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;

        let outcome: Result<serde_json::Value> = async {
            let value = page
                .evaluate(src)
                .await?
                .into_value::<serde_json::Value>()?;
            Ok(value)
        }
        .await;

        results.push(match outcome {
            Ok(value) => match value.get("status").and_then(|s| s.as_str()) {
                Some("Pass") => result(&name, Status::Pass, detail_of(&value)),
                Some("Warn") => result(&name, Status::Warn, detail_of(&value)),
                Some("Fail") => result(&name, Status::Fail, detail_of(&value)),
                _ => result(
                    &name,
                    Status::Warn,
                    "custom check did not return { status: \"Pass\"|\"Warn\"|\"Fail\", detail }",
                ),
            },
            Err(e) => result(&name, Status::Warn, format!("custom check errored: {e}")),
        });
    }
    Ok(results)
}

fn detail_of(value: &serde_json::Value) -> String {
    value
        .get("detail")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn run_safely_converts_an_error_into_a_warn_result_instead_of_propagating() {
        let failing = async { Err::<CheckResult, _>(anyhow::anyhow!("boom")) };
        let outcome = run_safely("Some check", failing).await;
        assert_eq!(outcome.name, "Some check");
        assert_eq!(outcome.status, Status::Warn);
        assert!(outcome.detail.contains("boom"));
    }
}
