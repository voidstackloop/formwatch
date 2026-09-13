//! Append-only audit logging.
//!
//! Enterprise deployments often need a tamper-evident record of *what
//! formwatch did* — which URLs it touched and when — separate from the
//! rich result history. Each completed run is appended as one JSON line
//! (JSON Lines), deliberately **without** screenshots or other captured
//! page content, so the audit trail itself carries no personal data.

use crate::checks::Status;
use crate::error::Result;
use crate::history::RunResult;
use serde::Serialize;
use std::io::Write;
use std::path::Path;

#[derive(Serialize)]
struct AuditEntry<'a> {
    timestamp: i64,
    name: &'a str,
    url: &'a str,
    checks: Vec<AuditCheck<'a>>,
}

#[derive(Serialize)]
struct AuditCheck<'a> {
    name: &'a str,
    status: Status,
}

/// Renders one audit line for `run` (no trailing newline) — pure, so it
/// can be tested without touching the filesystem.
pub fn line_for(run: &RunResult) -> Result<String> {
    let entry = AuditEntry {
        timestamp: run.timestamp,
        name: &run.name,
        url: &run.url,
        checks: run
            .checks
            .iter()
            .map(|c| AuditCheck {
                name: &c.name,
                status: c.status,
            })
            .collect(),
    };
    Ok(serde_json::to_string(&entry)?)
}

/// Appends one audit line for `run` to `path`, creating parent
/// directories as needed. Append mode means concurrent processes (or
/// successive runs) never clobber one another.
pub fn append(path: &Path, run: &RunResult) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let line = line_for(run)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;

    fn run() -> RunResult {
        RunResult {
            schema_version: crate::history::SCHEMA_VERSION,
            name: "Permit".into(),
            url: "https://city.gov/permit".into(),
            timestamp: 42,
            checks: vec![
                CheckResult {
                    name: "Accessibility".into(),
                    status: Status::Fail,
                    detail: "2 violations".into(),
                    screenshot: Some("data:image/png;base64,AAAA".into()),
                },
                CheckResult {
                    name: "Mobile usability".into(),
                    status: Status::Pass,
                    detail: "ok".into(),
                    screenshot: None,
                },
            ],
        }
    }

    #[test]
    fn audit_line_records_verdicts_but_never_screenshots() {
        let line = line_for(&run()).expect("render");
        assert!(line.contains("\"url\":\"https://city.gov/permit\""));
        assert!(line.contains("\"status\":\"Fail\""));
        assert!(line.contains("\"status\":\"Pass\""));
        assert!(
            !line.contains("data:image"),
            "the audit trail must not carry captured page content: {line}"
        );
    }

    #[test]
    fn append_writes_one_json_line_and_creates_parents() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-append");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("audit.jsonl");

        append(&path, &run()).expect("append");
        append(&path, &run()).expect("append again");

        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per run: {text}");
        assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
