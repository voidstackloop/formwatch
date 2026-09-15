//! LLM-powered semantic checks.
//!
//! The built-in checks are deliberately heuristic: they can tell that a
//! required field has *an* error message and an `aria-describedby`, but
//! not whether that message is any good. Judging wording — is "Invalid
//! input" clear? does the instructions section actually say what to
//! upload? — needs language understanding. This module adds two optional
//! checks that ask an LLM to score wording clarity:
//!
//! - **Error wording (LLM)** — judges the validation messages on the page.
//! - **Instructions (LLM)** — judges instructions and required-document
//!   guidance.
//!
//! Design constraints:
//!
//! - **Off by default.** Nothing is sent anywhere unless the operator
//!   enables it (`--llm`, config, or env).
//! - **Never fatal.** A missing API key, a provider error, a timeout, or
//!   an unparseable reply all degrade to a single `Warn`, never a crash
//!   or a dropped run.
//! - **Privacy-aware.** Page text is redacted (best-effort) and
//!   truncated before it leaves the machine, and verdicts are cached on
//!   disk so unchanged pages aren't re-sent.
//!
//! See [`prompt`] for the request/response contract, [`provider`] for the
//! backends, [`redact`] for the scrubbing, and [`cache`] for the cache.

pub mod cache;
pub mod prompt;
pub mod provider;
pub mod redact;

use crate::checks::{self, CheckResult, Status};
use crate::error::{Error, Result};
use crate::llm::prompt::Verdict;
use crate::llm::provider::{Provider, build_request, parse_response, parse_usage};
use chromiumoxide::Page;
use std::path::PathBuf;
use std::time::Duration;

/// Everything needed to run the semantic checks.
#[derive(Clone)]
pub struct LlmOptions {
    /// Whether the checks run at all.
    pub enabled: bool,
    /// Which backend to call.
    pub provider: Provider,
    /// Model name.
    pub model: String,
    /// Explicit API key. If `None`, the provider's environment variables
    /// are consulted (see [`Provider::api_key_envs`]).
    pub api_key: Option<String>,
    /// Override for the provider's API root (OpenAI-compatible endpoints).
    pub base_url: Option<String>,
    /// Per-request timeout.
    pub timeout: Duration,
    /// How many times to retry a transient provider failure (HTTP 429,
    /// 5xx, or a network error). `0` disables retries.
    pub max_retries: u32,
    /// Maximum characters of page text sent in one prompt.
    pub max_input_chars: usize,
    /// Minimum score (1-5) considered a pass.
    pub threshold: u8,
    /// Whether a below-threshold score is a `Fail` (default: `Warn`).
    pub fail: bool,
    /// Whether to redact obvious PII before sending (default: true).
    pub redact: bool,
    /// Whether to cache verdicts on disk (default: true).
    pub cache: bool,
    /// Cache directory override.
    pub cache_dir: Option<PathBuf>,
}

/// Hand-written so the API key can never be printed by a `{:?}` (a debug
/// log line, an error context, a panic message). The derive would have
/// exposed it verbatim.
impl std::fmt::Debug for LlmOptions {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LlmOptions")
            .field("enabled", &self.enabled)
            .field("provider", &self.provider)
            .field("model", &self.model)
            .field("api_key", &self.api_key.as_ref().map(|_| "<redacted>"))
            .field("base_url", &self.base_url)
            .field("timeout", &self.timeout)
            .field("max_retries", &self.max_retries)
            .field("max_input_chars", &self.max_input_chars)
            .field("threshold", &self.threshold)
            .field("fail", &self.fail)
            .field("redact", &self.redact)
            .field("cache", &self.cache)
            .field("cache_dir", &self.cache_dir)
            .finish()
    }
}

impl Default for LlmOptions {
    fn default() -> Self {
        let provider = Provider::default();
        Self {
            enabled: false,
            provider,
            model: provider.default_model().to_string(),
            api_key: None,
            base_url: None,
            timeout: Duration::from_secs(30),
            max_retries: 2,
            max_input_chars: 6000,
            threshold: 3,
            fail: false,
            redact: true,
            cache: true,
            cache_dir: None,
        }
    }
}

impl LlmOptions {
    /// The base URL to use, falling back to the provider default.
    pub fn resolved_base_url(&self) -> String {
        self.base_url
            .clone()
            .unwrap_or_else(|| self.provider.default_base_url().to_string())
    }

    /// The API key to use, falling back to the provider's env vars.
    pub fn resolved_api_key(&self) -> Option<String> {
        self.api_key
            .clone()
            .filter(|k| !k.trim().is_empty())
            .or_else(|| {
                self.provider
                    .api_key_envs()
                    .iter()
                    .find_map(|key| std::env::var(key).ok())
                    .filter(|v| !v.trim().is_empty())
            })
    }

    fn resolved_cache_dir(&self) -> Option<PathBuf> {
        if !self.cache {
            return None;
        }
        self.cache_dir.clone().or_else(cache::Cache::default_dir)
    }
}

/// The canned reply the mock provider returns — a valid, mid-high verdict.
const MOCK_RESPONSE: &str = r#"{"score": 4, "issues": ["Name the specific field in each message."], "summary": "Messages are generally clear and specific."}"#;

/// A minimal HTTP client over one provider.
pub struct LlmClient {
    http: reqwest::Client,
    options: LlmOptions,
    cache: cache::Cache,
}

impl LlmClient {
    /// Builds a client for `options`.
    pub fn new(options: &LlmOptions) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(options.timeout)
            .build()
            .map_err(|e| Error::Llm(format!("building HTTP client: {e}")))?;
        Ok(Self {
            http,
            options: options.clone(),
            cache: cache::Cache::new(options.resolved_cache_dir()),
        })
    }

    /// Sends one system/user exchange and returns the assistant's raw text.
    async fn complete(&self, system: &str, user: &str) -> Result<String> {
        let mut attempt = 0u32;
        loop {
            match self.complete_once(system, user).await {
                Ok(text) => return Ok(text),
                Err(failure) => {
                    if !failure.retryable || attempt >= self.options.max_retries {
                        return Err(Error::Llm(failure.message));
                    }
                    let delay = retry_delay(attempt, failure.retry_after);
                    tracing::warn!(
                        attempt = attempt + 1,
                        max = self.options.max_retries,
                        delay_ms = delay.as_millis() as u64,
                        "LLM request failed transiently; retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// One attempt. Failures carry whether they're worth retrying and any
    /// server-requested delay.
    async fn complete_once(
        &self,
        system: &str,
        user: &str,
    ) -> std::result::Result<String, LlmFailure> {
        if self.options.provider == Provider::Mock {
            return Ok(MOCK_RESPONSE.to_string());
        }
        let api_key = self.options.resolved_api_key().ok_or_else(|| {
            LlmFailure::permanent(format!(
                "no API key for {} (set {})",
                self.options.provider.label(),
                self.options.provider.api_key_envs().join(" or ")
            ))
        })?;
        let request = build_request(
            self.options.provider,
            &self.options.resolved_base_url(),
            &api_key,
            &self.options.model,
            system,
            user,
        )
        .map_err(LlmFailure::permanent)?;

        let mut builder = self.http.post(&request.url);
        for (name, value) in &request.headers {
            builder = builder.header(name, value);
        }
        let response = builder
            .json(&request.body)
            .send()
            .await
            .map_err(|e| LlmFailure::retryable(format!("request to provider failed: {e}")))?;
        let status = response.status();
        let retry_after = response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        // Read the body as text *before* branching on status: an error
        // response (429/5xx) very often carries an HTML or plain-text body
        // from a gateway or proxy, and parsing that as JSON first turned a
        // perfectly retryable status into a permanent "non-JSON body"
        // failure — so the documented retry on 429/5xx never happened.
        let raw = response
            .text()
            .await
            .map_err(|e| LlmFailure::retryable(format!("reading provider response failed: {e}")))?;
        if !status.is_success() {
            let message = format!(
                "provider returned HTTP {status}: {}",
                truncate_chars(&raw, 200)
            );
            return Err(if status_is_retryable(status.as_u16()) {
                LlmFailure {
                    message,
                    retryable: true,
                    retry_after,
                }
            } else {
                LlmFailure::permanent(message)
            });
        }
        let body: serde_json::Value = serde_json::from_str(&raw)
            .map_err(|e| LlmFailure::permanent(format!("provider returned non-JSON body: {e}")))?;
        if let Some(tokens) = parse_usage(self.options.provider, &body) {
            tracing::debug!(
                provider = self.options.provider.label(),
                model = %self.options.model,
                tokens,
                "LLM request completed"
            );
        }
        parse_response(self.options.provider, &body).ok_or_else(|| {
            LlmFailure::permanent("could not find assistant text in provider response")
        })
    }

    /// Returns a verdict for `check`, using the cache when possible.
    pub async fn judge(&self, check: &str, system: &str, user: &str) -> Result<Verdict> {
        let key = cache::cache_key(
            self.options.provider.label(),
            &self.options.resolved_base_url(),
            &self.options.model,
            check,
            user,
        );
        if let Some(verdict) = self.cache.get(&key) {
            tracing::debug!(check, "LLM verdict cache hit");
            return Ok(verdict);
        }
        let raw = self.complete(system, user).await?;
        let verdict = prompt::parse_verdict(&raw).ok_or_else(|| {
            Error::Llm(format!(
                "could not parse a verdict from the model reply: {}",
                truncate_chars(&raw, 200)
            ))
        })?;
        self.cache.put(&key, &verdict);
        Ok(verdict)
    }
}

/// An internal failure from one provider attempt, carrying whether the
/// caller should retry it.
struct LlmFailure {
    message: String,
    retryable: bool,
    retry_after: Option<Duration>,
}

impl LlmFailure {
    fn permanent(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: false,
            retry_after: None,
        }
    }

    fn retryable(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            retryable: true,
            retry_after: None,
        }
    }
}

/// Whether an HTTP status is worth retrying: rate limiting and server
/// errors are, everything else (auth, bad request, ...) is not.
fn status_is_retryable(code: u16) -> bool {
    code == 429 || (500..600).contains(&code)
}

/// Parses a `Retry-After` header (seconds form), capped so a hostile or
/// broken server can't stall a run indefinitely.
fn parse_retry_after(value: &str) -> Option<Duration> {
    value
        .trim()
        .parse::<u64>()
        .ok()
        .map(|secs| Duration::from_secs(secs.min(60)))
}

/// Backoff before the next attempt: the server's `Retry-After` when given
/// (already capped to 60s by [`parse_retry_after`]), otherwise 500ms, 1s,
/// 2s, ... capped at 8s.
fn retry_delay(attempt: u32, retry_after: Option<Duration>) -> Duration {
    const BASE_MS: u64 = 500;
    const CAP: Duration = Duration::from_secs(8);
    if let Some(after) = retry_after {
        return after;
    }
    Duration::from_millis(BASE_MS.saturating_mul(2u64.saturating_pow(attempt))).min(CAP)
}

/// Sends one tiny probe request to verify the provider, credentials, and
/// network path work — used by `formwatch doctor --llm`.
pub async fn probe(options: &LlmOptions) -> Result<String> {
    let mut probe_options = options.clone();
    probe_options.enabled = true;
    // A probe should always exercise the network; never serve a cached
    // verdict or write one.
    probe_options.cache = false;
    let client = LlmClient::new(&probe_options)?;
    let verdict = client
        .judge(
            "probe",
            "You are a connectivity probe. Reply only with the requested JSON.",
            "Reply with exactly: {\"score\": 5, \"issues\": [], \"summary\": \"ok\"}",
        )
        .await?;
    Ok(format!(
        "{} model {} responded (probe score {})",
        probe_options.provider.label(),
        probe_options.model,
        verdict.score
    ))
}

struct CheckKind {
    name: &'static str,
    system: &'static str,
    extract_js: &'static str,
    build_user: fn(&str) -> String,
    empty_detail: &'static str,
}

const ERROR_KIND: CheckKind = CheckKind {
    name: "Error wording (LLM)",
    system: prompt::ERROR_SYSTEM,
    extract_js: ERROR_TEXT_JS,
    build_user: prompt::error_user_prompt,
    empty_detail: "No validation error messages found to review.",
};

const INSTRUCTION_KIND: CheckKind = CheckKind {
    name: "Instructions (LLM)",
    system: prompt::INSTRUCTIONS_SYSTEM,
    extract_js: INSTRUCTION_TEXT_JS,
    build_user: prompt::instructions_user_prompt,
    empty_detail: "No instruction or help text found to review.",
};

/// Collects validation error wording: native `validationMessage`s plus the
/// text of conventional error regions. Pierces open shadow roots.
const ERROR_TEXT_JS: &str = "(() => {
    const deep = (root, sel) => { const out=[]; const visit=(n)=>{ if(n.matches&&n.matches(sel)) out.push(n); if(n.shadowRoot) for(const c of n.shadowRoot.children) visit(c); for(const c of n.children) visit(c); }; visit(root); return out; };
    const out = []; const seen = new Set();
    const push = (t) => { t=(t||'').replace(/\\s+/g,' ').trim(); if(t && t.length<=400 && !seen.has(t)){seen.add(t); out.push(t);} };
    for (const el of deep(document, 'input, select, textarea')) {
        try { if (el.willValidate && !el.checkValidity() && el.validationMessage) push(el.validationMessage); } catch(e) {}
    }
    for (const el of deep(document, '[role=alert], [aria-live], [aria-invalid=true], .error, .invalid, [class*=error i], [id*=error i]')) {
        push(el.innerText || el.textContent);
    }
    return out.slice(0, 40);
})()";

/// Collects instruction/help text: labels, legends, hints, and prose that
/// looks like guidance. Falls back to the page's visible text.
const INSTRUCTION_TEXT_JS: &str = "(() => {
    const deep = (root, sel) => { const out=[]; const visit=(n)=>{ if(n.matches&&n.matches(sel)) out.push(n); if(n.shadowRoot) for(const c of n.shadowRoot.children) visit(c); for(const c of n.children) visit(c); }; visit(root); return out; };
    const parts = [];
    const accept = (t) => t && t.length > 20 && /required|must|upload|attach|document|submit|provide|enter|format|size|deadline|eligib/i.test(t);
    for (const el of deep(document, 'label, legend, p, li, small, [class*=hint i], [class*=help i], [class*=instruction i]')) {
        const t = (el.innerText || el.textContent || '').replace(/\\s+/g,' ').trim();
        if (accept(t)) parts.push(t);
    }
    if (parts.length === 0) parts.push((document.body.innerText || '').replace(/\\s+/g,' ').trim());
    return parts.slice(0, 60).join('\\n');
})()";

fn result(name: &str, status: Status, detail: impl Into<String>) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status,
        detail: detail.into(),
        screenshot: None,
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        text.to_string()
    } else {
        let mut shortened: String = text.chars().take(max).collect();
        shortened.push_str(" …[truncated]");
        shortened
    }
}

fn detail_for(verdict: &Verdict) -> String {
    let mut detail = format!("score {}/5. {}", verdict.score, verdict.summary);
    if !verdict.issues.is_empty() {
        detail.push_str(&format!(" Issues: {}", verdict.issues.join("; ")));
    }
    detail.trim_end().to_string()
}

async fn extract(page: &Page, kind: &CheckKind) -> Result<String> {
    let value: serde_json::Value = page
        .evaluate(kind.extract_js)
        .await
        .map_err(|e| Error::Llm(format!("running the extraction script: {e}")))?
        .into_value()
        .map_err(|e| Error::Llm(format!("reading the extraction result: {e}")))?;
    Ok(match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    })
}

async fn attach_screenshot(page: &Page, capture: bool, mut result: CheckResult) -> CheckResult {
    if capture && result.status != Status::Pass {
        result.screenshot = checks::capture_screenshot(page).await;
    }
    result
}

async fn run_one(
    page: &Page,
    client: &LlmClient,
    options: &LlmOptions,
    capture: bool,
    kind: &CheckKind,
) -> CheckResult {
    let extracted = match extract(page, kind).await {
        Ok(text) => text,
        Err(e) => {
            let result = result(
                kind.name,
                Status::Warn,
                format!("could not read page text: {e:#}"),
            );
            return attach_screenshot(page, capture, result).await;
        }
    };
    let text = if options.redact {
        redact::redact(&extracted)
    } else {
        extracted
    };
    let text = truncate_chars(&text, options.max_input_chars);
    if text.trim().is_empty() {
        return result(kind.name, Status::Pass, kind.empty_detail);
    }

    let user = (kind.build_user)(&text);
    let judged =
        tokio::time::timeout(options.timeout, client.judge(kind.name, kind.system, &user)).await;

    let result = match judged {
        Ok(Ok(verdict)) => {
            let status = prompt::status_for(&verdict, options.threshold, options.fail);
            result(kind.name, status, detail_for(&verdict))
        }
        Ok(Err(e)) => result(
            kind.name,
            Status::Warn,
            format!("LLM check did not complete: {e:#}"),
        ),
        Err(_) => result(
            kind.name,
            Status::Warn,
            format!(
                "LLM check did not complete: timed out after {}s",
                options.timeout.as_secs()
            ),
        ),
    };
    attach_screenshot(page, capture, result).await
}

/// Runs the semantic checks against `page`, returning their results (or an
/// empty vec when the feature is disabled). A configuration that prevents
/// any call — no API key — yields a single explanatory `Warn` rather than
/// silently doing nothing.
pub async fn run_semantic_checks(
    page: &Page,
    options: &LlmOptions,
    capture: bool,
) -> Vec<CheckResult> {
    if !options.enabled {
        return Vec::new();
    }
    if options.provider.requires_api_key() && options.resolved_api_key().is_none() {
        return vec![result(
            "LLM review",
            Status::Warn,
            format!(
                "skipped: no API key for {} (set {})",
                options.provider.label(),
                options.provider.api_key_envs().join(" or ")
            ),
        )];
    }

    tracing::warn!(
        provider = options.provider.label(),
        model = %options.model,
        redacted = options.redact,
        "LLM semantic checks enabled — page text is sent to the configured provider"
    );

    let client = match LlmClient::new(options) {
        Ok(client) => client,
        Err(e) => {
            return vec![result(
                "LLM review",
                Status::Warn,
                format!("could not initialize the LLM client: {e:#}"),
            )];
        }
    };

    // The two checks are independent; run them concurrently so the LLM
    // latency is paid once, not twice.
    let (errors, instructions) = tokio::join!(
        run_one(page, &client, options, capture, &ERROR_KIND),
        run_one(page, &client, options, capture, &INSTRUCTION_KIND),
    );
    vec![errors, instructions]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        let long = "日本語".repeat(10);
        let cut = truncate_chars(&long, 5);
        assert!(cut.starts_with("日本語"));
        assert!(cut.contains("truncated"));
    }

    #[test]
    fn mock_has_no_api_key_requirement_and_the_others_do() {
        assert!(!Provider::Mock.requires_api_key());
        assert!(Provider::OpenAi.requires_api_key());
        assert!(Provider::Anthropic.requires_api_key());
    }

    #[test]
    fn only_rate_limit_and_server_errors_are_retryable() {
        assert!(status_is_retryable(429));
        assert!(status_is_retryable(500));
        assert!(status_is_retryable(503));
        assert!(!status_is_retryable(400));
        assert!(!status_is_retryable(401));
        assert!(!status_is_retryable(404));
    }

    #[test]
    fn retry_after_is_parsed_and_capped() {
        assert_eq!(parse_retry_after("2"), Some(Duration::from_secs(2)));
        assert_eq!(parse_retry_after(" 10 "), Some(Duration::from_secs(10)));
        // A server asking for an hour is capped to a minute.
        assert_eq!(parse_retry_after("3600"), Some(Duration::from_secs(60)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
    }

    #[test]
    fn backoff_doubles_and_prefers_the_server_delay() {
        assert_eq!(retry_delay(0, None), Duration::from_millis(500));
        assert_eq!(retry_delay(1, None), Duration::from_millis(1000));
        assert_eq!(retry_delay(4, None), Duration::from_secs(8)); // capped
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(3))),
            Duration::from_secs(3)
        );
    }

    #[tokio::test]
    async fn mock_provider_returns_a_parseable_verdict_without_network() {
        let options = LlmOptions {
            enabled: true,
            provider: Provider::Mock,
            cache: false,
            ..LlmOptions::default()
        };
        let client = LlmClient::new(&options).expect("client");
        let verdict = client
            .judge("Error wording (LLM)", "sys", "user prompt")
            .await
            .expect("judge");
        assert!(verdict.score >= 3);
        assert!(!verdict.summary.is_empty());
    }
}
