use anyhow::{Context, Result};
use chromiumoxide::{Browser, BrowserConfig, BrowserFetcher, BrowserFetcherOptions, Page};
use futures::StreamExt;
use std::path::PathBuf;
use tokio::task::JoinHandle;

/// Downloads a Chrome-for-Testing build into ~/.cache/formwatch/chrome. Only
/// runs when no system Chrome/Chromium can be auto-detected, so a plain
/// `cargo install formwatch` still works with no root/apt step.
///
/// ponytail: not safe for concurrent callers racing to populate a cold
/// cache (two formwatch processes started at once on a machine with no
/// Chrome yet, or — the way this was actually found — `cargo test`'s
/// default test parallelism on a machine without system Chrome) can
/// corrupt each other's extraction. Normal usage launches the browser
/// once per process, so this doesn't bite real `formwatch test`/`monitor`
/// runs; if it needs to be made safe, the fix is downloading into a
/// per-process temp dir and atomically renaming into place. Contributors
/// without system Chrome: run something that warms the cache once (e.g.
/// `cargo run -- test file://...`) before `cargo test`.
async fn fetch_chrome() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let cache_dir = PathBuf::from(home).join(".cache/formwatch/chrome");
    std::fs::create_dir_all(&cache_dir).context("creating chrome cache dir")?;
    let already_cached = cache_dir
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if !already_cached {
        eprintln!(
            "No Chrome/Chromium found — downloading one to {} (first run only)...",
            cache_dir.display()
        );
    }
    let opts = BrowserFetcherOptions::builder()
        .with_path(&cache_dir)
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;
    let fetcher = BrowserFetcher::new(opts);
    let info = fetcher.fetch().await.context("failed to download Chrome")?;
    Ok(info.executable_path)
}

async fn build_config(headful: bool) -> Result<BrowserConfig> {
    let mut builder = BrowserConfig::builder();
    if headful {
        builder = builder.with_head();
    }
    if let Ok(config) = builder.build() {
        return Ok(config);
    }

    let exe = fetch_chrome().await?;
    let mut builder = BrowserConfig::builder().chrome_executable(exe);
    if headful {
        builder = builder.with_head();
    }
    builder.build().map_err(|e| anyhow::anyhow!(e))
}

/// Launches a headless (or headful, for debugging) Chrome and hands back the
/// browser plus the background task that pumps its CDP event loop — that
/// task must stay alive for the whole session or every later call hangs.
pub async fn launch(headful: bool) -> Result<(Browser, JoinHandle<()>)> {
    let config = build_config(headful).await?;
    let (browser, mut handler) = Browser::launch(config)
        .await
        .context("failed to launch Chrome")?;
    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok((browser, handle))
}

pub async fn open(browser: &Browser, url: &str) -> Result<Page> {
    let page = browser
        .new_page(url)
        .await
        .with_context(|| format!("failed to open {url}"))?;
    page.wait_for_navigation()
        .await
        .with_context(|| format!("navigation to {url} never finished"))?;

    // A DNS failure, connection refused, or blocked port doesn't make
    // wait_for_navigation error — Chrome "successfully" navigates to its
    // own chrome-error://chromewebdata/ interstitial instead. Without
    // this check every later check would silently run against that
    // interstitial (its own accessibility/mobile/etc. "issues") instead
    // of failing clearly with "the page didn't load".
    let landed_on: String = page.evaluate("location.href").await?.into_value()?;
    if landed_on.starts_with("chrome-error://") {
        anyhow::bail!(
            "{url} failed to load (landed on {landed_on} — unreachable, DNS failure, connection refused, or a blocked port)"
        );
    }
    Ok(page)
}
