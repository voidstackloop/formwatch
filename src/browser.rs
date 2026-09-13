use anyhow::{Context, Result};
use chromiumoxide::{Browser, BrowserConfig, BrowserFetcher, BrowserFetcherOptions, Page};
use futures::StreamExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::task::JoinHandle;

/// Downloads a Chrome-for-Testing build into ~/.cache/formwatch/chrome. Only
/// runs when no system Chrome/Chromium can be auto-detected, so a plain
/// `cargo install formwatch` still works with no root/apt step.
///
/// Safe for concurrent callers racing to populate a cold cache (two
/// formwatch processes started at once on a machine with no Chrome yet, or
/// `cargo test`'s default test parallelism on a machine without system
/// Chrome): each downloads into its own private temp directory, then
/// atomically renames it into place. Whichever caller's `fs::rename` lands
/// first wins; every other caller's rename fails (the destination now
/// exists and is non-empty), which is exactly the signal to discard its
/// own copy and defer to the winner. No lock file needed — the
/// atomicity of a same-filesystem rename provides the race-safety.
async fn fetch_chrome() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME is not set")?;
    let cache_dir = PathBuf::from(home).join(".cache/formwatch/chrome");

    if !already_downloaded(&cache_dir) {
        eprintln!(
            "No Chrome/Chromium found — downloading one to {} (first run only)...",
            cache_dir.display()
        );
        download_into(&cache_dir).await?;
    }

    // Whether we just won the race, lost it, or the cache was already warm,
    // BrowserFetcher::fetch() against the now-populated cache_dir is a
    // local-only existence check (no network) that resolves the final
    // executable path — see chromiumoxide_fetcher's BrowserFetcher::local().
    fetch_at(&cache_dir).await
}

fn already_downloaded(cache_dir: &Path) -> bool {
    cache_dir
        .read_dir()
        .map(|mut d| d.next().is_some())
        .unwrap_or(false)
}

async fn fetch_at(path: &Path) -> Result<PathBuf> {
    let opts = BrowserFetcherOptions::builder()
        .with_path(path)
        .build()
        .map_err(|e| anyhow::anyhow!(e))?;
    let info = BrowserFetcher::new(opts)
        .fetch()
        .await
        .context("failed to download Chrome")?;
    Ok(info.executable_path)
}

/// Downloads into a private, unique directory next to `cache_dir`, then
/// hands it to [`install_or_discard`] to atomically become `cache_dir` —
/// or to be thrown away if a racing caller already got there first.
async fn download_into(cache_dir: &Path) -> Result<()> {
    let parent = cache_dir
        .parent()
        .context("chrome cache dir has no parent")?;
    std::fs::create_dir_all(parent).context("creating chrome cache dir's parent")?;

    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = parent.join(format!(".chrome-download-{}-{nanos}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp); // stale leftover from a crashed prior run, if any

    fetch_at(&tmp).await?;
    install_or_discard(&tmp, cache_dir);
    Ok(())
}

/// Moves a completed download from `tmp` into `dest`. If `dest` already
/// exists (a racing caller's download won first), the rename fails —
/// that failure is the race signal, not a real error — so `tmp` is
/// discarded instead, leaving the winner's `dest` untouched.
fn install_or_discard(tmp: &Path, dest: &Path) {
    if std::fs::rename(tmp, dest).is_err() {
        let _ = std::fs::remove_dir_all(tmp);
    }
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

/// How many times [`open`] attempts to load a page before giving up. A
/// real government site under real-world conditions can have a
/// transient DNS blip or a dropped connection that has nothing to do
/// with the form itself being broken — retrying a couple of times
/// before reporting "Page load: Fail" avoids mistaking network noise
/// for a real finding, without masking a form that's genuinely down
/// (every attempt still has to fail for that to be reported).
const OPEN_ATTEMPTS: u32 = 3;

/// Backoff between attempts; doubles each retry (500ms, then 1s).
const OPEN_RETRY_BACKOFF: Duration = Duration::from_millis(500);

/// Opens `url` in a new page and waits for it to load, retrying up to
/// [`OPEN_ATTEMPTS`] times with backoff on failure. Returns `Err` only
/// if every attempt failed to load at all — including the case where
/// Chrome "successfully" navigates to its own error interstitial (a DNS
/// failure, connection refused, or blocked port), which callers should
/// treat as the page genuinely being unreachable, not a real result.
pub async fn open(browser: &Browser, url: &str) -> Result<Page> {
    let mut last_err = None;
    for attempt in 0..OPEN_ATTEMPTS {
        match open_once(browser, url).await {
            Ok(page) => return Ok(page),
            Err(e) => last_err = Some(e),
        }
        if attempt + 1 < OPEN_ATTEMPTS {
            tokio::time::sleep(OPEN_RETRY_BACKOFF * 2u32.pow(attempt)).await;
        }
    }
    Err(last_err.expect("loop runs at least once, always setting last_err on failure"))
}

async fn open_once(browser: &Browser, url: &str) -> Result<Page> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_or_discard_moves_tmp_into_place_when_nothing_won_the_race() {
        let base = std::env::temp_dir().join("formwatch-test-install-or-discard-clean");
        let _ = std::fs::remove_dir_all(&base);
        let tmp = base.join("tmp");
        let dest = base.join("dest");
        std::fs::create_dir_all(&tmp).expect("create tmp");
        std::fs::write(tmp.join("chrome"), b"the download").expect("write marker file");

        install_or_discard(&tmp, &dest);

        assert!(!tmp.exists(), "tmp should have been moved, not left behind");
        assert_eq!(
            std::fs::read(dest.join("chrome")).expect("dest should contain the download"),
            b"the download"
        );

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn install_or_discard_defers_to_a_dest_that_already_won_the_race() {
        // Simulates losing the race: `dest` was already populated (by
        // another caller's `install_or_discard`) by the time this one
        // runs. The loser's `tmp` must be discarded, and the winner's
        // `dest` must survive untouched — not merged, not corrupted.
        let base = std::env::temp_dir().join("formwatch-test-install-or-discard-race");
        let _ = std::fs::remove_dir_all(&base);
        let tmp = base.join("tmp");
        let dest = base.join("dest");
        std::fs::create_dir_all(&tmp).expect("create tmp");
        std::fs::write(tmp.join("chrome"), b"the loser's download").expect("write marker file");
        std::fs::create_dir_all(&dest).expect("create dest");
        std::fs::write(dest.join("chrome"), b"the winner's download").expect("write marker file");

        install_or_discard(&tmp, &dest);

        assert!(
            !tmp.exists(),
            "the losing tmp dir should be discarded, not left behind"
        );
        assert_eq!(
            std::fs::read(dest.join("chrome")).expect("dest should be untouched"),
            b"the winner's download",
            "the winner's already-installed download must survive unchanged"
        );

        let _ = std::fs::remove_dir_all(&base);
    }
}
