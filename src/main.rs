use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use formwatch::{browser, checks, history, report, runner};
use futures::stream::{self, StreamExt};
use owo_colors::OwoColorize;
use std::path::{Path, PathBuf};

/// How many forms `monitor` checks at once. Each form gets its own
/// browser Page (an independent execution context — `TARGET_FORM_JS`'s
/// per-page `window` state can't leak between forms the way it could
/// have leaked between *checks on the same form*, which is why that fix
/// was necessary but this one is safe), so running several concurrently
/// is a straightforward wall-clock win. Bounded rather than unlimited to
/// stay a good citizen — this checks real third-party government sites,
/// not infrastructure formwatch controls.
const MAX_CONCURRENT_FORMS: usize = 4;

#[derive(Parser)]
#[command(
    name = "formwatch",
    version,
    about = "Tests public-service forms for broken flows, accessibility, mobile usability, and lost input."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Where run history is stored/read from (default: .formwatch/history).
    #[arg(long, global = true, default_value = ".formwatch/history")]
    history_dir: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Scaffold a starter forms.yml (and an example custom check) to get going.
    Init {
        /// Where to write the starter config.
        #[arg(default_value = "forms.yml")]
        path: PathBuf,
    },
    /// Run every check against a single form URL.
    Test {
        url: String,
        /// Label to record this form under (defaults to the URL).
        #[arg(long)]
        name: Option<String>,
        /// Actually click submit (a real POST) instead of stopping at native validation.
        #[arg(long)]
        submit: bool,
        /// Seconds to sit idle before checking for lost input / session-timeout wording.
        #[arg(long, default_value_t = 5)]
        wait: u64,
        /// Show the browser window instead of running headless.
        #[arg(long)]
        headful: bool,
        /// Also run every *.js file in this directory as a custom check.
        #[arg(long)]
        checks_dir: Option<PathBuf>,
        /// Print the run as JSON instead of colored text (for scripts/CI).
        #[arg(long)]
        json: bool,
    },
    /// Run `test` against every form listed in one or more YAML configs
    /// (a shell glob like `community-forms/**/*.yml` works — each match
    /// becomes a separate argument the shell hands to formwatch).
    Monitor {
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        #[arg(long)]
        submit: bool,
        #[arg(long, default_value_t = 5)]
        wait: u64,
        #[arg(long)]
        headful: bool,
        #[arg(long)]
        checks_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Print (or render to HTML/JSON) the most recent run of every form formwatch knows about.
    Report {
        #[arg(long, conflicts_with = "json")]
        html: bool,
        #[arg(long)]
        json: bool,
        /// Where to write --html or --json output (default: print JSON to stdout, or
        /// formwatch-report.html for --html).
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[derive(serde::Deserialize)]
struct FormsConfig {
    forms: Vec<FormEntry>,
}

#[derive(serde::Deserialize)]
struct FormEntry {
    name: String,
    url: String,
}

const STARTER_FORMS_YML: &str = r#"# formwatch monitors public-service forms for broken flows, accessibility,
# mobile usability, lost input, and unclear validation/document requirements.
#
# Add one entry per form you want to track, then run:
#   formwatch monitor forms.yml
#
# Every run's results are saved under .formwatch/history/ so `monitor` and
# `formwatch report` can show you what changed since last time.

forms:
  - name: Example — replace with a real form
    url: https://example.gov/apply
"#;

const EXAMPLE_CUSTOM_CHECK_JS: &str = r#"// Custom check example. Run it with:
//   formwatch test <url> --checks-dir checks
//
// Every *.js file in --checks-dir is evaluated against the page — the
// same mechanism formwatch's built-in accessibility check uses to run
// axe-core. A script must evaluate (directly, or via a Promise, so
// `async () => {...}` works too) to an object shaped like this one. The
// check's name comes from the filename, not anything returned here.

(() => {
    const hasLang = document.documentElement.hasAttribute('lang');
    return {
        status: hasLang ? 'Pass' : 'Warn',
        detail: hasLang
            ? `Page declares lang="${document.documentElement.getAttribute('lang')}".`
            : 'No lang attribute on <html> — screen readers may mispronounce the page.',
    };
})()
"#;

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let history_dir = cli.history_dir;
    let mut any_fail = false;

    match cli.command {
        Command::Init { path } => init(&path)?,
        Command::Test {
            url,
            name,
            submit,
            wait,
            headful,
            checks_dir,
            json,
        } => {
            let (browser, _handle) = browser::launch(headful).await?;
            let run = runner::run_one(
                &browser,
                &history_dir,
                name.unwrap_or_else(|| url.clone()),
                url,
                submit,
                wait,
                checks_dir.as_deref(),
            )
            .await?;
            any_fail |= print_run(&history_dir, &run, json)?;
        }
        Command::Monitor {
            configs,
            submit,
            wait,
            headful,
            checks_dir,
            json,
        } => {
            let mut entries = vec![];
            for config in &configs {
                match load_forms_config(config) {
                    Ok(cfg) => entries.extend(cfg.forms),
                    Err(e) => {
                        eprintln!("{}: {e:#}", format!("skipping {}", config.display()).red());
                        // A config that couldn't even be read/parsed is a
                        // real failure, not a soft warning — exit 1 so a
                        // typo'd filename or broken YAML doesn't produce a
                        // silently-green CI job, contradicting the exit-code
                        // contract this tool documents.
                        any_fail = true;
                    }
                }
            }

            let (browser, _handle) = browser::launch(headful).await?;
            let checks_dir_ref = checks_dir.as_deref();
            let mut indexed: Vec<(usize, history::RunResult)> =
                stream::iter(entries.into_iter().enumerate())
                    .map(|(i, entry)| {
                        let browser = &browser;
                        let history_dir = &history_dir;
                        async move {
                            match runner::run_one(
                                browser,
                                history_dir,
                                entry.name.clone(),
                                entry.url,
                                submit,
                                wait,
                                checks_dir_ref,
                            )
                            .await
                            {
                                Ok(run) => Some((i, run)),
                                Err(e) => {
                                    eprintln!("{}: {e:#}", format!("{} failed", entry.name).red());
                                    None
                                }
                            }
                        }
                    })
                    .buffer_unordered(MAX_CONCURRENT_FORMS)
                    .filter_map(|x| async move { x })
                    .collect()
                    .await;
            // buffer_unordered completes in whichever order each form's
            // checks finish, not config order — restore config order so
            // "monitor a, b, c" doesn't print in a different order every
            // run depending on network timing luck.
            indexed.sort_by_key(|(i, _)| *i);
            let runs: Vec<history::RunResult> = indexed.into_iter().map(|(_, r)| r).collect();
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
                any_fail |= runs.iter().any(run_has_failure);
            } else {
                for run in &runs {
                    any_fail |= print_run(&history_dir, run, false)?;
                }
            }
        }
        Command::Report { html, json, out } => {
            if html {
                let path = out.unwrap_or_else(|| PathBuf::from("formwatch-report.html"));
                std::fs::write(&path, report::render_html(&history_dir)?)?;
                println!("Wrote {}", path.display());
            } else if json {
                let body = serde_json::to_string_pretty(&history::all_known_forms(&history_dir)?)?;
                match out {
                    Some(path) => {
                        std::fs::write(&path, body)?;
                        println!("Wrote {}", path.display());
                    }
                    None => println!("{body}"),
                }
            } else {
                print_report(&history_dir)?;
            }
        }
    }

    if any_fail {
        std::process::exit(1);
    }
    Ok(())
}

fn load_forms_config(path: &Path) -> Result<FormsConfig> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    serde_yaml::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

fn init(path: &Path) -> Result<()> {
    if path.exists() {
        bail!(
            "{} already exists — remove it first if you want a fresh starter config.",
            path.display()
        );
    }
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, STARTER_FORMS_YML)
        .with_context(|| format!("writing {}", path.display()))?;
    println!("Wrote {}", path.display());

    let checks_dir = Path::new("checks");
    if !checks_dir.exists() {
        std::fs::create_dir(checks_dir)?;
        std::fs::write(checks_dir.join("example.js"), EXAMPLE_CUSTOM_CHECK_JS)?;
        println!("Wrote checks/example.js (an example custom check)");
    }

    println!(
        "\nNext steps:\n  1. Edit {} with real form URLs.\n  2. formwatch monitor {}\n  3. formwatch monitor {} --checks-dir checks   (to also run the example custom check)",
        path.display(),
        path.display(),
        path.display()
    );
    Ok(())
}

fn run_has_failure(run: &history::RunResult) -> bool {
    run.checks.iter().any(|c| c.status == checks::Status::Fail)
}

/// Prints one run (JSON or colored text, plus the diff against its
/// previous run) and reports whether it contains any Fail — the caller
/// aggregates that into the process exit code.
fn print_run(history_dir: &Path, run: &history::RunResult, json: bool) -> Result<bool> {
    if json {
        println!("{}", serde_json::to_string_pretty(run)?);
        return Ok(run_has_failure(run));
    }

    println!("\n{}", format!("== {} ({}) ==", run.name, run.url).bold());
    for check in &run.checks {
        print_check(check);
    }

    let prior = history::load_runs(history_dir, &run.url)?;
    if let Some(prev) = prior.iter().rev().find(|r| r.timestamp < run.timestamp) {
        print_changes(&history::diff(prev, run));
    }
    Ok(run_has_failure(run))
}

fn print_check(check: &checks::CheckResult) {
    let label = match check.status {
        checks::Status::Pass => check.status.label().green().to_string(),
        checks::Status::Warn => check.status.label().yellow().to_string(),
        checks::Status::Fail => check.status.label().red().to_string(),
    };
    println!("  [{label}] {}: {}", check.name, check.detail);
}

fn print_changes(changes: &[history::CheckChange]) {
    if changes.is_empty() {
        return;
    }
    println!("  {}", "Changed since previous run:".bold());
    for c in changes {
        println!("    {}: {} -> {}", c.name, c.from, c.to);
    }
}

/// Plain-text `formwatch report`. Reuses report::build rather than
/// re-fetching history and re-deriving the same data by hand — it also
/// means this shows "changed since previous run" the same way --html
/// does, instead of silently omitting it like the hand-rolled version
/// used to.
fn print_report(history_dir: &Path) -> Result<()> {
    let report = report::build(history_dir)?;
    if report.forms.is_empty() {
        println!(
            "No runs recorded yet. Run `formwatch test <url>` or `formwatch monitor forms.yml` first."
        );
        return Ok(());
    }
    for form in &report.forms {
        println!(
            "\n{}",
            format!("== {} ({}) ==", form.run.name, form.run.url).bold()
        );
        for check in &form.run.checks {
            print_check(check);
        }
        print_changes(&form.changes);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use checks::{CheckResult, Status};

    fn check(status: Status) -> CheckResult {
        CheckResult {
            name: "Accessibility".to_string(),
            status,
            detail: String::new(),
        }
    }

    fn run(checks: Vec<CheckResult>) -> history::RunResult {
        history::RunResult {
            name: "x".into(),
            url: "https://example.test/form".into(),
            timestamp: 0,
            checks,
        }
    }

    #[test]
    fn run_has_failure_is_true_only_when_a_check_actually_failed() {
        // The exit code is the whole point of running this in CI — a Warn
        // (or an all-Pass run) must exit 0, only a real Fail should turn
        // CI red.
        assert!(!run_has_failure(&run(vec![
            check(Status::Pass),
            check(Status::Warn)
        ])));
        assert!(run_has_failure(&run(vec![
            check(Status::Pass),
            check(Status::Fail)
        ])));
    }

    #[test]
    fn load_forms_config_parses_name_and_url_per_entry() {
        let dir = std::env::temp_dir().join("formwatch-test-load-forms-config");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("forms.yml");
        std::fs::write(
            &path,
            "forms:\n  - name: City Permit\n    url: https://city.gov/permit\n",
        )
        .expect("write config");

        let cfg = load_forms_config(&path).expect("parse config");
        assert_eq!(cfg.forms.len(), 1);
        assert_eq!(cfg.forms[0].name, "City Permit");
        assert_eq!(cfg.forms[0].url, "https://city.gov/permit");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn load_forms_config_fails_loudly_on_malformed_yaml_instead_of_silently_skipping() {
        // main() turns this Err into any_fail = true specifically so a
        // typo'd config doesn't produce a silently-green CI job — that
        // contract only holds if parsing a broken file actually errors.
        let dir = std::env::temp_dir().join("formwatch-test-load-forms-config-bad");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create dir");
        let path = dir.join("forms.yml");
        std::fs::write(&path, "not: [valid, forms.yml\n").expect("write config");

        assert!(load_forms_config(&path).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
