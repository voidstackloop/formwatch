//! Exercises `runner::run_one`'s resilience to a page that never loads —
//! needs a real Chrome, same as `checks_test.rs`.

use formwatch::checks::Status;
use formwatch::{browser, history, runner};

#[tokio::test]
async fn unreachable_form_is_recorded_as_a_failed_page_load_not_lost() {
    // A refused local connection needs no network access, so this is
    // deterministic in CI. Regression test for a bug where a page that
    // failed to load (DNS failure, connection refused, blocked port —
    // Chrome "succeeds" by navigating to its own chrome-error:// page
    // rather than erroring) made run_one run the *entire* check suite
    // against that interstitial, producing a misleading report (fake
    // "accessibility violations" etc. about Chrome's own error page)
    // instead of clearly reporting that the form itself never loaded —
    // arguably the single most important thing for a form-monitoring
    // tool to catch.
    let (browser, _handle) = browser::launch(false).await.expect("launch chrome");
    let history_dir = std::env::temp_dir().join("formwatch-test-unreachable-history");
    let _ = std::fs::remove_dir_all(&history_dir);

    let run = runner::run_one(
        &browser,
        &history_dir,
        "Unreachable form".to_string(),
        "http://127.0.0.1:9999/".to_string(),
        false,
        1,
        None,
    )
    .await
    .expect("run_one should never return Err just because the page didn't load");

    assert_eq!(
        run.checks.len(),
        1,
        "expected a single Page load result, not the full check suite: {:#?}",
        run.checks
    );
    assert_eq!(run.checks[0].name, "Page load");
    assert_eq!(run.checks[0].status, Status::Fail);

    let saved = history::load_runs(&history_dir, &run.url).expect("load_runs");
    assert_eq!(
        saved.len(),
        1,
        "the failed run should still be persisted to history"
    );

    let _ = std::fs::remove_dir_all(&history_dir);
}
