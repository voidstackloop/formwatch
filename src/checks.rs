use anyhow::{Context, Result};
use chromiumoxide::Page;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::Duration;

/// The outcome of one check. `Fail` means the check found a real problem
/// with the form; `Warn` covers everything short of that — a heuristic
/// that couldn't be conclusive, a check that couldn't complete, or a
/// finding that's informational rather than a defect; `Pass` means the
/// check found nothing wrong.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Status {
    /// The check found nothing wrong.
    Pass,
    /// Something worth a human's attention, but not a confirmed defect —
    /// a heuristic limit, an inconclusive result, or a check that
    /// couldn't complete (see `detail` for why).
    Warn,
    /// The check found a real problem with the form.
    Fail,
}

/// One check's result: which check it was, what it found, and a
/// human-readable detail string explaining the verdict. This is the
/// unit [`crate::history`] persists and [`crate::report`] renders — the
/// `name` is what ties a check to its history across separate runs, so
/// it should stay stable once a check ships (renaming a built-in check,
/// or a custom check's filename, effectively starts its history over).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckResult {
    /// Which check produced this — a built-in check's fixed name (e.g.
    /// "Accessibility"), or a custom check's filename stem.
    pub name: String,
    /// The verdict.
    pub status: Status,
    /// Human-readable explanation of the verdict — what was found, and
    /// often the raw counts/booleans a report reader would want to see.
    pub detail: String,
    /// A `data:image/png;base64,...` screenshot of the page at the moment
    /// this check finished, present only when `status` isn't `Pass` — a
    /// Pass needs no evidence, and capturing one for every check on every
    /// run would bloat history for no benefit. `#[serde(default)]` so
    /// history files written before this field existed still deserialize.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub screenshot: Option<String>,
}

fn result(name: &str, status: Status, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        detail: detail.into(),
        screenshot: None,
    }
}

impl Status {
    /// The uppercase label shown in terminal and plain-text report
    /// output — `"PASS"`, `"WARN"`, or `"FAIL"`.
    pub fn label(&self) -> &'static str {
        match self {
            Status::Pass => "PASS",
            Status::Warn => "WARN",
            Status::Fail => "FAIL",
        }
    }

    /// The lowercase CSS class used for this status's badge in the HTML
    /// report template.
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

/// Recursively finds every element under `root` matching `sel`, descending
/// into *open* shadow roots as well as regular light-DOM children — plain
/// `querySelectorAll` only ever sees the latter, since a shadow root is by
/// design invisible to selectors run from outside it. A form (or a field,
/// or a label) rendered inside a web component's shadow DOM — common in
/// modern government-site design systems — was previously indistinguishable
/// from a form that plain didn't exist: every check saw an empty page.
///
/// This can't and doesn't reach into a *closed* shadow root (the platform's
/// own encapsulation, deliberately impossible to inspect from outside) or
/// across into a cross-origin `<iframe>` (a separate browsing context this
/// page's JS has no access to at all) — both are real, permanent limits,
/// not bugs, and are called out in the README rather than silently
/// mishandled.
const DEEP_QUERY_JS: &str = "(root, sel) => {
    const out = [];
    const visit = (node) => {
        if (node.matches && node.matches(sel)) out.push(node);
        if (node.shadowRoot) for (const c of node.shadowRoot.children) visit(c);
        for (const c of node.children) visit(c);
    };
    visit(root);
    return out;
}";

/// Resolves to "the form under test": the `<form>` with the most input/
/// select/textarea descendants, not just the first one in document order.
/// A real page often has an incidental header/footer search box as its
/// own tiny `<form>` — blindly using `document.querySelector('form')`
/// (the original approach) would silently fill, click, and validate
/// *that* instead of the actual application form. Picked once per page
/// session and cached on `window` so every check agrees on the same
/// form, and so "the form disappeared" means specifically this form is
/// gone, not merely that some other form on the page still exists.
///
/// Searches with a self-contained copy of [`DEEP_QUERY_JS`]'s logic (not a
/// spliced-in reference to it — this string is used verbatim by callers
/// that also need to embed `DEEP_QUERY_JS` itself alongside it, and Rust's
/// `format!` can't nest one captured `const` inside another) so a form
/// living inside a web component's open shadow root is found at all.
const TARGET_FORM_JS: &str = "(() => {
    if (window.__formwatchFormPicked) {
        // Not document.contains(): that's defined in terms of ordinary
        // (non-shadow-including) descendants, so it's false for a form
        // living inside an open shadow root even while the form is very
        // much still on the page — isConnected is the one DOM primitive
        // that's actually shadow-aware, walking the shadow-including root
        // chain up to the document.
        return window.__formwatchForm?.isConnected ? window.__formwatchForm : null;
    }
    window.__formwatchFormPicked = true;
    const deepQueryAll = (root, sel) => {
        const out = [];
        const visit = (node) => {
            if (node.matches && node.matches(sel)) out.push(node);
            if (node.shadowRoot) for (const c of node.shadowRoot.children) visit(c);
            for (const c of node.children) visit(c);
        };
        visit(root);
        return out;
    };
    const forms = deepQueryAll(document, 'form');
    forms.sort((a, b) => deepQueryAll(b, 'input, select, textarea').length - deepQueryAll(a, 'input, select, textarea').length);
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
                const fields = form ? ({DEEP_QUERY_JS})(form, 'input, textarea, select') : [];
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

/// Ceiling for `wait_for_text_change` after clicking a wizard's Next
/// button — generous enough for a slow, legitimately-animated step
/// transition, but `wait_for_text_change` itself usually returns far
/// sooner (most transitions are synchronous DOM updates, detected on
/// the very first poll).
const STEP_TRANSITION_MAX_WAIT: Duration = Duration::from_millis(3000);

/// Polls `document.body.innerText` every 50ms until it differs from
/// `before` or `max_wait` elapses. Replaces what used to be a fixed
/// sleep, which was both slower than necessary for the common case
/// (most step transitions are synchronous DOM updates that finish in a
/// handful of milliseconds, not several hundred) and less reliable for
/// an uncommon one (a slower, animated transition could take longer
/// than a short fixed wait and get wrongly reported as "this step is
/// broken").
async fn wait_for_text_change(page: &Page, before: &str, max_wait: Duration) -> Result<String> {
    const POLL_INTERVAL: Duration = Duration::from_millis(50);
    let mut waited = Duration::ZERO;
    loop {
        tokio::time::sleep(POLL_INTERVAL).await;
        waited += POLL_INTERVAL;
        let after: String = page
            .evaluate("document.body.innerText")
            .await?
            .into_value()?;
        if after != before || waited >= max_wait {
            return Ok(after);
        }
    }
}

/// Only buttons the user could actually see and click, scoped to the
/// target form — a hidden earlier step's "Next" stays in the DOM after
/// `hidden` is set on it, so without the visibility filter a wizard would
/// loop forever re-clicking a stale control; without the form scope, a
/// button in some other `<form>` on the page (e.g. a header search box)
/// could get misclassified as this form's Next/submit control.
fn visible_buttons_js() -> String {
    format!(
        "(() => {{ const f = {TARGET_FORM_JS}; return f ? ({DEEP_QUERY_JS})(f, 'button, input[type=submit], input[type=button]').filter(b => b.offsetParent !== null) : []; }})()"
    )
}

const LABEL_JS: &str = "b => (b.textContent.trim() || b.value || b.getAttribute('aria-label') || b.title || '').trim()";

/// Matches "advance to the next step" wording. English plus a modest set
/// of other major languages — not remotely exhaustive (full i18n
/// coverage needs a translation database, not a regex), but every
/// addition here can only ever recognize *more* valid Next buttons,
/// never misclassify an unrelated one — unlike a structural
/// "guess which lone button must be Next" heuristic, which could just as
/// easily misclick an unrelated Cancel/Clear button. Non-Latin scripts
/// are deliberately left outside the `\b` word-boundary group: JS `\b`
/// is defined in terms of ASCII `\w`, so wrapping e.g. Arabic or Chinese
/// text in `\b...\b` would silently never match at all.
const NEXT_WORDS_JS: &str =
    r"/\b(next|continue)\b|siguiente|continuar|suivant|weiter|nächste|próximo|التالي|下一步/i";

/// Same idea as `NEXT_WORDS_JS`, for "submit the final step" wording.
const FINAL_WORDS_JS: &str = r"/\b(submit|apply|finish|send|complete)\b|enviar|aplicar|finalizar|soumettre|envoyer|terminer|senden|abschicken|concluir|إرسال|提交/i";

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
            if (buttons.some(b => {NEXT_WORDS_JS}.test(label(b)))) return 'next';
            if (buttons.some(b => b.type === 'submit' || {FINAL_WORDS_JS}.test(label(b)))) return 'final';
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
                page.evaluate(click_matching_js(&format!(
                    "{NEXT_WORDS_JS}.test(label(b))"
                )))
                .await?;
                let after = wait_for_text_change(page, &before, STEP_TRANSITION_MAX_WAIT).await?;
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
                page.evaluate(click_matching_js(&format!(
                    "b.type === 'submit' || {FINAL_WORDS_JS}.test(label(b))"
                )))
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
            .evaluate(format!(
                r#"(() => {{
                    const els = ({DEEP_QUERY_JS})(document, 'a, button, input, select, textarea');
                    let small = 0;
                    for (const el of els) {{
                        const r = el.getBoundingClientRect();
                        if (r.width === 0 && r.height === 0) continue;
                        if (r.width < 44 || r.height < 44) small += 1;
                    }}
                    return small;
                }})()"#,
            ))
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
                const f = {TARGET_FORM_JS};
                const el = f ? ({DEEP_QUERY_JS})(f, 'input[type=text], input:not([type]), textarea')[0] : undefined;
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
            r#"(() => {{
                const f = {TARGET_FORM_JS};
                const el = f ? ({DEEP_QUERY_JS})(f, 'input[type=text], input:not([type]), textarea')[0] : undefined;
                return el?.value === '{marker}';
            }})()"#
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
            "(() => {{ const f = {TARGET_FORM_JS}; return f ? ({DEEP_QUERY_JS})(f, '[required], [aria-required=true]').length : 0; }})()"
        ))
        .await?
        .into_value()?;

    // Fields using only aria-required (no native `required` attribute)
    // are invisible to reportValidity()/:invalid — the browser's native
    // constraint validation was never actually exercising anything for
    // them. Tracked separately so an all-aria-required form doesn't get
    // a false Pass just because nothing native-invalid was found.
    let native_required_count: i64 = page
        .evaluate(format!("(() => {{ const f = {TARGET_FORM_JS}; return f ? ({DEEP_QUERY_JS})(f, '[required]').length : 0; }})()"))
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
                const req = ({DEEP_QUERY_JS})(f, '[required], [aria-required=true]')[0];
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
                const invalid = ({DEEP_QUERY_JS})(f, ':invalid, [aria-invalid=true]');
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
            "(() => {{ const f = {TARGET_FORM_JS}; return f ? ({DEEP_QUERY_JS})(f, 'input[type=file]').length : 0; }})()"
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
                const inputs = f ? ({DEEP_QUERY_JS})(f, 'input[type=file]') : [];
                let unlabeled = 0;
                for (const el of inputs) {{
                    // Not a `label[for="${{el.id}}"]` selector: an id containing a
                    // double quote (legal in HTML) breaks that selector with a
                    // DOMException instead of just not matching.
                    const byFor = el.id && ({DEEP_QUERY_JS})(document, 'label').find((l) => l.htmlFor === el.id)?.textContent.trim();
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
                const inputs = f ? ({DEEP_QUERY_JS})(f, 'input[type=file]') : [];
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
                const fields = f ? ({DEEP_QUERY_JS})(f,
                    'input[type=text], input[type=email], input[type=tel], input:not([type])'
                ) : [];
                let missing = 0;
                for (const el of fields) {{
                    if (el.disabled || el.offsetParent === null || el.hasAttribute('autocomplete')) continue;
                    const label = (
                        ({DEEP_QUERY_JS})(document, 'label').find((l) => l.htmlFor === el.id)?.textContent
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

/// Detects a bot-protection/CAPTCHA challenge (reCAPTCHA, hCaptcha,
/// Cloudflare Turnstile, or a generic "verify you're human" interstitial)
/// on the page. Never a Fail: none of this is evidence the *form* is
/// broken — a real human, unlike a headless browser, sails through a
/// CAPTCHA fine — it's a heuristic limit on what formwatch itself could
/// verify, exactly the kind of thing `Status::Warn` exists for. Without
/// this, a page sitting behind Cloudflare's "Just a moment..." challenge
/// just looks like "No `<form>` element found": a real, misleading
/// finding with the wrong explanation.
pub async fn check_bot_protection(page: &Page) -> Result<CheckResult> {
    let detail: String = page
        .evaluate(
            r#"(() => {
                const title = document.title.toLowerCase();
                const bodyText = document.body.innerText.toLowerCase();

                const found = [];
                if (document.querySelector('.g-recaptcha, iframe[src*="recaptcha"], script[src*="recaptcha"]')
                    || typeof window.grecaptcha !== 'undefined') found.push('reCAPTCHA');
                if (document.querySelector('.h-captcha, iframe[src*="hcaptcha"], script[src*="hcaptcha"]')
                    || typeof window.hcaptcha !== 'undefined') found.push('hCaptcha');
                if (document.querySelector('.cf-turnstile, iframe[src*="challenges.cloudflare.com"]')) found.push('Cloudflare Turnstile');
                const cloudflareChallengePage = title.includes('just a moment')
                    || bodyText.includes('checking your browser')
                    || bodyText.includes('checking if the site connection is secure');
                if (cloudflareChallengePage) found.push('Cloudflare challenge page');
                if (found.length === 0 && /verify you.{0,3}re (a )?human|are you a robot|prove you.{0,3}re not a robot|complete the (security check|challenge)/.test(bodyText)) {
                    found.push('generic bot-check wording');
                }

                if (found.length === 0) return 'none';
                const formStillPresent = document.querySelector('form') !== null;
                const blocking = cloudflareChallengePage || !formStillPresent;
                return JSON.stringify({ found, blocking });
            })()"#,
        )
        .await?
        .into_value()?;

    if detail == "none" {
        return Ok(result(
            "Bot protection",
            Status::Pass,
            "No bot-protection challenge detected.",
        ));
    }

    let parsed: serde_json::Value = serde_json::from_str(&detail)?;
    let found: Vec<String> = parsed["found"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    let blocking = parsed["blocking"].as_bool().unwrap_or(false);
    let names = found.join(", ");

    Ok(result(
        "Bot protection",
        Status::Warn,
        if blocking {
            format!(
                "{names} detected, with no form visible — automated testing was likely blocked \
                 before it ever reached the real form. This isn't evidence the form itself is \
                 broken; a real user (unlike formwatch) would pass this challenge."
            )
        } else {
            format!(
                "{names} detected on the page. The form itself was still found and checked, but \
                 real submission (--submit) will be blocked by the challenge — formwatch can't \
                 and shouldn't try to solve it."
            )
        },
    ))
}

/// Default ceiling for a single check — generous for a slow real-world
/// page, but bounded. `check_submission_flow` gets its own longer
/// ceiling (an 8-step wizard can legitimately take a while) and
/// `check_input_persistence` gets one that scales with its own
/// user-requested `--wait`, rather than sharing this default.
const CHECK_TIMEOUT: Duration = Duration::from_secs(20);

/// Captures a full-page PNG screenshot as a ready-to-embed
/// `data:image/png;base64,...` URI. Best-effort: a screenshot is
/// supporting evidence, not the check result itself, so a capture
/// failure (e.g. a page that navigated away mid-check) returns `None`
/// rather than turning a real check result into an error.
async fn capture_screenshot(page: &Page) -> Option<String> {
    use base64::Engine;
    let png = page
        .screenshot(
            chromiumoxide::page::ScreenshotParams::builder()
                .full_page(true)
                .build(),
        )
        .await
        .ok()?;
    Some(format!(
        "data:image/png;base64,{}",
        base64::engine::general_purpose::STANDARD.encode(png)
    ))
}

/// Runs one check's future to completion, converting an Err *or* a hang
/// past `timeout` into a Warn result instead of letting it propagate or
/// block forever. `run_all` checks many real-world pages unattended
/// (nightly `monitor` runs); one check hitting a transient CDP hiccup,
/// an unusual page structure, or a page whose JS genuinely never
/// resolves (an infinite loop, a fetch() that never returns) shouldn't
/// erase every other check's result for that form, or hang the whole
/// tool indefinitely — previously there was no timeout anywhere in the
/// check engine at all.
///
/// Pure conversion logic, deliberately kept separate from [`run_safely`]
/// so its error/timeout-handling can be unit tested without a real Page
/// (a synthetic future is enough) — [`run_safely`]'s extra step of
/// capturing a screenshot genuinely needs one.
async fn run_with_timeout(
    name: &str,
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<CheckResult>>,
) -> CheckResult {
    match tokio::time::timeout(timeout, fut).await {
        Ok(Ok(check)) => check,
        Ok(Err(e)) => result(name, Status::Warn, format!("check did not complete: {e:#}")),
        Err(_) => result(
            name,
            Status::Warn,
            format!(
                "check did not complete: timed out after {}s",
                timeout.as_secs()
            ),
        ),
    }
}

/// [`run_with_timeout`], additionally attaching a screenshot to any
/// non-Pass result (a real Fail/Warn, or a synthetic one from
/// `run_with_timeout`'s own error/timeout branches) — a report reader
/// shouldn't have to re-run formwatch against a possibly-already-changed
/// page just to see what the check actually saw.
async fn run_safely(
    page: &Page,
    name: &str,
    timeout: Duration,
    fut: impl std::future::Future<Output = Result<CheckResult>>,
) -> CheckResult {
    let mut check = run_with_timeout(name, timeout, fut).await;
    if check.status != Status::Pass {
        check.screenshot = capture_screenshot(page).await;
    }
    check
}

/// Runs every built-in check against `page` and returns all of their
/// results — always, even if one check errors or times out internally,
/// or a real `--submit` navigates away from the form partway through.
/// `wait_secs` is passed through to [`check_input_persistence`];
/// `allow_submit` gates whether [`check_submission_flow`] clicks the
/// real submit button.
pub async fn run_all(page: &Page, allow_submit: bool, wait_secs: u64) -> Vec<CheckResult> {
    // An 8-step wizard, each step waiting up to STEP_TRANSITION_MAX_WAIT,
    // can legitimately take longer than the default CHECK_TIMEOUT.
    const WIZARD_TIMEOUT: Duration = Duration::from_secs(60);
    // The user explicitly controls how long this one waits (README
    // advertises passing a large --wait to test a real session
    // timeout) — its own ceiling has to scale with that, not share the
    // fixed default, or a legitimate long --wait would get cut off.
    let input_persistence_timeout = Duration::from_secs(wait_secs.saturating_add(30));
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

    let submission = run_safely(
        page,
        "Submission flow",
        WIZARD_TIMEOUT,
        check_submission_flow(page, allow_submit),
    )
    .await;

    if let Some(url) = original_url {
        let _ = page.goto(&url).await;
        let _ = page.wait_for_navigation().await;
        let _ = fill_form_fields(page).await;
    }

    vec![
        run_safely(
            page,
            "Bot protection",
            CHECK_TIMEOUT,
            check_bot_protection(page),
        )
        .await,
        submission,
        run_safely(
            page,
            "Accessibility",
            CHECK_TIMEOUT,
            check_accessibility(page),
        )
        .await,
        run_safely(
            page,
            "Mobile usability",
            CHECK_TIMEOUT,
            check_mobile_usability(page),
        )
        .await,
        run_safely(
            page,
            "Validation errors",
            CHECK_TIMEOUT,
            check_validation_errors(page),
        )
        .await,
        run_safely(
            page,
            "Required documents",
            CHECK_TIMEOUT,
            check_required_documents(page),
        )
        .await,
        run_safely(
            page,
            "Autofill hints",
            CHECK_TIMEOUT,
            check_autofill_hints(page),
        )
        .await,
        run_safely(
            page,
            "Input persistence",
            input_persistence_timeout,
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

        let outcome: std::result::Result<Result<serde_json::Value>, tokio::time::error::Elapsed> =
            tokio::time::timeout(CHECK_TIMEOUT, async {
                let value = page
                    .evaluate(src)
                    .await?
                    .into_value::<serde_json::Value>()?;
                Ok(value)
            })
            .await;

        let mut check = match outcome {
            Ok(Ok(value)) => match value.get("status").and_then(|s| s.as_str()) {
                Some("Pass") => result(&name, Status::Pass, detail_of(&value)),
                Some("Warn") => result(&name, Status::Warn, detail_of(&value)),
                Some("Fail") => result(&name, Status::Fail, detail_of(&value)),
                _ => result(
                    &name,
                    Status::Warn,
                    "custom check did not return { status: \"Pass\"|\"Warn\"|\"Fail\", detail }",
                ),
            },
            Ok(Err(e)) => result(&name, Status::Warn, format!("custom check errored: {e}")),
            // A custom check that hangs (an infinite loop, a Promise that
            // never resolves) shouldn't block every other check, or the
            // whole tool, forever.
            Err(_) => result(
                &name,
                Status::Warn,
                format!("custom check timed out after {}s", CHECK_TIMEOUT.as_secs()),
            ),
        };
        if check.status != Status::Pass {
            check.screenshot = capture_screenshot(page).await;
        }
        results.push(check);
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
    async fn run_with_timeout_converts_an_error_into_a_warn_result_instead_of_propagating() {
        let failing = async { Err::<CheckResult, _>(anyhow::anyhow!("boom")) };
        let outcome = run_with_timeout("Some check", Duration::from_secs(5), failing).await;
        assert_eq!(outcome.name, "Some check");
        assert_eq!(outcome.status, Status::Warn);
        assert!(outcome.detail.contains("boom"));
    }

    #[tokio::test]
    async fn run_with_timeout_converts_a_hang_into_a_warn_result_instead_of_blocking_forever() {
        // A future that never resolves (an infinite JS loop, a Promise
        // that never settles) used to hang run_all — and by extension
        // the whole `monitor` run — indefinitely, with no way to recover.
        let hangs = std::future::pending::<Result<CheckResult>>();
        let outcome = run_with_timeout("Some check", Duration::from_millis(50), hangs).await;
        assert_eq!(outcome.name, "Some check");
        assert_eq!(outcome.status, Status::Warn);
        assert!(
            outcome.detail.contains("timed out"),
            "got: {}",
            outcome.detail
        );
    }
}
