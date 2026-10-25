//! History analytics behind `trends`, `flakes`, `diff`, `history`, and
//! `export`: pass-rate summaries, streaks, flaky-check reports, and
//! full-history dumps. All pure over [`crate::history`] data — no browser,
//! no network — so CI dashboards and humans get the same numbers.

use crate::checks::Status;
use crate::history::{self, RunResult};
use std::path::Path;

/// Per-form trend over its recorded runs.
#[derive(Debug, Clone)]
pub struct FormTrend {
    /// Form label (latest run's name).
    pub name: String,
    /// Form URL.
    pub url: String,
    /// Number of runs considered.
    pub runs: usize,
    /// Share of runs whose worst check was Pass (0.0–1.0).
    pub pass_rate: f64,
    /// Worst status in the latest run.
    pub current: Status,
    /// Check-level streaks as of the latest run.
    pub streaks: Vec<history::CheckStreak>,
    /// Checks currently flapping (2+ transitions).
    pub flaky: Vec<history::Flakiness>,
}

/// Summarizes the last `last_n` runs (0 = all) for one form's URL.
pub fn trend_for(history_dir: &Path, url: &str, last_n: usize) -> Option<FormTrend> {
    let mut runs = history::load_runs(history_dir, url).ok()?;
    if runs.is_empty() {
        return None;
    }
    if last_n > 0 && runs.len() > last_n {
        runs = runs[runs.len() - last_n..].to_vec();
    }
    let latest = runs.last()?.clone();
    let total = runs.len() as f64;
    let passing = runs
        .iter()
        .filter(|r| r.checks.iter().all(|c| c.status == Status::Pass))
        .count() as f64;
    let current = if latest.checks.iter().any(|c| c.status == Status::Fail) {
        Status::Fail
    } else if latest.checks.iter().any(|c| c.status == Status::Warn) {
        Status::Warn
    } else {
        Status::Pass
    };
    let flaky = history::flakiness(&runs)
        .into_iter()
        .filter(|f| f.is_flaky())
        .collect();
    Some(FormTrend {
        name: latest.name.clone(),
        url: latest.url.clone(),
        runs: runs.len(),
        pass_rate: passing / total,
        current,
        streaks: history::streaks(&runs),
        flaky,
    })
}

/// Trends for every known form, newest form first.
pub fn all_trends(history_dir: &Path, last_n: usize) -> Vec<FormTrend> {
    let mut forms = history::all_known_forms(history_dir).unwrap_or_default();
    forms.sort_by_key(|a| std::cmp::Reverse(a.timestamp));
    forms
        .iter()
        .filter_map(|r| trend_for(history_dir, &r.url, last_n))
        .collect()
}

/// Human-readable one-block-per-form rendering for `trends`.
pub fn render_trends_text(trends: &[FormTrend]) -> String {
    if trends.is_empty() {
        return "No history yet — run `formwatch test` or `monitor` first.\n".into();
    }
    let mut out = String::new();
    for t in trends {
        out.push_str(&format!(
            "{} ({})\n  runs: {}  pass rate: {:.0}%  current: {}\n",
            t.name,
            t.url,
            t.runs,
            t.pass_rate * 100.0,
            match t.current {
                Status::Pass => "PASS",
                Status::Warn => "WARN",
                Status::Fail => "FAIL",
            }
        ));
        for s in t.streaks.iter().filter(|s| s.status != Status::Pass) {
            out.push_str(&format!(
                "  {} {} ×{} since {}\n",
                s.status.label(),
                s.name,
                s.count,
                chrono::DateTime::from_timestamp(s.since, 0)
                    .map(|d| d.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| s.since.to_string())
            ));
        }
        for f in &t.flaky {
            out.push_str(&format!(
                "  FLAKY {} ({} transitions, now {})\n",
                f.name,
                f.transitions,
                f.current.label()
            ));
        }
    }
    out
}

/// Every recorded run for one form, oldest first — the `history` command.
pub fn history_for(history_dir: &Path, url_or_name: &str) -> Vec<RunResult> {
    // Accept either the URL or the form's display name.
    if let Ok(runs) = history::load_runs(history_dir, url_or_name)
        && !runs.is_empty()
    {
        return runs;
    }
    history::all_known_forms(history_dir)
        .unwrap_or_default()
        .into_iter()
        .find(|r| r.name == url_or_name)
        .map(|r| history::load_runs(history_dir, &r.url).unwrap_or_default())
        .unwrap_or_default()
}

/// Renders a compact run-history table.
pub fn render_history_text(runs: &[RunResult]) -> String {
    if runs.is_empty() {
        return "No runs found for that form.\n".into();
    }
    let mut out = String::from("timestamp            pass warn fail worst\n");
    for r in runs {
        let (mut p, mut w, mut f) = (0, 0, 0);
        for c in &r.checks {
            match c.status {
                Status::Pass => p += 1,
                Status::Warn => w += 1,
                Status::Fail => f += 1,
            }
        }
        let worst = if f > 0 {
            "FAIL"
        } else if w > 0 {
            "WARN"
        } else {
            "PASS"
        };
        let ts = chrono::DateTime::from_timestamp(r.timestamp, 0)
            .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| r.timestamp.to_string());
        out.push_str(&format!("{ts} {p:>4} {w:>4} {f:>4} {worst}\n"));
    }
    out
}

/// All recorded runs across every form, oldest first — the `export` source.
pub fn dump_all(history_dir: &Path) -> Vec<RunResult> {
    let mut all = Vec::new();
    let forms = history::all_known_forms(history_dir).unwrap_or_default();
    for form in forms {
        if let Ok(runs) = history::load_runs(history_dir, &form.url) {
            all.extend(runs);
        }
    }
    all.sort_by_key(|r| (r.timestamp, r.url.clone()));
    all
}

/// Full history as newline-delimited JSON (one run per line, for `jq`/warehouses).
pub fn export_jsonl(runs: &[RunResult]) -> String {
    let mut out = String::new();
    for r in runs {
        if let Ok(line) = serde_json::to_string(r) {
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// Full history as CSV (one row per check).
pub fn export_csv(runs: &[RunResult]) -> String {
    let mut out = String::from("timestamp,form,url,check,status,detail\n");
    for r in runs {
        for c in &r.checks {
            out.push_str(&format!(
                "{},{},{},{},{},{}\n",
                r.timestamp,
                csv_cell(&r.name),
                csv_cell(&r.url),
                csv_cell(&c.name),
                c.status.label(),
                csv_cell(&c.detail)
            ));
        }
    }
    out
}

fn csv_cell(s: &str) -> String {
    if s.contains([',', '"', '\n']) {
        format!("\"{}\"", s.replace('"', "\"\""))
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;

    fn run(name: &str, url: &str, ts: i64, statuses: &[Status]) -> RunResult {
        RunResult {
            schema_version: crate::history::SCHEMA_VERSION,
            name: name.into(),
            url: url.into(),
            timestamp: ts,
            checks: statuses
                .iter()
                .enumerate()
                .map(|(i, s)| CheckResult {
                    name: format!("Check{i}"),
                    status: *s,
                    detail: String::new(),
                    screenshot: None,
                })
                .collect(),
        }
    }

    fn seed(dir: &Path) {
        for (ts, st) in [(1, Status::Pass), (2, Status::Fail), (3, Status::Fail)] {
            history::save_run(dir, &run("F", "https://x.gov/f", ts, &[st, Status::Pass])).unwrap();
        }
    }

    #[test]
    fn trend_computes_pass_rate_and_streaks() {
        let dir = std::env::temp_dir().join("formwatch-test-trend");
        let _ = std::fs::remove_dir_all(&dir);
        seed(&dir);
        let t = trend_for(&dir, "https://x.gov/f", 0).unwrap();
        assert_eq!(t.runs, 3);
        assert!((t.pass_rate - 1.0 / 3.0).abs() < 1e-9);
        assert_eq!(t.current, Status::Fail);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn export_round_trips_through_jsonl() {
        let runs = vec![run("F", "https://x.gov/f", 7, &[Status::Warn])];
        let text = export_jsonl(&runs);
        assert_eq!(text.lines().count(), 1);
        let back: RunResult = serde_json::from_str(text.trim()).unwrap();
        assert_eq!(back.name, "F");
        let csv = export_csv(&runs);
        assert!(csv.starts_with("timestamp,form,url,check,status,detail"));
        assert!(csv.contains("WARN"));
    }
}
