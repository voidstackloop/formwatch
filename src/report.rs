use crate::checks::CheckResult;
use crate::history::{self, CheckChange, Flakiness, RunResult};
use anyhow::Result;
use askama::Template;
use chrono::DateTime;
use std::path::Path;

/// One form's entry in a report: its latest run, what changed since the
/// run before that (empty if there wasn't one, or nothing changed), and
/// a human-readable version of its timestamp.
pub struct FormReport {
    /// The form's most recent run.
    pub run: RunResult,
    /// Per-check status changes since this form's previous run, if any.
    pub changes: Vec<CheckChange>,
    /// Every current check's transition count across this form's
    /// *entire* recorded history, not just the last two runs — see
    /// [`history::flakiness`].
    pub flaky: Vec<Flakiness>,
    /// `run.timestamp`, formatted for display (e.g. `"2026-09-11 14:30
    /// UTC"`) rather than a raw Unix timestamp.
    pub when: String,
}

impl FormReport {
    /// This check's flakiness record, if it's actually flaky (2+
    /// transitions across recorded history) — `None` for a check that's
    /// never changed or changed exactly once, so the template only has
    /// to ask one question to decide whether to show a badge.
    pub fn flakiness_of(&self, check: &CheckResult) -> Option<&Flakiness> {
        self.flaky
            .iter()
            .find(|f| f.name == check.name && f.is_flaky())
    }
}

/// The full report model — every form formwatch has ever recorded a run
/// for, newest-first. Renders as HTML via [`askama::Template`] using
/// `templates/report.html`; the plain-text `formwatch report` command
/// walks the same struct field-by-field instead of using this
/// implementation.
#[derive(Template)]
#[template(path = "report.html")]
pub struct ReportTemplate {
    /// Every known form's latest run, each with its own diff.
    pub forms: Vec<FormReport>,
    /// When this report was generated, formatted for display.
    pub generated_at: String,
}

fn human_time(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Builds the report model: latest run for every form formwatch has ever
/// recorded under `history_dir`, each paired with what changed since its
/// previous run.
pub fn build(history_dir: &Path) -> Result<ReportTemplate> {
    let mut forms = vec![];
    for run in history::all_known_forms(history_dir)? {
        let prior_runs = history::load_runs(history_dir, &run.url)?;
        let changes = prior_runs
            .iter()
            .rev()
            .find(|r| r.timestamp < run.timestamp)
            .map(|prev| history::diff(prev, &run))
            .unwrap_or_default();
        let flaky = history::flakiness(&prior_runs);
        let when = human_time(run.timestamp);
        forms.push(FormReport {
            run,
            changes,
            flaky,
            when,
        });
    }
    Ok(ReportTemplate {
        forms,
        generated_at: human_time(chrono::Utc::now().timestamp()),
    })
}

/// Builds the report and renders it as a complete HTML page (what
/// `formwatch report --html` writes to disk).
pub fn render_html(history_dir: &Path) -> Result<String> {
    Ok(build(history_dir)?.render()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckResult, Status};

    #[test]
    fn build_includes_changes_since_the_previous_run() {
        // print_report (the plain `formwatch report`) used to hand-roll
        // its own history lookup and never showed "changed since previous
        // run" at all, unlike --html. It was fixed by having it reuse
        // this function instead — this test is really about build()
        // correctly populating `changes`, which print_report now just
        // has to print.
        let dir = std::env::temp_dir().join("formwatch-test-report-build-changes");
        let _ = std::fs::remove_dir_all(&dir);

        let check = |status| CheckResult {
            name: "Accessibility".to_string(),
            status,
            detail: String::new(),
            screenshot: None,
        };
        let older = RunResult {
            name: "x".into(),
            url: "https://city.gov/apply".into(),
            timestamp: 1,
            checks: vec![check(Status::Pass)],
        };
        let newer = RunResult {
            name: "x".into(),
            url: "https://city.gov/apply".into(),
            timestamp: 2,
            checks: vec![check(Status::Fail)],
        };
        history::save_run(&dir, &older).expect("save older");
        history::save_run(&dir, &newer).expect("save newer");

        let report = build(&dir).expect("build");
        assert_eq!(report.forms.len(), 1);
        assert_eq!(
            report.forms[0].run.timestamp, 2,
            "should report the latest run"
        );
        assert_eq!(report.forms[0].changes.len(), 1);
        assert_eq!(report.forms[0].changes[0].from, Status::Pass);
        assert_eq!(report.forms[0].changes[0].to, Status::Fail);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_flags_a_flaky_check_but_not_a_stable_one() {
        // report::build wires history::flakiness in via prior_runs — this
        // test is really about that wiring, not flakiness()'s own logic
        // (already covered directly in history.rs's tests): a check that
        // flip-flopped across three runs should be flagged, and one that
        // changed once (a genuine regression) should not.
        let dir = std::env::temp_dir().join("formwatch-test-report-build-flaky");
        let _ = std::fs::remove_dir_all(&dir);

        let checks = |flappy, sustained| {
            vec![
                CheckResult {
                    name: "Flappy".to_string(),
                    status: flappy,
                    detail: String::new(),
                    screenshot: None,
                },
                CheckResult {
                    name: "Sustained".to_string(),
                    status: sustained,
                    detail: String::new(),
                    screenshot: None,
                },
            ]
        };
        for (ts, flappy, sustained) in [
            (1, Status::Pass, Status::Pass),
            (2, Status::Fail, Status::Fail),
            (3, Status::Pass, Status::Fail),
        ] {
            history::save_run(
                &dir,
                &RunResult {
                    name: "x".into(),
                    url: "https://city.gov/apply".into(),
                    timestamp: ts,
                    checks: checks(flappy, sustained),
                },
            )
            .expect("save run");
        }

        let report = build(&dir).expect("build");
        let form = &report.forms[0];
        let flappy_check = form.run.checks.iter().find(|c| c.name == "Flappy").unwrap();
        let sustained_check = form
            .run
            .checks
            .iter()
            .find(|c| c.name == "Sustained")
            .unwrap();

        assert!(
            form.flakiness_of(flappy_check).is_some(),
            "a check that flip-flopped across history should be flagged flaky"
        );
        assert!(
            form.flakiness_of(sustained_check).is_none(),
            "a check with a single sustained change is a real regression, not flakiness"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
