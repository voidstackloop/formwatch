//! Exercises the check engine against the fixtures in `fixtures/`, which
//! have deliberate, known bugs planted in them. Needs a real Chrome (or
//! formwatch's own fetcher — see src/browser.rs) since these launch an
//! actual browser; there's no way to test `page.evaluate()` behavior
//! without one.

use chromiumoxide::{Browser, Page};
use formwatch::{browser, checks};
use tokio::task::JoinHandle;

async fn open_fixture(name: &str) -> (Browser, Page, JoinHandle<()>) {
    let (browser, handle) = browser::launch(false).await.expect("launch chrome");
    let path = std::fs::canonicalize(format!("fixtures/{name}")).expect("fixture exists");
    let url = format!("file://{}", path.display());
    let page = browser::open(&browser, &url).await.expect("open fixture");
    (browser, page, handle)
}

fn find<'a>(results: &'a [checks::CheckResult], name: &str) -> &'a checks::CheckResult {
    results
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no check named {name:?} in {results:#?}"))
}

fn status_of(results: &[checks::CheckResult], name: &str) -> checks::Status {
    find(results, name).status
}

#[tokio::test]
async fn single_page_fixture_catches_its_planted_bugs() {
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let results = checks::run_all(&page, false, 1).await;

    // fixtures/test-form.html deliberately has: a missing alt attribute,
    // an unlabeled required field, an unlabeled file upload with no
    // format guidance, and tiny tap targets — each should surface as a
    // Fail on the check that's supposed to catch it.
    assert_eq!(status_of(&results, "Accessibility"), checks::Status::Fail);
    assert_eq!(
        status_of(&results, "Validation errors"),
        checks::Status::Fail
    );
    assert_eq!(
        status_of(&results, "Required documents"),
        checks::Status::Fail
    );
    // fullname has autocomplete="name" (should be excluded); email has
    // none and its label matches the "email" guess (should be counted).
    let autofill = find(&results, "Autofill hints");
    assert_eq!(autofill.status, checks::Status::Warn);
    assert!(
        autofill.detail.starts_with("1 field(s)"),
        "got: {}",
        autofill.detail
    );

    // The form itself is well-formed and single-step, so filling and
    // validating it (without --submit) should pass cleanly. The fixture's
    // date field has min="2030-01-01" specifically to catch a regression
    // where formwatch fills date fields with a hardcoded past date,
    // tripping the field's own min constraint for no real reason.
    let submission = find(&results, "Submission flow");
    assert_eq!(submission.status, checks::Status::Pass);
    assert!(
        submission.detail.contains("reports valid"),
        "expected the date field's dummy value to respect min=\"2030-01-01\", got: {}",
        submission.detail
    );
    assert_eq!(
        status_of(&results, "Input persistence"),
        checks::Status::Pass
    );
}

#[tokio::test]
async fn non_pass_checks_get_a_screenshot_and_pass_checks_dont() {
    // fixtures/test-form.html's Accessibility check is a real Fail;
    // its Submission flow check is a real Pass. A report reader needs
    // to see evidence for the former without re-running formwatch
    // against a possibly-already-changed page — but capturing one for
    // every check regardless of outcome would just bloat history for
    // no benefit.
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let results = checks::run_all(&page, false, 1).await;

    let accessibility = find(&results, "Accessibility");
    assert_eq!(accessibility.status, checks::Status::Fail);
    let shot = accessibility
        .screenshot
        .as_deref()
        .expect("a Fail should carry a screenshot");
    assert!(
        shot.starts_with("data:image/png;base64,"),
        "got: {}",
        &shot[..shot.len().min(40)]
    );

    let submission = find(&results, "Submission flow");
    assert_eq!(submission.status, checks::Status::Pass);
    assert!(
        submission.screenshot.is_none(),
        "a Pass shouldn't carry a screenshot"
    );
}

#[tokio::test]
async fn multi_step_wizard_is_walked_to_the_final_submit() {
    let (_browser, page, _handle) = open_fixture("multi-step-form.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow");

    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("Advanced through 2 step"),
        "expected the check to report walking both Next/Continue steps, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn slow_but_legitimate_step_transition_is_not_reported_as_broken() {
    // fixtures/slow-step-transition.html's Next button takes 1200ms to
    // show the next step (an animated transition, a network-dependent
    // step). Regression test for a bug where a fixed 700ms wait after
    // clicking Next reported this working form as "that step appears
    // broken" just because the transition took longer than the
    // hardcoded wait — replaced with polling up to a much longer
    // ceiling, which also resolves much faster for the common case of a
    // synchronous (near-instant) transition.
    let (_browser, page, _handle) = open_fixture("slow-step-transition.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow");
    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("Advanced through 1 step"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn unrelated_timeout_copy_does_not_false_positive_session_warning() {
    // fixtures/multi-step-form.html has a footer note mentioning "times
    // out"/"timed out" in an unrelated (network, not session) context —
    // regression test for a bug where a bare "timed out" match anywhere
    // on the page, not anchored to "session", triggered a false Warn.
    let (_browser, page, _handle) = open_fixture("multi-step-form.html").await;
    let result = checks::check_input_persistence(&page, 1)
        .await
        .expect("check_input_persistence");

    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("session-timeout wording seen=false"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn required_checkbox_as_first_field_is_actually_invalidated() {
    // fixtures/checkbox-first-form.html's only required field is a
    // checkbox. Fill it (as run_all's check_submission_flow does, making
    // it checked/valid) before checking validation — regression test for
    // a bug where forcing "one required field invalid" set .value on a
    // checkbox, which is a silent no-op, so the check never actually
    // exercised the browser's invalid-field handling.
    let (_browser, page, _handle) = open_fixture("checkbox-first-form.html").await;
    checks::check_submission_flow(&page, false)
        .await
        .expect("fill+validate");

    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    assert!(
        result
            .detail
            .contains("Native validation blocked submit: true"),
        "expected forcing the required checkbox unchecked to block validation, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn multi_form_page_submits_the_bigger_form_not_the_header_search_box() {
    // fixtures/multi-form-page.html has a tiny header search <form> before
    // the real "apply-form". Regression test for a bug where every JS
    // selector was unscoped (`document.querySelector('form')`,
    // `document.querySelectorAll('form button, ...')`), so filling,
    // clicking, and validating operated on whichever form happened to be
    // first in the document — the search box, not the form under test.
    // With --submit this used to actually click the wrong live control.
    let (_browser, page, _handle) = open_fixture("multi-form-page.html").await;
    let result = checks::check_submission_flow(&page, true)
        .await
        .expect("check_submission_flow");
    assert_eq!(result.status, checks::Status::Pass);

    let final_url: String = page
        .evaluate("location.href")
        .await
        .unwrap()
        .into_value()
        .unwrap();
    assert!(
        final_url.contains("applicant="),
        "expected the apply-form's fields in the URL, got: {final_url}"
    );
    assert!(
        !final_url.contains("?q="),
        "the header search form's field leaked into the URL: {final_url}"
    );
}

#[tokio::test]
async fn multi_form_page_validation_targets_the_bigger_form() {
    let (_browser, page, _handle) = open_fixture("multi-form-page.html").await;
    checks::check_submission_flow(&page, false)
        .await
        .expect("fill+validate");
    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    // apply-form has 2 required fields; the header search form has 0.
    assert!(
        result.detail.starts_with("2 required field(s)"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn input_persistence_targets_the_real_form_not_a_header_search_box() {
    // fixtures/multi-form-persistence.html's header search field clears
    // itself shortly after typing (an unrelated widget's own behavior) —
    // the real apply-form's field is untouched. Regression test for a
    // bug where the field selector was unscoped, so on a multi-form page
    // this check could test the search box instead of the form under
    // test, reporting a false Fail ("input lost") for a session-handling
    // problem that doesn't actually exist on the real form.
    let (_browser, page, _handle) = open_fixture("multi-form-persistence.html").await;
    let result = checks::check_input_persistence(&page, 1)
        .await
        .expect("check_input_persistence");
    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("input retained=true"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn input_that_vanishes_with_no_explanation_is_a_fail() {
    // fixtures/input-vanishes-form.html clears the field on its own,
    // shortly after the user types, with nothing on the page explaining
    // why — the core "lost input" scenario this check exists to catch,
    // never directly asserted before (every other input-persistence test
    // asserts the Pass path).
    let (_browser, page, _handle) = open_fixture("input-vanishes-form.html").await;
    let result = checks::check_input_persistence(&page, 1)
        .await
        .expect("check_input_persistence");
    assert_eq!(result.status, checks::Status::Fail, "got: {result:?}");
    assert!(
        result.detail.contains("input retained=false"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn input_loss_explained_by_a_session_notice_is_a_warn_not_a_fail() {
    // fixtures/session-timeout-warning-form.html surfaces a session-expiry
    // notice — the site explaining an apparent loss should be a Warn, not
    // the same Fail as a silent, unexplained one.
    let (_browser, page, _handle) = open_fixture("session-timeout-warning-form.html").await;
    let result = checks::check_input_persistence(&page, 1)
        .await
        .expect("check_input_persistence");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("session-timeout wording seen=true"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn no_text_field_to_test_is_reported_not_silently_skipped() {
    // fixtures/no-text-field-form.html has only a checkbox and a select —
    // check_input_persistence has nothing to plant a marker in and should
    // say so plainly, rather than the caller mistaking a missing check for
    // an untested-and-fine one.
    let (_browser, page, _handle) = open_fixture("no-text-field-form.html").await;
    let result = checks::check_input_persistence(&page, 1)
        .await
        .expect("check_input_persistence");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("No text field found to test"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn icon_only_next_button_is_recognized_via_aria_label() {
    // fixtures/icon-only-buttons.html's Next/Submit buttons have no text,
    // only an SVG icon child and aria-label — regression test for two
    // compounding bugs: label() not checking aria-label at all, and
    // (once added) checking untrimmed textContent first, which is
    // truthy whitespace from indentation around the <svg> child and
    // short-circuits past aria-label before ever reaching it.
    let (_browser, page, _handle) = open_fixture("icon-only-buttons.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow");
    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("Advanced through 1 step"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn quote_in_element_id_does_not_break_the_label_lookup() {
    // fixtures/quote-in-id-form.html's file input has id='weird"id' (legal
    // HTML). Regression test for building a `label[for="${el.id}"]` CSS
    // selector string directly from an id — a quote in the id breaks the
    // selector with a DOMException, which run_safely then reported as
    // "check did not complete" instead of a real answer.
    let (_browser, page, _handle) = open_fixture("quote-in-id-form.html").await;
    let result = checks::check_required_documents(&page)
        .await
        .expect("check_required_documents");
    assert!(
        result.detail.contains("Without an accessible label: 0"),
        "expected the label to be found despite the quote in the id, got: {}",
        result.detail
    );
    // The label is found, but this fixture's file input has no `accept`
    // attribute and no nearby text naming a format/size — the Warn branch
    // of check_required_documents, never directly asserted before.
    assert_eq!(result.status, checks::Status::Warn);
    assert!(
        result
            .detail
            .contains("Without format/size guidance (accept attribute or nearby text): 1"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn required_document_upload_with_accept_and_a_label_passes() {
    // fixtures/well-documented-upload-form.html's file input is labeled and
    // has an `accept` attribute — the Pass branch of
    // check_required_documents, never directly asserted before (only the
    // Fail and Warn branches were).
    let (_browser, page, _handle) = open_fixture("well-documented-upload-form.html").await;
    let result = checks::check_required_documents(&page)
        .await
        .expect("check_required_documents");
    assert_eq!(result.status, checks::Status::Pass, "got: {result:?}");
}

#[tokio::test]
async fn form_with_no_required_fields_warns_instead_of_a_false_pass() {
    // fixtures/no-required-fields-form.html has nothing marked required or
    // aria-required — native constraint validation has nothing to
    // exercise, so check_validation_errors should say so (Warn) rather
    // than claim a false Pass. The required_count == 0 early return, never
    // directly asserted before.
    let (_browser, page, _handle) = open_fixture("no-required-fields-form.html").await;
    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("couldn't exercise validation"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn page_text_promising_a_document_upload_with_no_file_field_is_flagged() {
    // fixtures/mentions-documents-no-upload-form.html tells the user to
    // upload a document but never actually provides a file input — the
    // "0 file inputs" Warn branch of check_required_documents, never
    // directly asserted before.
    let (_browser, page, _handle) = open_fixture("mentions-documents-no-upload-form.html").await;
    let result = checks::check_required_documents(&page)
        .await
        .expect("check_required_documents");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("no file upload field was found"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn aria_required_only_fields_get_a_warn_not_a_false_pass() {
    // fixtures/aria-required-not-attribute.html's fields use only
    // aria-required (no native `required` attribute) with real custom JS
    // validation on submit. Native reportValidity()/:invalid never see
    // aria-required, so without this fix the check always reported a
    // false Pass — "native validation" that never actually ran anything.
    let (_browser, page, _handle) = open_fixture("aria-required-not-attribute.html").await;
    checks::check_submission_flow(&page, false)
        .await
        .expect("fill+validate");
    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    assert_eq!(result.status, checks::Status::Warn);
    assert!(
        result.detail.contains("couldn't exercise them"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn keyup_gated_next_button_is_enabled_by_filling_the_field() {
    // fixtures/keyup-gated-next.html's Next button starts disabled and
    // is only re-enabled by a `keyup` listener on the text field.
    // Regression test for a bug where formwatch only dispatched
    // input/change events, so a real, working form using this common
    // vanilla-JS pattern (char-counters, "enable on type") looked
    // permanently stuck.
    let (_browser, page, _handle) = open_fixture("keyup-gated-next.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow");
    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("Advanced through 1 step"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn wizard_step_buttons_labeled_in_arabic_are_recognized() {
    // fixtures/rtl-arabic-form.html's wizard uses Arabic wording
    // ("التالي" = Next, "إرسال" = Submit) — regression test for the
    // button classifier only recognizing English Next/Continue/Submit
    // wording. Not full i18n coverage (that needs a translation
    // database), just confirming the widened word list actually works
    // end to end, not just in isolation.
    let (_browser, page, _handle) = open_fixture("rtl-arabic-form.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow");
    assert_eq!(result.status, checks::Status::Pass);
    assert!(
        result.detail.contains("Advanced through 1 step"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn multi_step_wizard_real_submit_reaches_the_thank_you_page() {
    let (_browser, page, _handle) = open_fixture("multi-step-form.html").await;
    let result = checks::check_submission_flow(&page, true)
        .await
        .expect("check_submission_flow");

    assert_eq!(result.status, checks::Status::Pass);
    assert!(result.detail.contains("success=true"));
}

#[tokio::test]
async fn full_run_with_submit_still_checks_the_form_not_the_confirmation_page() {
    // fixtures/multi-step-form.html's confirmation fully replaces
    // document.body.innerHTML on real submit — regression test for a bug
    // where every check after check_submission_flow then silently ran
    // against that confirmation markup (no <form> at all) instead of the
    // form itself.
    let (_browser, page, _handle) = open_fixture("multi-step-form.html").await;
    let results = checks::run_all(&page, true, 1).await;

    let submission = find(&results, "Submission flow");
    assert_eq!(submission.status, checks::Status::Pass);
    assert!(submission.detail.contains("success=true"));

    // If the bug were present, this would see 0 required fields (the
    // confirmation page has no <form>) instead of the form's real 2.
    let validation = find(&results, "Validation errors");
    assert!(
        validation.detail.starts_with("2 required field(s)"),
        "expected the form's required fields, not the confirmation page's, got: {}",
        validation.detail
    );
}

#[tokio::test]
async fn mobile_usability_flags_small_tap_targets() {
    // fixtures/test-form.html has a deliberately tiny 20x20px button
    // (`.tiny-btn`). Called directly rather than only inferred through
    // run_all's aggregate result, so a regression in the tap-target scan
    // itself would be caught here specifically.
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let result = checks::check_mobile_usability(&page)
        .await
        .expect("check_mobile_usability");
    assert_eq!(result.status, checks::Status::Warn);
    assert!(
        !result.detail.contains("Tap targets under 44x44px: 0"),
        "expected the tiny button to be counted, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn mobile_usability_flags_horizontal_overflow() {
    // fixtures/horizontal-overflow.html has a 1200px-wide element on a
    // page whose meta viewport makes 375px the layout width. Regression
    // coverage for the overflow half of the mobile check, which the
    // tap-target test above never exercised.
    let (_browser, page, _handle) = open_fixture("horizontal-overflow.html").await;
    let result = checks::check_mobile_usability(&page)
        .await
        .expect("check_mobile_usability");
    assert!(
        result
            .detail
            .contains("Horizontal overflow at 375px width: true"),
        "expected overflow to be detected, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn run_custom_checks_runs_every_script_and_names_by_filename() {
    // fixtures/custom-checks/ has three scripts covering the three
    // branches run_custom_checks handles: a real Pass, a real Fail, and
    // a malformed return value that should degrade to a Warn rather than
    // erroring the whole run. Previously exercised only manually via the
    // CLI (e.g. while verifying the custom-check timeout) — this was the
    // only public function in the whole library with zero automated
    // coverage of its own.
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let dir = std::path::Path::new("fixtures/custom-checks");
    let results = checks::run_custom_checks(&page, dir)
        .await
        .expect("run_custom_checks");
    assert_eq!(results.len(), 3);

    let pass = find(&results, "always-pass");
    assert_eq!(pass.status, checks::Status::Pass);
    assert_eq!(pass.detail, "always fine");

    let fail = find(&results, "always-fail");
    assert_eq!(fail.status, checks::Status::Fail);
    assert_eq!(fail.detail, "always broken");

    let malformed = find(&results, "malformed");
    assert_eq!(malformed.status, checks::Status::Warn);
    assert!(
        malformed.detail.contains("did not return"),
        "got: {}",
        malformed.detail
    );
}

#[tokio::test]
async fn form_inside_an_open_shadow_root_is_found_and_checked() {
    // fixtures/shadow-dom-form.html renders its actual <form> inside an
    // open shadow root on a custom element — the way many modern
    // government-site design systems structure forms. Plain
    // document.querySelectorAll('form') can't see into a shadow root at
    // all, so before the deep-query fix every check here silently saw an
    // empty page (0 fields, 0 required, no upload field) instead of a
    // real result.
    let (_browser, page, _handle) = open_fixture("shadow-dom-form.html").await;
    let results = checks::run_all(&page, false, 1).await;

    let submission = find(&results, "Submission flow");
    assert_eq!(
        submission.status,
        checks::Status::Pass,
        "got: {submission:?}"
    );
    assert!(
        submission.detail.contains("filled 1 field"),
        "expected the shadow-rooted text field to be found and filled, got: {}",
        submission.detail
    );

    let validation = find(&results, "Validation errors");
    assert!(
        validation.detail.starts_with("1 required field(s)"),
        "expected the shadow-rooted required field to be counted, got: {}",
        validation.detail
    );

    let documents = find(&results, "Required documents");
    assert!(
        documents.detail.starts_with("1 upload field(s)"),
        "expected the shadow-rooted file input to be found, got: {}",
        documents.detail
    );
}

#[tokio::test]
async fn deep_query_recurses_through_two_levels_of_nested_shadow_roots() {
    // fixtures/nested-shadow-dom-form.html nests a second open shadow
    // root *inside* the first one's <form> — the outer host has a shadow
    // root containing the form, and the form itself contains another
    // host whose own shadow root contains the actual required field.
    //
    // shadow-dom-form.html's single level of nesting doesn't actually
    // exercise DEEP_QUERY_JS's own shadow-crossing at all: once
    // TARGET_FORM_JS's separate, self-contained copy of the walk resolves
    // `f`, the input is a same-tree descendant of `f` (both directly in
    // shadow root #1), so a plain non-shadow-aware querySelectorAll
    // starting at `f` would already find it. Only a *second*, independent
    // shadow root nested inside the form — as here — requires
    // DEEP_QUERY_JS itself (used by check_validation_errors and friends,
    // called with `f` as the root, not `document`) to actually cross a
    // shadow boundary. Confirmed by temporarily stripping DEEP_QUERY_JS's
    // shadow-crossing entirely: shadow-dom-form.html's test still passed
    // (as predicted), while this one failed.
    let (_browser, page, _handle) = open_fixture("nested-shadow-dom-form.html").await;
    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    assert!(
        result.detail.starts_with("1 required field(s)"),
        "expected the doubly-nested required field to be found, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn closed_shadow_root_degrades_to_a_clean_no_form_found_not_a_crash() {
    // fixtures/closed-shadow-dom-form.html's form lives inside a CLOSED
    // shadow root — genuinely, deliberately impossible to inspect from
    // outside (the platform's own encapsulation, not a formwatch gap).
    // The deep-query walk's `if (node.shadowRoot)` guard should simply
    // skip it (a closed root's `.shadowRoot` getter returns null to
    // outside code), degrading to an honest "no form found" rather than
    // throwing partway through a check.
    let (_browser, page, _handle) = open_fixture("closed-shadow-dom-form.html").await;
    let result = checks::check_submission_flow(&page, false)
        .await
        .expect("check_submission_flow should not error, just report no form");
    assert_eq!(result.status, checks::Status::Fail);
    assert!(
        result.detail.contains("No <form> element found"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn recaptcha_widget_on_a_real_form_warns_but_still_finds_the_form() {
    // fixtures/recaptcha-widget-form.html has a real, testable form that
    // also embeds a reCAPTCHA widget — common on real government forms.
    // This should Warn (not Fail: it's not evidence the form is broken)
    // and note the form itself was still found.
    let (_browser, page, _handle) = open_fixture("recaptcha-widget-form.html").await;
    let result = checks::check_bot_protection(&page)
        .await
        .expect("check_bot_protection");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("reCAPTCHA"),
        "got: {}",
        result.detail
    );
    assert!(
        result.detail.contains("form itself was still found"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn cloudflare_challenge_page_is_reported_as_blocking_not_a_broken_form() {
    // fixtures/cloudflare-challenge-page.html mimics the full-page "Just
    // a moment..." interstitial Cloudflare shows instead of the real
    // site — no <form> at all, since access itself is blocked. Without
    // this check, check_submission_flow's "No <form> element found"
    // would be the only signal, misleadingly implying the form is broken
    // rather than "automated testing got blocked before reaching it."
    let (_browser, page, _handle) = open_fixture("cloudflare-challenge-page.html").await;
    let result = checks::check_bot_protection(&page)
        .await
        .expect("check_bot_protection");
    assert_eq!(result.status, checks::Status::Warn, "got: {result:?}");
    assert!(
        result.detail.contains("Cloudflare challenge page"),
        "got: {}",
        result.detail
    );
    assert!(
        result.detail.contains("no form visible"),
        "got: {}",
        result.detail
    );
}

#[tokio::test]
async fn ordinary_form_with_no_challenge_passes_bot_protection() {
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let result = checks::check_bot_protection(&page)
        .await
        .expect("check_bot_protection");
    assert_eq!(result.status, checks::Status::Pass, "got: {result:?}");
}

#[tokio::test]
async fn a_described_required_field_is_not_counted_as_unlabeled_invalid() {
    // Regression test found by building the demo site: the invalid-field
    // query was scoped at the form with a bare `:invalid`, and `form:invalid`
    // matches whenever any control inside is invalid — so the form element
    // itself (which has no aria-describedby) was counted as an unlabeled
    // invalid field. A form whose *actual* invalid field has a proper
    // accessible description was therefore failed.
    let (_browser, page, _handle) = open_fixture("described-required-form.html").await;
    checks::check_submission_flow(&page, false)
        .await
        .expect("fill+validate");
    let result = checks::check_validation_errors(&page)
        .await
        .expect("check_validation_errors");
    assert!(
        result
            .detail
            .contains("without a screen-reader-visible error message: 0"),
        "got: {}",
        result.detail
    );
    assert_eq!(result.status, checks::Status::Pass, "got: {result:?}");
}

#[tokio::test]
async fn llm_semantic_checks_run_against_a_page_with_the_mock_provider() {
    // The LLM checks are optional and network-backed in production; the
    // `mock` provider makes the whole pipeline (extraction -> redaction ->
    // prompt -> parse -> verdict -> status) exercisable offline. Both
    // checks must appear, and because the mock verdict scores 4 (>= the
    // default threshold of 3), both pass — and crucially the detail says
    // "score 4", proving extraction actually found text to judge rather
    // than short-circuiting on the "nothing to review" path.
    let (_browser, page, _handle) = open_fixture("llm-semantic-form.html").await;
    let options = formwatch::llm::LlmOptions {
        enabled: true,
        provider: formwatch::llm::provider::Provider::Mock,
        cache: false,
        ..formwatch::llm::LlmOptions::default()
    };
    let results = formwatch::llm::run_semantic_checks(&page, &options, false).await;

    assert_eq!(results.len(), 2, "expected two LLM checks: {results:#?}");
    for check in &results {
        assert!(check.name.contains("LLM"), "got name {:?}", check.name);
        assert_eq!(
            check.status,
            checks::Status::Pass,
            "mock score 4 should pass: {check:?}"
        );
        assert!(
            check.detail.contains("score 4"),
            "check should have judged real extracted text, got: {}",
            check.detail
        );
    }
}

#[tokio::test]
async fn duplicate_name_between_two_text_fields_is_a_fail() {
    // fixtures/duplicate-name-form.html has two unrelated text inputs
    // both named "email" — on submit, standard form encoding keeps only
    // one of the two values, with zero client-side signal that anything
    // was lost. The fixture also has a legitimate radio group and a
    // legitimate checkbox group sharing a name each; neither should be
    // flagged, since that's how those groups are meant to work.
    let (_browser, page, _handle) = open_fixture("duplicate-name-form.html").await;
    let result = checks::check_duplicate_names(&page)
        .await
        .expect("check_duplicate_names");
    assert_eq!(result.status, checks::Status::Fail, "got: {result:?}");
    assert!(result.detail.contains("email"), "got: {}", result.detail);
    assert!(
        !result.detail.contains("contact"),
        "a legitimate radio group must not be flagged, got: {}",
        result.detail
    );
    assert!(
        !result.detail.contains("interests"),
        "a legitimate checkbox group must not be flagged, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn form_with_no_duplicate_names_passes() {
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let result = checks::check_duplicate_names(&page)
        .await
        .expect("check_duplicate_names");
    assert_eq!(result.status, checks::Status::Pass, "got: {result:?}");
}

#[tokio::test]
async fn required_looking_label_with_no_required_attribute_is_a_fail() {
    // fixtures/required-indicator-mismatch-form.html's "Full name *"
    // looks required to any user but was never wired up with the real
    // attribute — native validation and screen readers both treat it as
    // optional. "Email *" is the correctly-wired case (same visual
    // promise, backed by `required`), proving no false positive on a
    // form that got it right; "Comments" has neither a required-looking
    // label nor the attribute, and is irrelevant to this check either way.
    let (_browser, page, _handle) = open_fixture("required-indicator-mismatch-form.html").await;
    let result = checks::check_required_indicator_mismatch(&page)
        .await
        .expect("check_required_indicator_mismatch");
    assert_eq!(result.status, checks::Status::Fail, "got: {result:?}");
    assert!(
        result.detail.contains("Full name"),
        "got: {}",
        result.detail
    );
    assert!(
        !result.detail.contains("Email"),
        "the correctly-wired field must not be flagged, got: {}",
        result.detail
    );
    assert!(
        !result.detail.contains("Comments"),
        "a field with no required-looking label is irrelevant to this check, got: {}",
        result.detail
    );
}

#[tokio::test]
async fn form_with_no_required_looking_labels_passes_the_mismatch_check() {
    let (_browser, page, _handle) = open_fixture("test-form.html").await;
    let result = checks::check_required_indicator_mismatch(&page)
        .await
        .expect("check_required_indicator_mismatch");
    assert_eq!(result.status, checks::Status::Pass, "got: {result:?}");
}
