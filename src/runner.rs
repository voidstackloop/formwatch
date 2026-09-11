use crate::{browser, checks, history};
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
pub async fn run_one(
    browser: &Browser,
    history_dir: &Path,
    name: String,
    url: String,
    submit: bool,
    wait: u64,
    checks_dir: Option<&Path>,
) -> Result<history::RunResult> {
    let checks = match browser::open(browser, &url).await {
        Ok(page) => {
            let mut checks = checks::run_all(&page, submit, wait).await;
            if let Some(dir) = checks_dir {
                match checks::run_custom_checks(&page, dir).await {
                    Ok(custom) => checks.extend(custom),
                    Err(e) => checks.push(checks::CheckResult {
                        name: "Custom checks".to_string(),
                        status: checks::Status::Warn,
                        detail: format!("couldn't run --checks-dir {}: {e:#}", dir.display()),
                    }),
                }
            }
            let _ = page.close().await;
            checks
        }
        Err(e) => vec![checks::CheckResult {
            name: "Page load".to_string(),
            status: checks::Status::Fail,
            detail: format!("{e:#}"),
        }],
    };

    let ts = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() as i64;
    let run = history::RunResult {
        name,
        url,
        timestamp: ts,
        checks,
    };
    history::save_run(history_dir, &run)?;
    Ok(run)
}
