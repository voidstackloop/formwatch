//! Baseline / allow-list of accepted findings.
//!
//! A mature deployment rarely has zero findings; it has a known set it is
//! tracking. Without a baseline, `formwatch monitor` fails CI on day one
//! and every day after, so people stop looking. A baseline records the
//! findings that are accepted *at their current severity*:
//!
//! - A current finding is **suppressed** if a baseline entry for the same
//!   form URL and check has a status at least as severe as the current
//!   one.
//! - A finding that is **worse** than its baseline (a baselined `WARN`
//!   that is now `FAIL`) is a real breach — you cannot accept a warn and
//!   silently absorb a fail.
//! - An entry whose finding has improved or disappeared is **stale** and
//!   reported, so the baseline gets cleaned up rather than rotting.
//!
//! The file is versioned JSON, generated from the current history with
//! `formwatch baseline --write`.

use crate::checks::{CheckResult, Status};
use crate::error::Result;
use crate::history::RunResult;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// Schema version of the baseline file. Bumping it is a breaking change.
pub const BASELINE_SCHEMA_VERSION: u32 = 1;

fn severity(status: Status) -> u8 {
    match status {
        Status::Pass => 0,
        Status::Warn => 1,
        Status::Fail => 2,
    }
}

/// One accepted finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// The form URL this finding belongs to (the stable identity).
    pub url: String,
    /// The form's human label, for readability only.
    #[serde(default)]
    pub name: String,
    /// Which check was accepted.
    pub check: String,
    /// The severity that was accepted.
    pub status: Status,
}

/// A versioned set of accepted findings.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Baseline {
    /// See [`BASELINE_SCHEMA_VERSION`].
    pub schema_version: u32,
    /// When the baseline was generated (Unix seconds).
    #[serde(default)]
    pub generated_at: i64,
    /// The accepted findings.
    #[serde(default)]
    pub entries: Vec<Entry>,
}

impl Baseline {
    /// An empty baseline.
    pub fn empty() -> Self {
        Self {
            schema_version: BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: Vec::new(),
        }
    }

    /// Builds a baseline from the latest run of every known form, keeping
    /// every non-Pass check. When `runs` happens to hold more than one run
    /// for a URL, the newest one wins for each (url, check) — keeping the
    /// older would record a stale severity that could then absorb (or, if
    /// more severe, wrongly uphold) a later finding.
    pub fn from_runs(runs: &[RunResult]) -> Self {
        let mut latest: std::collections::HashMap<(&str, &str), (i64, Entry)> =
            std::collections::HashMap::new();
        for run in runs {
            for c in run.checks.iter().filter(|c| c.status != Status::Pass) {
                let key = (run.url.as_str(), c.name.as_str());
                if latest.get(&key).is_none_or(|(ts, _)| run.timestamp >= *ts) {
                    latest.insert(
                        key,
                        (
                            run.timestamp,
                            Entry {
                                url: run.url.clone(),
                                name: run.name.clone(),
                                check: c.name.clone(),
                                status: c.status,
                            },
                        ),
                    );
                }
            }
        }
        let mut entries: Vec<Entry> = latest.into_values().map(|(_, entry)| entry).collect();
        entries.sort_by(|a, b| {
            (a.url.as_str(), a.check.as_str()).cmp(&(b.url.as_str(), b.check.as_str()))
        });
        Self {
            schema_version: BASELINE_SCHEMA_VERSION,
            generated_at: chrono::Utc::now().timestamp(),
            entries,
        }
    }

    /// The status accepted for `(url, check)`, if any.
    pub fn accepted_status(&self, url: &str, check: &str) -> Option<Status> {
        self.entries
            .iter()
            .find(|e| e.url == url && e.check == check)
            .map(|e| e.status)
    }

    /// Whether `current` is covered by the baseline for `(url, check)`
    /// without being worse than what was accepted.
    pub fn is_accepted(&self, url: &str, check: &str, current: Status) -> bool {
        self.accepted_status(url, check)
            .map(|accepted| severity(current) <= severity(accepted))
            .unwrap_or(false)
    }

    /// Entries that are no longer needed: the finding is gone, or has
    /// improved below the accepted severity. Reported so a baseline can be
    /// regenerated instead of silently accumulating dead entries.
    pub fn stale<'a>(&'a self, runs: &[RunResult]) -> Vec<&'a Entry> {
        self.entries
            .iter()
            .filter(|entry| {
                let current = runs
                    .iter()
                    .find(|r| r.url == entry.url)
                    .and_then(|run| run.checks.iter().find(|c| c.name == entry.check));
                match current {
                    // Check disappeared entirely: stale.
                    None => true,
                    // Improved (or resolved) below the accepted severity.
                    Some(c) => severity(c.status) < severity(entry.status),
                }
            })
            .collect()
    }

    /// Reads a baseline from `path`.
    pub fn load(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)?;
        Ok(serde_json::from_str(&text)?)
    }

    /// Loads `path`, falling back to an empty baseline (with a warning)
    /// when the file is missing or unreadable — so a first run doesn't
    /// hard-fail just because no baseline has been written yet.
    pub fn load_or_empty(path: &Path) -> Self {
        match Self::load(path) {
            Ok(baseline) => {
                if baseline.schema_version != BASELINE_SCHEMA_VERSION {
                    tracing::warn!(
                        path = %path.display(),
                        found = baseline.schema_version,
                        expected = BASELINE_SCHEMA_VERSION,
                        "baseline schema version differs from this build"
                    );
                }
                baseline
            }
            Err(e) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "no usable baseline found; treating all findings as new"
                );
                Self::empty()
            }
        }
    }

    /// Writes the baseline to `path`, creating parent directories.
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    /// Number of accepted findings.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the baseline has no entries.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Whether a single check breaches the fail-on policy, given an optional
/// baseline. This is the one place the suppression rule lives, so the exit
/// code, the plain-text annotation, and notifications all agree.
pub fn check_breaches(
    run: &RunResult,
    check: &CheckResult,
    fail_on: crate::config::FailOn,
    baseline: Option<&Baseline>,
) -> bool {
    let threshold_met = match fail_on {
        crate::config::FailOn::Fail => check.status == Status::Fail,
        crate::config::FailOn::Warn => check.status == Status::Fail || check.status == Status::Warn,
    };
    threshold_met
        && !baseline
            .map(|b| b.is_accepted(&run.url, &check.name, check.status))
            .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::SCHEMA_VERSION;

    fn check(name: &str, status: Status) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: String::new(),
            screenshot: None,
        }
    }

    fn run(url: &str, checks: Vec<CheckResult>) -> RunResult {
        RunResult {
            schema_version: SCHEMA_VERSION,
            name: "x".into(),
            url: url.into(),
            timestamp: 1,
            checks,
        }
    }

    #[test]
    fn from_runs_keeps_only_non_pass_findings() {
        let runs = vec![run(
            "https://a.gov",
            vec![
                check("Accessibility", Status::Fail),
                check("Mobile usability", Status::Pass),
                check("Validation errors", Status::Warn),
            ],
        )];
        let baseline = Baseline::from_runs(&runs);
        assert_eq!(baseline.len(), 2);
        assert!(baseline.entries.iter().any(|e| e.check == "Accessibility"));
        assert!(
            baseline
                .entries
                .iter()
                .any(|e| e.check == "Validation errors")
        );
        assert!(
            !baseline
                .entries
                .iter()
                .any(|e| e.check == "Mobile usability")
        );
    }

    #[test]
    fn from_runs_keeps_the_newest_status_for_a_repeated_url() {
        let older = RunResult {
            schema_version: SCHEMA_VERSION,
            name: "x".into(),
            url: "https://a.gov".into(),
            timestamp: 1,
            checks: vec![check("Accessibility", Status::Fail)],
        };
        let newer = RunResult {
            schema_version: SCHEMA_VERSION,
            name: "x".into(),
            url: "https://a.gov".into(),
            timestamp: 2,
            checks: vec![check("Accessibility", Status::Warn)],
        };
        let baseline = Baseline::from_runs(&[older, newer]);
        assert_eq!(baseline.len(), 1);
        assert_eq!(
            baseline.entries[0].status,
            Status::Warn,
            "the newest run's status must win, not the first one seen"
        );
    }

    #[test]
    fn accepts_equal_or_better_but_not_worse() {
        let baseline = Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: vec![Entry {
                url: "https://a.gov".into(),
                name: "a".into(),
                check: "Accessibility".into(),
                status: Status::Warn,
            }],
        };
        // Equal: accepted.
        assert!(baseline.is_accepted("https://a.gov", "Accessibility", Status::Warn));
        // Better: accepted.
        assert!(baseline.is_accepted("https://a.gov", "Accessibility", Status::Pass));
        // Worse: a Warn baseline must not absorb a Fail.
        assert!(!baseline.is_accepted("https://a.gov", "Accessibility", Status::Fail));
        // Unknown: not accepted.
        assert!(!baseline.is_accepted("https://a.gov", "Other", Status::Pass));
        assert!(!baseline.is_accepted("https://b.gov", "Accessibility", Status::Warn));
    }

    #[test]
    fn stale_entries_are_reported_when_resolved() {
        let baseline = Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: vec![
                Entry {
                    url: "https://a.gov".into(),
                    name: "a".into(),
                    check: "Accessibility".into(),
                    status: Status::Fail,
                },
                Entry {
                    url: "https://a.gov".into(),
                    name: "a".into(),
                    check: "Gone".into(),
                    status: Status::Warn,
                },
            ],
        };
        // Accessibility improved to Pass; "Gone" no longer runs.
        let runs = vec![run(
            "https://a.gov",
            vec![check("Accessibility", Status::Pass)],
        )];
        let stale = baseline.stale(&runs);
        assert_eq!(stale.len(), 2);
    }

    #[test]
    fn breach_rule_respects_the_baseline() {
        let result = run("https://a.gov", vec![check("Accessibility", Status::Fail)]);
        let baseline = Baseline {
            schema_version: BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: vec![Entry {
                url: "https://a.gov".into(),
                name: "a".into(),
                check: "Accessibility".into(),
                status: Status::Fail,
            }],
        };
        let check = &result.checks[0];
        use crate::config::FailOn;
        assert!(!check_breaches(
            &result,
            check,
            FailOn::Fail,
            Some(&baseline)
        ));
        // Without the baseline, it breaches.
        assert!(check_breaches(&result, check, FailOn::Fail, None));
        // An accepted Warn doesn't absorb a Fail.
        let warn_baseline = Baseline {
            entries: vec![Entry {
                url: "https://a.gov".into(),
                name: "a".into(),
                check: "Accessibility".into(),
                status: Status::Warn,
            }],
            ..Baseline::empty()
        };
        assert!(check_breaches(
            &result,
            check,
            FailOn::Fail,
            Some(&warn_baseline)
        ));
    }

    #[test]
    fn round_trips_through_the_filesystem() {
        let dir = std::env::temp_dir().join("formwatch-test-baseline");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("baseline.json");
        let baseline = Baseline::from_runs(&[run(
            "https://a.gov",
            vec![check("Accessibility", Status::Fail)],
        )]);
        baseline.save(&path).expect("save");
        let loaded = Baseline::load(&path).expect("load");
        assert_eq!(loaded.entries, baseline.entries);
        assert_eq!(loaded.schema_version, BASELINE_SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
