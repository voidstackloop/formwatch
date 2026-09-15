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
    audit, browser, browser::BrowserOptions, browser::OpenOptions, checks, demo, doctor, export,
    history, issues, legal, limiter::HostPacer, metrics, notify, report, runner, serve,
};
use futures::stream::{self, StreamExt};
use owo_colors::OwoColorize;
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
    about = "Tests public-service forms for broken flows, accessibility, mobile usability, and lost input.",
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
        /// Where to write --junit/--sarif output (default: stdout).
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
        /// Where to write --junit/--sarif output (default: stdout).
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
    /// Print (or render to HTML/JSON/JUnit/SARIF) the most recent run of every form formwatch knows about.
    Report {
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif", "prometheus"])]
        html: bool,
        #[arg(long)]
        json: bool,
        /// Render as JUnit XML (for CI test dashboards).
        #[arg(long, conflicts_with_all = ["json", "sarif", "prometheus"])]
        junit: bool,
        /// Render as SARIF 2.1.0 (for code-scanning UIs).
        #[arg(long, conflicts_with_all = ["json", "prometheus"])]
        sarif: bool,
        /// Render as Prometheus text-exposition metrics.
        #[arg(long, conflicts_with_all = ["json", "junit", "sarif"])]
        prometheus: bool,
        /// Where to write output (default: stdout for JSON, else a named file).
        #[arg(long)]
        out: Option<PathBuf>,
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
    /// Serve read-only HTTP endpoints (/healthz, /readyz, /metrics, /api/forms).
    Serve {
        /// Address to bind (default: 127.0.0.1:8080).
        #[arg(long)]
        addr: Option<SocketAddr>,
    },
    /// Serve the bundled demo site so you can see what formwatch catches.
    Demo {
        /// Address to bind (default: 127.0.0.1:8099).
        #[arg(long)]
        addr: Option<SocketAddr>,
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
        llm,
    } = Cli::parse();
    logging::init(verbose, quiet, log_format);

    let config = Config::load(config.as_deref())?;
    let accepted = legal::is_accepted(accept_terms, config.accept_terms);
    let globals = Globals {
        history_dir,
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
        Command::Serve { addr } => {
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
            serve::run(history_dir, addr).await?;
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
            let run = runner::run_one_with(
                &browser,
                &settings.history_dir,
                name.unwrap_or_else(|| url.clone()),
                url,
                &settings.run,
                &settings.open,
                settings.checks_dir.as_deref(),
                llm_client.as_ref(),
            )
            .await?;
            if let Some(path) = &settings.audit_log {
                audit::append(path, &run)?;
            }
            if junit || sarif {
                emit_structured(
                    std::slice::from_ref(&run),
                    junit,
                    sarif,
                    out,
                    settings.baseline.as_ref(),
                )?;
                any_fail |= run_breaches(&run, settings.fail_on, settings.baseline.as_ref());
            } else {
                any_fail |= print_run(
                    &settings.history_dir,
                    &run,
                    json,
                    settings.fail_on,
                    settings.baseline.as_ref(),
                )?;
            }
            notify_runs(&settings, std::slice::from_ref(&run)).await;
            github_issues_for_runs(&settings, std::slice::from_ref(&run)).await;
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
            let run_opts = &settings.run;
            let open_opts = &settings.open;
            let checks_dir_ref = settings.checks_dir.as_deref();
            let history_dir = &settings.history_dir;

            let mut indexed: Vec<(usize, history::RunResult)> = paced(
                stream::iter(entries.into_iter().enumerate()),
                Duration::from_millis(settings.delay_ms),
            )
            .map(|(i, entry)| {
                let browser = &browser;
                let pacer = &pacer;
                let llm_client = llm_client.as_ref();
                async move {
                    if let Some(host) = host_of(&entry.url) {
                        pacer.wait_turn(&host).await;
                    }
                    match runner::run_one_with(
                        browser,
                        history_dir,
                        entry.name.clone(),
                        entry.url,
                        run_opts,
                        open_opts,
                        checks_dir_ref,
                        llm_client,
                    )
                    .await
                    {
                        Ok(run) => Some((i, run)),
                        Err(e) => {
                            tracing::error!(
                                form = %entry.name,
                                error = format!("{e:#}"),
                                "form check failed"
                            );
                            None
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

            for run in &runs {
                if let Some(path) = &settings.audit_log {
                    audit::append(path, run)?;
                }
            }

            if junit || sarif {
                emit_structured(&runs, junit, sarif, out, settings.baseline.as_ref())?;
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
                        history_dir,
                        run,
                        false,
                        settings.fail_on,
                        settings.baseline.as_ref(),
                    )?;
                }
            }

            notify_runs(&settings, &runs).await;
            github_issues_for_runs(&settings, &runs).await;
        }
        Command::Report {
            html,
            json,
            junit,
            sarif,
            prometheus,
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
        },
        browser: BrowserOptions {
            headful,
            proxy: globals.proxy.clone().or_else(|| config.proxy.clone()),
            insecure: globals.insecure || config.insecure.unwrap_or(false),
            no_sandbox: globals.no_sandbox || config.no_sandbox.unwrap_or(false),
            request_timeout: None,
        },
        open: OpenOptions::default(),
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
/// warning, not a reason to lose a completed run's results.
async fn notify_runs(settings: &Settings, runs: &[history::RunResult]) {
    let Some(url) = settings.webhook_url.as_deref() else {
        return;
    };
    let mut regressions = match notify::regressions_from_history(&settings.history_dir, runs) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = format!("{e:#}"), "could not compute regressions");
            return;
        }
    };
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
/// run's results.
async fn github_issues_for_runs(settings: &Settings, runs: &[history::RunResult]) {
    let Some(gh) = &settings.github_issues else {
        return;
    };
    let mut events = match issues::events_from_history(&settings.history_dir, runs) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                error = format!("{e:#}"),
                "could not compute GitHub issue events"
            );
            return;
        }
    };
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

/// Renders `runs` as JUnit XML or SARIF and writes it to `out` (stdout when
/// none) — the `--junit`/`--sarif` output for `test`/`monitor`. Baseline-aware.
fn emit_structured(
    runs: &[history::RunResult],
    junit: bool,
    sarif: bool,
    out: Option<PathBuf>,
    baseline: Option<&Baseline>,
) -> Result<()> {
    let body = match (junit, sarif) {
        (true, _) => export::junit_xml(runs, baseline),
        (false, true) => serde_json::to_string_pretty(&export::sarif_json(runs, baseline))?,
        (false, false) => String::new(),
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

/// Prints one run (JSON or colored text, plus the diff against its
/// previous run) and reports whether it breaches the `--fail-on` policy —
/// the caller aggregates that into the process exit code.
fn print_run(
    history_dir: &Path,
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
    let prior = history::load_runs(history_dir, &run.url)?;
    let flaky = history::flakiness(&prior);
    for check in &run.checks {
        let baselined = check.status != checks::Status::Pass
            && baseline
                .map(|b| b.is_accepted(&run.url, &check.name, check.status))
                .unwrap_or(false);
        print_check(check, is_flaky(&flaky, check), baselined);
    }

    if let Some(prev) = prior.iter().rev().find(|r| r.timestamp < run.timestamp) {
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
}
