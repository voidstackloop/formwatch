use crate::history::{self, CheckChange, RunResult};
use anyhow::Result;
use askama::Template;
use chrono::DateTime;
use std::path::Path;

pub struct FormReport {
    pub run: RunResult,
    pub changes: Vec<CheckChange>,
    pub when: String,
}

#[derive(Template)]
#[template(path = "report.html")]
pub struct ReportTemplate {
    pub forms: Vec<FormReport>,
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
