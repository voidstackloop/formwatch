use crate::{browser, checks, history, options, vuln};
use anyhow::Result;
use chromiumoxide::Browser;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Opens `url`, runs every check against it, and saves the result under
/// `history_dir` — never returning Err just because the page itself
/// failed to load or `checks_dir` had a problem. An unreachable
/// public-service form is itself the most severe possible finding;
/// losing the whole report because the browser couldn't even navigate
/// there would hide exactly the outage this tool exists to catch.
///
/// Uses default [`options::RunOptions`] and [`browser::OpenOptions`];
/// callers needing custom timeouts, screenshot suppression, or retry
/// policy should use [`run_one_with`].
pub async fn run_one(
    browser: &Browser,
    history_dir: &Path,
    name: String,
    url: String,
    submit: bool,
    wait: u64,
    checks_dir: Option<&Path>,
) -> Result<history::RunResult> {
    run_one_with(
        browser,
        history_dir,
        name,
        url,
        &options::RunOptions {
            allow_submit: submit,
            wait_secs: wait,
            ..options::RunOptions::default()
        },
        &browser::OpenOptions::default(),
        checks_dir,
        None,
    )
    .await
}

/// [`run_one`], honoring caller-supplied run and browser-open options.
///
/// `llm_client` should be built once per `test`/`monitor` invocation (not
/// once per form) and passed by reference to every call, so concurrent
/// forms share one HTTP connection pool to the LLM provider instead of each
/// paying a fresh handshake. Pass `None` when `opts.llm` is `None` or the
/// client failed to build.
#[allow(clippy::too_many_arguments)]
pub async fn run_one_with(
    browser: &Browser,
    history_dir: &Path,
    name: String,
    url: String,
    opts: &options::RunOptions,
    open_opts: &browser::OpenOptions,
    checks_dir: Option<&Path>,
    llm_client: Option<&crate::llm::LlmClient>,
) -> Result<history::RunResult> {
    let started = std::time::Instant::now();
    let checks = match browser::open_with_capture(browser, &url, open_opts).await {
        Ok((page, capture)) => {
            // Feed the header/cookie checks the browser's own view of
            // this load (CDP capture), so they analyze exactly what
            // Chrome received instead of re-requesting the page.
            if let Some(doc) = &capture {
                vuln::seed_capture(doc);
            }
            let mut checks = checks::run_all_with(&page, opts).await;
            if let Some(dir) = checks_dir {
                match checks::run_custom_checks_with(&page, dir, opts).await {
                    Ok(custom) => checks.extend(custom),
                    Err(e) => checks.push(checks::CheckResult {
                        name: "Custom checks".to_string(),
                        status: checks::Status::Warn,
                        detail: format!("couldn't run --checks-dir {}: {e:#}", dir.display()),
                        screenshot: None,
                    }),
                }
            }
            if let Some(llm) = &opts.llm {
                checks.extend(
                    crate::llm::run_semantic_checks(&page, llm_client, llm, opts.screenshots).await,
                );
            }
            let _ = page.close().await;
            // Performance budget: a form can be "up" yet too slow to
            // complete. The budget is a synthetic check (not part of the
            // check engine) so history/`--json` consumers see it like any
            // other result, and `--fail-on`/`--baseline` apply uniformly.
            if let Some(budget_ms) = opts.max_load_ms {
                let elapsed_ms = started.elapsed().as_millis();
                let status = if elapsed_ms <= u128::from(budget_ms) {
                    checks::Status::Pass
                } else {
                    checks::Status::Fail
                };
                checks.push(checks::CheckResult {
                    name: "Load budget".to_string(),
                    status,
                    detail: format!("run took {elapsed_ms}ms (budget {budget_ms}ms)"),
                    screenshot: None,
                });
            }
            checks
        }
        Err(e) => vec![checks::CheckResult {
            name: "Page load".to_string(),
            status: checks::Status::Fail,
            detail: format!("{e:#}"),
            screenshot: None,
        }],
    };

    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let run = history::RunResult {
        schema_version: history::SCHEMA_VERSION,
        name,
        url,
        timestamp: ts,
        checks,
    };
    history::save_run(history_dir, &run)?;
    tracing::info!(form = %run.name, url = %run.url, "run recorded");
    Ok(run)
}
