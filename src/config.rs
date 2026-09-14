//! Configuration file and environment-variable support.
//!
//! Precedence is: **explicit CLI flag > environment variable >
//! configuration file > built-in default**. This module owns the file and
//! environment layers; `main` applies the CLI on top.
//!
//! A config file is looked for at `--config PATH`, then `./formwatch.yml`
//! (or `.yaml`), then `$XDG_CONFIG_HOME/formwatch/config.yml` (via
//! [`dirs::config_dir`]). Every field is optional, so a partial file is
//! fine and an absent file is equivalent to an empty one.
//!
//! ```yaml
//! history_dir: .formwatch/history
//! wait: 5
//! max_concurrent: 4
//! per_host_delay_ms: 1000
//! screenshots: true
//! proxy: http://proxy.internal:8080
//! notify:
//!   webhook_url: https://hooks.slack.com/services/...
//!   on: regression
//! ```

use crate::error::{Error, Result};
use crate::llm::provider::Provider;
use crate::shard::Shard;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// What event should trigger a webhook notification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NotifyOn {
    /// Notify on every run.
    Always,
    /// Notify only when a check regresses to `Fail`. The default.
    Regression,
}

/// Which check status should make the process exit non-zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "lowercase")]
pub enum FailOn {
    /// Exit 1 on any `FAIL` check (the default).
    #[default]
    Fail,
    /// Exit 1 on any `FAIL` *or* `WARN` check — stricter, for teams that
    /// want warnings to be actionable too.
    Warn,
}

/// Webhook notification settings.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct NotifyConfig {
    /// Where to POST. Slack incoming webhooks and generic JSON endpoints
    /// are both supported.
    pub webhook_url: Option<String>,
    /// When to notify; `None` means [`NotifyOn::Regression`].
    pub on: Option<NotifyOn>,
}

/// Optional LLM semantic-check settings (`llm:` in the config file).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct LlmConfig {
    /// Whether the semantic checks run.
    pub enabled: Option<bool>,
    /// Backend.
    pub provider: Option<Provider>,
    /// Model name.
    pub model: Option<String>,
    /// API key (prefer the provider's environment variable).
    pub api_key: Option<String>,
    /// Provider API root override (for OpenAI-compatible endpoints).
    pub base_url: Option<String>,
    /// Per-request timeout, in seconds.
    pub timeout_secs: Option<u64>,
    /// Retries for transient provider failures.
    pub max_retries: Option<u32>,
    /// Maximum characters of page text sent in one prompt.
    pub max_input_chars: Option<usize>,
    /// Minimum passing score (1-5).
    pub threshold: Option<u8>,
    /// Whether a below-threshold score is a `Fail` rather than a `Warn`.
    pub fail: Option<bool>,
    /// Whether to redact obvious PII before sending.
    pub redact: Option<bool>,
    /// Whether to cache verdicts on disk.
    pub cache: Option<bool>,
    /// Cache directory override.
    pub cache_dir: Option<PathBuf>,
}

/// The full configuration file schema. All fields optional.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    /// Where run history is stored/read.
    pub history_dir: Option<PathBuf>,
    /// Directory of custom `*.js` checks.
    pub checks_dir: Option<PathBuf>,
    /// Seconds to idle before the input-persistence check.
    pub wait: Option<u64>,
    /// Default for whether to click the real submit (still gated by
    /// authorized-use acknowledgement).
    pub submit: Option<bool>,
    /// Default for showing the browser window.
    pub headful: Option<bool>,
    /// Milliseconds between starting each form in `monitor`.
    pub delay_ms: Option<u64>,
    /// Milliseconds between starting requests to the *same* host.
    pub per_host_delay_ms: Option<u64>,
    /// Maximum concurrent forms in `monitor`.
    pub max_concurrent: Option<usize>,
    /// Deterministic shard selector for `monitor`, e.g. `2/5`.
    pub shard: Option<Shard>,
    /// Base per-check timeout, in seconds.
    pub check_timeout_secs: Option<u64>,
    /// Whether non-Pass checks capture screenshots (`false` for
    /// privacy-sensitive deployments).
    pub screenshots: Option<bool>,
    /// HTTP(S) proxy URL, passed to Chrome.
    pub proxy: Option<String>,
    /// Ignore TLS certificate errors (self-signed proxies, test hosts).
    pub insecure: Option<bool>,
    /// Launch Chrome with `--no-sandbox` (containers/CI).
    pub no_sandbox: Option<bool>,
    /// Path to append an audit-log JSONL file to.
    pub audit_log: Option<PathBuf>,
    /// Path to a baseline of accepted findings.
    pub baseline: Option<PathBuf>,
    /// Address for `formwatch serve`, e.g. `127.0.0.1:8080`.
    pub serve_addr: Option<String>,
    /// Default retention for `formwatch prune`: keep the last N runs.
    pub keep_last: Option<usize>,
    /// Default retention for `formwatch prune`: keep runs newer than N days.
    pub keep_days: Option<u64>,
    /// Record acknowledgment of the authorized-use notice.
    pub accept_terms: Option<bool>,
    /// Which status makes the process exit non-zero (`fail` or `warn`).
    pub fail_on: Option<FailOn>,
    /// Webhook notification settings.
    pub notify: Option<NotifyConfig>,
    /// Optional LLM semantic-check settings.
    pub llm: Option<LlmConfig>,
}

/// Reads a boolean-ish environment value.
fn env_bool(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Option<bool>> {
    let Some(raw) = get(key) else { return Ok(None) };
    match raw.trim().to_ascii_lowercase().as_str() {
        "" => Ok(None),
        "1" | "true" | "yes" | "on" => Ok(Some(true)),
        "0" | "false" | "no" | "off" => Ok(Some(false)),
        other => Err(Error::Config(format!(
            "{key}: expected a boolean, got {other:?}"
        ))),
    }
}

/// Reads and parses an environment value.
fn env_parse<T>(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Option<T>>
where
    T: std::str::FromStr,
    T::Err: std::fmt::Display,
{
    let Some(raw) = get(key) else { return Ok(None) };
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    trimmed
        .parse::<T>()
        .map(Some)
        .map_err(|e| Error::Config(format!("{key}: {e}")))
}

/// Reads a string-ish environment value (empty means unset).
fn env_string(get: &impl Fn(&str) -> Option<String>, key: &str) -> Option<String> {
    get(key).filter(|v| !v.trim().is_empty())
}

impl Config {
    /// Applies `FORMWATCH_*` environment variables over the file values.
    pub fn apply_env(&mut self) -> Result<()> {
        self.apply_env_with(|k| std::env::var(k).ok())
    }

    /// [`apply_env`](Self::apply_env) against an arbitrary lookup, so the
    /// precedence logic can be tested without mutating process-global
    /// environment state.
    pub fn apply_env_with(&mut self, get: impl Fn(&str) -> Option<String>) -> Result<()> {
        macro_rules! set {
            ($field:ident, $key:literal, $parser:ident) => {
                if let Some(v) = $parser(&get, $key)? {
                    self.$field = Some(v);
                }
            };
        }
        set!(history_dir, "FORMWATCH_HISTORY_DIR", env_path);
        set!(checks_dir, "FORMWATCH_CHECKS_DIR", env_path);
        set!(wait, "FORMWATCH_WAIT", env_parse);
        set!(submit, "FORMWATCH_SUBMIT", env_bool);
        set!(headful, "FORMWATCH_HEADFUL", env_bool);
        set!(delay_ms, "FORMWATCH_DELAY_MS", env_parse);
        set!(per_host_delay_ms, "FORMWATCH_PER_HOST_DELAY_MS", env_parse);
        set!(max_concurrent, "FORMWATCH_MAX_CONCURRENT", env_parse);
        set!(shard, "FORMWATCH_SHARD", env_parse);
        set!(
            check_timeout_secs,
            "FORMWATCH_CHECK_TIMEOUT_SECS",
            env_parse
        );
        set!(screenshots, "FORMWATCH_SCREENSHOTS", env_bool);
        set!(proxy, "FORMWATCH_PROXY", env_parse);
        set!(insecure, "FORMWATCH_INSECURE", env_bool);
        set!(no_sandbox, "FORMWATCH_NO_SANDBOX", env_bool);
        set!(audit_log, "FORMWATCH_AUDIT_LOG", env_path);
        set!(baseline, "FORMWATCH_BASELINE", env_path);
        set!(serve_addr, "FORMWATCH_SERVE_ADDR", env_parse);
        set!(keep_last, "FORMWATCH_KEEP_LAST", env_parse);
        set!(keep_days, "FORMWATCH_KEEP_DAYS", env_parse);
        set!(accept_terms, "FORMWATCH_ACCEPT_TERMS", env_bool);
        set!(fail_on, "FORMWATCH_FAIL_ON", env_fail_on);

        if let Some(url) = env_string(&get, "FORMWATCH_WEBHOOK_URL") {
            self.notify
                .get_or_insert_with(NotifyConfig::default)
                .webhook_url = Some(url);
        }
        if let Some(on) = env_string(&get, "FORMWATCH_WEBHOOK_ON") {
            let parsed = match on.trim().to_ascii_lowercase().as_str() {
                "always" => NotifyOn::Always,
                "regression" => NotifyOn::Regression,
                other => {
                    return Err(Error::Config(format!(
                        "FORMWATCH_WEBHOOK_ON: expected 'always' or 'regression', got {other:?}"
                    )));
                }
            };
            self.notify.get_or_insert_with(NotifyConfig::default).on = Some(parsed);
        }

        macro_rules! llm_set {
            ($field:ident, $key:literal, $parser:ident) => {
                if let Some(v) = $parser(&get, $key)? {
                    self.llm.get_or_insert_with(LlmConfig::default).$field = Some(v);
                }
            };
        }
        llm_set!(enabled, "FORMWATCH_LLM", env_bool);
        llm_set!(model, "FORMWATCH_LLM_MODEL", env_parse);
        llm_set!(api_key, "FORMWATCH_LLM_API_KEY", env_parse);
        llm_set!(base_url, "FORMWATCH_LLM_BASE_URL", env_parse);
        llm_set!(timeout_secs, "FORMWATCH_LLM_TIMEOUT_SECS", env_parse);
        llm_set!(max_retries, "FORMWATCH_LLM_MAX_RETRIES", env_parse);
        llm_set!(max_input_chars, "FORMWATCH_LLM_MAX_INPUT_CHARS", env_parse);
        llm_set!(threshold, "FORMWATCH_LLM_THRESHOLD", env_parse);
        llm_set!(fail, "FORMWATCH_LLM_FAIL", env_bool);
        llm_set!(redact, "FORMWATCH_LLM_REDACT", env_bool);
        llm_set!(cache, "FORMWATCH_LLM_CACHE", env_bool);
        llm_set!(cache_dir, "FORMWATCH_LLM_CACHE_DIR", env_path);
        if let Some(provider) = env_string(&get, "FORMWATCH_LLM_PROVIDER") {
            let parsed = provider.parse::<Provider>().map_err(Error::Config)?;
            self.llm.get_or_insert_with(LlmConfig::default).provider = Some(parsed);
        }
        Ok(())
    }

    /// Which config file to load: an explicit path if given, else the
    /// first of `./formwatch.yml`, `./formwatch.yaml`, or the user config
    /// dir.
    pub fn resolve_path(explicit: Option<&Path>) -> Option<PathBuf> {
        if let Some(path) = explicit {
            return Some(path.to_path_buf());
        }
        for candidate in ["formwatch.yml", "formwatch.yaml"] {
            let path = PathBuf::from(candidate);
            if path.is_file() {
                return Some(path);
            }
        }
        if let Some(dir) = dirs::config_dir() {
            for name in ["config.yml", "config.yaml", "formwatch.yml"] {
                let path = dir.join("formwatch").join(name);
                if path.is_file() {
                    return Some(path);
                }
            }
        }
        None
    }

    /// Loads configuration. An explicitly requested file that doesn't
    /// exist is an error (a typo shouldn't silently change behavior); an
    /// absent auto-discovered file is just an empty config.
    pub fn load(explicit: Option<&Path>) -> Result<Config> {
        let Some(path) = Self::resolve_path(explicit) else {
            return Ok(Config::default());
        };
        if !path.exists() {
            if explicit.is_some() {
                return Err(Error::Config(format!(
                    "config file {} does not exist",
                    path.display()
                )));
            }
            return Ok(Config::default());
        }
        let text = std::fs::read_to_string(&path)?;
        let mut config: Config = serde_yaml::from_str(&text)
            .map_err(|e| Error::Config(format!("parsing {}: {e}", path.display())))?;
        config.apply_env()?;
        Ok(config)
    }
}

/// Environment path values: empty means unset.
fn env_path(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Option<PathBuf>> {
    Ok(env_string(get, key).map(PathBuf::from))
}

/// Environment values for the fail-on policy.
fn env_fail_on(get: &impl Fn(&str) -> Option<String>, key: &str) -> Result<Option<FailOn>> {
    let Some(raw) = env_string(get, key) else {
        return Ok(None);
    };
    match raw.trim().to_ascii_lowercase().as_str() {
        "fail" => Ok(Some(FailOn::Fail)),
        "warn" => Ok(Some(FailOn::Warn)),
        other => Err(Error::Config(format!(
            "{key}: expected 'fail' or 'warn', got {other:?}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_partial_config_file() {
        let cfg: Config = serde_yaml::from_str(
            "wait: 12\nmax_concurrent: 2\nnotify:\n  webhook_url: https://example.com/hook\n  on: always\n",
        )
        .expect("parse");
        assert_eq!(cfg.wait, Some(12));
        assert_eq!(cfg.max_concurrent, Some(2));
        assert_eq!(
            cfg.notify.as_ref().and_then(|n| n.on),
            Some(NotifyOn::Always)
        );
        assert!(cfg.history_dir.is_none());
    }

    #[test]
    fn env_overrides_the_file_values() {
        let mut cfg: Config = serde_yaml::from_str("wait: 1\nmax_concurrent: 4\n").expect("parse");
        let fake = |key: &str| -> Option<String> {
            match key {
                "FORMWATCH_WAIT" => Some("99".to_string()),
                "FORMWATCH_SCREENSHOTS" => Some("false".to_string()),
                "FORMWATCH_WEBHOOK_URL" => Some("https://example.com/hook".to_string()),
                _ => None,
            }
        };
        cfg.apply_env_with(fake).expect("apply env");
        assert_eq!(cfg.wait, Some(99), "env should override the file");
        assert_eq!(cfg.screenshots, Some(false));
        assert_eq!(
            cfg.max_concurrent,
            Some(4),
            "unset env keeps the file value"
        );
        assert_eq!(
            cfg.notify.and_then(|n| n.webhook_url).as_deref(),
            Some("https://example.com/hook")
        );
    }

    #[test]
    fn empty_env_value_is_treated_as_unset() {
        let mut cfg = Config::default();
        cfg.apply_env_with(|_| Some("  ".to_string()))
            .expect("apply");
        assert!(cfg.history_dir.is_none());
        assert!(cfg.wait.is_none());
    }

    #[test]
    fn an_invalid_boolean_env_value_is_a_loud_error() {
        let mut cfg = Config::default();
        let err = cfg
            .apply_env_with(|k| (k == "FORMWATCH_SUBMIT").then(|| "maybe".to_string()))
            .expect_err("should reject");
        assert!(err.to_string().contains("FORMWATCH_SUBMIT"));
    }

    #[test]
    fn an_explicit_missing_config_file_is_an_error_but_autodiscovery_is_not() {
        let missing = std::env::temp_dir().join("formwatch-no-such-config.yml");
        let err = Config::load(Some(&missing)).expect_err("explicit missing should error");
        assert!(err.to_string().contains("does not exist"));
    }
}
