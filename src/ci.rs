//! Generates starter CI so a `monitor` run happens on every push and on
//! a schedule — the cheapest way to turn a one-off form check into
//! continuous monitoring. Pure string rendering; writing the file is the
//! caller's job so this stays testable without touching the repo.

/// Returns a GitHub Actions workflow that installs formwatch, validates
/// the registry, and runs `monitor` (plus `vuln`-adjacent `test` on the
/// first form is left to the user). `forms` is the registry path as it
/// should appear in the workflow (e.g. `forms.yml`).
pub fn github_workflow(forms: &str, schedule_cron: &str) -> String {
    format!(
        r#"name: formwatch

on:
  push:
  pull_request:
  schedule:
    - cron: '{schedule_cron}'

jobs:
  forms:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - name: Install formwatch
        run: cargo install --git https://github.com/voidstackloop/formwatch --locked
      - name: Validate registry
        run: formwatch validate {forms}
      - name: Monitor forms
        run: formwatch monitor {forms} --junit --out junit.xml
      - name: Upload JUnit
        if: always()
        uses: actions/upload-artifact@v4
        with:
          name: formwatch-junit
          path: junit.xml
"#
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workflow_contains_schedule_and_forms_path() {
        let wf = github_workflow("community-forms/site.yml", "0 8 * * 1");
        assert!(wf.contains("0 8 * * 1"));
        assert!(wf.contains("community-forms/site.yml"));
        assert!(wf.contains("formwatch validate"));
        assert!(wf.contains("actions/upload-artifact"));
    }
}
