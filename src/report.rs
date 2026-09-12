use crate::history::{self, CheckChange, RunResult};
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
    /// `run.timestamp`, formatted for display (e.g. `"2026-09-11 14:30
    /// UTC"`) rather than a raw Unix timestamp.
    pub when: String,
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
        let when = human_time(run.timestamp);
        forms.push(FormReport { run, changes, when });
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
}
