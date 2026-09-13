use crate::checks::{CheckResult, Status};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

/// Version of the persisted/`--json` run schema.
///
/// Every [`RunResult`] written from this build carries this value in its
/// `schema_version` field. It exists so downstream consumers (the
/// community dashboard, CI pipelines, external integrations) can detect a
/// breaking shape change instead of silently misreading newer data.
/// Bumping it is a breaking change. Records written before the field
/// existed deserialize as version 1 via [`default_schema_version`].
pub const SCHEMA_VERSION: u32 = 1;

fn default_schema_version() -> u32 {
    SCHEMA_VERSION
}

/// One complete run of every check against one form — what
/// [`save_run`]/[`load_runs`] persist and load, and the JSON shape of
/// `--json` output. This is also the community dashboard's data
/// contract (`results/index.json`): changing this shape is a breaking
/// change for anything reading history off disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunResult {
    /// Version of this record's schema (see [`SCHEMA_VERSION`]). Defaults
    /// to 1 for history files written before the field existed.
    #[serde(default = "default_schema_version")]
    pub schema_version: u32,
    /// The form's label — from `--name`, a `forms.yml` entry's `name`,
    /// or the URL itself if neither was given.
    pub name: String,
    /// The URL that was checked. Also the key used to find this run's
    /// history directory and to diff it against its previous run.
    pub url: String,
    /// Unix timestamp (seconds) of when this run happened.
    pub timestamp: i64,
    /// Every check's result, in the order `run_all`/`run_custom_checks`
    /// produced them.
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

/// Persists `run` under `base` (e.g. `.formwatch/history`, or whatever
/// `--history-dir` points at), in a subdirectory keyed by its URL.
/// Returns the path it was written to. Never overwrites an existing run:
/// if a file for this exact timestamp already exists, appends a `-N`
/// suffix instead, so two runs completing within the same wall-clock
/// second don't silently clobber each other.
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

/// One check's status change between two consecutive runs of the same
/// form — what [`diff`] produces and what "Changed since previous run"
/// (in both plain-text and `--html` output) is built from.
#[derive(Debug, Clone)]
pub struct CheckChange {
    /// Which check changed status. Matched by name, not position, so
    /// changes still make sense if checks are reordered or a new one is
    /// added between runs.
    pub name: String,
    /// The status it had in the older run.
    pub from: Status,
    /// The status it has in the newer run.
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

/// One check's behavior across a form's *entire* recorded history, not
/// just the single most recent run — whether it's been flip-flopping
/// between statuses rather than settling into one sustained state. A
/// flaky check calls for a different response (investigate *why* it's
/// inconsistent) than a genuine regression or fix (investigate the one
/// real change); [`diff`] alone can't tell the two apart since it only
/// ever compares two adjacent runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flakiness {
    /// Which check this is — matched by name across every run, the same
    /// way [`diff`] matches between two.
    pub name: String,
    /// How many times this check's status differed from the run right
    /// before it, across every run recorded for the form. 0 means it's
    /// never changed; 1 means a single sustained change (a genuine
    /// regression or fix); 2 or more means it's genuinely flapping back
    /// and forth, not settling into a state.
    pub transitions: usize,
    /// The status as of the most recent run.
    pub current: Status,
}

impl Flakiness {
    /// True once a check has changed status more than once across its
    /// recorded history — flapping, not a single sustained change.
    pub fn is_flaky(&self) -> bool {
        self.transitions >= 2
    }
}

/// Computes every check's transition count across `runs` (oldest first,
/// as [`load_runs`] returns them) for one form. Only checks present in
/// the most recent run are reported — a check that's since been removed
/// (a custom check deleted, a built-in one renamed) has nothing current
/// to report flakiness *of*.
pub fn flakiness(runs: &[RunResult]) -> Vec<Flakiness> {
    let Some(latest) = runs.last() else {
        return vec![];
    };
    latest
        .checks
        .iter()
        .map(|c| {
            let history: Vec<Status> = runs
                .iter()
                .filter_map(|r| r.checks.iter().find(|x| x.name == c.name))
                .map(|x| x.status)
                .collect();
            let transitions = history.windows(2).filter(|w| w[0] != w[1]).count();
            Flakiness {
                name: c.name.clone(),
                transitions,
                current: c.status,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn check(name: &str, status: Status) -> CheckResult {
        CheckResult {
            name: name.to_string(),
            status,
            detail: String::new(),
            screenshot: None,
        }
    }

    fn run(checks: Vec<CheckResult>) -> RunResult {
        RunResult {
            schema_version: SCHEMA_VERSION,
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
    fn flakiness_is_zero_for_a_check_that_never_changed() {
        let runs = vec![
            run(vec![check("Accessibility", Status::Pass)]),
            run(vec![check("Accessibility", Status::Pass)]),
            run(vec![check("Accessibility", Status::Pass)]),
        ];
        let report = flakiness(&runs);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].transitions, 0);
        assert!(!report[0].is_flaky());
    }

    #[test]
    fn flakiness_does_not_flag_a_single_sustained_regression() {
        // diff() alone would call this "a regression" — which is exactly
        // right here: it changed once and stayed changed. Flakiness is
        // specifically about the *unstable* case diff can't see because
        // it only ever compares two adjacent runs.
        let runs = vec![
            run(vec![check("Accessibility", Status::Pass)]),
            run(vec![check("Accessibility", Status::Fail)]),
            run(vec![check("Accessibility", Status::Fail)]),
        ];
        let report = flakiness(&runs);
        assert_eq!(report[0].transitions, 1);
        assert!(
            !report[0].is_flaky(),
            "a single sustained change is a real regression, not flakiness"
        );
    }

    #[test]
    fn flakiness_flags_a_check_that_flips_back_and_forth() {
        let runs = vec![
            run(vec![check("Accessibility", Status::Pass)]),
            run(vec![check("Accessibility", Status::Fail)]),
            run(vec![check("Accessibility", Status::Pass)]),
            run(vec![check("Accessibility", Status::Fail)]),
        ];
        let report = flakiness(&runs);
        assert_eq!(report[0].transitions, 3);
        assert!(report[0].is_flaky());
    }

    #[test]
    fn flakiness_only_reports_checks_present_in_the_latest_run() {
        let runs = vec![
            run(vec![check("Removed check", Status::Fail)]),
            run(vec![check("Accessibility", Status::Pass)]),
        ];
        let report = flakiness(&runs);
        assert_eq!(report.len(), 1);
        assert_eq!(report[0].name, "Accessibility");
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

    #[test]
    fn all_known_forms_returns_each_forms_newest_run_sorted_newest_first() {
        // Never directly tested before — only inferred through
        // report::build's single-form test, which doesn't exercise
        // "each form's own newest, independent of the others" or the
        // cross-form sort order at all.
        let dir = std::env::temp_dir().join("formwatch-test-all-known-forms");
        let _ = fs::remove_dir_all(&dir);

        // Form A: two runs, newer one at ts=300.
        save_run(&dir, &run_at("https://city.gov/a", 100)).expect("save");
        save_run(&dir, &run_at("https://city.gov/a", 300)).expect("save");
        // Form B: a single run at ts=200 — between A's two runs, so a
        // naive "last file written" or "first form found" approach would
        // get the ordering wrong.
        save_run(&dir, &run_at("https://city.gov/b", 200)).expect("save");

        let forms = all_known_forms(&dir).expect("all_known_forms");
        assert_eq!(forms.len(), 2, "one entry per form, not per run");
        assert_eq!(forms[0].url, "https://city.gov/a");
        assert_eq!(
            forms[0].timestamp, 300,
            "should be A's newest run, not its oldest"
        );
        assert_eq!(forms[1].url, "https://city.gov/b");
        assert_eq!(forms[1].timestamp, 200);

        let _ = fs::remove_dir_all(&dir);
    }

    fn run_at(url: &str, timestamp: i64) -> RunResult {
        RunResult {
            schema_version: SCHEMA_VERSION,
            name: "x".into(),
            url: url.into(),
            timestamp,
            checks: vec![],
        }
    }

    #[test]
    fn missing_schema_version_defaults_to_one() {
        // History written before the field existed must still load.
        let parsed: RunResult = serde_json::from_str(
            r#"{"name":"x","url":"https://city.gov/a","timestamp":1,"checks":[]}"#,
        )
        .expect("parse legacy record");
        assert_eq!(parsed.schema_version, SCHEMA_VERSION);
    }
}
