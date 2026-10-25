use anyhow::{Context, Result, bail};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use formwatch::baseline::{self, Baseline};
use formwatch::config::{Config, FailOn, LlmConfig, NotifyOn};
use formwatch::limiter::host_of;
use formwatch::llm::provider::Provider;
use formwatch::llm::{LlmClient, LlmOptions};
use formwatch::logging::{self, LogFormat};
use formwatch::options::RunOptions;
use formwatch::shard::Shard;
use formwatch::{
    audit, browser, browser::BrowserOptions, browser::OpenOptions, checks, ci, demo, doctor,
    export, history, insights, issues, legal, limiter::HostPacer, metrics, notify, report, runner,
    serve, sitemap, validate, vuln,
};
use futures::stream::{self, StreamExt};
use owo_colors::OwoColorize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How many forms `monitor` checks at once by default. Each form gets its
/// own browser Page (an independent execution context — `TARGET_FORM_JS`'s
/// per-page `window` state can't leak between forms the way it could
/// have leaked between *checks on the same form*, which is why that fix
/// was necessary but this one is safe), so running several concurrently
/// is a straightforward wall-clock win. Bounded rather than unlimited to
/// stay a good citizen — this checks real third-party government sites,
/// not infrastructure formwatch controls. Overridable with
/// `--max-concurrent` / `FORMWATCH_MAX_CONCURRENT` / config.
const MAX_CONCURRENT_FORMS: usize = 4;

#[derive(Parser)]
#[command(
    name = "formwatch",
    version,
    about = "Tests public-service forms for broken flows, accessibility, mobile usability, lost input, and common passive vulnerabilities.",
    after_help = "Use formwatch only on forms you own or are authorized to test. Run `formwatch legal` for the full authorized-use notice."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Config file to load (default: ./formwatch.yml, then the user config dir).
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Where run history is stored/read from (default: .formwatch/history).
    #[arg(long, global = true)]
    history_dir: Option<PathBuf>,

    /// Increase diagnostic verbosity: -v for info, -vv for debug. Repeatable.
    #[arg(short, long, global = true, action = clap::ArgAction::Count)]
    verbose: u8,

    /// Only log errors to stderr.
    #[arg(short, long, global = true)]
    quiet: bool,

    /// Format for diagnostic logs on stderr.
    #[arg(long, global = true, value_enum, default_value_t = LogFormat::Text)]
    log_format: LogFormat,

    /// Acknowledge the authorized-use notice (required for --submit).
    #[arg(long, global = true)]
    accept_terms: bool,

    /// Do not capture screenshots on non-Pass checks (privacy-sensitive use).
    #[arg(long, global = true)]
    no_screenshots: bool,

    /// HTTP(S) proxy for Chrome.
    #[arg(long, global = true)]
    proxy: Option<String>,

    /// Ignore TLS certificate errors (trusted test hosts / internal proxies only).
    #[arg(long, global = true)]
    insecure: bool,

    /// Launch Chrome with --no-sandbox (required in some container/CI environments).
    #[arg(long, global = true)]
    no_sandbox: bool,

    /// Append one JSON audit line per run to this file.
    #[arg(long, global = true)]
    audit_log: Option<PathBuf>,

    /// Baseline of accepted findings (JSON): only new or worsened findings fail.
    #[arg(long, global = true)]
    baseline: Option<PathBuf>,

    /// Minimum milliseconds between starting requests to the same host (0 = off).
    #[arg(long, global = true)]
    per_host_delay_ms: Option<u64>,

    /// Retry a form whose page never loaded this many extra times (default 0).
    /// Only load failures retry — a form that loads but fails checks is a
    /// real result, not a transient worth re-probing a third-party site for.
    #[arg(long, global = true)]
    retries: Option<u32>,

    /// Fail forms whose wall-clock run exceeds this many milliseconds
    /// (emits a synthetic `Load budget` check, so --fail-on applies).
    #[arg(long, global = true)]
    max_load_ms: Option<u64>,

    /// Maximum forms to check concurrently in `monitor` (default 4).
    #[arg(long, global = true)]
    max_concurrent: Option<usize>,

    /// Check only this shard of the configured forms, e.g. `2/5`.
    #[arg(long, global = true)]
    shard: Option<Shard>,

    /// Base per-check timeout in seconds (default 20).
    #[arg(long, global = true)]
    check_timeout_secs: Option<u64>,

    /// Status that makes the process exit non-zero: fail (default) or warn.
    #[arg(long, global = true, value_enum)]
    fail_on: Option<FailOn>,

    /// Skip the passive vulnerability checks (insecure transport, form
    /// data protection, security headers, information disclosure).
    #[arg(long, global = true)]
    skip_vuln: bool,

    #[command(flatten)]
    llm: LlmArgs,
}

/// LLM semantic-check options (`--llm-*`). Grouped so the main CLI struct
/// stays readable.
#[derive(clap::Args, Default, Clone)]
struct LlmArgs {
    /// Enable LLM semantic review of error/instruction wording. Sends
    /// (redacted) page text to the configured provider.
    #[arg(long, global = true)]
    llm: bool,
    /// LLM provider.
    #[arg(long, global = true, value_enum)]
    llm_provider: Option<Provider>,
    /// Model name (default depends on the provider).
    #[arg(long, global = true)]
    llm_model: Option<String>,
    /// API key (prefer the provider's env var, e.g. OPENAI_API_KEY).
    #[arg(long, global = true)]
    llm_api_key: Option<String>,
    /// Provider API root, for OpenAI-compatible endpoints.
    #[arg(long, global = true)]
    llm_base_url: Option<String>,
    /// Retries for transient provider failures (default 2).
    #[arg(long, global = true)]
    llm_max_retries: Option<u32>,
    /// Minimum passing score, 1-5 (default 3).
    #[arg(long, global = true)]
    llm_threshold: Option<u8>,
    /// Treat a below-threshold score as FAIL instead of WARN.
    #[arg(long, global = true)]
    llm_fail: bool,
    /// Disable PII redaction before sending page text.
    #[arg(long, global = true)]
    llm_no_redact: bool,
    /// Disable the on-disk LLM verdict cache.
    #[arg(long, global = true)]
    llm_no_cache: bool,
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
        #[arg(long)]
        wait: Option<u64>,
        /// Show the browser window instead of running headless.
        #[arg(long)]
        headful: bool,
        /// Also run every *.js file in this directory as a custom check.
        #[arg(long)]
        checks_dir: Option<PathBuf>,
        /// Print the run as JSON instead of colored text (for scripts/CI).
        #[arg(long)]
        json: bool,
        /// Render the run as JUnit XML (for CI test dashboards).
        #[arg(long, conflicts_with_all = ["json", "sarif"])]
        junit: bool,
        /// Render the run as SARIF 2.1.0 (for code-scanning UIs).
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
        /// Render the run as CSV (one row per check, for spreadsheets).
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif"])]
        csv: bool,
        /// Where to write --junit/--sarif/--csv output (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// POST a notification to this webhook (Slack or generic JSON).
        #[arg(long)]
        webhook_url: Option<String>,
        /// Auto-create/close a GitHub Issue in this "owner/repo" when a
        /// check regresses to FAIL / recovers to PASS (falls back to
        /// github_issues.repo in config).
        #[arg(long)]
        github_issues_repo: Option<String>,
    },
    /// Run `test` against every form listed in one or more YAML configs
    /// (a shell glob like `community-forms/**/*.yml` works — each match
    /// becomes a separate argument the shell hands to formwatch).
    Monitor {
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        #[arg(long)]
        submit: bool,
        #[arg(long)]
        wait: Option<u64>,
        #[arg(long)]
        headful: bool,
        #[arg(long)]
        checks_dir: Option<PathBuf>,
        #[arg(long)]
        json: bool,
        /// Render the runs as JUnit XML (for CI test dashboards).
        #[arg(long, conflicts_with_all = ["json", "sarif"])]
        junit: bool,
        /// Render the runs as SARIF 2.1.0 (for code-scanning UIs).
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
        /// Render the runs as CSV (one row per check, for spreadsheets).
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif"])]
        csv: bool,
        /// Where to write --junit/--sarif/--csv output (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Milliseconds to wait before starting each form's checks (0 = no
        /// delay). A real-world courtesy knob for a large forms.yml against
        /// third-party sites you don't control — spreads the load out
        /// instead of firing up to MAX_CONCURRENT_FORMS requests at once.
        #[arg(long)]
        delay_ms: Option<u64>,
        /// POST a notification to this webhook (Slack or generic JSON).
        #[arg(long)]
        webhook_url: Option<String>,
        /// Auto-create/close a GitHub Issue in this "owner/repo" when a
        /// check regresses to FAIL / recovers to PASS (falls back to
        /// github_issues.repo in config).
        #[arg(long)]
        github_issues_repo: Option<String>,
    },
    /// Keep re-running `monitor` against every form in one or more YAML
    /// configs, watching for regressions until stopped. Each cycle saves
    /// history (so `formwatch serve` stays live), notifies on new
    /// regressions, and prints a one-line summary — full detail only for
    /// runs that breach `--fail-on`.
    Watch {
        #[arg(required = true)]
        configs: Vec<PathBuf>,
        #[arg(long)]
        submit: bool,
        #[arg(long)]
        wait: Option<u64>,
        #[arg(long)]
        headful: bool,
        #[arg(long)]
        checks_dir: Option<PathBuf>,
        /// Seconds between full passes over all forms (default 300).
        #[arg(long)]
        interval_secs: Option<u64>,
        /// Stop after this many cycles (default: run until Ctrl+C).
        /// Useful for CI smoke runs.
        #[arg(long)]
        max_cycles: Option<u64>,
        /// Milliseconds to wait before starting each form's checks (0 = no
        /// delay).
        #[arg(long)]
        delay_ms: Option<u64>,
        /// Emit one compact JSON object per cycle to stdout (JSONL — one
        /// line per cycle) instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Render each cycle as JUnit XML (for CI test dashboards).
        #[arg(long, conflicts_with_all = ["json", "sarif"])]
        junit: bool,
        /// Render each cycle as SARIF 2.1.0 (for code-scanning UIs).
        #[arg(long, conflicts_with = "json")]
        sarif: bool,
        /// Render each cycle as CSV (one row per check, for spreadsheets).
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif"])]
        csv: bool,
        /// Where to write --junit/--sarif/--csv output (default: stdout).
        /// The file is overwritten each cycle, so it always holds the
        /// latest pass.
        #[arg(long)]
        out: Option<PathBuf>,
        /// POST a notification to this webhook (Slack or generic JSON).
        #[arg(long)]
        webhook_url: Option<String>,
        /// Auto-create/close a GitHub Issue in this "owner/repo" when a
        /// check regresses to FAIL / recovers to PASS (falls back to
        /// github_issues.repo in config).
        #[arg(long)]
        github_issues_repo: Option<String>,
    },
    /// Print (or render to HTML/JSON/JUnit/SARIF/CSV) the most recent run of every form formwatch knows about.
    Report {
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif", "prometheus", "csv", "markdown"])]
        html: bool,
        #[arg(long)]
        json: bool,
        /// Render as JUnit XML (for CI test dashboards).
        #[arg(long, conflicts_with_all = ["json", "sarif", "prometheus", "csv"])]
        junit: bool,
        /// Render as SARIF 2.1.0 (for code-scanning UIs).
        #[arg(long, conflicts_with_all = ["json", "prometheus", "csv"])]
        sarif: bool,
        /// Render as Prometheus text-exposition metrics.
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif", "csv"])]
        prometheus: bool,
        /// Render as CSV (one row per check, for spreadsheets).
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif", "prometheus", "markdown"])]
        csv: bool,
        /// Render as Markdown (one section per form, for issues/wikis).
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif", "prometheus", "csv"])]
        markdown: bool,
        /// Where to write output (default: stdout for JSON, else a named file).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Scan one form URL for common passive vulnerabilities only (no
    /// attack payloads are ever sent — DOM and response headers only).
    Vuln {
        url: String,
        /// Label to show for this form (defaults to the URL).
        #[arg(long)]
        name: Option<String>,
        /// Show the browser window instead of running headless.
        #[arg(long)]
        headful: bool,
        /// Print the findings as JSON instead of human-readable text.
        #[arg(long)]
        json: bool,
        /// Only show findings at or above this severity
        /// (low, medium, high, critical).
        #[arg(long)]
        min_severity: Option<String>,
    },
    /// Inspect the tamper-evident audit log: verify its hash chain or show recent entries.
    Audit {
        /// Verify the hash chain and report breaks (exit 1 when broken).
        #[arg(long)]
        verify: bool,
        /// Show the last N entries (default 10). Ignored with --verify.
        #[arg(long)]
        show: Option<usize>,
        /// Print entries (or the verify report) as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Print the full authorized-use / legal notice and exit.
    Legal,
    /// Check that the local environment (Chrome, writable dirs, config) is ready.
    Doctor {
        /// Print the results as JSON.
        #[arg(long)]
        json: bool,
        /// Also probe the configured LLM provider (connectivity + credentials).
        #[arg(long)]
        probe_llm: bool,
    },
    /// Send a test notification to a webhook to verify it is reachable.
    NotifyTest {
        /// Webhook URL (falls back to the config's notify.webhook_url).
        #[arg(long)]
        webhook_url: Option<String>,
        /// Print the outcome as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Generate or print a baseline of currently accepted findings.
    Baseline {
        /// Write a baseline generated from the current history.
        #[arg(long)]
        write: bool,
        /// Output path (default for --write: .formwatch/baseline.json).
        #[arg(long)]
        out: Option<PathBuf>,
        /// Print the baseline as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Generate a shell completion script.
    Completions {
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Serve read-only HTTP endpoints (/healthz, /readyz, /metrics, /api/forms, /api/summary).
    Serve {
        /// Address to bind (default: 127.0.0.1:8080).
        #[arg(long)]
        addr: Option<SocketAddr>,
        /// Require `Authorization: Bearer <token>` on every data endpoint
        /// (/healthz and /readyz stay open for probes).
        #[arg(long)]
        api_token: Option<String>,
    },
    /// Serve the bundled demo site so you can see what formwatch catches.
    Demo {
        /// Address to bind (default: 127.0.0.1:8099).
        #[arg(long)]
        addr: Option<SocketAddr>,
    },
    /// Lint one or more forms.yml registries without launching a browser.
    Validate {
        /// Registry files to lint (default: forms.yml).
        #[arg(default_value = "forms.yml")]
        configs: Vec<PathBuf>,
        /// Also GET each syntactically-valid URL and report reachability.
        #[arg(long)]
        probe: bool,
        /// Print issues as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Pass-rate, current status, and streaks per form over history.
    Trends {
        /// Only consider the last N runs per form (default: all history).
        #[arg(long)]
        last_n: Option<usize>,
        /// Exit 1 when any form's pass rate is below this (0.0-1.0).
        #[arg(long)]
        min_pass_rate: Option<f64>,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Checks that keep flip-flopping instead of settling (2+ transitions).
    Flakes {
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show what changed between a form's last two runs.
    Diff {
        /// Form URL or display name.
        target: String,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Show a form's recorded run history (newest last).
    History {
        /// Form URL or display name.
        target: String,
        /// Only show the last N runs.
        #[arg(long)]
        last_n: Option<usize>,
        /// Print runs as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Dump full history (every run of every form) for warehouses/dashboards.
    Export {
        /// Format: `jsonl` (one run per line), `json` (pretty array), or
        /// `csv` (one row per check).
        #[arg(long, default_value = "jsonl")]
        format: String,
        /// Where to write (default: stdout).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Discover candidate form pages from a sitemap (or a site root).
    Sitemap {
        /// Sitemap URL, or a site root (robots.txt + /sitemap.xml tried).
        url: String,
        /// Cap on candidate page URLs (default 200).
        #[arg(long)]
        max_urls: Option<usize>,
        /// Fetch each page and keep only ones containing a <form>.
        #[arg(long)]
        check_forms: bool,
        /// Print as JSON.
        #[arg(long)]
        json: bool,
    },
    /// Write a starter GitHub Actions workflow that validates + monitors.
    Ci {
        /// Registry path as it should appear in the workflow.
        #[arg(long, default_value = "forms.yml")]
        forms: String,
        /// Cron schedule for the scheduled run.
        #[arg(long, default_value = "0 8 * * 1")]
        schedule: String,
        /// Where to write (default: .github/workflows/formwatch.yml).
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Delete old history runs, keeping the most recent ones.
    Prune {
        /// Keep only the N most recent runs of each form.
        #[arg(long, conflicts_with = "keep_days")]
        keep_last: Option<usize>,
        /// Keep only runs newer than N days.
        #[arg(long, conflicts_with = "keep_last")]
        keep_days: Option<u64>,
        /// Report what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(serde::Deserialize)]
struct FormsConfig {
    forms: Vec<FormEntry>,
}

#[derive(serde::Deserialize, Clone)]
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
#
# IMPORTANT: only list forms you own or have written permission to test.
# See `formwatch legal` (or docs/legal.md) before pointing this at a
# third-party site.

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

/// Everything resolved from CLI + config + environment for one invocation.
struct Settings {
    history_dir: PathBuf,
    retries: u32,
    checks_dir: Option<PathBuf>,
    run: RunOptions,
    browser: BrowserOptions,
    open: OpenOptions,
    delay_ms: u64,
    per_host_delay_ms: u64,
    max_concurrent: usize,
    shard: Option<Shard>,
    audit_log: Option<PathBuf>,
    baseline: Option<Baseline>,
    webhook_url: Option<String>,
    webhook_on: NotifyOn,
    github_issues: Option<GithubIssuesSettings>,
    accepted: bool,
    fail_on: FailOn,
}

/// Resolved settings for GitHub Issues auto-tracking — only constructed
/// when a repo is actually configured, so callers can check
/// `settings.github_issues.is_some()` instead of re-checking a bare repo
/// string against emptiness everywhere.
struct GithubIssuesSettings {
    repo: String,
    token: String,
    labels: Vec<String>,
}

/// Global options shared by every subcommand, lifted out of [`Cli`]
/// before the command is destructured.
#[derive(Default)]
struct Globals {
    history_dir: Option<PathBuf>,
    retries: Option<u32>,
    max_load_ms: Option<u64>,
    no_screenshots: bool,
    proxy: Option<String>,
    insecure: bool,
    no_sandbox: bool,
    audit_log: Option<PathBuf>,
    baseline: Option<PathBuf>,
    per_host_delay_ms: Option<u64>,
    max_concurrent: Option<usize>,
    shard: Option<Shard>,
    check_timeout_secs: Option<u64>,
    fail_on: Option<FailOn>,
    skip_vuln: bool,
    llm: LlmArgs,
}

#[tokio::main]
async fn main() -> Result<()> {
    let Cli {
        command,
        config,
        history_dir,
        verbose,
        quiet,
        log_format,
        accept_terms,
        no_screenshots,
        proxy,
        insecure,
        no_sandbox,
        audit_log,
        baseline,
        per_host_delay_ms,
        max_concurrent,
        shard,
        check_timeout_secs,
        fail_on,
        skip_vuln,
        llm,
        retries,
        max_load_ms,
    } = Cli::parse();
    logging::init(verbose, quiet, log_format);

    let config = Config::load(config.as_deref())?;
    let accepted = legal::is_accepted(accept_terms, config.accept_terms);
    let globals = Globals {
        history_dir,
        retries,
        max_load_ms,
        no_screenshots,
        proxy,
        insecure,
        no_sandbox,
        audit_log,
        baseline,
        per_host_delay_ms,
        max_concurrent,
        shard,
        check_timeout_secs,
        fail_on,
        skip_vuln,
        llm,
    };

    let mut any_fail = false;

    match command {
        Command::Init { path } => init(&path)?,
        Command::Legal => println!("{}", legal::notice()),
        Command::Doctor { json, probe_llm } => {
            doctor_command(&globals, &config, accepted, json, probe_llm).await?
        }
        Command::NotifyTest { webhook_url, json } => {
            notify_test(&config, webhook_url, json).await?
        }
        Command::Baseline { write, out, json } => {
            baseline_command(&globals, &config, write, out, json)?
        }
        Command::Serve { addr, api_token } => {
            let addr = match addr {
                Some(addr) => addr,
                None => match &config.serve_addr {
                    Some(raw) => raw
                        .parse()
                        .with_context(|| format!("invalid serve_addr {raw:?}"))?,
                    None => "127.0.0.1:8080".parse().expect("valid default address"),
                },
            };
            let history_dir = settings_history_dir(&globals, &config);
            let token = api_token
                .or_else(|| config.serve_api_token.clone())
                .or_else(|| std::env::var("FORMWATCH_SERVE_API_TOKEN").ok())
                .filter(|t| !t.trim().is_empty());
            serve::run_with(history_dir, addr, serve::ServeOptions { api_token: token }).await?;
        }
        Command::Demo { addr } => {
            let addr = addr.unwrap_or_else(|| "127.0.0.1:8099".parse().expect("valid default"));
            demo::run(addr).await?;
        }
        Command::Prune {
            keep_last,
            keep_days,
            dry_run,
        } => {
            let history_dir = settings_history_dir(&globals, &config);
            let retain = match (
                keep_last.or(config.keep_last),
                keep_days.or(config.keep_days),
            ) {
                (Some(n), _) => history::Retain::Last(n),
                (None, Some(d)) => history::Retain::Days(d),
                (None, None) => bail!(
                    "specify --keep-last N or --keep-days D (or set keep_last / keep_days in config)"
                ),
            };
            let report = history::prune(
                &history_dir,
                retain,
                dry_run,
                chrono::Utc::now().timestamp(),
            )?;
            let verb = if dry_run { "would remove" } else { "removed" };
            println!(
                "{verb} {} run(s) across {} form(s); kept {}.",
                report.removed, report.forms, report.kept
            );
            if dry_run {
                println!("(dry run — nothing was deleted)");
            }
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            clap_complete::generate(shell, &mut cmd, "formwatch", &mut std::io::stdout());
        }
        Command::Test {
            url,
            name,
            submit,
            wait,
            headful,
            checks_dir,
            json,
            junit,
            sarif,
            csv,
            out,
            webhook_url,
            github_issues_repo,
        } => {
            let settings = resolve(
                &globals,
                &config,
                submit || config.submit.unwrap_or(false),
                wait.or(config.wait).unwrap_or(5),
                headful || config.headful.unwrap_or(false),
                checks_dir.or_else(|| config.checks_dir.clone()),
                0,
                webhook_url.or_else(|| clone_config_webhook(&config)),
                github_issues_repo.or_else(|| clone_config_github_issues_repo(&config)),
                accepted,
            );
            guard_submit(settings.run.allow_submit, settings.accepted)?;

            let (browser, _handle) = browser::launch_with(&settings.browser).await?;
            let pacer = HostPacer::new(Duration::from_millis(settings.per_host_delay_ms));
            let llm_client = build_llm_client(&settings.run.llm);
            if let Some(host) = host_of(&url) {
                pacer.wait_turn(&host).await;
            }
            let started = std::time::Instant::now();
            let label = name.unwrap_or_else(|| url.clone());
            let mut run = runner::run_one_with(
                &browser,
                &settings.history_dir,
                label.clone(),
                url.clone(),
                &settings.run,
                &settings.open,
                settings.checks_dir.as_deref(),
                llm_client.as_ref(),
            )
            .await?;
            let mut attempt = 0u32;
            while is_load_failure(&run) && attempt < settings.retries {
                attempt += 1;
                tracing::warn!(attempt, "page load failed; retrying");
                tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(4)))).await;
                run = runner::run_one_with(
                    &browser,
                    &settings.history_dir,
                    label.clone(),
                    url.clone(),
                    &settings.run,
                    &settings.open,
                    settings.checks_dir.as_deref(),
                    llm_client.as_ref(),
                )
                .await?;
            }
            if let Some(path) = &settings.audit_log {
                let ctx = audit::AuditContext::capture(
                    "test",
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    settings.run.allow_submit,
                    match settings.fail_on {
                        FailOn::Fail => "fail",
                        FailOn::Warn => "warn",
                    },
                );
                audit::append_with_context(path, &run, &ctx)?;
            }
            let prior_by_url = load_prior_by_url(&settings.history_dir, std::slice::from_ref(&run));
            if junit || sarif || csv {
                emit_structured(
                    std::slice::from_ref(&run),
                    junit,
                    sarif,
                    csv,
                    out,
                    settings.baseline.as_ref(),
                )?;
                any_fail |= run_breaches(&run, settings.fail_on, settings.baseline.as_ref());
            } else {
                any_fail |= print_run(
                    prior_by_url.get(&run.url).map(Vec::as_slice).unwrap_or(&[]),
                    &run,
                    json,
                    settings.fail_on,
                    settings.baseline.as_ref(),
                )?;
            }
            notify_runs(&settings, std::slice::from_ref(&run), &prior_by_url).await;
            github_issues_for_runs(&settings, std::slice::from_ref(&run), &prior_by_url).await;
        }
        Command::Vuln {
            url,
            name,
            headful,
            json,
            min_severity,
        } => {
            let settings = resolve(
                &globals,
                &config,
                false,
                5,
                headful || config.headful.unwrap_or(false),
                None,
                0,
                None,
                None,
                accepted,
            );
            let label = name.unwrap_or_else(|| url.clone());
            let min = match min_severity.as_deref() {
                None => None,
                Some(s) => Some(s.parse::<vuln::Severity>().map_err(anyhow::Error::msg)?),
            };
            let (browser, _handle) = browser::launch_with(&settings.browser).await?;
            let pacer = HostPacer::new(Duration::from_millis(settings.per_host_delay_ms));
            if let Some(host) = host_of(&url) {
                pacer.wait_turn(&host).await;
            }
            let started = std::time::Instant::now();
            let (page, capture) = browser::open_with_capture(&browser, &url, &settings.open)
                .await
                .with_context(|| format!("opening {url}"))?;
            if let Some(doc) = &capture {
                vuln::seed_capture(doc);
            }
            let checks = vuln::run_vuln_checks_filtered(&page, min).await;
            let _ = page.close().await;
            let timestamp = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0);
            let run = history::RunResult {
                schema_version: history::SCHEMA_VERSION,
                name: label,
                url: url.clone(),
                timestamp,
                checks,
            };
            if let Some(path) = &settings.audit_log {
                let ctx = audit::AuditContext::capture(
                    "vuln",
                    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64,
                    false,
                    match settings.fail_on {
                        FailOn::Fail => "fail",
                        FailOn::Warn => "warn",
                    },
                );
                audit::append_with_context(path, &run, &ctx)?;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&run.checks)?);
            } else {
                println!("Vulnerability scan: {} ({})", run.name, run.url);
                if let Some(m) = min {
                    println!("(showing {} and above)", m.label());
                }
                for check in &run.checks {
                    println!("{} {}", check.status.label(), check.name);
                    for line in check.detail.lines() {
                        println!("  {line}");
                    }
                }
            }
            any_fail |= run_breaches(&run, settings.fail_on, None);
        }
        Command::Monitor {
            configs,
            submit,
            wait,
            headful,
            checks_dir,
            json,
            junit,
            sarif,
            csv,
            out,
            delay_ms,
            webhook_url,
            github_issues_repo,
        } => {
            let settings = resolve(
                &globals,
                &config,
                submit || config.submit.unwrap_or(false),
                wait.or(config.wait).unwrap_or(5),
                headful || config.headful.unwrap_or(false),
                checks_dir.or_else(|| config.checks_dir.clone()),
                delay_ms.or(config.delay_ms).unwrap_or(0),
                webhook_url.or_else(|| clone_config_webhook(&config)),
                github_issues_repo.or_else(|| clone_config_github_issues_repo(&config)),
                accepted,
            );
            guard_submit(settings.run.allow_submit, settings.accepted)?;

            let mut entries = vec![];
            for config_path in &configs {
                match load_forms_config(config_path) {
                    Ok(cfg) => entries.extend(cfg.forms),
                    Err(e) => {
                        tracing::error!(
                            config = %config_path.display(),
                            error = format!("{e:#}"),
                            "skipping unreadable config"
                        );
                        any_fail = true;
                    }
                }
            }

            let entries = match settings.shard {
                Some(shard) => {
                    let total = entries.len();
                    let selected = shard.select(entries);
                    tracing::info!(
                        shard = %shard,
                        selected = selected.len(),
                        total,
                        "shard selected"
                    );
                    selected
                }
                None => entries,
            };

            let (browser, _handle) = browser::launch_with(&settings.browser).await?;
            let pacer = HostPacer::new(Duration::from_millis(settings.per_host_delay_ms));
            let llm_client = build_llm_client(&settings.run.llm);
            let runs = check_all_forms(
                &settings,
                "monitor",
                &entries,
                &browser,
                &pacer,
                llm_client.as_ref(),
            )
            .await?;

            let prior_by_url = load_prior_by_url(&settings.history_dir, &runs);
            if junit || sarif || csv {
                emit_structured(&runs, junit, sarif, csv, out, settings.baseline.as_ref())?;
                any_fail |= runs
                    .iter()
                    .any(|r| run_breaches(r, settings.fail_on, settings.baseline.as_ref()));
            } else if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
                any_fail |= runs
                    .iter()
                    .any(|r| run_breaches(r, settings.fail_on, settings.baseline.as_ref()));
            } else {
                for run in &runs {
                    any_fail |= print_run(
                        prior_by_url.get(&run.url).map(Vec::as_slice).unwrap_or(&[]),
                        run,
                        false,
                        settings.fail_on,
                        settings.baseline.as_ref(),
                    )?;
                }
            }

            notify_runs(&settings, &runs, &prior_by_url).await;
            github_issues_for_runs(&settings, &runs, &prior_by_url).await;
        }
        Command::Watch {
            configs,
            submit,
            wait,
            headful,
            checks_dir,
            interval_secs,
            max_cycles,
            delay_ms,
            json,
            junit,
            sarif,
            csv,
            out,
            webhook_url,
            github_issues_repo,
        } => {
            let settings = resolve(
                &globals,
                &config,
                submit || config.submit.unwrap_or(false),
                wait.or(config.wait).unwrap_or(5),
                headful || config.headful.unwrap_or(false),
                checks_dir.or_else(|| config.checks_dir.clone()),
                delay_ms.or(config.delay_ms).unwrap_or(0),
                webhook_url.or_else(|| clone_config_webhook(&config)),
                github_issues_repo.or_else(|| clone_config_github_issues_repo(&config)),
                accepted,
            );
            guard_submit(settings.run.allow_submit, settings.accepted)?;

            let mut entries = vec![];
            for config_path in &configs {
                match load_forms_config(config_path) {
                    Ok(cfg) => entries.extend(cfg.forms),
                    Err(e) => {
                        tracing::error!(
                            config = %config_path.display(),
                            error = format!("{e:#}"),
                            "skipping unreadable config"
                        );
                        any_fail = true;
                    }
                }
            }

            let entries = match settings.shard {
                Some(shard) => {
                    let total = entries.len();
                    let selected = shard.select(entries);
                    tracing::info!(
                        shard = %shard,
                        selected = selected.len(),
                        total,
                        "shard selected"
                    );
                    selected
                }
                None => entries,
            };
            if entries.is_empty() {
                bail!("nothing to watch: all configs were unreadable or listed no forms");
            }
            let interval =
                Duration::from_secs(interval_secs.or(config.watch_interval_secs).unwrap_or(300));
            if interval.is_zero() {
                bail!("--interval-secs must be greater than zero");
            }

            let (browser, _handle) = browser::launch_with(&settings.browser).await?;
            let pacer = HostPacer::new(Duration::from_millis(settings.per_host_delay_ms));
            let llm_client = build_llm_client(&settings.run.llm);
            // Machine mode (any export flag): stdout carries only the
            // per-cycle artifact — one JSON object per line for --json
            // (JSONL), one document per cycle otherwise — so humans get
            // nothing on stdout and parsers never choke on prose. The
            // startup/stop notes go to stderr instead.
            let machine = json || junit || sarif || csv;
            let startup = format!(
                "Watching {} form(s) every {}s — Ctrl+C to stop. Each cycle saves history, so `formwatch serve` stays live.",
                entries.len(),
                interval.as_secs()
            );
            if machine {
                eprintln!("{startup}");
            } else {
                println!("{startup}");
            }
            let mut interrupted = false;
            let mut watch_failed = false;
            let mut cycle = 0u64;
            loop {
                cycle += 1;
                let cycle_started = std::time::Instant::now();
                let runs = match check_all_forms(
                    &settings,
                    "watch",
                    &entries,
                    &browser,
                    &pacer,
                    llm_client.as_ref(),
                )
                .await
                {
                    Ok(runs) => runs,
                    Err(e) => {
                        // A dead browser or lost history dir must not kill
                        // a days-long watchdog: log it and try again next
                        // cycle instead.
                        tracing::error!(error = format!("{e:#}"), "watch cycle failed");
                        let note =
                            format!("[cycle {cycle}] checks failed: {e:#} — retrying next cycle");
                        if machine {
                            eprintln!("{note}");
                        } else {
                            println!("{note}");
                        }
                        watch_failed = true;
                        tokio::select! {
                            () = tokio::time::sleep(interval) => continue,
                            _ = tokio::signal::ctrl_c() => {
                                interrupted = true;
                                break;
                            }
                        }
                    }
                };
                let prior_by_url = load_prior_by_url(&settings.history_dir, &runs);
                // A breach is a breach regardless of output shape — the
                // exit code (and notifications below) must not depend on
                // whether a human or a machine is watching.
                let breached = runs
                    .iter()
                    .any(|r| run_breaches(r, settings.fail_on, settings.baseline.as_ref()));
                watch_failed |= breached;
                if json {
                    // Compact, not pretty: exactly one line per cycle so
                    // the stream is valid JSONL.
                    println!("{}", serde_json::to_string(&runs)?);
                } else if junit || sarif || csv {
                    emit_structured(
                        &runs,
                        junit,
                        sarif,
                        csv,
                        out.clone(),
                        settings.baseline.as_ref(),
                    )?;
                } else {
                    let (pass, warn, fail) = summarize_cycle(&runs);
                    println!(
                        "[cycle {cycle} {}] {} form(s): {pass} pass, {warn} warn, {fail} fail ({:?})",
                        chrono::Utc::now().format("%Y-%m-%d %H:%M:%S UTC"),
                        runs.len(),
                        cycle_started.elapsed(),
                    );
                    for run in &runs {
                        if run_breaches(run, settings.fail_on, settings.baseline.as_ref()) {
                            print_run(
                                prior_by_url.get(&run.url).map(Vec::as_slice).unwrap_or(&[]),
                                run,
                                false,
                                settings.fail_on,
                                settings.baseline.as_ref(),
                            )?;
                        }
                    }
                }
                notify_runs(&settings, &runs, &prior_by_url).await;
                github_issues_for_runs(&settings, &runs, &prior_by_url).await;
                if let Some(max) = max_cycles
                    && cycle >= max
                {
                    break;
                }
                tokio::select! {
                    () = tokio::time::sleep(interval) => {},
                    _ = tokio::signal::ctrl_c() => {
                        interrupted = true;
                        let note = format!("Stopped after {cycle} cycle(s).");
                        if machine {
                            eprintln!("{note}");
                        } else {
                            println!("{note}");
                        }
                        break;
                    }
                }
            }
            // A user interrupt is a clean stop, not a failure; a bounded
            // `--max-cycles` run reports breaches through the exit code.
            if !interrupted {
                any_fail |= watch_failed;
            }
        }
        Command::Report {
            html,
            json,
            junit,
            sarif,
            prometheus,
            csv,
            markdown,
            out,
        } => {
            let runs = history::all_known_forms(&settings_history_dir(&globals, &config))?;
            let baseline = globals
                .baseline
                .clone()
                .or_else(|| config.baseline.clone())
                .map(|path| Baseline::load_or_empty(&path));
            let default_name;
            let body = if html {
                default_name = Some(PathBuf::from("formwatch-report.html"));
                Some(report::render_html(&settings_history_dir(
                    &globals, &config,
                ))?)
            } else if json {
                default_name = None;
                Some(serde_json::to_string_pretty(&runs)?)
            } else if junit {
                default_name = Some(PathBuf::from("formwatch-report-junit.xml"));
                Some(export::junit_xml(&runs, baseline.as_ref()))
            } else if sarif {
                default_name = Some(PathBuf::from("formwatch-report.sarif.json"));
                Some(serde_json::to_string_pretty(&export::sarif_json(
                    &runs,
                    baseline.as_ref(),
                ))?)
            } else if prometheus {
                default_name = None;
                Some(metrics::render(&runs))
            } else if csv {
                default_name = Some(PathBuf::from("formwatch-report.csv"));
                Some(export::csv(&runs, baseline.as_ref()))
            } else if markdown {
                default_name = Some(PathBuf::from("formwatch-report.md"));
                Some(export::markdown(&runs, baseline.as_ref()))
            } else {
                default_name = None;
                None
            };
            match body {
                Some(body) => match out.or(default_name) {
                    Some(path) => {
                        write_output(&path, &body)?;
                        println!("Wrote {}", path.display());
                    }
                    None => println!("{body}"),
                },
                None => print_report(&settings_history_dir(&globals, &config))?,
            }
        }
        Command::Audit { verify, show, json } => {
            audit_command(&globals, &config, verify, show, json)?;
        }
        Command::Validate {
            configs,
            probe,
            json,
        } => {
            let mut paths = vec![];
            for p in &configs {
                if p.as_os_str() == "forms.yml" && !p.exists() {
                    // Match `monitor`'s friendliness: the default file is
                    // optional, an explicit one is an error.
                    continue;
                }
                paths.push(p.clone());
            }
            if paths.is_empty() {
                println!("No registry files found (default: forms.yml).");
            } else {
                let issues = validate::validate_files(&paths);
                if json {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &issues
                                .iter()
                                .map(
                                    |i| serde_json::json!({"level": i.level, "message": i.message})
                                )
                                .collect::<Vec<_>>()
                        )?
                    );
                } else if issues.is_empty() {
                    println!("{} registry file(s) look good.", paths.len());
                } else {
                    for issue in &issues {
                        println!("[{}] {}", issue.level.to_uppercase(), issue.message);
                    }
                }
                any_fail |= issues.iter().any(|i| i.level == "error");
                if probe {
                    let urls = validate::http_urls_in(&paths);
                    let probed = validate::probe_urls(&urls).await;
                    let down: Vec<&str> = probed
                        .iter()
                        .filter(|p| !p.reachable)
                        .map(|p| p.url.as_str())
                        .collect();
                    if json {
                        println!(
                            "{}",
                            serde_json::to_string_pretty(
                                &probed
                                    .iter()
                                    .map(|p| serde_json::json!({
                                        "url": p.url, "reachable": p.reachable, "status": p.status,
                                    }))
                                    .collect::<Vec<_>>()
                            )?
                        );
                    } else if down.is_empty() {
                        println!("Probe: {} URL(s) reachable.", probed.len());
                    } else {
                        for u in &down {
                            println!("[UNREACHABLE] {u}");
                        }
                    }
                    any_fail |= !down.is_empty();
                }
            }
        }
        Command::Trends {
            last_n,
            min_pass_rate,
            json,
        } => {
            if let Some(r) = min_pass_rate
                && !(0.0..=1.0).contains(&r)
            {
                bail!("--min-pass-rate must be between 0.0 and 1.0, got {r}");
            }
            let history_dir = settings_history_dir(&globals, &config);
            let trends = insights::all_trends(&history_dir, last_n.unwrap_or(0));
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &trends
                            .iter()
                            .map(|t| serde_json::json!({
                                "name": t.name, "url": t.url, "runs": t.runs,
                                "pass_rate": t.pass_rate, "current": t.current.label(),
                                "flaky": t.flaky.iter().map(|f| &f.name).collect::<Vec<_>>(),
                            }))
                            .collect::<Vec<_>>()
                    )?
                );
            } else {
                print!("{}", insights::render_trends_text(&trends));
            }
            if let Some(min) = min_pass_rate
                && trends.iter().any(|t| t.pass_rate < min)
            {
                any_fail = true;
            }
        }
        Command::Flakes { json } => {
            let history_dir = settings_history_dir(&globals, &config);
            let mut flaky: Vec<(String, history::Flakiness)> = vec![];
            for form in history::all_known_forms(&history_dir)? {
                let runs = history::load_runs(&history_dir, &form.url)?;
                for f in history::flakiness(&runs) {
                    if f.is_flaky() {
                        flaky.push((form.name.clone(), f));
                    }
                }
            }
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &flaky
                            .iter()
                            .map(|(form, f)| serde_json::json!({
                                "form": form, "check": f.name,
                                "transitions": f.transitions, "current": f.current.label(),
                            }))
                            .collect::<Vec<_>>()
                    )?
                );
            } else if flaky.is_empty() {
                println!("No flaky checks — every check is stable across its history.");
            } else {
                for (form, f) in &flaky {
                    println!(
                        "FLAKY {form}: {} ({} transitions, now {})",
                        f.name,
                        f.transitions,
                        f.current.label()
                    );
                }
            }
            any_fail |= !flaky.is_empty();
        }
        Command::Diff { target, json } => {
            let history_dir = settings_history_dir(&globals, &config);
            let runs = insights::history_for(&history_dir, &target);
            if runs.len() < 2 {
                bail!(
                    "not enough history for {target:?}: need 2 runs, found {}",
                    runs.len()
                );
            }
            let (prev, curr) = (&runs[runs.len() - 2], &runs[runs.len() - 1]);
            let changes = history::diff(prev, curr);
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &changes
                            .iter()
                            .map(|c| serde_json::json!({
                                "check": c.name, "from": c.from.label(), "to": c.to.label(),
                            }))
                            .collect::<Vec<_>>()
                    )?
                );
            } else if changes.is_empty() {
                println!("No status changes between the last two runs of {target}.");
            } else {
                for c in &changes {
                    println!("{}: {} -> {}", c.name, c.from.label(), c.to.label());
                }
            }
            any_fail |= changes.iter().any(|c| c.to == checks::Status::Fail);
        }
        Command::History {
            target,
            last_n,
            json,
        } => {
            let history_dir = settings_history_dir(&globals, &config);
            let mut runs = insights::history_for(&history_dir, &target);
            if let Some(n) = last_n
                && runs.len() > n
            {
                runs = runs[runs.len() - n..].to_vec();
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else {
                print!("{}", insights::render_history_text(&runs));
            }
        }
        Command::Export { format, out } => {
            let history_dir = settings_history_dir(&globals, &config);
            let runs = insights::dump_all(&history_dir);
            let body = match format.as_str() {
                "jsonl" => insights::export_jsonl(&runs),
                "json" => serde_json::to_string_pretty(&runs)?,
                "csv" => insights::export_csv(&runs),
                other => bail!("unknown --format {other:?}: expected 'jsonl', 'json', or 'csv'"),
            };
            match out {
                Some(path) => {
                    write_output(&path, &body)?;
                    println!("Wrote {} ({} run(s))", path.display(), runs.len());
                }
                None => print!("{body}"),
            }
        }
        Command::Sitemap {
            url,
            max_urls,
            check_forms,
            json,
        } => {
            let max = max_urls.unwrap_or(200);
            if max == 0 {
                bail!("--max-urls must be greater than zero");
            }
            let pages = sitemap::discover_from_arg(&url, max).await?;
            if json {
                if check_forms {
                    let found = sitemap::filter_to_forms(&pages).await;
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &found
                                .iter()
                                .map(|(p, a)| serde_json::json!({"page": p, "actions": a}))
                                .collect::<Vec<_>>()
                        )?
                    );
                } else {
                    println!("{}", serde_json::to_string_pretty(&pages)?);
                }
            } else if check_forms {
                let found = sitemap::filter_to_forms(&pages).await;
                if found.is_empty() {
                    println!("Checked {} page(s): no <form> elements found.", pages.len());
                } else {
                    for (page, actions) in &found {
                        println!("{page}");
                        for a in actions {
                            println!("  -> {a}");
                        }
                    }
                }
            } else {
                for p in &pages {
                    println!("{p}");
                }
                eprintln!(
                    "{} candidate page(s). Re-run with --check-forms to keep only pages containing a <form>.",
                    pages.len()
                );
            }
        }
        Command::Ci {
            forms,
            schedule,
            out,
        } => {
            let body = ci::github_workflow(&forms, &schedule);
            let path = out.unwrap_or_else(|| PathBuf::from(".github/workflows/formwatch.yml"));
            write_output(&path, &body)?;
            println!("Wrote {}", path.display());
        }
    }

    if any_fail {
        std::process::exit(1);
    }
    Ok(())
}

/// The config's webhook URL, if any.
fn clone_config_webhook(config: &Config) -> Option<String> {
    config.notify.as_ref().and_then(|n| n.webhook_url.clone())
}

/// The config's GitHub Issues repo, if any.
fn clone_config_github_issues_repo(config: &Config) -> Option<String> {
    config.github_issues.as_ref().and_then(|g| g.repo.clone())
}

/// Resolves [`GithubIssuesSettings`] from a repo string (already merged
/// CLI > config), the config's own token/labels, and — when the config
/// gave no token — the environment: `GITHUB_TOKEN` (set automatically in
/// GitHub Actions) then `GH_TOKEN` (the `gh` CLI's own convention).
/// Returns `None` (not an error) when no repo is configured at all, or
/// when a repo is configured but no token can be found anywhere — the
/// caller should warn, not fail the run, for the latter.
fn resolve_github_issues(repo: Option<String>, config: &Config) -> Option<GithubIssuesSettings> {
    let repo = repo?;
    let cfg = config.github_issues.as_ref();
    let token = cfg
        .and_then(|g| g.token.clone())
        .or_else(|| std::env::var("GITHUB_TOKEN").ok())
        .or_else(|| std::env::var("GH_TOKEN").ok())
        .filter(|t| !t.trim().is_empty());
    let Some(token) = token else {
        tracing::warn!(
            repo,
            "github-issues-repo is set but no token was found (github_issues.token config, \
             GITHUB_TOKEN, or GH_TOKEN) — regressions won't be tracked as issues this run"
        );
        return None;
    };
    let labels = cfg
        .and_then(|g| g.labels.clone())
        .unwrap_or_else(|| vec!["formwatch".to_string()]);
    Some(GithubIssuesSettings {
        repo,
        token,
        labels,
    })
}

/// Resolves the effective settings for a `test`/`monitor` invocation,
/// applying CLI > environment > file > default precedence.
#[allow(clippy::too_many_arguments)]
fn resolve(
    globals: &Globals,
    config: &Config,
    submit: bool,
    wait: u64,
    headful: bool,
    checks_dir: Option<PathBuf>,
    delay_ms: u64,
    webhook_url: Option<String>,
    github_issues_repo: Option<String>,
    accepted: bool,
) -> Settings {
    Settings {
        history_dir: globals
            .history_dir
            .clone()
            .or_else(|| config.history_dir.clone())
            .unwrap_or_else(|| PathBuf::from(".formwatch/history")),
        checks_dir,
        run: RunOptions {
            allow_submit: submit,
            wait_secs: wait,
            screenshots: if globals.no_screenshots {
                false
            } else {
                config.screenshots.unwrap_or(true)
            },
            check_timeout: Duration::from_secs(
                globals
                    .check_timeout_secs
                    .or(config.check_timeout_secs)
                    .unwrap_or(20),
            ),
            llm: build_llm_options(&globals.llm, config.llm.as_ref()),
            skip_vuln: globals.skip_vuln,
            max_load_ms: globals.max_load_ms.or(config.max_load_ms),
        },
        browser: BrowserOptions {
            headful,
            proxy: globals.proxy.clone().or_else(|| config.proxy.clone()),
            insecure: globals.insecure || config.insecure.unwrap_or(false),
            no_sandbox: globals.no_sandbox || config.no_sandbox.unwrap_or(false),
            request_timeout: None,
        },
        open: OpenOptions::default(),
        retries: globals.retries.or(config.retries).unwrap_or(0),
        delay_ms,
        per_host_delay_ms: globals
            .per_host_delay_ms
            .or(config.per_host_delay_ms)
            .unwrap_or(0),
        max_concurrent: globals
            .max_concurrent
            .or(config.max_concurrent)
            .unwrap_or(MAX_CONCURRENT_FORMS),
        shard: globals.shard.or(config.shard),
        audit_log: globals
            .audit_log
            .clone()
            .or_else(|| config.audit_log.clone()),
        baseline: globals
            .baseline
            .clone()
            .or_else(|| config.baseline.clone())
            .map(|path| Baseline::load_or_empty(&path)),
        webhook_url,
        webhook_on: config
            .notify
            .as_ref()
            .and_then(|n| n.on)
            .unwrap_or(NotifyOn::Regression),
        github_issues: resolve_github_issues(github_issues_repo, config),
        accepted,
        fail_on: globals.fail_on.or(config.fail_on).unwrap_or(FailOn::Fail),
    }
}

/// Builds the shared [`LlmClient`] once per `test`/`monitor` invocation, so
/// every form's LLM checks reuse one HTTP connection pool instead of each
/// paying a fresh handshake to the provider. `None` when LLM checks are
/// disabled, or (rare) client construction failed — logged once here
/// rather than once per form.
fn build_llm_client(llm: &Option<LlmOptions>) -> Option<LlmClient> {
    let options = llm.as_ref()?;
    match LlmClient::new(options) {
        Ok(client) => Some(client),
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "could not initialize the LLM client; LLM checks will be skipped this run"
            );
            None
        }
    }
}

/// Builds [`LlmOptions`] from CLI + config, or `None` when the feature is
/// disabled. API keys are only taken from an explicit flag/config; the
/// provider's environment variable is consulted later, at call time.
fn build_llm_options(args: &LlmArgs, config: Option<&LlmConfig>) -> Option<LlmOptions> {
    let enabled = args.llm || config.and_then(|c| c.enabled).unwrap_or(false);
    if !enabled {
        return None;
    }
    let provider = args
        .llm_provider
        .or_else(|| config.and_then(|c| c.provider))
        .unwrap_or_default();
    let model = args
        .llm_model
        .clone()
        .or_else(|| config.and_then(|c| c.model.clone()))
        .unwrap_or_else(|| provider.default_model().to_string());
    Some(LlmOptions {
        enabled: true,
        provider,
        model,
        api_key: args
            .llm_api_key
            .clone()
            .or_else(|| config.and_then(|c| c.api_key.clone())),
        base_url: args
            .llm_base_url
            .clone()
            .or_else(|| config.and_then(|c| c.base_url.clone())),
        timeout: Duration::from_secs(config.and_then(|c| c.timeout_secs).unwrap_or(30)),
        max_retries: args
            .llm_max_retries
            .or_else(|| config.and_then(|c| c.max_retries))
            .unwrap_or(2),
        max_input_chars: config.and_then(|c| c.max_input_chars).unwrap_or(6000),
        threshold: args
            .llm_threshold
            .or_else(|| config.and_then(|c| c.threshold))
            .unwrap_or(3)
            .clamp(1, 5),
        fail: args.llm_fail || config.and_then(|c| c.fail).unwrap_or(false),
        redact: !args.llm_no_redact && config.and_then(|c| c.redact).unwrap_or(true),
        cache: !args.llm_no_cache && config.and_then(|c| c.cache).unwrap_or(true),
        cache_dir: config.and_then(|c| c.cache_dir.clone()),
    })
}

/// `report` doesn't run forms, so it only needs the history directory.
fn settings_history_dir(globals: &Globals, config: &Config) -> PathBuf {
    globals
        .history_dir
        .clone()
        .or_else(|| config.history_dir.clone())
        .unwrap_or_else(|| PathBuf::from(".formwatch/history"))
}

/// Enforces the authorized-use gate for a real submission, and prints a
/// one-time reminder for any `test`/`monitor` run. The reminder goes
/// through `tracing` (stderr, honoring `--quiet` and `--log-format json`)
/// so it never corrupts a machine-readable log stream.
fn guard_submit(submit: bool, accepted: bool) -> Result<()> {
    if submit {
        if let Err(msg) = legal::check_submit_authorized(accepted) {
            eprintln!("\n{}\n", legal::notice());
            eprintln!("{}", msg.red().bold());
            // A distinct exit code, so a script can tell "refused for lack
            // of acknowledgement" apart from "a check failed".
            std::process::exit(2);
        }
    } else if !accepted {
        tracing::warn!("{}", legal::SHORT_REMINDER);
    }
    Ok(())
}

/// `formwatch baseline`: with `--write`, generate a baseline from the
/// current history and save it; otherwise print the configured baseline
/// (or a preview generated from current history).
fn baseline_command(
    globals: &Globals,
    config: &Config,
    write: bool,
    out: Option<PathBuf>,
    json: bool,
) -> Result<()> {
    let history_dir = settings_history_dir(globals, config);
    let configured = globals.baseline.clone().or_else(|| config.baseline.clone());

    if write {
        let runs = history::all_known_forms(&history_dir)?;
        let baseline = Baseline::from_runs(&runs);
        let path = out
            .or(configured)
            .unwrap_or_else(|| PathBuf::from(".formwatch/baseline.json"));
        baseline.save(&path)?;
        println!(
            "Wrote {} ({} accepted finding(s))",
            path.display(),
            baseline.len()
        );
        return Ok(());
    }

    let baseline = match &configured {
        Some(path) if path.exists() => Baseline::load(path)?,
        _ => Baseline::from_runs(&history::all_known_forms(&history_dir)?),
    };

    if json {
        println!("{}", serde_json::to_string_pretty(&baseline)?);
    } else if baseline.is_empty() {
        println!("No accepted findings.");
    } else {
        println!("{} accepted finding(s):", baseline.len());
        for entry in &baseline.entries {
            println!(
                "  [{}] {} — {}",
                entry.status.label(),
                entry.url,
                entry.check
            );
        }
    }
    Ok(())
}

/// `formwatch audit`: verify the tamper-evident hash chain, or show the
/// most recent entries. Verification exits non-zero when the chain is
/// broken so CI can gate on it.
fn audit_command(
    globals: &Globals,
    config: &Config,
    verify: bool,
    show: Option<usize>,
    json: bool,
) -> Result<()> {
    let Some(path) = globals
        .audit_log
        .clone()
        .or_else(|| config.audit_log.clone())
    else {
        bail!("no audit log configured — pass --audit-log FILE or set audit_log in config");
    };
    if verify {
        let report = audit::verify(&path)?;
        if json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "path": path.display().to_string(),
                    "entries": report.entries,
                    "v2_entries": report.v2_entries,
                    "legacy_entries": report.legacy_entries,
                    "ok": report.ok,
                    "broken_at": report.broken_at,
                    "reason": report.reason,
                }))?
            );
        } else if report.ok {
            println!(
                "Audit chain OK: {} entrie(s) verify ({} v2, {} legacy) in {}",
                report.entries,
                report.v2_entries,
                report.legacy_entries,
                path.display()
            );
        } else {
            eprintln!(
                "Audit chain BROKEN at line {}: {}",
                report.broken_at.unwrap_or(0),
                report.reason.as_deref().unwrap_or("unknown reason")
            );
        }
        if !report.ok {
            std::process::exit(1);
        }
        return Ok(());
    }
    let n = show.unwrap_or(10);
    let entries = audit::tail(&path, n)?;
    if entries.is_empty() {
        println!("No audit entries yet in {}.", path.display());
        return Ok(());
    }
    if json {
        println!("{}", serde_json::to_string_pretty(&entries)?);
        return Ok(());
    }
    for v in &entries {
        let ts = v.get("timestamp").and_then(|t| t.as_i64()).unwrap_or(0);
        let when = chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
            .unwrap_or_else(|| ts.to_string());
        let outcome = v.get("outcome").and_then(|o| o.as_str()).unwrap_or("?");
        let url = v.get("url").and_then(|u| u.as_str()).unwrap_or("?");
        let actor = v.get("actor").and_then(|a| a.as_str()).unwrap_or("?");
        let host = v.get("hostname").and_then(|h| h.as_str()).unwrap_or("?");
        println!("[{when}] {outcome} {url} (by {actor} on {host})");
    }
    Ok(())
}

/// `formwatch doctor`: prints the local environment self-check, exiting
/// non-zero if any check failed.
async fn doctor_command(
    globals: &Globals,
    config: &Config,
    accepted: bool,
    json: bool,
    probe_llm: bool,
) -> Result<()> {
    let history_dir = settings_history_dir(globals, config);
    let audit_log = globals
        .audit_log
        .clone()
        .or_else(|| config.audit_log.clone());
    let proxy = globals.proxy.clone().or_else(|| config.proxy.clone());
    let mut results = doctor::run(&doctor::Context {
        history_dir: &history_dir,
        audit_log: audit_log.as_deref(),
        proxy: proxy.as_deref(),
        accepted,
    });

    if probe_llm {
        // Probe the configured provider regardless of whether the checks
        // are switched on, so `doctor --llm` can validate credentials
        // before a real run.
        let mut args = globals.llm.clone();
        args.llm = true;
        let check = match build_llm_options(&args, config.llm.as_ref()) {
            None => doctor::Check {
                name: "LLM provider",
                ok: true,
                detail: "not enabled".to_string(),
            },
            Some(options) => match formwatch::llm::probe(&options).await {
                Ok(detail) => doctor::Check {
                    name: "LLM provider",
                    ok: true,
                    detail,
                },
                Err(e) => doctor::Check {
                    name: "LLM provider",
                    ok: false,
                    detail: format!("{e:#}"),
                },
            },
        };
        results.push(check);
    }

    let ok = results.iter().all(|c| c.ok);
    if json {
        let checks: Vec<serde_json::Value> = results
            .iter()
            .map(|c| serde_json::json!({ "name": c.name, "ok": c.ok, "detail": c.detail }))
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "version": env!("CARGO_PKG_VERSION"),
                "schema_version": history::SCHEMA_VERSION,
                "ok": ok,
                "checks": checks,
            }))?
        );
    } else {
        println!(
            "{}",
            format!(
                "formwatch {} — environment check",
                env!("CARGO_PKG_VERSION")
            )
            .bold()
        );
        for c in &results {
            let label = if c.ok {
                "OK".green().to_string()
            } else {
                "FAIL".red().to_string()
            };
            println!("  [{label}] {}: {}", c.name, c.detail);
        }
        println!(
            "\n{}",
            if ok {
                "All checks passed.".green().to_string()
            } else {
                "Some checks failed.".red().to_string()
            }
        );
    }
    if !ok {
        std::process::exit(1);
    }
    Ok(())
}

/// `formwatch notify-test`: sends one synthetic regression so an operator
/// can confirm a webhook actually works before relying on it.
async fn notify_test(config: &Config, webhook_url: Option<String>, json: bool) -> Result<()> {
    let Some(url) = webhook_url.or_else(|| clone_config_webhook(config)) else {
        bail!("no webhook URL given — pass --webhook-url or set notify.webhook_url in config");
    };
    let sample = vec![notify::Regression {
        form: "Example form".to_string(),
        url: "https://example.gov/apply".to_string(),
        check: "Accessibility".to_string(),
        from: checks::Status::Warn,
        to: checks::Status::Fail,
    }];
    let payload = notify::payload_for(&url, &sample, chrono::Utc::now().timestamp());
    match notify::send(&url, &payload).await {
        Ok(()) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": true,
                        "webhook": url,
                        "kind": if notify::is_slack_webhook(&url) { "slack" } else { "generic" },
                    }))?
                );
            } else {
                println!(
                    "{}",
                    format!("Delivered a test notification to {url}").green()
                );
            }
            Ok(())
        }
        Err(e) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "ok": false,
                        "webhook": url,
                        "error": format!("{e:#}"),
                    }))?
                );
            } else {
                eprintln!("{}: {e:#}", "Test notification failed".red());
            }
            std::process::exit(1);
        }
    }
}

/// Sends a webhook notification for `runs` if one is configured and the
/// notify policy calls for it. Never fatal: a failed notification is a
/// warning, not a reason to lose a completed run's results. `prior` is
/// loaded once per invocation (see [`load_prior_by_url`]) and shared with
/// [`print_run`]/[`github_issues_for_runs`] rather than reloaded here.
async fn notify_runs(
    settings: &Settings,
    runs: &[history::RunResult],
    prior: &HashMap<String, Vec<history::RunResult>>,
) {
    let Some(url) = settings.webhook_url.as_deref() else {
        return;
    };
    let mut regressions = notify::regressions_from_runs(runs, prior);
    // A regression that the baseline already accepts isn't worth paging on.
    if let Some(baseline) = &settings.baseline {
        regressions.retain(|r| !baseline.is_accepted(&r.url, &r.check, r.to));
    }
    if settings.webhook_on == NotifyOn::Regression && regressions.is_empty() {
        return;
    }
    let payload = notify::payload_for(url, &regressions, chrono::Utc::now().timestamp());
    match notify::send(url, &payload).await {
        Ok(()) => tracing::info!(
            webhook = url,
            count = regressions.len(),
            "notification delivered"
        ),
        Err(e) => tracing::warn!(error = format!("{e:#}"), "notification failed"),
    }
}

/// Creates/closes GitHub Issues for `runs`' regressions/recoveries if a
/// repo is configured. Never fatal, same as [`notify_runs`]: a dead
/// token or a rate limit is a warning, not a reason to lose a completed
/// run's results. `prior` is loaded once per invocation (see
/// [`load_prior_by_url`]) and shared with [`print_run`]/[`notify_runs`]
/// rather than reloaded here.
async fn github_issues_for_runs(
    settings: &Settings,
    runs: &[history::RunResult],
    prior: &HashMap<String, Vec<history::RunResult>>,
) {
    let Some(gh) = &settings.github_issues else {
        return;
    };
    let mut events = issues::events_from_runs(runs, prior);
    // A baselined Fail is an accepted finding — it shouldn't spawn a
    // tracking issue any more than it should page a webhook. A recovery
    // (-> Pass) closes an issue regardless of baseline state.
    if let Some(baseline) = &settings.baseline {
        events.retain(|e| {
            e.to != checks::Status::Fail || !baseline.is_accepted(&e.url, &e.check, e.to)
        });
    }
    if events.is_empty() {
        return;
    }
    let outcomes = issues::reconcile(&gh.repo, &gh.token, &gh.labels, &events).await;
    for outcome in outcomes {
        match outcome {
            issues::Outcome::Created { check, number } => {
                tracing::info!(check, number, "GitHub issue created")
            }
            issues::Outcome::AlreadyTracked { check, number } => {
                tracing::debug!(check, number, "GitHub issue already open")
            }
            issues::Outcome::Closed { check, number } => {
                tracing::info!(check, number, "GitHub issue closed on recovery")
            }
            issues::Outcome::Skipped => {}
            issues::Outcome::Failed { check, error } => {
                tracing::warn!(check, error, "GitHub issue reconciliation failed")
            }
        }
    }
}

/// Paces a stream so each item is only yielded `delay` after the item
/// before it (a no-op pass-through when `delay` is zero) — used to
/// spread `monitor`'s per-form checks out over time instead of firing
/// up to `MAX_CONCURRENT_FORMS` requests at once, a real-world courtesy
/// against third-party sites this tool doesn't control. A plain
/// `buffer_unordered(N)` alone doesn't pace anything: it happily starts
/// all N immediately, then refills as each one finishes.
/// True when `run` never actually loaded the page: the single `Page
/// load` Fail check the runner emits instead of real results. Only these
/// runs are worth retrying — a form that loads but fails checks produced
/// a genuine verdict, and re-probing a third-party site for it would just
/// add load without new information.
fn is_load_failure(run: &history::RunResult) -> bool {
    run.checks.len() == 1
        && run.checks[0].name == "Page load"
        && run.checks[0].status == checks::Status::Fail
}

/// One full pass over every configured form: concurrent per-form runs
/// returned in config order, plus one audit entry per run recorded under
/// `command`. The engine shared by `monitor` (called once) and each
/// `watch` cycle. A form whose check fails to execute is logged and
/// skipped — never fatal to the pass (an audit-write failure still
/// aborts, as before, so a silent audit gap can't masquerade as clean
/// history).
async fn check_all_forms(
    settings: &Settings,
    command: &str,
    entries: &[FormEntry],
    browser: &chromiumoxide::Browser,
    pacer: &HostPacer,
    llm_client: Option<&LlmClient>,
) -> Result<Vec<history::RunResult>> {
    let run_opts = &settings.run;
    let open_opts = &settings.open;
    let checks_dir_ref = settings.checks_dir.as_deref();
    let history_dir = &settings.history_dir;

    let mut indexed: Vec<(usize, history::RunResult)> = paced(
        stream::iter(entries.iter().cloned().enumerate()),
        Duration::from_millis(settings.delay_ms),
    )
    .map(|(i, entry)| async move {
        if let Some(host) = host_of(&entry.url) {
            pacer.wait_turn(&host).await;
        }
        // Flake-aware retries: a page that never loads (DNS blip, slow
        // government CDN) is retried with backoff; a loaded page with
        // failing checks is a real verdict and is kept as-is.
        let mut attempt = 0u32;
        loop {
            match runner::run_one_with(
                browser,
                history_dir,
                entry.name.clone(),
                entry.url.clone(),
                run_opts,
                open_opts,
                checks_dir_ref,
                llm_client,
            )
            .await
            {
                Ok(run) => {
                    if is_load_failure(&run) && attempt < settings.retries {
                        attempt += 1;
                        tracing::warn!(
                            form = %entry.name,
                            attempt,
                            "page load failed; retrying"
                        );
                        tokio::time::sleep(Duration::from_secs(2u64.pow(attempt.min(4)))).await;
                        continue;
                    }
                    break Some((i, run));
                }
                Err(e) => {
                    tracing::error!(
                        form = %entry.name,
                        error = format!("{e:#}"),
                        "form check failed"
                    );
                    break None;
                }
            }
        }
    })
    .buffer_unordered(settings.max_concurrent.max(1))
    .filter_map(|x| async move { x })
    .collect()
    .await;
    indexed.sort_by_key(|(i, _)| *i);
    let runs: Vec<history::RunResult> = indexed.into_iter().map(|(_, r)| r).collect();

    // One audit entry per run; duration is per-invocation here
    // (concurrent forms share the wall clock), so record 0 and
    // let the per-form detail live in history instead.
    let fail_on = match settings.fail_on {
        FailOn::Fail => "fail",
        FailOn::Warn => "warn",
    };
    for run in &runs {
        if let Some(path) = &settings.audit_log {
            let ctx = audit::AuditContext::capture(command, 0, settings.run.allow_submit, fail_on);
            audit::append_with_context(path, run, &ctx)?;
        }
    }
    Ok(runs)
}

/// Tallies one pass over forms by worst status per run: a run with any
/// `Fail` counts as failed even if it also warns, and one with no
/// failures but a `Warn` counts as warned. Returns
/// `(pass, warn, fail)`.
fn summarize_cycle(runs: &[history::RunResult]) -> (usize, usize, usize) {
    let mut pass = 0;
    let mut warn = 0;
    let mut fail = 0;
    for run in runs {
        if run.checks.iter().any(|c| c.status == checks::Status::Fail) {
            fail += 1;
        } else if run.checks.iter().any(|c| c.status == checks::Status::Warn) {
            warn += 1;
        } else {
            pass += 1;
        }
    }
    (pass, warn, fail)
}

fn paced<S: stream::Stream>(source: S, delay: Duration) -> impl stream::Stream<Item = S::Item> {
    source.then(move |item| async move {
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
        item
    })
}

/// Writes `contents` to `path`, creating its parent directory first if
/// needed — `report --out some/new/dir/file.json` (or `--html`'s own
/// `--out`) failing with a bare "No such file or directory" because
/// nothing had created `some/new/dir/` yet is exactly the bug already
/// fixed once for `formwatch init subdir/file.yml`; this is the same bug
/// in a different command that fix never reached.
fn write_output(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(path, contents).with_context(|| format!("writing {}", path.display()))
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
    write_output(path, STARTER_FORMS_YML)?;
    println!("Wrote {}", path.display());

    let checks_dir = Path::new("checks");
    if !checks_dir.exists() {
        std::fs::create_dir(checks_dir)?;
        std::fs::write(checks_dir.join("example.js"), EXAMPLE_CUSTOM_CHECK_JS)?;
        println!("Wrote checks/example.js (an example custom check)");
    }

    println!(
        "\nNext steps:\n  1. Edit {} with real form URLs.\n  2. formwatch monitor {}\n  3. formwatch monitor {} --checks-dir checks   (to also run the example custom check)\n\nOnly add forms you are authorized to test — see `formwatch legal`.",
        path.display(),
        path.display(),
        path.display()
    );
    Ok(())
}

fn run_breaches(run: &history::RunResult, fail_on: FailOn, baseline: Option<&Baseline>) -> bool {
    run.checks
        .iter()
        .any(|c| baseline::check_breaches(run, c, fail_on, baseline))
}

/// Renders `runs` as JUnit XML, SARIF, or CSV and writes it to `out`
/// (stdout when none) — the `--junit`/`--sarif`/`--csv` output for
/// `test`/`monitor`. Baseline-aware.
fn emit_structured(
    runs: &[history::RunResult],
    junit: bool,
    sarif: bool,
    csv: bool,
    out: Option<PathBuf>,
    baseline: Option<&Baseline>,
) -> Result<()> {
    let body = match (junit, sarif, csv) {
        (true, _, _) => export::junit_xml(runs, baseline),
        (false, true, _) => serde_json::to_string_pretty(&export::sarif_json(runs, baseline))?,
        (false, false, true) => export::csv(runs, baseline),
        (false, false, false) => String::new(),
    };
    match out {
        Some(path) => {
            write_output(&path, &body)?;
            println!("Wrote {}", path.display());
        }
        None => println!("{body}"),
    }
    Ok(())
}

/// Loads each of `runs`' URLs' history exactly once per invocation
/// (deduplicating repeated URLs) and shares it across [`print_run`],
/// [`notify_runs`], and [`github_issues_for_runs`] — each of which
/// otherwise independently calls [`history::load_runs`] for the very same
/// URL, tripling history reads (each re-parsing every historical run,
/// screenshots included) for a single `test`/`monitor` invocation. A URL
/// that fails to load gets an empty history rather than aborting the run.
fn load_prior_by_url(
    history_dir: &Path,
    runs: &[history::RunResult],
) -> HashMap<String, Vec<history::RunResult>> {
    let mut prior = HashMap::new();
    for run in runs {
        prior
            .entry(run.url.clone())
            .or_insert_with(|| history::load_runs(history_dir, &run.url).unwrap_or_default());
    }
    prior
}

/// Prints one run (JSON or colored text, plus the diff against its
/// previous run) and reports whether it breaches the `--fail-on` policy —
/// the caller aggregates that into the process exit code. `prior` is
/// `run`'s own history, loaded once per invocation by the caller (see
/// [`load_prior_by_url`]) rather than by this function.
fn print_run(
    prior: &[history::RunResult],
    run: &history::RunResult,
    json: bool,
    fail_on: FailOn,
    baseline: Option<&Baseline>,
) -> Result<bool> {
    if json {
        println!("{}", serde_json::to_string_pretty(run)?);
        return Ok(run_breaches(run, fail_on, baseline));
    }

    println!("\n{}", format!("== {} ({}) ==", run.name, run.url).bold());
    let flaky = history::flakiness(prior);
    for check in &run.checks {
        let baselined = check.status != checks::Status::Pass
            && baseline
                .map(|b| b.is_accepted(&run.url, &check.name, check.status))
                .unwrap_or(false);
        print_check(check, is_flaky(&flaky, check), baselined);
    }

    if let Some(prev) = history::previous_run(prior, run.timestamp) {
        print_changes(&history::diff(prev, run));
    }
    Ok(run_breaches(run, fail_on, baseline))
}

fn is_flaky(flaky: &[history::Flakiness], check: &checks::CheckResult) -> bool {
    flaky.iter().any(|f| f.name == check.name && f.is_flaky())
}

fn print_check(check: &checks::CheckResult, flaky: bool, baselined: bool) {
    let label = match check.status {
        checks::Status::Pass => check.status.label().green().to_string(),
        checks::Status::Warn => check.status.label().yellow().to_string(),
        checks::Status::Fail => check.status.label().red().to_string(),
    };
    let flaky_note = if flaky {
        " [FLAKY]".yellow().to_string()
    } else {
        String::new()
    };
    let baselined_note = if baselined {
        " [BASELINED]".cyan().to_string()
    } else {
        String::new()
    };
    let screenshot_note = if check.screenshot.is_some() {
        " (screenshot captured — see --html/--json report)"
    } else {
        ""
    };
    println!(
        "  [{label}] {}{flaky_note}{baselined_note}: {}{screenshot_note}",
        check.name, check.detail
    );
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
            print_check(check, form.flakiness_of(check).is_some(), false);
            if let Some(streak) = form.streak_of(check) {
                println!(
                    "      \u{21b3} {} for the last {} run(s), since {}",
                    streak.status,
                    streak.count,
                    human_ts(streak.since)
                );
            }
        }
        print_changes(&form.changes);
    }
    Ok(())
}

/// Formats a Unix timestamp for plain-text output.
fn human_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d %H:%M UTC").to_string())
        .unwrap_or_else(|| ts.to_string())
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
            screenshot: None,
        }
    }

    fn run(checks: Vec<CheckResult>) -> history::RunResult {
        history::RunResult {
            schema_version: history::SCHEMA_VERSION,
            name: "x".into(),
            url: "https://example.test/form".into(),
            timestamp: 0,
            checks,
        }
    }

    #[test]
    fn load_prior_by_url_loads_each_url_once_even_with_repeated_urls() {
        // notify_runs/github_issues_for_runs/print_run all used to call
        // history::load_runs independently for the same URL; this map is
        // what replaced that. A `monitor` config could in principle list
        // the same URL twice, so this also checks the dedup itself, not
        // just that the map has the right shape.
        let dir = std::env::temp_dir().join("formwatch-test-load-prior-by-url");
        let _ = std::fs::remove_dir_all(&dir);
        let a = history::RunResult {
            schema_version: history::SCHEMA_VERSION,
            name: "A".into(),
            url: "https://city.gov/a".into(),
            timestamp: 1,
            checks: vec![],
        };
        let b = history::RunResult {
            url: "https://city.gov/b".into(),
            ..a.clone()
        };
        history::save_run(&dir, &a).expect("save a");
        history::save_run(&dir, &b).expect("save b");

        let prior = load_prior_by_url(&dir, &[a.clone(), a.clone(), b.clone()]);
        assert_eq!(prior.len(), 2, "one entry per distinct URL");
        assert_eq!(prior[&a.url].len(), 1);
        assert_eq!(prior[&b.url].len(), 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_llm_client_is_none_when_disabled_and_some_when_enabled() {
        // The client is built once per invocation (not once per form, see
        // runner::run_one_with) — this only checks the wiring itself: no
        // options means no client, and enabled options (Mock needs no API
        // key) build one successfully rather than erroring.
        assert!(build_llm_client(&None).is_none());
        let options = LlmOptions {
            enabled: true,
            provider: Provider::Mock,
            ..LlmOptions::default()
        };
        assert!(build_llm_client(&Some(options)).is_some());
    }

    #[test]
    fn fail_on_policy_controls_which_statuses_breach() {
        // The exit code is the whole point of running this in CI. Under the
        // default `--fail-on fail`, a Warn (or an all-Pass run) must exit 0;
        // under `--fail-on warn` a Warn is a breach too.
        let with_warn = run(vec![check(Status::Pass), check(Status::Warn)]);
        let with_fail = run(vec![check(Status::Pass), check(Status::Fail)]);
        let all_pass = run(vec![check(Status::Pass)]);

        assert!(!run_breaches(&with_warn, FailOn::Fail, None));
        assert!(run_breaches(&with_fail, FailOn::Fail, None));
        assert!(!run_breaches(&all_pass, FailOn::Fail, None));

        assert!(run_breaches(&with_warn, FailOn::Warn, None));
        assert!(run_breaches(&with_fail, FailOn::Warn, None));
        assert!(!run_breaches(&all_pass, FailOn::Warn, None));
    }

    #[test]
    fn history_dir_precedence_is_cli_then_config_then_default() {
        let config = Config {
            history_dir: Some(PathBuf::from("/config/history")),
            ..Config::default()
        };

        let cli = Globals {
            history_dir: Some(PathBuf::from("/cli/history")),
            ..Globals::default()
        };
        assert_eq!(
            settings_history_dir(&cli, &config),
            PathBuf::from("/cli/history"),
            "an explicit --history-dir must win over config"
        );

        let no_cli = Globals::default();
        assert_eq!(
            settings_history_dir(&no_cli, &config),
            PathBuf::from("/config/history"),
            "config fills in when the CLI flag is absent"
        );
        assert_eq!(
            settings_history_dir(&no_cli, &Config::default()),
            PathBuf::from(".formwatch/history"),
            "the built-in default applies when neither is set"
        );
    }

    #[test]
    fn resolve_prefers_cli_over_config_for_fail_on_and_screenshots() {
        let globals = Globals {
            fail_on: Some(FailOn::Warn),
            no_screenshots: true,
            ..Globals::default()
        };
        let config = Config {
            fail_on: Some(FailOn::Fail),
            screenshots: Some(true),
            ..Config::default()
        };

        let settings = resolve(
            &globals, &config, false, 5, false, None, 0, None, None, false,
        );
        assert_eq!(settings.fail_on, FailOn::Warn, "CLI --fail-on wins");
        assert!(!settings.run.screenshots, "--no-screenshots wins");
        assert_eq!(
            settings.max_concurrent, MAX_CONCURRENT_FORMS,
            "unset values fall back to the built-in default"
        );
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

    #[test]
    fn write_output_creates_a_new_directory_that_does_not_exist_yet() {
        // Found via a real deploy: `formwatch report --json --out
        // results/index.json` failed with a bare "No such file or
        // directory" the very first time it ran against a fresh
        // checkout, because nothing had created results/ yet — the same
        // bug class already fixed once for `formwatch init
        // subdir/file.yml`, in a command that fix never reached.
        let dir = std::env::temp_dir().join("formwatch-test-write-output");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("index.json");

        write_output(&path, "[]").expect("write_output should create nested/ itself");

        assert_eq!(std::fs::read_to_string(&path).expect("read back"), "[]");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn paced_with_zero_delay_does_not_slow_the_stream() {
        let start = std::time::Instant::now();
        let items: Vec<i32> = paced(stream::iter(0..5), Duration::ZERO).collect().await;
        assert_eq!(items, vec![0, 1, 2, 3, 4]);
        assert!(
            start.elapsed() < Duration::from_millis(200),
            "zero delay should add no meaningful wait, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn paced_with_a_delay_spaces_items_out() {
        // --delay-ms is meant to spread monitor's per-form requests out
        // over time, not just cap concurrency — this is the one property
        // that actually distinguishes `paced` from a plain pass-through,
        // so it's the one worth proving directly rather than trusting
        // that `.then()` obviously does the right thing.
        let delay = Duration::from_millis(30);
        let start = std::time::Instant::now();
        let items: Vec<i32> = paced(stream::iter(0..3), delay).collect().await;
        assert_eq!(items, vec![0, 1, 2]);
        assert!(
            start.elapsed() >= delay * 3,
            "expected at least {:?} total, took {:?}",
            delay * 3,
            start.elapsed()
        );
    }

    #[test]
    fn watch_json_is_one_line_per_cycle() {
        // `watch --json` promises JSONL: exactly one line per cycle, so
        // a machine can split on newlines and parse each line. Pretty
        // printing would break that contract with embedded newlines.
        let runs = vec![run(vec![check(Status::Pass), check(Status::Warn)])];
        let line = serde_json::to_string(&runs).expect("serialize");
        assert!(!line.contains('\n'), "must be a single line");
        let back: Vec<history::RunResult> =
            serde_json::from_str(line.trim()).expect("must parse back");
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].checks.len(), 2);
    }

    #[test]
    fn summarize_cycle_tallies_runs_by_worst_status() {
        // One line per cycle is the whole point of `watch` output, so
        // the tally it prints gets its own test: a run counts once, by
        // its worst check (a Fail beats a Warn on the same run).
        let all_pass = run(vec![check(Status::Pass), check(Status::Pass)]);
        let warned = run(vec![check(Status::Pass), check(Status::Warn)]);
        let failed = run(vec![check(Status::Warn), check(Status::Fail)]);
        assert_eq!(summarize_cycle(&[all_pass, warned, failed]), (1, 1, 1));
        assert_eq!(summarize_cycle(&[]), (0, 0, 0));
    }
}
