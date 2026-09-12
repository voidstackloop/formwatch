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
