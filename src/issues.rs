//! GitHub Issues auto-tracking for regressions.
//!
//! A webhook notification (see [`crate::notify`]) tells *someone* a check
//! regressed; nothing then tracks it as work. This module turns a
//! regression into an actual GitHub Issue in a configured repo — created
//! once when a check first fails, left alone on every later run while it's
//! still failing (no duplicate issues, no spam), and closed automatically
//! the moment the check recovers to `Pass`. A `Warn` transition does
//! neither: `Warn` isn't a confirmed defect (see [`crate::checks::Status`]),
//! so it's not the "open a ticket" or "close the ticket" signal either.
//!
//! Every open/close decision is driven entirely by [`history::diff`]
//! transitions, the same primitive [`crate::notify`] uses for webhooks —
//! this module doesn't invent a second notion of "what changed."
//!
//! Like the LLM checks and webhooks, a failure here is a warning, not a
//! reason to fail a run: a dead GitHub token or a rate limit shouldn't
//! lose a completed check's results.

use crate::checks::Status;
use crate::error::{Error, Result};
use crate::history::{self, RunResult};
use std::collections::HashMap;
use std::time::Duration;

/// One check's status transition for one form, exactly as recorded by
/// [`history::diff`] — but capturing *every* transition (`Warn` included),
/// not just the "regressed to Fail" subset [`crate::notify::Regression`]
/// deliberately narrows to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueEvent {
    /// The form's label.
    pub form: String,
    /// The form's URL.
    pub url: String,
    /// Which check changed.
    pub check: String,
    /// The status it had before.
    pub from: Status,
    /// The status it has now.
    pub to: Status,
}

/// Every status transition across `runs`, each diffed against `prior`'s
/// entry for its URL — mirrors [`crate::notify::regressions_from_runs`]
/// exactly, except keeping every transition rather than filtering to
/// `-> Fail` only. `prior` is loaded once per invocation by the caller and
/// shared with anything else that needs the same history (see
/// `main::load_prior_by_url`) instead of this re-reading it from disk.
pub fn events_from_runs(
    runs: &[RunResult],
    prior: &HashMap<String, Vec<RunResult>>,
) -> Vec<IssueEvent> {
    let mut events = Vec::new();
    for run in runs {
        let Some(form_history) = prior.get(&run.url) else {
            continue;
        };
        if let Some(prev) = history::previous_run(form_history, run.timestamp) {
            events.extend(history::diff(prev, run).into_iter().map(|c| IssueEvent {
                form: run.name.clone(),
                url: run.url.clone(),
                check: c.name,
                from: c.from,
                to: c.to,
            }));
        }
    }
    events
}

/// A stable, greppable marker embedded (as an HTML comment, invisible in
/// GitHub's rendered view) in every tracking issue's body, used to find
/// an existing open issue for the same `(url, check)` pair again later.
/// The raw url/check text is embedded directly rather than hashed —
/// there's no reason to obscure it, and a maintainer reading the issue
/// source can see exactly what it's keyed on.
fn marker(url: &str, check: &str) -> String {
    // `-->` inside either value would prematurely close the HTML comment;
    // neither a URL nor a check name plausibly contains it, but strip it
    // defensively rather than trust that.
    let clean = |s: &str| s.replace("-->", "");
    format!(
        "<!-- formwatch-issue-key: {} | {} -->",
        clean(url),
        clean(check)
    )
}

fn title_for(event: &IssueEvent) -> String {
    format!("{}: {} is failing", event.form, event.check)
}

fn body_for(event: &IssueEvent) -> String {
    format!(
        "formwatch detected a regression:\n\n\
         - **Form:** {}\n\
         - **URL:** {}\n\
         - **Check:** {} ({} -> {})\n\n\
         Run `formwatch test {}` for details, or see the HTML/JSON report.\n\n\
         {}",
        event.form,
        event.url,
        event.check,
        event.from,
        event.to,
        event.url,
        marker(&event.url, &event.check)
    )
}

const USER_AGENT: &str = "formwatch";
const API_VERSION: &str = "2022-11-28";

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(15))
        .user_agent(USER_AGENT)
        .build()
        .map_err(|e| Error::Notify(format!("building GitHub client: {e}")))
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut s: String = text.chars().take(max).collect();
        s.push('…');
        s
    }
}

fn api_error(context: &str, status: reqwest::StatusCode, body: &str) -> Error {
    Error::Notify(format!(
        "{context}: GitHub API returned HTTP {status}: {}",
        truncate_chars(body, 200)
    ))
}

/// The number of an open issue already tracking `(url, check)`, if any.
async fn find_open_issue(
    client: &reqwest::Client,
    repo: &str,
    token: &str,
    url: &str,
    check: &str,
) -> Result<Option<u64>> {
    let query = format!(
        "repo:{repo} in:body is:issue state:open \"{}\"",
        marker(url, check)
    );
    let response = client
        .get("https://api.github.com/search/issues")
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .query(&[("q", query.as_str())])
        .send()
        .await
        .map_err(|e| Error::Notify(format!("searching GitHub issues: {e}")))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| Error::Notify(format!("parsing GitHub search response: {e}")))?;
    if !status.is_success() {
        return Err(api_error(
            "searching GitHub issues",
            status,
            &body.to_string(),
        ));
    }
    Ok(body["items"]
        .as_array()
        .and_then(|items| items.first())
        .and_then(|item| item["number"].as_u64()))
}

/// Backoff between [`find_open_issue`] retries — see
/// [`find_open_issue_with_retry`] for why this exists at all.
const SEARCH_RETRY_BACKOFF: &[Duration] = &[Duration::from_secs(2), Duration::from_secs(4)];

/// [`find_open_issue`], retried a couple of times with backoff on a miss.
///
/// GitHub's search index is eventually consistent: a just-created issue
/// can briefly not show up in search results at all. Demonstrated live
/// (not just suspected) while building this feature — calling
/// [`reconcile`] twice in immediate succession for the same regression
/// created two separate issues instead of finding the first one, because
/// the second call's search ran before the first call's issue had been
/// indexed. Real usage calls `reconcile` once per formwatch invocation,
/// invocations realistically hours or days apart, so this race is far
/// less likely to matter in practice than in that back-to-back test —
/// but "far less likely" isn't "impossible," and retrying costs nothing
/// in the common case (only reached on an actual miss).
async fn find_open_issue_with_retry(
    client: &reqwest::Client,
    repo: &str,
    token: &str,
    url: &str,
    check: &str,
) -> Result<Option<u64>> {
    if let Some(n) = find_open_issue(client, repo, token, url, check).await? {
        return Ok(Some(n));
    }
    for backoff in SEARCH_RETRY_BACKOFF {
        tokio::time::sleep(*backoff).await;
        if let Some(n) = find_open_issue(client, repo, token, url, check).await? {
            return Ok(Some(n));
        }
    }
    Ok(None)
}

async fn create_issue(
    client: &reqwest::Client,
    repo: &str,
    token: &str,
    event: &IssueEvent,
    labels: &[String],
) -> Result<u64> {
    let response = client
        .post(format!("https://api.github.com/repos/{repo}/issues"))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .json(&serde_json::json!({
            "title": title_for(event),
            "body": body_for(event),
            "labels": labels,
        }))
        .send()
        .await
        .map_err(|e| Error::Notify(format!("creating GitHub issue: {e}")))?;
    let status = response.status();
    let body: serde_json::Value = response
        .json()
        .await
        .map_err(|e| Error::Notify(format!("parsing GitHub create-issue response: {e}")))?;
    if !status.is_success() {
        return Err(api_error(
            "creating a GitHub issue",
            status,
            &body.to_string(),
        ));
    }
    body["number"]
        .as_u64()
        .ok_or_else(|| Error::Notify("GitHub create-issue response had no number".to_string()))
}

async fn comment(
    client: &reqwest::Client,
    repo: &str,
    token: &str,
    number: u64,
    body: &str,
) -> Result<()> {
    let response = client
        .post(format!(
            "https://api.github.com/repos/{repo}/issues/{number}/comments"
        ))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await
        .map_err(|e| Error::Notify(format!("commenting on GitHub issue {number}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(api_error("commenting on a GitHub issue", status, &text));
    }
    Ok(())
}

async fn close_issue(client: &reqwest::Client, repo: &str, token: &str, number: u64) -> Result<()> {
    let response = client
        .patch(format!(
            "https://api.github.com/repos/{repo}/issues/{number}"
        ))
        .bearer_auth(token)
        .header("Accept", "application/vnd.github+json")
        .header("X-GitHub-Api-Version", API_VERSION)
        .json(&serde_json::json!({ "state": "closed" }))
        .send()
        .await
        .map_err(|e| Error::Notify(format!("closing GitHub issue {number}: {e}")))?;
    let status = response.status();
    if !status.is_success() {
        let text = response.text().await.unwrap_or_default();
        return Err(api_error("closing a GitHub issue", status, &text));
    }
    Ok(())
}

/// What happened for one [`IssueEvent`] — returned so a caller can log a
/// summary; never returned as an `Err` from [`reconcile`] itself.
#[derive(Debug)]
pub enum Outcome {
    /// `-> Fail` with no open issue yet: one was created.
    Created {
        /// Which check.
        check: String,
        /// The new issue's number.
        number: u64,
    },
    /// `-> Fail` but an issue is already open: left alone.
    AlreadyTracked {
        /// Which check.
        check: String,
        /// The already-open issue's number.
        number: u64,
    },
    /// `-> Pass` with an open issue: commented and closed.
    Closed {
        /// Which check.
        check: String,
        /// The closed issue's number.
        number: u64,
    },
    /// A transition this module doesn't act on (`Warn`, or `-> Pass`/`->
    /// Fail` with no matching open issue to close/no action needed).
    Skipped,
    /// The GitHub API call itself failed; the event is otherwise dropped,
    /// not retried — the next run's own diff will surface it again if
    /// it's still relevant.
    Failed {
        /// Which check.
        check: String,
        /// A human-readable description of what went wrong.
        error: String,
    },
}

/// Reconciles every event in `events` against `repo`'s issues. Never
/// fatal: each event's own API failure is caught and reported as
/// [`Outcome::Failed`], not propagated — a dead token or a rate limit on
/// one event shouldn't stop the rest from being processed, or lose the
/// run's own results.
pub async fn reconcile(
    repo: &str,
    token: &str,
    labels: &[String],
    events: &[IssueEvent],
) -> Vec<Outcome> {
    let client = match client() {
        Ok(c) => c,
        Err(e) => {
            let error = format_error(&e);
            return events
                .iter()
                .filter(|ev| ev.to == Status::Fail || ev.to == Status::Pass)
                .map(|ev| Outcome::Failed {
                    check: ev.check.clone(),
                    error: error.clone(),
                })
                .collect();
        }
    };

    let mut outcomes = Vec::with_capacity(events.len());
    for event in events {
        let outcome = reconcile_one(&client, repo, token, labels, event).await;
        outcomes.push(outcome);
    }
    outcomes
}

async fn reconcile_one(
    client: &reqwest::Client,
    repo: &str,
    token: &str,
    labels: &[String],
    event: &IssueEvent,
) -> Outcome {
    match event.to {
        Status::Fail => {
            match find_open_issue_with_retry(client, repo, token, &event.url, &event.check).await {
                Ok(Some(number)) => Outcome::AlreadyTracked {
                    check: event.check.clone(),
                    number,
                },
                Ok(None) => match create_issue(client, repo, token, event, labels).await {
                    Ok(number) => Outcome::Created {
                        check: event.check.clone(),
                        number,
                    },
                    Err(e) => Outcome::Failed {
                        check: event.check.clone(),
                        error: format_error(&e),
                    },
                },
                Err(e) => Outcome::Failed {
                    check: event.check.clone(),
                    error: format_error(&e),
                },
            }
        }
        Status::Pass => {
            match find_open_issue_with_retry(client, repo, token, &event.url, &event.check).await {
                Ok(Some(number)) => {
                    let note = format!(
                        "formwatch: **{}** recovered to PASS for {} ({}).",
                        event.check, event.form, event.url
                    );
                    if let Err(e) = comment(client, repo, token, number, &note).await {
                        return Outcome::Failed {
                            check: event.check.clone(),
                            error: format_error(&e),
                        };
                    }
                    match close_issue(client, repo, token, number).await {
                        Ok(()) => Outcome::Closed {
                            check: event.check.clone(),
                            number,
                        },
                        Err(e) => Outcome::Failed {
                            check: event.check.clone(),
                            error: format_error(&e),
                        },
                    }
                }
                Ok(None) => Outcome::Skipped,
                Err(e) => Outcome::Failed {
                    check: event.check.clone(),
                    error: format_error(&e),
                },
            }
        }
        Status::Warn => Outcome::Skipped,
    }
}

fn format_error(e: &Error) -> String {
    format!("{e}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(url: &str, ts: i64, status: Status) -> RunResult {
        RunResult {
            schema_version: history::SCHEMA_VERSION,
            name: "Permit".into(),
            url: url.into(),
            timestamp: ts,
            checks: vec![crate::checks::CheckResult {
                name: "Accessibility".into(),
                status,
                detail: String::new(),
                screenshot: None,
            }],
        }
    }

    #[test]
    fn marker_is_stable_and_distinguishes_url_and_check() {
        let a = marker("https://city.gov/apply", "Accessibility");
        let b = marker("https://city.gov/apply", "Accessibility");
        assert_eq!(a, b, "must be deterministic to find the same issue again");
        assert_ne!(
            a,
            marker("https://city.gov/apply", "Mobile usability"),
            "different checks must not collide"
        );
        assert_ne!(
            a,
            marker("https://city.gov/renew", "Accessibility"),
            "different forms must not collide"
        );
    }

    #[test]
    fn marker_defends_against_a_value_closing_the_html_comment_early() {
        let m = marker("https://city.gov/apply-->evil", "Accessibility");
        assert!(
            !m.contains("-->evil"),
            "a value containing an early comment-close must not let text \
             escape the marker comment: {m}"
        );
    }

    #[test]
    fn events_from_runs_keeps_every_transition_not_just_fail() {
        let dir = std::env::temp_dir().join("formwatch-test-issues-events");
        let _ = std::fs::remove_dir_all(&dir);
        let url = "https://city.gov/apply";

        history::save_run(&dir, &run(url, 1, Status::Pass)).expect("save");
        history::save_run(&dir, &run(url, 2, Status::Warn)).expect("save");
        history::save_run(&dir, &run(url, 3, Status::Fail)).expect("save");

        let loaded = history::load_runs(&dir, url).expect("load");
        let latest = loaded.last().unwrap().clone();
        let prior = HashMap::from([(url.to_string(), loaded)]);
        let events = events_from_runs(std::slice::from_ref(&latest), &prior);

        // latest (ts=3, Fail) diffed against its predecessor (ts=2, Warn).
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].from, Status::Warn);
        assert_eq!(events[0].to, Status::Fail);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn title_and_body_name_the_form_check_and_transition() {
        let event = IssueEvent {
            form: "Business License".into(),
            url: "https://city.gov/license".into(),
            check: "Accessibility".into(),
            from: Status::Pass,
            to: Status::Fail,
        };
        let title = title_for(&event);
        assert!(title.contains("Business License"));
        assert!(title.contains("Accessibility"));

        let body = body_for(&event);
        assert!(body.contains("https://city.gov/license"));
        assert!(body.contains("PASS -> FAIL"));
        assert!(body.contains(&marker(&event.url, &event.check)));
    }
}
