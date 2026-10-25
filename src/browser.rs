use anyhow::{Context, Result};
use chromiumoxide::browser::BrowserConfigBuilder;
use chromiumoxide::cdp::browser_protocol::network::{
    EnableParams, EventResponseReceived, GetCookiesParams, ResourceType,
};
use chromiumoxide::listeners::EventStream;
use chromiumoxide::{Browser, BrowserConfig, BrowserFetcher, BrowserFetcherOptions, Page};
use futures::StreamExt;
use std::collections::HashMap;
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
    let cache_dir = chrome_cache_dir().context("HOME is not set")?;

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

/// The directory formwatch downloads Chrome-for-Testing into
/// (`$HOME/.cache/formwatch/chrome`), if `HOME` is set. Exposed so
/// `formwatch doctor` can report whether a cached build exists without
/// launching or downloading anything.
pub fn chrome_cache_dir() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
        .map(|home| PathBuf::from(home).join(".cache/formwatch/chrome"))
}

/// Whether formwatch already has a Chrome build cached from a previous
/// run (see [`chrome_cache_dir`]).
pub fn has_cached_chrome() -> bool {
    chrome_cache_dir()
        .map(|dir| already_downloaded(&dir))
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

    // The fetcher writes its archive to `<tmp>/<rev>.zip` and only creates
    // the revision subdirectory during unzip — so `<tmp>` itself must
    // exist before the download starts, or a truly cold cache fails with
    // a bare "Failed to create archive file: No such file or directory".
    std::fs::create_dir_all(&tmp).context("creating chrome download temp dir")?;

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

/// A fresh, unique profile directory for one Chrome instance.
///
/// chromiumoxide's own default (when nothing calls `.user_data_dir(...)`)
/// is a single *fixed* path shared by every launch on the machine
/// (`$TMPDIR/chromiumoxide-runner`) — harmless for one process at a time,
/// but two concurrent launches collide on Chrome's own SingletonLock
/// file for that shared profile and one of them fails outright
/// ("Failed to create a ProcessSingleton for your profile directory").
/// This never showed up in local runs (evidently never enough real
/// concurrent launches at once to hit the race), but reliably did the
/// first time `cargo test` ran on GitHub Actions' faster, more-parallel
/// runner — exactly the kind of latent concurrency bug a dev machine
/// can hide indefinitely.
fn unique_profile_dir() -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    std::env::temp_dir().join(format!(
        "formwatch-chrome-profile-{}-{nanos}",
        std::process::id()
    ))
}

/// Options that shape how Chrome is launched. Defaults reproduce
/// formwatch's historical behavior exactly (headless, no proxy, no TLS
/// relaxation).
#[derive(Debug, Clone, Default)]
pub struct BrowserOptions {
    /// Show the browser window instead of running headless.
    pub headful: bool,
    /// HTTP(S) proxy URL. Passed to Chrome via `--proxy-server`.
    pub proxy: Option<String>,
    /// Ignore TLS certificate errors. Only for a trusted internal proxy
    /// or a known test host — never for general web use.
    pub insecure: bool,
    /// Launch Chrome with `--no-sandbox`. Required in many container
    /// environments where the kernel sandbox is unavailable; never use it
    /// on a host browsing untrusted pages as a normal user.
    pub no_sandbox: bool,
    /// Optional CDP request timeout.
    pub request_timeout: Option<Duration>,
}

/// How many times [`open`] attempts to load a page before giving up, and
/// the backoff between attempts. Defaults are the historical behavior:
/// 3 attempts, 500ms then 1s.
#[derive(Debug, Clone)]
pub struct OpenOptions {
    /// Number of attempts before reporting the page unreachable.
    pub attempts: u32,
    /// Base backoff; doubles on each retry.
    pub backoff: Duration,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self {
            attempts: 3,
            backoff: Duration::from_millis(500),
        }
    }
}

/// Applies the launch flags shared by both the system-Chrome and
/// downloaded-Chrome paths.
fn apply_launch_flags(
    mut builder: BrowserConfigBuilder,
    opts: &BrowserOptions,
) -> BrowserConfigBuilder {
    if let Some(proxy) = &opts.proxy {
        builder = builder.arg(format!("--proxy-server={proxy}"));
    }
    if opts.insecure {
        builder = builder.arg("--ignore-certificate-errors");
    }
    if opts.no_sandbox {
        builder = builder.no_sandbox();
    }
    if let Some(timeout) = opts.request_timeout {
        builder = builder.request_timeout(timeout);
    }
    builder
}

async fn build_config(opts: &BrowserOptions) -> Result<BrowserConfig> {
    let base = BrowserConfig::builder().user_data_dir(unique_profile_dir());
    let base = apply_launch_flags(base, opts);
    let base = if opts.headful { base.with_head() } else { base };
    if let Ok(config) = base.build() {
        return Ok(config);
    }

    let exe = fetch_chrome().await?;
    let base = BrowserConfig::builder()
        .chrome_executable(exe)
        .user_data_dir(unique_profile_dir());
    let base = apply_launch_flags(base, opts);
    let base = if opts.headful { base.with_head() } else { base };
    base.build().map_err(|e| anyhow::anyhow!(e))
}

/// Launches a headless (or headful, for debugging) Chrome and hands back the
/// browser plus the background task that pumps its CDP event loop — that
/// task must stay alive for the whole session or every later call hangs.
pub async fn launch(headful: bool) -> Result<(Browser, JoinHandle<()>)> {
    launch_with(&BrowserOptions {
        headful,
        ..BrowserOptions::default()
    })
    .await
}

/// [`launch`], honoring a full [`BrowserOptions`] (proxy, TLS, timeout).
pub async fn launch_with(opts: &BrowserOptions) -> Result<(Browser, JoinHandle<()>)> {
    let config = build_config(opts).await?;
    tracing::debug!(headful = opts.headful, proxy = ?opts.proxy, "launching Chrome");
    let (browser, mut handler) = Browser::launch(config)
        .await
        .context("failed to launch Chrome")?;
    let handle = tokio::spawn(async move { while handler.next().await.is_some() {} });
    Ok((browser, handle))
}

/// Opens `url` in a new page and waits for it to load, retrying (with
/// default [`OpenOptions`]) on failure. Returns `Err` only if every
/// attempt failed to load at all — including the case where Chrome
/// "successfully" navigates to its own error interstitial (a DNS failure,
/// connection refused, or blocked port), which callers should treat as
/// the page genuinely being unreachable, not a real result.
pub async fn open(browser: &Browser, url: &str) -> Result<Page> {
    open_with(browser, url, &OpenOptions::default()).await
}

/// One raw cookie as the browser's own jar reports it (via CDP
/// `Network.getCookies`) — flags only, never values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapturedCookie {
    /// Cookie name (values are never captured).
    pub name: String,
    /// `Secure` attribute present.
    pub secure: bool,
    /// `HttpOnly` attribute present.
    pub httponly: bool,
    /// `SameSite` attribute value as sent (`Strict`/`Lax`/`None`).
    pub samesite: Option<String>,
    /// `Path` attribute value as sent.
    pub path: String,
    /// `Domain` attribute value as sent.
    pub domain: String,
}

/// The main document's response as the browser itself saw it: final URL,
/// response headers, and jar cookies — captured over CDP during the page
/// open, so header/cookie analysis runs against exactly what Chrome
/// received (same TLS session, UA, and cookies), not what a second,
/// look-different HTTP client would get.
#[derive(Debug, Clone, Default)]
pub struct CapturedDocument {
    /// Final URL after redirects.
    pub final_url: String,
    /// Lowercased response header name -> value (first wins).
    pub headers: HashMap<String, String>,
    /// Every `Set-Cookie` value from the document response.
    pub set_cookies: Vec<String>,
    /// Jar cookies with flags only (values never captured).
    pub cookies: Vec<CapturedCookie>,
}

/// [`open`], also returning the main document's response as the browser
/// saw it (headers + cookie flags via CDP). The capture is `None` when
/// it failed or there was nothing to capture — callers fall back to a
/// plain header fetch instead of losing the run.
pub async fn open_with_capture(
    browser: &Browser,
    url: &str,
    opts: &OpenOptions,
) -> Result<(Page, Option<CapturedDocument>)> {
    let attempts = opts.attempts.max(1);
    let mut last_err = None;
    for attempt in 0..attempts {
        match open_once(browser, url).await {
            Ok(page) => return Ok(page),
            Err(e) => {
                tracing::warn!(url, attempt = attempt + 1, error = %e, "page load attempt failed");
                last_err = Some(e);
            }
        }
        if attempt + 1 < attempts {
            // Saturating: `attempts` is a public field, and a large value
            // would otherwise overflow `backoff * 2^n` and panic.
            let backoff = opts.backoff.saturating_mul(2u32.saturating_pow(attempt));
            tokio::time::sleep(backoff).await;
        }
    }
    Err(last_err.expect("loop runs at least once, always setting last_err on failure"))
}

/// [`open`], honoring custom retry count and backoff.
pub async fn open_with(browser: &Browser, url: &str, opts: &OpenOptions) -> Result<Page> {
    Ok(open_with_capture(browser, url, opts).await?.0)
}

async fn open_once(browser: &Browser, url: &str) -> Result<(Page, Option<CapturedDocument>)> {
    // A blank page first so the Network listener below is attached
    // *before* the real navigation — subscribing after `new_page(url)`
    // would already have missed the document's response.
    let page = browser
        .new_page("about:blank")
        .await
        .with_context(|| format!("failed to open {url}"))?;
    let capture = NetworkCapture::start(&page).await;
    if let Err(e) = page.goto(url).await {
        tracing::debug!(url, error = %e, "goto returned early, still waiting for navigation");
    }
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
    let captured = match capture {
        Some(capture) => capture.finish(&page, &landed_on).await,
        None => None,
    };
    Ok((page, captured))
}

/// Best-effort CDP capture of the document response. Every step is
/// optional: if enabling Network, subscribing, or reading cookies fails,
/// the page itself still loaded fine and callers fall back to a plain
/// header fetch instead.
struct NetworkCapture {
    listener: EventStream<EventResponseReceived>,
}

impl NetworkCapture {
    async fn start(page: &Page) -> Option<Self> {
        if page.execute(EnableParams::builder().build()).await.is_err() {
            return None;
        }
        match page.event_listener::<EventResponseReceived>().await {
            Ok(listener) => Some(Self { listener }),
            Err(e) => {
                tracing::debug!(error = %e, "Network event subscription failed; header checks will refetch");
                None
            }
        }
    }

    /// Drains document responses seen during the load and keeps the one
    /// for the final URL (last document response as fallback), plus the
    /// browser's jar cookies.
    async fn finish(mut self, page: &Page, landed_on: &str) -> Option<CapturedDocument> {
        let mut documents = Vec::new();
        // The main document's response precedes load completion, but
        // delivery to this listener can lag a tick behind navigation —
        // drain whatever is already queued, then stop at the first
        // quiet poll instead of waiting out the clock.
        for _ in 0..20 {
            match tokio::time::timeout(Duration::from_millis(100), self.listener.next()).await {
                Ok(Some(event)) => {
                    if event.r#type == ResourceType::Document {
                        documents.push(event.response.clone());
                    }
                }
                _ => break,
            }
        }
        let response = documents
            .iter()
            .rev()
            .find(|r| r.url == landed_on)
            .or_else(|| documents.last())?;
        let mut headers = HashMap::new();
        let mut set_cookies = Vec::new();
        if let Some(obj) = response.headers.inner().as_object() {
            for (name, value) in obj {
                push_header_values(&mut headers, &mut set_cookies, &name.to_lowercase(), value);
            }
        }
        let cookies = page
            .execute(GetCookiesParams::builder().build())
            .await
            .map(|r| {
                r.result
                    .cookies
                    .into_iter()
                    .map(|c| CapturedCookie {
                        name: c.name,
                        secure: c.secure,
                        httponly: c.http_only,
                        samesite: c.same_site.map(|s| s.as_ref().to_string()),
                        path: c.path,
                        domain: c.domain,
                    })
                    .collect()
            })
            .unwrap_or_default();
        Some(CapturedDocument {
            final_url: response.url.clone(),
            headers,
            set_cookies,
            cookies,
        })
    }
}

/// Folds one CDP header value into the map, keeping every `Set-Cookie`
/// separately (Chromium hands repeats as one JSON array or as several
/// values — both occur in the wild). First value wins for ordinary
/// headers.
fn push_header_values(
    headers: &mut HashMap<String, String>,
    set_cookies: &mut Vec<String>,
    key: &str,
    value: &serde_json::Value,
) {
    match value {
        serde_json::Value::String(s) => {
            if key == "set-cookie" {
                set_cookies.push(s.clone());
            }
            headers.entry(key.to_string()).or_insert_with(|| s.clone());
        }
        serde_json::Value::Array(items) => {
            for item in items.iter().filter_map(|v| v.as_str()) {
                if key == "set-cookie" {
                    set_cookies.push(item.to_string());
                }
                headers
                    .entry(key.to_string())
                    .or_insert_with(|| item.to_string());
            }
        }
        other => {
            headers
                .entry(key.to_string())
                .or_insert_with(|| other.to_string());
        }
    }
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

    fn fold(pairs: &[(&str, serde_json::Value)]) -> (HashMap<String, String>, Vec<String>) {
        let mut headers = HashMap::new();
        let mut set_cookies = Vec::new();
        for (k, v) in pairs {
            push_header_values(&mut headers, &mut set_cookies, k, v);
        }
        (headers, set_cookies)
    }

    #[test]
    fn header_capture_keeps_every_set_cookie_and_first_wins_otherwise() {
        // Chromium hands repeats as one JSON array or as several values —
        // both occur in the wild, and both must preserve every cookie
        // while ordinary headers keep their first value.
        let (headers, set_cookies) = fold(&[
            (
                "content-security-policy",
                serde_json::json!("default-src 'self'"),
            ),
            (
                "set-cookie",
                serde_json::json!(["a=1; Secure", "b=2; HttpOnly"]),
            ),
            ("set-cookie", serde_json::json!("c=3; Path=/")),
            ("x-powered-by", serde_json::json!("PHP/8.1")),
            ("x-powered-by", serde_json::json!("PHP/9.9")),
        ]);
        assert_eq!(
            set_cookies,
            vec!["a=1; Secure", "b=2; HttpOnly", "c=3; Path=/"]
        );
        assert_eq!(headers["content-security-policy"], "default-src 'self'");
        assert_eq!(headers["x-powered-by"], "PHP/8.1");
    }
}
