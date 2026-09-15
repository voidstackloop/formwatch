//! Machine-readable export formats: JUnit XML and SARIF.
//!
//! `--json` covers scripting, but enterprise pipelines usually want a
//! format their existing tooling already understands: JUnit XML for test
//! dashboards, SARIF for code-scanning UIs (e.g. GitHub code scanning).
//! Both are rendered from the same [`RunResult`] the rest of formwatch
//! uses, so there's no second source of truth.

use crate::baseline::Baseline;
use crate::checks::Status;
use crate::history::RunResult;
use chrono::DateTime;

fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            // XML 1.0 forbids most control characters outright; drop them
            // rather than emit a document no parser will accept.
            c if (c as u32) < 0x20 && c != '\t' && c != '\n' && c != '\r' => {}
            // U+FFFE/U+FFFF are also forbidden in XML 1.0.
            '\u{FFFE}' | '\u{FFFF}' => {}
            other => out.push(other),
        }
    }
    out
}

fn rfc3339(ts: i64) -> String {
    DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%dT%H:%M:%SZ").to_string())
        .unwrap_or_else(|| ts.to_string())
}

/// Renders `runs` as a JUnit XML document. A `Fail` check is a
/// `<failure>` (turns CI red); a `Warn` is a `<skipped>` so it's visible
/// without failing the build; a `Pass` is a bare `<testcase>`. A finding
/// accepted by `baseline` is emitted as `<skipped>` regardless of
/// severity, so an allow-listed `FAIL` doesn't fail CI.
pub fn junit_xml(runs: &[RunResult], baseline: Option<&Baseline>) -> String {
    let accepted = |run: &RunResult, check: &crate::checks::CheckResult| {
        check.status != Status::Pass
            && baseline
                .map(|b| b.is_accepted(&run.url, &check.name, check.status))
                .unwrap_or(false)
    };

    let tests: usize = runs.iter().map(|r| r.checks.len()).sum();
    let failures: usize = runs
        .iter()
        .flat_map(|r| r.checks.iter().map(move |c| (r, c)))
        .filter(|(run, c)| c.status == Status::Fail && !accepted(run, c))
        .count();
    let skipped: usize = runs
        .iter()
        .flat_map(|r| r.checks.iter().map(move |c| (r, c)))
        .filter(|(run, c)| c.status == Status::Warn || accepted(run, c))
        .count();

    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str(&format!(
        "<testsuites name=\"formwatch\" tests=\"{tests}\" failures=\"{failures}\" skipped=\"{skipped}\">\n"
    ));

    for run in runs {
        let n = run.checks.len();
        let f = run
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail && !accepted(run, c))
            .count();
        let s = run
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn || accepted(run, c))
            .count();
        out.push_str(&format!(
            "  <testsuite name=\"{}\" tests=\"{n}\" failures=\"{f}\" skipped=\"{s}\" timestamp=\"{}\">\n",
            xml_escape(&run.name),
            rfc3339(run.timestamp),
        ));
        for check in &run.checks {
            let name = xml_escape(&check.name);
            let classname = xml_escape(&run.url);
            if accepted(run, check) {
                out.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{classname}\"><skipped message=\"BASELINED ({}): {}\"/></testcase>\n",
                    check.status.label(),
                    xml_escape(&check.detail),
                ));
                continue;
            }
            match check.status {
                Status::Pass => out.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{classname}\"/>\n"
                )),
                Status::Warn => out.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{classname}\"><skipped message=\"WARN: {}\"/></testcase>\n",
                    xml_escape(&check.detail),
                )),
                Status::Fail => out.push_str(&format!(
                    "    <testcase name=\"{name}\" classname=\"{classname}\"><failure message=\"FAIL: {}\">{}</failure></testcase>\n",
                    xml_escape(&check.detail),
                    xml_escape(&check.detail),
                )),
            }
        }
        out.push_str("  </testsuite>\n");
    }
    out.push_str("</testsuites>\n");
    out
}

/// A stable, lower-kebab rule id for a check name, suitable for SARIF.
fn rule_id(name: &str) -> String {
    let slug: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let slug = slug.trim_matches('-').to_string();
    format!("formwatch/{slug}")
}

/// Renders `runs` as a SARIF 2.1.0 document. Only non-Pass checks become
/// results; `Fail` maps to `error`, `Warn` to `warning`. A finding
/// accepted by `baseline` is marked `baselineState: "unchanged"` and
/// carries a SARIF `suppressions` entry, so code-scanning UIs can treat it
/// as pre-existing rather than new.
pub fn sarif_json(runs: &[RunResult], baseline: Option<&Baseline>) -> serde_json::Value {
    let mut rules: Vec<serde_json::Value> = Vec::new();
    let mut rule_ids: Vec<String> = Vec::new();
    let mut results: Vec<serde_json::Value> = Vec::new();

    for run in runs {
        for check in &run.checks {
            if check.status == Status::Pass {
                continue;
            }
            let accepted = baseline
                .map(|b| b.is_accepted(&run.url, &check.name, check.status))
                .unwrap_or(false);
            let id = rule_id(&check.name);
            if !rule_ids.contains(&id) {
                rules.push(serde_json::json!({
                    "id": id,
                    "name": check.name,
                    "shortDescription": { "text": check.name },
                }));
                rule_ids.push(id.clone());
            }
            let mut result = serde_json::json!({
                "ruleId": id,
                "level": if check.status == Status::Fail { "error" } else { "warning" },
                "message": { "text": check.detail },
                "baselineState": if accepted { "unchanged" } else { "new" },
                "locations": [{
                    "physicalLocation": {
                        "artifactLocation": { "uri": run.url }
                    }
                }],
                "properties": {
                    "formName": run.name,
                    "status": check.status.label(),
                    "baselined": accepted,
                }
            });
            if accepted {
                result["suppressions"] = serde_json::json!([{
                    "kind": "external",
                    "status": "accepted",
                    "justification": "Listed in the formwatch baseline."
                }]);
            }
            results.push(result);
        }
    }

    serde_json::json!({
        "$schema": "https://json.schemastore.org/sarif-2.1.0.json",
        "version": "2.1.0",
        "runs": [{
            "tool": {
                "driver": {
                    "name": "formwatch",
                    "informationUri": "https://github.com/voidstackloop/formwatch",
                    "version": env!("CARGO_PKG_VERSION"),
                    "rules": rules,
                }
            },
            "results": results,
        }]
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;
    use crate::history::SCHEMA_VERSION;

    fn check(name: &str, status: Status, detail: &str) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: detail.into(),
            screenshot: None,
        }
    }

    fn run() -> RunResult {
        RunResult {
            schema_version: SCHEMA_VERSION,
            name: "Permit <script>".into(),
            url: "https://city.gov/permit?x=1&y=2".into(),
            timestamp: 0,
            checks: vec![
                check("Accessibility", Status::Fail, "2 violations"),
                check("Mobile usability", Status::Warn, "overflow"),
                check("Autofill hints", Status::Pass, "ok"),
            ],
        }
    }

    #[test]
    fn junit_counts_and_maps_statuses() {
        let xml = junit_xml(&[run()], None);
        assert!(xml.starts_with("<?xml"));
        assert!(xml.contains("tests=\"3\""));
        assert!(xml.contains("failures=\"1\""));
        assert!(xml.contains("skipped=\"1\""));
        assert!(xml.contains("<failure message=\"FAIL: 2 violations\""));
        assert!(xml.contains("<skipped message=\"WARN: overflow\""));
        assert!(xml.contains("<testcase name=\"Autofill hints\""));
        // The form name contains markup; it must be escaped, not injected.
        assert!(xml.contains("Permit &lt;script&gt;"));
        assert!(!xml.contains("<script>"));
    }

    #[test]
    fn junit_turns_a_baselined_failure_into_a_skip() {
        let baseline = Baseline {
            schema_version: crate::baseline::BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: vec![crate::baseline::Entry {
                url: "https://city.gov/permit?x=1&y=2".into(),
                name: "Permit".into(),
                check: "Accessibility".into(),
                status: Status::Fail,
            }],
        };
        let xml = junit_xml(&[run()], Some(&baseline));
        assert!(xml.contains("failures=\"0\""), "{xml}");
        assert!(xml.contains("BASELINED (FAIL)"), "{xml}");
        assert!(
            !xml.contains("<failure"),
            "a baselined Fail must not fail CI"
        );
    }

    #[test]
    fn sarif_only_includes_non_pass_and_maps_levels() {
        let sarif = sarif_json(&[run()], None);
        assert_eq!(sarif["version"], "2.1.0");
        assert_eq!(sarif["runs"][0]["tool"]["driver"]["name"], "formwatch");
        let results = sarif["runs"][0]["results"].as_array().expect("results");
        assert_eq!(results.len(), 2, "Pass checks are excluded");
        let levels: Vec<&str> = results
            .iter()
            .map(|r| r["level"].as_str().unwrap())
            .collect();
        assert!(levels.contains(&"error"));
        assert!(levels.contains(&"warning"));
        assert_eq!(results[0]["ruleId"], "formwatch/accessibility");
    }

    #[test]
    fn sarif_marks_a_baselined_finding_as_unchanged_and_suppressed() {
        let baseline = Baseline {
            schema_version: crate::baseline::BASELINE_SCHEMA_VERSION,
            generated_at: 0,
            entries: vec![crate::baseline::Entry {
                url: "https://city.gov/permit?x=1&y=2".into(),
                name: "Permit".into(),
                check: "Accessibility".into(),
                status: Status::Fail,
            }],
        };
        let sarif = sarif_json(&[run()], Some(&baseline));
        let results = sarif["runs"][0]["results"].as_array().expect("results");
        let baselined = results
            .iter()
            .find(|r| r["ruleId"] == "formwatch/accessibility")
            .expect("accessibility result");
        assert_eq!(baselined["baselineState"], "unchanged");
        assert_eq!(baselined["suppressions"][0]["kind"], "external");
        // The non-baselined warning stays "new".
        let other = results
            .iter()
            .find(|r| r["ruleId"] == "formwatch/mobile-usability")
            .expect("mobile result");
        assert_eq!(other["baselineState"], "new");
    }
}
