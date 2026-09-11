use crate::checks::{CheckResult, Status};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    pub name: String,
    pub url: String,
    pub timestamp: i64,
    pub checks: Vec<CheckResult>,
}

/// A readable, *injective* directory name for a URL: alphanumeric bytes
/// pass through as-is, anything else becomes `_XX` (its hex byte value).
/// Collapsing every non-alphanumeric character to a single `_` (the
/// original approach) isn't injective — "apply-form" and "apply_form"
/// both became "apply_form", silently merging two different forms'
/// entire history into one directory. Since `_` here only ever starts a
/// 3-byte escape, never appears bare, the output is unambiguous: it can
/// always be re-split into single alphanumeric bytes or `_XX` triples.
fn slug(url: &str) -> String {
    url.trim_start_matches("https://")
        .trim_start_matches("http://")
        .bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("_{b:02x}")
            }
        })
        .collect()
}

fn dir_for(base: &Path, url: &str) -> PathBuf {
    base.join(slug(url))
}

pub fn save_run(base: &Path, run: &RunResult) -> Result<PathBuf> {
    let dir = dir_for(base, &run.url);
    fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    // timestamp is second-granularity, so two runs completing within the
    // same wall-clock second (plausible for a fast local fixture, or a
    // tight test/dev loop) would otherwise collide on the same filename
    // and the second write would silently clobber the first.
    let mut path = dir.join(format!("{}.json", run.timestamp));
    let mut suffix = 1;
    while path.exists() {
        path = dir.join(format!("{}-{suffix}.json", run.timestamp));
        suffix += 1;
    }

    fs::write(&path, serde_json::to_string_pretty(run)?)?;
    Ok(path)
}

/// All runs for this URL, oldest first.
pub fn load_runs(base: &Path, url: &str) -> Result<Vec<RunResult>> {
    let dir = dir_for(base, url);
    if !dir.exists() {
        return Ok(vec![]);
    }
    let mut runs = vec![];
    for entry in fs::read_dir(&dir)? {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) == Some("json") {
            let run: RunResult = serde_json::from_str(&fs::read_to_string(&path)?)
                .with_context(|| format!("parsing {}", path.display()))?;
            runs.push(run);
        }
    }
    runs.sort_by_key(|r| r.timestamp);
    Ok(runs)
}

/// Every URL formwatch has ever recorded a run for, newest run first.
pub fn all_known_forms(base: &Path) -> Result<Vec<RunResult>> {
    if !base.exists() {
        return Ok(vec![]);
    }
    let mut latest = vec![];
    for entry in fs::read_dir(base)? {
        let path = entry?.path();
        if !path.is_dir() {
            continue;
        }
        let mut newest: Option<RunResult> = None;
        for f in fs::read_dir(&path)? {
            let f = f?.path();
            if f.extension().and_then(|e| e.to_str()) == Some("json") {
                let run: RunResult = serde_json::from_str(&fs::read_to_string(&f)?)?;
                if newest.as_ref().is_none_or(|n| run.timestamp > n.timestamp) {
                    newest = Some(run);
                }
            }
        }
        if let Some(run) = newest {
            latest.push(run);
        }
    }
    latest.sort_by_key(|r| std::cmp::Reverse(r.timestamp));
    Ok(latest)
}

#[derive(Debug, Clone)]
pub struct CheckChange {
    pub name: String,
    pub from: Status,
    pub to: Status,
}

/// Per-check status changes between two runs of the same form, keyed by
/// check name so it survives checks being reordered or added/removed.
pub fn diff(prev: &RunResult, curr: &RunResult) -> Vec<CheckChange> {
    let mut changes = vec![];
    for c in &curr.checks {
        if let Some(p) = prev.checks.iter().find(|p| p.name == c.name)
            && p.status != c.status
        {
            changes.push(CheckChange {
                name: c.name.clone(),
                from: p.status,
                to: c.status,
            });
        }
    }
    changes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: Status) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status,
            detail: String::new(),
        }
    }

    fn run(checks: Vec<CheckResult>) -> RunResult {
        RunResult {
            name: "x".into(),
            url: "https://example.test/form".into(),
            timestamp: 0,
            checks,
        }
    }

    #[test]
    fn diff_reports_only_changed_checks_by_name() {
        let prev = run(vec![
            check("Accessibility", Status::Pass),
            check("Mobile usability", Status::Warn),
        ]);
        let curr = run(vec![
            check("Accessibility", Status::Fail),
            check("Mobile usability", Status::Warn),
        ]);

        let changes = diff(&prev, &curr);
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].name, "Accessibility");
        assert_eq!(changes[0].from, Status::Pass);
        assert_eq!(changes[0].to, Status::Fail);
    }

    #[test]
    fn diff_ignores_checks_new_to_the_current_run() {
        let prev = run(vec![check("Accessibility", Status::Pass)]);
        let curr = run(vec![
            check("Accessibility", Status::Pass),
            check("New check", Status::Fail),
        ]);

        assert!(diff(&prev, &curr).is_empty());
    }

    #[test]
    fn urls_differing_only_by_hyphen_vs_underscore_get_distinct_slugs() {
        // Regression test: collapsing every non-alphanumeric character to
        // a single '_' made "apply-form" and "apply_form" produce the
        // identical slug, silently merging two different forms' history.
        let hyphen = slug("https://city.gov/apply-form");
        let underscore = slug("https://city.gov/apply_form");
        assert_ne!(hyphen, underscore);
    }

    #[test]
    fn urls_differing_only_by_hyphen_vs_underscore_keep_independent_history() {
        let dir = std::env::temp_dir().join("formwatch-test-slug-collision");
        let _ = fs::remove_dir_all(&dir);

        let a = run_at("https://city.gov/apply-form", 1);
        let b = run_at("https://city.gov/apply_form", 2);
        save_run(&dir, &a).expect("save a");
        save_run(&dir, &b).expect("save b");

        let a_runs = load_runs(&dir, &a.url).expect("load a");
        let b_runs = load_runs(&dir, &b.url).expect("load b");
        assert_eq!(
            a_runs.len(),
            1,
            "form A's history should contain only its own run"
        );
        assert_eq!(
            b_runs.len(),
            1,
            "form B's history should contain only its own run"
        );
        assert_eq!(a_runs[0].url, a.url);
        assert_eq!(b_runs[0].url, b.url);

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_run_does_not_clobber_a_prior_run_with_the_same_timestamp() {
        // timestamps are second-granularity; two runs completing within
        // the same wall-clock second (plausible for a fast fixture, or a
        // tight dev/test loop) used to collide on the same filename and
        // the second write silently clobbered the first.
        let dir = std::env::temp_dir().join("formwatch-test-same-timestamp");
        let _ = fs::remove_dir_all(&dir);

        let a = run_at("https://city.gov/apply", 1000);
        let b = run_at("https://city.gov/apply", 1000);
        save_run(&dir, &a).expect("save a");
        save_run(&dir, &b).expect("save b");

        let saved = load_runs(&dir, &a.url).expect("load_runs");
        assert_eq!(
            saved.len(),
            2,
            "both same-timestamp runs should be preserved, not clobbered"
        );

        let _ = fs::remove_dir_all(&dir);
    }

    fn run_at(url: &str, timestamp: i64) -> RunResult {
        RunResult {
            name: "x".into(),
            url: url.into(),
            timestamp,
            checks: vec![],
        }
    }
}
