//! Prometheus metrics export.
//!
//! Beyond logs and the audit trail, a hosted deployment usually wants
//! numeric metrics its existing monitoring can scrape. This renders the
//! latest run of every known form in the Prometheus text exposition
//! format, so it can be served directly, written to a file, or dropped
//! into a node_exporter textfile-collector directory — no server required.
//!
//! Labels carry the form name, URL, and check name. That's potentially
//! high cardinality for a very large registry; operators who care can
//! scrape a subset or rely on the aggregate counts.

use crate::checks::Status;
use crate::history::RunResult;

fn escape_label(value: &str) -> String {
    // Prometheus label values escape backslash, double-quote, and newline.
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            other => out.push(other),
        }
    }
    out
}

fn status_code(status: Status) -> u8 {
    match status {
        Status::Pass => 0,
        Status::Warn => 1,
        Status::Fail => 2,
    }
}

/// Renders `runs` (the latest run per form) as Prometheus text format.
pub fn render(runs: &[RunResult]) -> String {
    let mut out = String::new();

    out.push_str("# HELP formwatch_forms Number of forms with a recorded run.\n");
    out.push_str("# TYPE formwatch_forms gauge\n");
    out.push_str(&format!("formwatch_forms {}\n", runs.len()));

    out.push_str(
        "# HELP formwatch_checks_failing Number of FAIL checks in the form's latest run.\n",
    );
    out.push_str("# TYPE formwatch_checks_failing gauge\n");
    for run in runs {
        let failing = run
            .checks
            .iter()
            .filter(|c| c.status == Status::Fail)
            .count();
        out.push_str(&format!(
            "formwatch_checks_failing{{form=\"{}\",url=\"{}\"}} {failing}\n",
            escape_label(&run.name),
            escape_label(&run.url),
        ));
    }

    out.push_str(
        "# HELP formwatch_checks_warning Number of WARN checks in the form's latest run.\n",
    );
    out.push_str("# TYPE formwatch_checks_warning gauge\n");
    for run in runs {
        let warning = run
            .checks
            .iter()
            .filter(|c| c.status == Status::Warn)
            .count();
        out.push_str(&format!(
            "formwatch_checks_warning{{form=\"{}\",url=\"{}\"}} {warning}\n",
            escape_label(&run.name),
            escape_label(&run.url),
        ));
    }

    out.push_str(
        "# HELP formwatch_check_last_run_timestamp_seconds Unix time of the form's latest run.\n",
    );
    out.push_str("# TYPE formwatch_check_last_run_timestamp_seconds gauge\n");
    for run in runs {
        out.push_str(&format!(
            "formwatch_check_last_run_timestamp_seconds{{form=\"{}\",url=\"{}\"}} {}\n",
            escape_label(&run.name),
            escape_label(&run.url),
            run.timestamp,
        ));
    }

    out.push_str(
        "# HELP formwatch_check_status Latest status of a check (0=pass, 1=warn, 2=fail).\n",
    );
    out.push_str("# TYPE formwatch_check_status gauge\n");
    for run in runs {
        for check in &run.checks {
            out.push_str(&format!(
                "formwatch_check_status{{form=\"{}\",url=\"{}\",check=\"{}\"}} {}\n",
                escape_label(&run.name),
                escape_label(&run.url),
                escape_label(&check.name),
                status_code(check.status),
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;
    use crate::history::SCHEMA_VERSION;

    fn check(name: &str, status: Status) -> CheckResult {
        CheckResult {
            name: name.into(),
            status,
            detail: String::new(),
            screenshot: None,
        }
    }

    fn run() -> RunResult {
        RunResult {
            schema_version: SCHEMA_VERSION,
            name: "Permit \"A\"\nline".into(),
            url: "https://city.gov/permit".into(),
            timestamp: 1700000000,
            checks: vec![
                check("Accessibility", Status::Fail),
                check("Mobile usability", Status::Warn),
                check("Autofill hints", Status::Pass),
            ],
        }
    }

    #[test]
    fn renders_help_type_and_expected_series() {
        let text = render(&[run()]);
        assert!(text.contains("# TYPE formwatch_forms gauge"));
        assert!(text.contains("formwatch_forms 1"));
        assert!(text.contains("formwatch_checks_failing{") && text.contains("} 1"));
        assert!(text.contains("formwatch_checks_warning{") && text.contains("} 1"));
        assert!(text.contains("formwatch_check_last_run_timestamp_seconds{"));
        assert!(text.contains("} 1700000000"));
    }

    #[test]
    fn check_status_uses_numeric_severity() {
        let text = render(&[run()]);
        assert!(text.contains("check=\"Accessibility\"} 2"));
        assert!(text.contains("check=\"Mobile usability\"} 1"));
        assert!(text.contains("check=\"Autofill hints\"} 0"));
    }

    #[test]
    fn label_values_are_escaped() {
        let text = render(&[run()]);
        // The form name contains a quote and a newline; both must be escaped
        // so the exposition stays parseable.
        assert!(text.contains("form=\"Permit \\\"A\\\"\\nline\""), "{text}");
        assert!(!text.contains("Permit \"A\"\nline"), "{text}");
    }

    #[test]
    fn empty_input_still_emits_the_form_count() {
        let text = render(&[]);
        assert!(text.contains("formwatch_forms 0"));
    }
}
