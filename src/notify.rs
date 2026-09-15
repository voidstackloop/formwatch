//! Regression notifications.
//!
//! A dashboard is pull-based: someone has to look at it. A regression
//! (a check that was fine and is now broken) is exactly the event a
//! maintainer wants pushed to them. This module finds regressions in a
//! batch of fresh runs by diffing each against its own previous history,
//! renders a payload suitable for either Slack or a generic webhook, and
//! delivers it.
//!
//! Only `Fail` transitions are reported by default (`NotifyOn::Regression`)
//! — the signal worth waking someone for. New forms going to a `Warn`
//! (heuristic limits, CAPTCHAs) are surfaced in the report, not paged on.

use crate::checks::Status;
use crate::error::{Error, Result};
use crate::history::{self, RunResult};
use std::collections::HashMap;
use std::time::Duration;

/// One check regressing to `Fail` for one form.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Regression {
    /// The form's label.
    pub form: String,
    /// The form's URL.
    pub url: String,
    /// Which check regressed.
    pub check: String,
    /// The status it had before (never `Fail`).
    pub from: Status,
    /// The status it has now (`Fail`).
    pub to: Status,
}

impl Regression {
    /// A one-line human summary, e.g.
    /// `Business License Renewal — Accessibility: WARN -> FAIL`.
    pub fn describe(&self) -> String {
        format!(
            "{} — {}: {} -> {}",
            self.form, self.check, self.from, self.to
        )
    }
}

/// Regressions between one form's previous run (`prev`) and its current
/// run (`curr`), by check name. Pure.
pub fn regressions_between(prev: &RunResult, curr: &RunResult) -> Vec<Regression> {
    history::diff(prev, curr)
        .into_iter()
        .filter(|change| change.to == Status::Fail)
        .map(|change| Regression {
            form: curr.name.clone(),
            url: curr.url.clone(),
            check: change.name,
            from: change.from,
            to: change.to,
        })
        .collect()
}

/// Every regression across `runs`, each diffed against `prior`'s entry for
/// its URL (the most recent run there from before `run.timestamp`, so this
/// still finds the correct predecessor even though `prior` was loaded
/// after the new run was already persisted). `prior` is loaded once per
/// invocation by the caller and shared with whatever else needs the same
/// history in the same invocation (see `main::load_prior_by_url`), rather
/// than this re-reading a form's entire history from disk itself.
pub fn regressions_from_runs(
    runs: &[RunResult],
    prior: &HashMap<String, Vec<RunResult>>,
) -> Vec<Regression> {
    let mut regressions = Vec::new();
    for run in runs {
        let Some(form_history) = prior.get(&run.url) else {
            continue;
        };
        if let Some(prev) = history::previous_run(form_history, run.timestamp) {
            regressions.extend(regressions_between(prev, run));
        }
    }
    regressions
}

/// Whether a webhook URL is a Slack incoming webhook (which wants a
/// `{ "text": ... }` body) rather than a generic webhook (which gets the
/// structured payload). Matches the *host* exactly — a substring test
/// would misclassify `https://example.com/?x=hooks.slack.com` and send it
/// a Slack-shaped payload.
pub fn is_slack_webhook(url: &str) -> bool {
    matches!(
        crate::limiter::host_of(url).as_deref(),
        Some("hooks.slack.com") | Some("hooks.slack-gov.com")
    )
}

/// The human-readable summary shared by both payload shapes.
pub fn summary(regressions: &[Regression]) -> String {
    if regressions.is_empty() {
        return "formwatch: no regressions.".to_string();
    }
    let mut text = format!("formwatch: {} regression(s) detected\n", regressions.len());
    for r in regressions {
        text.push_str("* ");
        text.push_str(&r.describe());
        text.push('\n');
    }
    text
}

/// The JSON body to POST, chosen to match the webhook's expected shape.
pub fn payload_for(url: &str, regressions: &[Regression], generated_at: i64) -> serde_json::Value {
    if is_slack_webhook(url) {
        serde_json::json!({ "text": summary(regressions) })
    } else {
        serde_json::json!({
            "source": "formwatch",
            "generated_at": generated_at,
            "summary": summary(regressions),
            "regressions": regressions
                .iter()
                .map(|r| serde_json::json!({
                    "form": r.form,
                    "url": r.url,
                    "check": r.check,
                    "from": r.from.label(),
                    "to": r.to.label(),
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Delivers `payload` to `url`. Bounded timeout so a dead webhook can't
/// stall a monitor run.
pub async fn send(url: &str, payload: &serde_json::Value) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| Error::Notify(format!("building webhook client: {e}")))?;
    let response = client
        .post(url)
        .json(payload)
        .send()
        .await
        .map_err(|e| Error::Notify(format!("posting to webhook: {e}")))?;
    if !response.status().is_success() {
        return Err(Error::Notify(format!(
            "webhook returned HTTP {}",
            response.status()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;
    use crate::history::RunResult;

    fn run(status: Status) -> RunResult {
        RunResult {
            schema_version: history::SCHEMA_VERSION,
            name: "Permit".into(),
            url: "https://city.gov/permit".into(),
            timestamp: 10,
            checks: vec![CheckResult {
                name: "Accessibility".into(),
                status,
                detail: String::new(),
                screenshot: None,
            }],
        }
    }

    #[test]
    fn only_fail_transitions_count_as_regressions() {
        // Pass -> Fail is the signal. Warn -> Pass is an improvement.
        // Pass -> Warn is a heuristic limit, not a regression.
        let regs = regressions_between(&run(Status::Pass), &run(Status::Fail));
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].check, "Accessibility");
        assert_eq!(regs[0].from, Status::Pass);
        assert_eq!(regs[0].to, Status::Fail);

        assert!(regressions_between(&run(Status::Warn), &run(Status::Pass)).is_empty());
        assert!(regressions_between(&run(Status::Pass), &run(Status::Warn)).is_empty());
    }

    #[test]
    fn slack_payload_is_text_and_generic_payload_is_structured() {
        let regs = regressions_between(&run(Status::Pass), &run(Status::Fail));
        let slack = payload_for("https://hooks.slack.com/services/x", &regs, 1);
        assert!(slack.get("text").is_some());
        assert!(slack.get("regressions").is_none());

        let generic = payload_for("https://example.com/hook", &regs, 1);
        assert_eq!(generic["source"], "formwatch");
        assert_eq!(generic["regressions"][0]["to"], "FAIL");
    }

    #[test]
    fn a_url_merely_mentioning_slack_is_not_treated_as_slack() {
        assert!(is_slack_webhook("https://hooks.slack.com/services/x"));
        assert!(is_slack_webhook("https://hooks.slack-gov.com/services/x"));
        assert!(!is_slack_webhook(
            "https://example.com/?next=hooks.slack.com"
        ));
        assert!(!is_slack_webhook("https://hooks.slack.com.evil.test/x"));
    }
}
