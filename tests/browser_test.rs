//! Exercises `browser::open`'s retry behavior — needs a real Chrome, same
//! as `checks_test.rs`.

use formwatch::browser;

#[tokio::test]
async fn open_retries_and_succeeds_once_a_transient_failure_resolves() {
    // A missing local file lands on the same chrome-error:// interstitial
    // as a real network failure (DNS, connection refused) — confirmed
    // directly before writing this test. Points at a file that doesn't
    // exist on the first attempt (so the first attempt genuinely fails,
    // not a no-op), then creates it during open()'s own retry backoff
    // window, proving it actually retries rather than failing once and
    // giving up.
    let (browser, _handle) = browser::launch(false).await.expect("launch chrome");
    let path = std::env::temp_dir().join("formwatch-test-open-retry.html");
    let _ = std::fs::remove_file(&path);
    let url = format!("file://{}", path.display());

    tokio::spawn({
        let path = path.clone();
        async move {
            // Well inside open()'s 500ms-then-1s backoff window (a real
            // attempt happens at t=0, ~500ms, and ~1500ms), and well after
            // the first attempt has already failed.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            std::fs::write(&path, "<html><body>ready</body></html>").expect("write file");
        }
    });

    let page = browser::open(&browser, &url)
        .await
        .expect("open should retry until the file exists and succeed");
    let text: String = page
        .evaluate("document.body.innerText")
        .await
        .expect("evaluate")
        .into_value()
        .expect("string");
    assert_eq!(text, "ready");

    let _ = std::fs::remove_file(&path);
}
