//! Passive form-vulnerability detection.
//!
//! These checks look for common, high-signal web-form vulnerabilities
//! **without sending any attack payloads**: no SQL-injection probes, no
//! XSS payloads, no fuzzing, no forced browsing. Everything here is
//! observable from the page's own DOM, its URL, and its response headers,
//! so a run is safe to point at third-party public-service forms you are
//! authorized to *read* — the same posture as the rest of formwatch.
//!
//! What is covered (each finding has a stable `FW-V###` id, a CWE
//! reference, and a confidence rating):
//!
//! - `FW-V001..V004` — insecure transport: page over HTTP, form action
//!   over HTTP, passwords on an HTTP page, HTTP subresources (mixed
//!   content).
//! - `FW-V005..V008` — form data protection: sensitive fields submitted
//!   with `GET` (leaks into URL/history/logs), missing CSRF token on a
//!   POST form, sensitive fields with autocomplete left on, `target=_blank`
//!   without `rel=noopener` (tabnabbing).
//! - `FW-V009..V013,V020,V021` — response headers: missing
//!   Content-Security-Policy (plus permissive directives when present),
//!   framing protection, HSTS, X-Content-Type-Options, Referrer-Policy,
//!   and version-disclosing `Server`/`X-Powered-By` banners.
//! - `FW-V016..V018` — cookie security: missing `Secure` on HTTPS,
//!   missing `HttpOnly`, `SameSite` misconfiguration, and `__Secure`-/
//!   `__Host`-prefix violations.
//! - `FW-V014,V015,V019` — information disclosure: verbose error text,
//!   secret-looking HTML comments, and sensitive values in the page URL.
//!
//! Severity maps onto [`crate::checks::Status`] so vuln findings flow
//! through history, reports, SARIF/JUnit/CSV exports, and notifications
//! like any other check: `Critical`/`High` fail, `Medium`/`Low` warn.
//! Confidence (`high`/`medium`/`low`) tells a triager how much to trust
//! a heuristic finding — a missing CSRF token, for example, is only
//! `medium` confidence because the app may rely on `SameSite` cookies.
//!
//! Response headers come from the browser's own page load, captured over
//! CDP during [`crate::browser::open_with_capture`] (same TLS session,
//! UA, and cookies the checks ran against) and shared between the
//! header, cookie, and disclosure checks through a short-lived
//! in-process cache. A plain GET is only a fallback for pages opened
//! without capture, so a normal run costs zero extra requests per form.

use crate::browser::{CapturedCookie, CapturedDocument};
use crate::checks::{CheckResult, Status};
use anyhow::{Context, Result};
use chromiumoxide::Page;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

/// Page served over HTTP.
pub const ID_HTTP_PAGE: &str = "FW-V001";
/// Form submits to an HTTP URL.
pub const ID_HTTP_FORM_ACTION: &str = "FW-V002";
/// Password field on an HTTP page or form.
pub const ID_PASSWORD_OVER_HTTP: &str = "FW-V003";
/// HTTP subresources on an HTTPS page (mixed content).
pub const ID_MIXED_CONTENT: &str = "FW-V004";
/// Sensitive fields submitted with GET.
pub const ID_GET_WITH_SENSITIVE: &str = "FW-V005";
/// POST form without an anti-CSRF token.
pub const ID_MISSING_CSRF_TOKEN: &str = "FW-V006";
/// Sensitive fields with autocomplete left enabled.
pub const ID_SENSITIVE_AUTOCOMPLETE: &str = "FW-V007";
/// `target=_blank` without `rel=noopener`.
pub const ID_UNSAFE_BLANK_TARGET: &str = "FW-V008";
/// Missing Content-Security-Policy header.
pub const ID_MISSING_CSP: &str = "FW-V009";
/// Missing clickjacking (framing) protection.
pub const ID_MISSING_FRAMING_PROTECTION: &str = "FW-V010";
/// Missing Strict-Transport-Security header.
pub const ID_MISSING_HSTS: &str = "FW-V011";
/// Missing X-Content-Type-Options header.
pub const ID_MISSING_NOSNIFF: &str = "FW-V012";
/// Missing or unsafe Referrer-Policy header.
pub const ID_REFERRER_POLICY: &str = "FW-V013";
/// Verbose error text on the page.
pub const ID_VERBOSE_ERRORS: &str = "FW-V014";
/// Secret-looking HTML comments.
pub const ID_SENSITIVE_COMMENTS: &str = "FW-V015";
/// Cookie without `Secure` on HTTPS.
pub const ID_COOKIE_NO_SECURE: &str = "FW-V016";
/// Cookie without `HttpOnly`.
pub const ID_COOKIE_NO_HTTPONLY: &str = "FW-V017";
/// Cookie `SameSite` misconfiguration.
pub const ID_COOKIE_SAMESITE: &str = "FW-V018";
/// Sensitive values in the page URL query string.
pub const ID_SENSITIVE_QUERY: &str = "FW-V019";
/// Version-disclosing server banner headers.
pub const ID_SERVER_BANNER: &str = "FW-V020";
/// Permissive Content-Security-Policy directives.
pub const ID_WEAK_CSP: &str = "FW-V021";

/// How bad one finding is. Serialized in details so `vuln --json` and
/// reports can filter or sort on it. Parseable from the `--min-severity`
/// flag (`low`, `medium`, `high`, `critical`, case-insensitive).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Severity {
    /// Exploitable on its own (passwords sent in cleartext).
    Critical,
    /// Directly enables interception or tampering.
    High,
    /// Defense-in-depth gap worth fixing.
    Medium,
    /// Hardening note, often a one-line fix.
    Low,
}

impl Severity {
    /// Short uppercase label used in human-readable detail strings.
    pub fn label(self) -> &'static str {
        match self {
            Severity::Critical => "CRITICAL",
            Severity::High => "HIGH",
            Severity::Medium => "MEDIUM",
            Severity::Low => "LOW",
        }
    }

    /// Ordering helper: higher means worse.
    pub fn rank(self) -> u8 {
        match self {
            Severity::Low => 0,
            Severity::Medium => 1,
            Severity::High => 2,
            Severity::Critical => 3,
        }
    }

    /// Whether this severity meets a minimum bar (`--min-severity`).
    pub fn at_least(self, min: Severity) -> bool {
        self.rank() >= min.rank()
    }
}

impl std::str::FromStr for Severity {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s.trim().to_lowercase().as_str() {
            "low" => Ok(Severity::Low),
            "medium" | "med" => Ok(Severity::Medium),
            "high" => Ok(Severity::High),
            "critical" | "crit" => Ok(Severity::Critical),
            _ => Err(format!(
                "invalid severity '{s}': expected low, medium, high, or critical"
            )),
        }
    }
}

/// How much to trust one finding. Heuristic signals (a missing CSRF
/// token, a comment mentioning a "key") are `Medium`/`Low` confidence so
/// triagers can tell directly-observed facts apart from guesses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Confidence {
    /// Directly observed (a header is absent, a cookie lacks a flag).
    High,
    /// Strong heuristic with a documented alternative explanation.
    Medium,
    /// Weak signal worth a human glance, not a page.
    Low,
}

impl Confidence {
    /// Short lowercase label used in human-readable detail strings.
    pub fn label(self) -> &'static str {
        match self {
            Confidence::High => "high",
            Confidence::Medium => "medium",
            Confidence::Low => "low",
        }
    }
}

/// One passive vulnerability finding: what it is, why it matters, and
/// how to fix it. `remediation` is always a concrete one-line action so
/// a report reader never has to guess the next step. `cwe` is the primary
/// CWE identifier for cross-referencing with other scanners.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Finding {
    /// Stable id, e.g. `FW-V001`.
    pub id: &'static str,
    /// How bad it is.
    pub severity: Severity,
    /// Primary CWE, e.g. `CWE-319`.
    pub cwe: &'static str,
    /// How much to trust it.
    pub confidence: Confidence,
    /// Short human title, e.g. `Form posts to an HTTP URL`.
    pub title: String,
    /// What was observed, with counts/evidence.
    pub detail: String,
    /// Concrete fix, e.g. `Serve the page and the form action over HTTPS.`.
    pub remediation: String,
}

impl Finding {
    fn new(
        id: &'static str,
        severity: Severity,
        cwe: &'static str,
        confidence: Confidence,
        title: impl Into<String>,
        detail: impl Into<String>,
        remediation: impl Into<String>,
    ) -> Self {
        Self {
            id,
            severity,
            cwe,
            confidence,
            title: title.into(),
            detail: detail.into(),
            remediation: remediation.into(),
        }
    }

    /// Whether this finding survives a `--min-severity` filter.
    pub fn visible_at(&self, min: Option<Severity>) -> bool {
        min.map(|m| self.severity.at_least(m)).unwrap_or(true)
    }
}

/// Keeps findings at or above `min` (all of them when `min` is `None`).
pub fn filter_min(findings: &[Finding], min: Option<Severity>) -> Vec<Finding> {
    findings
        .iter()
        .filter(|f| f.visible_at(min))
        .cloned()
        .collect()
}

/// The worst finding decides the check verdict: anything `High` or above
/// is a real vulnerability (`Fail`); `Medium`/`Low` findings are hardening
/// gaps (`Warn`); no findings is a `Pass`.
pub fn verdict(findings: &[Finding]) -> Status {
    if findings
        .iter()
        .any(|f| f.severity.rank() >= Severity::High.rank())
    {
        Status::Fail
    } else if findings.is_empty() {
        Status::Pass
    } else {
        Status::Warn
    }
}

fn render(title: &str, findings: &[Finding]) -> String {
    if findings.is_empty() {
        return format!("{title}: no issues found.");
    }
    let mut out = String::new();
    for f in findings {
        out.push_str(&format!(
            "[{} {} · {} · {} confidence] {} — {}. Fix: {}\n",
            f.severity.label(),
            f.id,
            f.cwe,
            f.confidence.label(),
            f.title,
            f.detail,
            f.remediation
        ));
    }
    out.truncate(out.trim_end().len());
    out
}

fn is_http(url: &str) -> bool {
    url.trim_start_matches(|c: char| c.is_whitespace())
        .to_lowercase()
        .starts_with("http://")
}

fn is_https(url: &str) -> bool {
    url.trim_start_matches(|c: char| c.is_whitespace())
        .to_lowercase()
        .starts_with("https://")
}

/// Names a finding's evidence without dumping dozens of entries into a
/// report: up to 5 items, then an "and N more" tail.
fn summarize_names(mut names: Vec<String>) -> String {
    names.sort();
    names.dedup();
    if names.len() <= 5 {
        return names.join(", ");
    }
    let extra = names.len() - 5;
    names.truncate(5);
    format!("{} (and {extra} more)", names.join(", "))
}

/// Transport signals collected from the live page (DOM + URL only).
#[derive(Debug, Clone, Default)]
pub struct TransportSignals {
    /// The page's own URL.
    pub page_url: String,
    /// Resolved `action` attributes of forms on the page.
    pub form_actions: Vec<String>,
    /// Whether any `input[type=password]` exists.
    pub has_password: bool,
    /// Count of `http://` subresource URLs (img/script/link/iframe).
    pub http_subresources: usize,
}

/// Pure transport analysis, unit-testable without a browser.
pub fn analyze_transport(sig: &TransportSignals) -> Vec<Finding> {
    let mut out = Vec::new();
    let page_http = is_http(&sig.page_url);
    if page_http {
        out.push(Finding::new(
            ID_HTTP_PAGE,
            Severity::High,
            "CWE-319",
            Confidence::High,
            "Page served over HTTP",
            format!(
                "The form page itself loads over HTTP ({}). Form input can be read or modified in transit.",
                sig.page_url
            ),
            "Serve the page over HTTPS and redirect HTTP to HTTPS.",
        ));
    }
    let insecure_actions: Vec<&String> = sig.form_actions.iter().filter(|a| is_http(a)).collect();
    if !insecure_actions.is_empty() {
        out.push(Finding::new(
            ID_HTTP_FORM_ACTION,
            Severity::High,
            "CWE-319",
            Confidence::High,
            "Form posts to an HTTP URL",
            format!(
                "{} form(s) submit to HTTP (e.g. {}). Submissions can be intercepted or altered.",
                insecure_actions.len(),
                insecure_actions[0]
            ),
            "Point every form action at an HTTPS endpoint.",
        ));
    }
    if sig.has_password && (page_http || !insecure_actions.is_empty()) {
        out.push(Finding::new(
            ID_PASSWORD_OVER_HTTP,
            Severity::Critical,
            "CWE-523",
            Confidence::High,
            "Password field on an HTTP page or form",
            "A password field is present while the page or its form action uses HTTP. Credentials travel in cleartext.",
            "Serve the page and the form action over HTTPS before collecting passwords.",
        ));
    }
    if sig.http_subresources > 0 {
        out.push(Finding::new(
            ID_MIXED_CONTENT,
            Severity::Medium,
            "CWE-829",
            Confidence::High,
            "Mixed content on the form page",
            format!(
                "{} subresource(s) (image/script/stylesheet/frame) load over HTTP on an HTTPS page and can be tampered with.",
                sig.http_subresources
            ),
            "Load all subresources over HTTPS or from the same origin.",
        ));
    }
    out
}

/// Form-integrity signals collected from the live page.
#[derive(Debug, Clone, Default)]
pub struct FormIntegritySignals {
    /// A sensitive field (password, SSN-like, card-like, email) is
    /// submitted with `method=get`.
    pub get_with_sensitive: bool,
    /// A POST form exists without any CSRF-token-looking hidden field.
    pub post_without_token: bool,
    /// Count of sensitive fields with autocomplete left enabled.
    pub sensitive_autocomplete_on: usize,
    /// Count of `target=_blank` links/forms without `rel=noopener`.
    pub unsafe_blank_targets: usize,
}

/// Pure form-integrity analysis, unit-testable without a browser.
pub fn analyze_form_integrity(sig: &FormIntegritySignals) -> Vec<Finding> {
    let mut out = Vec::new();
    if sig.get_with_sensitive {
        out.push(Finding::new(
            ID_GET_WITH_SENSITIVE,
            Severity::Medium,
            "CWE-598",
            Confidence::High,
            "Sensitive fields submitted with GET",
            "A form carrying a password or personal-data field uses method=GET, leaking values into URLs, history, and server logs.",
            "Use method=POST over HTTPS for forms carrying sensitive fields.",
        ));
    }
    if sig.post_without_token {
        out.push(Finding::new(
            ID_MISSING_CSRF_TOKEN,
            Severity::Low,
            "CWE-352",
            Confidence::Medium,
            "POST form without an anti-CSRF token",
            "A POST form has no hidden CSRF-token-looking field. The app may rely on SameSite cookies or double-submit cookies instead — worth confirming.",
            "Add a per-session CSRF token to state-changing forms, or document the alternative protection.",
        ));
    }
    if sig.sensitive_autocomplete_on > 0 {
        out.push(Finding::new(
            ID_SENSITIVE_AUTOCOMPLETE,
            Severity::Low,
            "CWE-200",
            Confidence::Medium,
            "Sensitive fields allow autocomplete",
            format!(
                "{} sensitive field(s) (password/SSN/card) leave autocomplete enabled, keeping values in the browser store on shared machines.",
                sig.sensitive_autocomplete_on
            ),
            "Set autocomplete=\"off\" (or \"new-password\" for credentials) on sensitive fields.",
        ));
    }
    if sig.unsafe_blank_targets > 0 {
        out.push(Finding::new(
            ID_UNSAFE_BLANK_TARGET,
            Severity::Low,
            "CWE-1022",
            Confidence::High,
            "New-tab link without noopener",
            format!(
                "{} link(s)/form(s) use target=_blank without rel=noopener, letting the opened page reach back via window.opener (tabnabbing).",
                sig.unsafe_blank_targets
            ),
            "Add rel=\"noopener\" to every target=_blank link and form.",
        ));
    }
    out
}

/// Splits a Content-Security-Policy value into `(directive, sources)`
/// pairs, lowercased. Malformed segments are skipped, never fatal.
fn parse_csp_directives(csp: &str) -> Vec<(String, Vec<String>)> {
    csp.split(';')
        .filter_map(|segment| {
            let mut tokens = segment.split_whitespace();
            let name = tokens.next()?.trim().to_lowercase();
            if name.is_empty() {
                return None;
            }
            Some((name, tokens.map(|t| t.trim().to_lowercase()).collect()))
        })
        .collect()
}

/// Pure security-header analysis over lowercased header name -> value
/// pairs. `is_https_page` gates HSTS (it is meaningless over HTTP).
/// Unit-testable without a browser.
pub fn analyze_headers(headers: &HashMap<String, String>, is_https_page: bool) -> Vec<Finding> {
    let mut out = Vec::new();
    let get = |name: &str| headers.get(name).map(|v| v.to_lowercase());
    let csp = get("content-security-policy");
    if csp.is_none() {
        out.push(Finding::new(
            ID_MISSING_CSP,
            Severity::Medium,
            "CWE-693",
            Confidence::High,
            "Missing Content-Security-Policy",
            "No Content-Security-Policy header: one injected script runs with full page privileges (stored/reflected XSS hits harder).",
            "Send a Content-Security-Policy header (start with default-src 'self' and tighten from violation reports).",
        ));
    } else if let Some(policy) = &csp {
        // The header exists — check whether it actually constrains
        // scripts. A policy of `default-src 'self'` with no script-src
        // is fine; one permitting 'unsafe-inline' barely is.
        let directives = parse_csp_directives(policy);
        let script_sources = directives
            .iter()
            .find(|(name, _)| name == "script-src")
            .or_else(|| directives.iter().find(|(name, _)| name == "default-src"))
            .map(|(_, sources)| sources.clone())
            .unwrap_or_default();
        let mut weaknesses = Vec::new();
        if script_sources.iter().any(|s| s == "'unsafe-inline'") {
            weaknesses.push("allows 'unsafe-inline' scripts");
        }
        if script_sources.iter().any(|s| s == "'unsafe-eval'") {
            weaknesses.push("allows 'unsafe-eval'");
        }
        if script_sources.iter().any(|s| s == "*" || s == "data:") {
            weaknesses.push("allows wildcard/data: script sources");
        }
        if !weaknesses.is_empty() {
            out.push(Finding::new(
                ID_WEAK_CSP,
                Severity::Medium,
                "CWE-693",
                Confidence::High,
                "Permissive Content-Security-Policy",
                format!(
                    "The CSP ({}) {}.",
                    policy.chars().take(120).collect::<String>(),
                    weaknesses.join("; "),
                ),
                "Remove 'unsafe-inline'/'unsafe-eval'/wildcards from script-src (nonces or hashes instead), keeping default-src restrictive.",
            ));
        }
    }
    let framing = get("x-frame-options").is_some()
        || csp
            .as_ref()
            .map(|v| v.contains("frame-ancestors"))
            .unwrap_or(false);
    if !framing {
        out.push(Finding::new(
            ID_MISSING_FRAMING_PROTECTION,
            Severity::Medium,
            "CWE-1021",
            Confidence::High,
            "Missing clickjacking protection",
            "Neither X-Frame-Options nor a CSP frame-ancestors directive is set, so the form could be embedded in an attacker's iframe (clickjacking).",
            "Send X-Frame-Options: SAMEORIGIN or a CSP frame-ancestors directive.",
        ));
    }
    if is_https_page && get("strict-transport-security").is_none() {
        out.push(Finding::new(
            ID_MISSING_HSTS,
            Severity::Medium,
            "CWE-319",
            Confidence::High,
            "Missing Strict-Transport-Security",
            "No HSTS header on an HTTPS page: first visits can still be downgraded to HTTP (SSL stripping).",
            "Send Strict-Transport-Security: max-age=31536000; includeSubDomains once HTTPS is solid.",
        ));
    }
    if get("x-content-type-options").is_none() {
        out.push(Finding::new(
            ID_MISSING_NOSNIFF,
            Severity::Low,
            "CWE-693",
            Confidence::High,
            "Missing X-Content-Type-Options",
            "No X-Content-Type-Options: nosniff. Browsers may MIME-sniff responses and execute attacker-controlled content.",
            "Send X-Content-Type-Options: nosniff.",
        ));
    }
    match get("referrer-policy").as_deref() {
        None => out.push(Finding::new(
            ID_REFERRER_POLICY,
            Severity::Low,
            "CWE-200",
            Confidence::High,
            "Missing Referrer-Policy",
            "No Referrer-Policy header: full form URLs (possibly carrying tokens or personal data) may leak to third parties via the Referer header.",
            "Send Referrer-Policy: strict-origin-when-cross-origin (or stricter).",
        )),
        Some("unsafe-url") => out.push(Finding::new(
            ID_REFERRER_POLICY,
            Severity::Medium,
            "CWE-200",
            Confidence::High,
            "Unsafe Referrer-Policy",
            "Referrer-Policy is explicitly unsafe-url: full URLs, including any query-carried tokens or personal data, are sent to every third party.",
            "Send Referrer-Policy: strict-origin-when-cross-origin (or stricter) instead.",
        )),
        _ => {}
    }
    out
}

/// Pure server-banner analysis: version-disclosing `Server` /
/// `X-Powered-By` / `X-AspNet*` headers hand attackers a fingerprint for
/// targeting known CVEs. Unit-testable without a browser.
pub fn analyze_server_banner(headers: &HashMap<String, String>) -> Vec<Finding> {
    let mut banners = Vec::new();
    if let Some(server) = headers.get("server")
        && has_version_token(server)
    {
        banners.push(format!("Server: {}", truncate(server, 100)));
    }
    for name in ["x-powered-by", "x-aspnet-version", "x-aspnetmvc-version"] {
        if let Some(value) = headers.get(name) {
            banners.push(format!("{name}: {}", truncate(value, 100)));
        }
    }
    if banners.is_empty() {
        return Vec::new();
    }
    vec![Finding::new(
        ID_SERVER_BANNER,
        Severity::Low,
        "CWE-200",
        Confidence::High,
        "Server version disclosure",
        format!(
            "Response headers disclose software versions ({}), helping attackers target known CVEs.",
            banners.join("; "),
        ),
        "Strip version numbers from Server/X-Powered-By headers (keep the product name or remove them entirely).",
    )]
}

/// Whether a header value carries a version token like `2.4.52` in
/// `Apache/2.4.52` or `nginx/1.18.0`. A bare product name (`nginx`,
/// `cloudflare`) is not a finding.
fn has_version_token(value: &str) -> bool {
    value.split([' ', ';', ',']).any(|token| {
        token
            .split_once('/')
            .map(|(_, version)| {
                version
                    .chars()
                    .next()
                    .map(|c| c.is_ascii_digit())
                    .unwrap_or(false)
            })
            .unwrap_or(false)
    })
}

fn truncate(value: &str, max: usize) -> String {
    if value.len() <= max {
        return value.to_string();
    }
    format!("{}…", value.chars().take(max).collect::<String>())
}

/// One parsed `Set-Cookie` response header: just the name and the
/// security attributes, never the value (which may be a session token
/// and must not end up in reports or logs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCookie {
    /// Cookie name (values are deliberately never stored).
    pub name: String,
    /// `Secure` attribute present.
    pub secure: bool,
    /// `HttpOnly` attribute present.
    pub httponly: bool,
    /// `SameSite` attribute value as sent (`Lax`/`Strict`/`None`/other).
    pub samesite: Option<String>,
    /// `Path` attribute value as sent.
    pub path: Option<String>,
    /// `Domain` attribute value as sent.
    pub domain: Option<String>,
}

/// Parses one `Set-Cookie` header value into its name and security
/// attributes. Returns `None` for empty/malformed values. The cookie
/// *value* is discarded on purpose — it may be a live session token.
pub fn parse_set_cookie(header: &str) -> Option<ParsedCookie> {
    let mut parts = header.split(';');
    let first = parts.next()?.trim();
    if first.is_empty() {
        return None;
    }
    let (name, _) = first.split_once('=').unwrap_or((first, ""));
    let name = name.trim().to_string();
    if name.is_empty() {
        return None;
    }
    let mut cookie = ParsedCookie {
        name,
        secure: false,
        httponly: false,
        samesite: None,
        path: None,
        domain: None,
    };
    for attr in parts {
        let attr = attr.trim();
        if attr.eq_ignore_ascii_case("secure") {
            cookie.secure = true;
        } else if attr.eq_ignore_ascii_case("httponly") {
            cookie.httponly = true;
        } else if let Some((key, value)) = attr.split_once('=') {
            match key.trim().to_lowercase().as_str() {
                "samesite" => cookie.samesite = Some(value.trim().to_string()),
                "path" => cookie.path = Some(value.trim().to_string()),
                "domain" => cookie.domain = Some(value.trim().to_string()),
                _ => {}
            }
        }
    }
    Some(cookie)
}

/// Pure cookie-security analysis. `is_https_page` suppresses `Secure`
/// findings over plain HTTP (the transport checks already fail there —
/// repeating it per cookie would just be noise). Unit-testable without
/// a browser.
pub fn analyze_cookies(cookies: &[ParsedCookie], is_https_page: bool) -> Vec<Finding> {
    if cookies.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    let names = |pick: fn(&&ParsedCookie) -> bool| {
        summarize_names(
            cookies
                .iter()
                .filter(pick)
                .map(|c| c.name.clone())
                .collect(),
        )
    };
    if is_https_page {
        let missing: Vec<&ParsedCookie> = cookies.iter().filter(|c| !c.secure).collect();
        if !missing.is_empty() {
            out.push(Finding::new(
                ID_COOKIE_NO_SECURE,
                Severity::Medium,
                "CWE-614",
                Confidence::High,
                "Cookies without Secure on HTTPS",
                format!(
                    "{} cookie(s) without Secure ({}): if any HTTP URL on this host is ever hit, they travel in cleartext and sessions can be stolen.",
                    missing.len(),
                    names(|c| !c.secure),
                ),
                "Set Secure on every cookie served over HTTPS.",
            ));
        }
        // `__Secure-`/`__Host-` prefixed names are a browser-enforced
        // contract: silently accepting a non-conforming one means the
        // protection the name promises is not real.
        let prefix_violations: Vec<String> = cookies
            .iter()
            .filter(|c| {
                let lower = c.name.to_lowercase();
                (lower.starts_with("__host-")
                    && (!c.secure || c.domain.is_some() || c.path.as_deref() != Some("/")))
                    || (lower.starts_with("__secure-") && !c.secure)
            })
            .map(|c| c.name.clone())
            .collect();
        if !prefix_violations.is_empty() {
            out.push(Finding::new(
                ID_COOKIE_NO_SECURE,
                Severity::Medium,
                "CWE-614",
                Confidence::High,
                "Cookie prefix contract violated",
                format!(
                    "__Secure-/__Host- prefixed cookie(s) missing their required attributes ({}): __Host- requires Secure, Path=/, and no Domain; __Secure- requires Secure. Browsers reject these outright, so the intended protection is absent.",
                    summarize_names(prefix_violations),
                ),
                "Give __Host- cookies Secure, Path=/, and no Domain (or drop the prefix).",
            ));
        }
    }
    let samesite_none_insecure: Vec<&ParsedCookie> = cookies
        .iter()
        .filter(|c| {
            c.samesite
                .as_ref()
                .map(|s| s.eq_ignore_ascii_case("none"))
                .unwrap_or(false)
                && !c.secure
        })
        .collect();
    if !samesite_none_insecure.is_empty() {
        out.push(Finding::new(
            ID_COOKIE_SAMESITE,
            Severity::Medium,
            "CWE-1275",
            Confidence::High,
            "SameSite=None without Secure",
            format!(
                "{} cookie(s) declare SameSite=None without Secure ({}): modern browsers reject them, so cross-site flows relying on them silently break.",
                samesite_none_insecure.len(),
                names(|c| c
                    .samesite
                    .as_ref()
                    .map(|s| s.eq_ignore_ascii_case("none"))
                    .unwrap_or(false)
                    && !c.secure),
            ),
            "Pair SameSite=None with Secure, or switch to SameSite=Lax.",
        ));
    }
    let samesite_missing = cookies.iter().filter(|c| c.samesite.is_none()).count();
    if samesite_missing > 0 {
        out.push(Finding::new(
            ID_COOKIE_SAMESITE,
            Severity::Low,
            "CWE-1275",
            Confidence::High,
            "Cookies without an explicit SameSite",
            format!(
                "{samesite_missing} cookie(s) set no SameSite attribute ({}). Modern browsers default to Lax, but pinning it explicitly protects users on older browsers and documents intent.",
                names(|c| c.samesite.is_none()),
            ),
            "Set SameSite=Lax (or Strict for session cookies) explicitly.",
        ));
    }
    let no_httponly = cookies.iter().filter(|c| !c.httponly).count();
    if no_httponly > 0 {
        out.push(Finding::new(
            ID_COOKIE_NO_HTTPONLY,
            Severity::Low,
            "CWE-1004",
            Confidence::Medium,
            "Cookies without HttpOnly",
            format!(
                "{no_httponly} cookie(s) lack HttpOnly ({}): any XSS can read them via document.cookie. (Skip this for cookies JavaScript genuinely needs.)",
                names(|c| !c.httponly),
            ),
            "Set HttpOnly on every cookie JavaScript does not need to read.",
        ));
    }
    out
}

/// Query-string keys whose presence in a URL means sensitive data is
/// sitting in history, logs, and `Referer` headers. Password-likes are
/// `High`; the rest are `Medium`.
fn sensitive_query_severity(key: &str) -> Option<Severity> {
    let key = key.to_lowercase();
    if ["password", "passwd", "pwd", "pass"].contains(&key.as_str()) {
        Some(Severity::High)
    } else if [
        "token",
        "access_token",
        "access-token",
        "auth",
        "auth_token",
        "session",
        "sessionid",
        "session_id",
        "secret",
        "api_key",
        "apikey",
        "ssn",
        "card",
        "account",
    ]
    .contains(&key.as_str())
    {
        Some(Severity::Medium)
    } else {
        None
    }
}

/// Pure URL-query analysis over the raw query string (without the
/// leading `?`, fragment already stripped). Unit-testable.
pub fn analyze_url_query(query: &str) -> Vec<Finding> {
    if query.is_empty() {
        return Vec::new();
    }
    let mut high = Vec::new();
    let mut medium = Vec::new();
    for pair in query.split('&') {
        let key = pair.split_once('=').map(|(k, _)| k).unwrap_or(pair);
        // Percent-decode just enough to catch `%70assword`-style
        // evasion of a naive match; failures fall back to raw text.
        let decoded = percent_decode(key);
        match sensitive_query_severity(&decoded) {
            Some(Severity::High | Severity::Critical) => high.push(key.to_string()),
            Some(_) => medium.push(key.to_string()),
            None => {}
        }
    }
    let mut out = Vec::new();
    if !high.is_empty() {
        out.push(Finding::new(
            ID_SENSITIVE_QUERY,
            Severity::High,
            "CWE-598",
            Confidence::Medium,
            "Passwords in the page URL",
            format!(
                "Password-like query parameter(s) ({}) in the page URL: URLs persist in browser history, server logs, and Referer headers.",
                summarize_names(high),
            ),
            "Never put credentials in URLs; submit them in a POST body over HTTPS.",
        ));
    }
    if !medium.is_empty() {
        out.push(Finding::new(
            ID_SENSITIVE_QUERY,
            Severity::Medium,
            "CWE-598",
            Confidence::Medium,
            "Sensitive values in the page URL",
            format!(
                "Sensitive-looking query parameter(s) ({}) in the page URL: URLs persist in browser history, server logs, and Referer headers.",
                summarize_names(medium),
            ),
            "Move tokens and personal data out of URLs (POST body, headers, or server-side session).",
        ));
    }
    out
}
/// Minimal `%XX` decoder for query-key matching (plus `+` → space).
/// Invalid sequences pass through untouched. Decodes into bytes first:
/// pushing each raw byte as a `char` would corrupt multi-byte UTF-8
/// (mojibake) and could miss encoded sensitive keys.
fn percent_decode(input: &str) -> String {
    let mut raw: Vec<u8> = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let (Some(h), Some(l)) = (
                bytes.get(i + 1).and_then(|b| (*b as char).to_digit(16)),
                bytes.get(i + 2).and_then(|b| (*b as char).to_digit(16)),
            )
        {
            raw.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        raw.push(if bytes[i] == b'+' { b' ' } else { bytes[i] });
        i += 1;
    }
    String::from_utf8_lossy(&raw).into_owned()
}

/// The query part of a URL (after `?`, before `#`), or empty.
fn query_part(url: &str) -> &str {
    url.split_once('?')
        .map(|(_, rest)| rest.split_once('#').map(|(q, _)| q).unwrap_or(rest))
        .unwrap_or("")
}

/// Information-disclosure signals collected from the live page.
#[derive(Debug, Clone, Default)]
pub struct DisclosureSignals {
    /// The page body mentions stack traces / SQL errors / debug output.
    pub verbose_errors: bool,
    /// HTML comments look like embedded secrets or keys.
    pub sensitive_comments: bool,
    /// The page's own URL (for query-string analysis).
    pub page_url: String,
}

/// Pure disclosure analysis over DOM signals. URL-query and banner
/// findings are added by the check from the same page URL and its
/// (cached) response headers. Unit-testable without a browser.
pub fn analyze_disclosure(sig: &DisclosureSignals) -> Vec<Finding> {
    let mut out = Vec::new();
    if sig.verbose_errors {
        out.push(Finding::new(
            ID_VERBOSE_ERRORS,
            Severity::Low,
            "CWE-209",
            Confidence::Medium,
            "Verbose error text on the page",
            "The page contains stack-trace, SQL-error, or debug-style wording that reveals internals (frameworks, paths, queries).",
            "Show generic error messages to users and log details server-side only.",
        ));
    }
    if sig.sensitive_comments {
        out.push(Finding::new(
            ID_SENSITIVE_COMMENTS,
            Severity::Medium,
            "CWE-615",
            Confidence::Low,
            "Sensitive-looking HTML comments",
            "HTML comments mention secrets, keys, tokens, or TODO credentials visible to anyone via View Source.",
            "Remove secrets and credential TODOs from shipped HTML; keep them in a tracker or vault.",
        ));
    }
    out
}

fn check_result(name: &str, findings: &[Finding]) -> CheckResult {
    CheckResult {
        name: name.to_string(),
        status: verdict(findings),
        detail: render(name, findings),
        screenshot: None,
    }
}

/// Collects transport signals from the live page DOM.
async fn collect_transport(page: &Page) -> Result<TransportSignals> {
    let data: serde_json::Value = page
        .evaluate(
            r#"(() => {
                const pageUrl = location.href;
                const actions = [...document.querySelectorAll('form')]
                    .map(f => f.action || '')
                    .filter(a => a.length > 0);
                const hasPassword = document.querySelector('input[type="password"]') !== null;
                const sub = [...document.querySelectorAll('img[src], script[src], link[href], iframe[src]')]
                    .map(el => el.src || el.href || '')
                    .filter(u => u.toLowerCase().startsWith('http://')).length;
                return { pageUrl, actions, hasPassword, httpSubresources: sub };
            })()"#,
        )
        .await?
        .into_value()?;
    Ok(TransportSignals {
        page_url: data["pageUrl"].as_str().unwrap_or("").to_string(),
        form_actions: data["actions"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
        has_password: data["hasPassword"].as_bool().unwrap_or(false),
        http_subresources: data["httpSubresources"].as_u64().unwrap_or(0) as usize,
    })
}

/// Collects form-integrity signals from the live page DOM.
async fn collect_integrity(page: &Page) -> Result<FormIntegritySignals> {
    let data: serde_json::Value = page
        .evaluate(
            r#"(() => {
                const sensitiveSel = 'input[type="password"], input[name*="ssn" i], input[name*="social" i], input[name*="card" i], input[name*="cc-number" i], input[type="email"]';
                const tokenRe = /csrf|xsrf|authenticity|__requestverification|csrfmiddleware|_token/i;
                let getWithSensitive = false;
                let postWithoutToken = false;
                for (const f of document.querySelectorAll('form')) {
                    const method = (f.method || 'get').toLowerCase();
                    const hasSensitive = [...f.querySelectorAll(sensitiveSel)].length > 0;
                    const hasToken = [...f.querySelectorAll('input[type="hidden"]')]
                        .some(i => tokenRe.test(i.name || '') || tokenRe.test(i.id || ''));
                    const hasMetaToken = !!document.querySelector('meta[name*="csrf" i], meta[name*="token" i]');
                    if (method === 'get' && hasSensitive) getWithSensitive = true;
                    if (method === 'post' && !hasToken && !hasMetaToken) postWithoutToken = true;
                }
                const sensitiveAutocompleteOn = [...document.querySelectorAll(sensitiveSel)]
                    .filter(el => (el.getAttribute('autocomplete') || '').toLowerCase() !== 'off'
                        && (el.getAttribute('autocomplete') || '').toLowerCase() !== 'new-password').length;
                const unsafeBlank = [...document.querySelectorAll('a[target="_blank"], form[target="_blank"]')]
                    .filter(el => !(el.getAttribute('rel') || '').toLowerCase().includes('noopener')).length;
                return { getWithSensitive, postWithoutToken, sensitiveAutocompleteOn, unsafeBlankTargets: unsafeBlank };
            })()"#,
        )
        .await?
        .into_value()?;
    Ok(FormIntegritySignals {
        get_with_sensitive: data["getWithSensitive"].as_bool().unwrap_or(false),
        post_without_token: data["postWithoutToken"].as_bool().unwrap_or(false),
        sensitive_autocomplete_on: data["sensitiveAutocompleteOn"].as_u64().unwrap_or(0) as usize,
        unsafe_blank_targets: data["unsafeBlankTargets"].as_u64().unwrap_or(0) as usize,
    })
}

/// Collects disclosure signals from the live page DOM.
async fn collect_disclosure(page: &Page) -> Result<DisclosureSignals> {
    let data: serde_json::Value = page
        .evaluate(
            r#"(() => {
                const text = (document.body ? document.body.innerText : '').toLowerCase();
                const verbose = /stack trace|traceback|sql syntax|ora-|exception in|\.php on line|\.aspx|server error in/i.test(text);
                const walker = document.createTreeWalker(document, NodeFilter.SHOW_COMMENT);
                const secretRe = /password|secret|api[_-]?key|aws_|begin (rsa )?private key|todo.*(key|token|password|secret)/i;
                let sensitive = false;
                let n = walker.nextNode();
                let seen = 0;
                while (n && seen < 200) { seen += 1; if (secretRe.test(n.nodeValue || '')) { sensitive = true; break; } n = walker.nextNode(); }
                return { verboseErrors: verbose, sensitiveComments: sensitive, pageUrl: location.href };
            })()"#,
        )
        .await?
        .into_value()?;
    Ok(DisclosureSignals {
        verbose_errors: data["verboseErrors"].as_bool().unwrap_or(false),
        sensitive_comments: data["sensitiveComments"].as_bool().unwrap_or(false),
        page_url: data["pageUrl"].as_str().unwrap_or("").to_string(),
    })
}

/// Response headers for one URL: the header map (lowercased names,
/// first value wins), every `Set-Cookie` value (names only are ever
/// analyzed — values are discarded at parse time), and the final URL
/// after redirects.
#[derive(Debug, Clone, Default)]
pub struct FetchedHeaders {
    /// Final URL after following redirects.
    pub final_url: String,
    /// Whether the final URL is HTTPS.
    pub is_https: bool,
    /// Lowercased header name -> value (first value wins).
    pub headers: HashMap<String, String>,
    /// Raw `Set-Cookie` header values.
    pub set_cookies: Vec<String>,
    /// Jar cookies with flags only, from the browser's own load via CDP.
    /// Preferred over parsing `set_cookies` when non-empty: the jar is
    /// authoritative (no header-merging ambiguity) and includes cookies
    /// set by subresources and scripts, not just this response.
    pub structured_cookies: Vec<ParsedCookie>,
}

/// Seeds the header cache with the browser's own view of a page load
/// (see [`CapturedDocument`]), so [`fetch_headers`] serves the header,
/// cookie, and disclosure checks from the capture instead of
/// re-requesting the page. Called once per form by the runner right
/// after the page opens; entries live 60 seconds like any other.
pub fn seed_capture(doc: &CapturedDocument) {
    let fetched = FetchedHeaders {
        is_https: is_https(&doc.final_url),
        final_url: doc.final_url.clone(),
        headers: doc.headers.clone(),
        set_cookies: doc.set_cookies.clone(),
        structured_cookies: doc
            .cookies
            .iter()
            .cloned()
            .map(ParsedCookie::from)
            .collect(),
    };
    let mut cache = HEADER_CACHE.lock().expect("header cache poisoned");
    cache.retain(|_, (at, _)| at.elapsed() < HEADER_CACHE_TTL);
    if cache.len() >= HEADER_CACHE_MAX {
        cache.clear();
    }
    cache.insert(doc.final_url.clone(), (Instant::now(), fetched));
}

impl From<CapturedCookie> for ParsedCookie {
    fn from(c: CapturedCookie) -> Self {
        ParsedCookie {
            name: c.name,
            secure: c.secure,
            httponly: c.httponly,
            samesite: c.samesite,
            path: Some(c.path),
            domain: Some(c.domain),
        }
    }
}

/// Chooses the cookie set to analyze: the browser jar when capture
/// provided one, else the document response's `Set-Cookie` values
/// (splitting Chromium's newline-joined repeats). Pure and
/// unit-testable.
fn cookies_for_analysis(fetched: &FetchedHeaders) -> Vec<ParsedCookie> {
    if !fetched.structured_cookies.is_empty() {
        return fetched.structured_cookies.clone();
    }
    fetched
        .set_cookies
        .iter()
        .flat_map(|h| h.split('\n'))
        .filter_map(parse_set_cookie)
        .collect()
}

/// Short-lived cache so the header, cookie, and disclosure checks share
/// one plain GET per URL instead of each re-requesting the page.
/// Entries live 60 seconds; the map is pruned on insert so a huge
/// registry cannot grow it without bound.
static HEADER_CACHE: LazyLock<Mutex<HashMap<String, (Instant, FetchedHeaders)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

const HEADER_CACHE_TTL: Duration = Duration::from_secs(60);
const HEADER_CACHE_MAX: usize = 1024;

/// Fetches response headers for `url`, serving a browser-captured load
/// from the cache when the runner seeded one (the normal case — zero
/// extra requests). Only when no capture exists does this fall back to
/// a plain GET (no payloads, no crawling), following up to 5 redirects
/// with a browser-like User-Agent.
pub async fn fetch_headers(url: &str) -> Result<FetchedHeaders> {
    if let Some(cached) = {
        let cache = HEADER_CACHE.lock().expect("header cache poisoned");
        cache
            .get(url)
            .filter(|(at, _)| at.elapsed() < HEADER_CACHE_TTL)
            .map(|(_, fetched)| fetched.clone())
    } {
        return Ok(cached);
    }
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .redirect(reqwest::redirect::Policy::limited(5))
        .user_agent("formwatch-vuln-scan (+https://github.com/voidstackloop/formwatch)")
        .build()
        .context("building header-fetch client")?;
    let response = client
        .get(url)
        .send()
        .await
        .with_context(|| format!("fetching response headers for {url}"))?;
    let final_url = response.url().to_string();
    let mut headers = HashMap::new();
    for (name, value) in response.headers().iter() {
        if let Ok(v) = value.to_str() {
            headers
                .entry(name.as_str().to_lowercase())
                .or_insert_with(|| v.to_string());
        }
    }
    let set_cookies: Vec<String> = response
        .headers()
        .get_all(reqwest::header::SET_COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok().map(str::to_string))
        .collect();
    let fetched = FetchedHeaders {
        is_https: is_https(&final_url),
        final_url,
        headers,
        set_cookies,
        // A plain GET has no jar access (HttpOnly cookies are invisible
        // outside the browser) — only CDP capture provides these.
        structured_cookies: Vec::new(),
    };
    {
        let mut cache = HEADER_CACHE.lock().expect("header cache poisoned");
        cache.retain(|_, (at, _)| at.elapsed() < HEADER_CACHE_TTL);
        if cache.len() >= HEADER_CACHE_MAX {
            cache.clear();
        }
        cache.insert(url.to_string(), (Instant::now(), fetched.clone()));
    }
    Ok(fetched)
}

/// Returns the page's current URL, or a `Warn`-shaped error when the
/// page exposes none (e.g. a data: URL context).
async fn page_url(page: &Page) -> Result<String> {
    let url: String = page
        .evaluate("location.href")
        .await?
        .into_value()
        .unwrap_or_default();
    if url.is_empty() {
        anyhow::bail!("could not determine the page URL, skipping header fetch");
    }
    Ok(url)
}

/// Pure finding collectors below: each gathers its signals and returns
/// findings, so [`run_vuln_checks_filtered`] can apply `--min-severity`
/// before rendering while the public `check_*` wrappers keep their
/// exact names and verdict behavior for `run_all`.
async fn findings_insecure_transport(page: &Page) -> Result<Vec<Finding>> {
    Ok(analyze_transport(&collect_transport(page).await?))
}

async fn findings_form_data_protection(page: &Page) -> Result<Vec<Finding>> {
    Ok(analyze_form_integrity(&collect_integrity(page).await?))
}

async fn findings_security_headers(page: &Page) -> Result<Vec<Finding>> {
    let url = page_url(page).await?;
    let fetched = fetch_headers(&url).await?;
    let mut findings = analyze_headers(&fetched.headers, fetched.is_https);
    findings.extend(analyze_server_banner(&fetched.headers));
    Ok(findings)
}

async fn findings_cookie_security(page: &Page) -> Result<Vec<Finding>> {
    let url = page_url(page).await?;
    let fetched = fetch_headers(&url).await?;
    let cookies = cookies_for_analysis(&fetched);
    if cookies.is_empty() {
        return Ok(Vec::new());
    }
    Ok(analyze_cookies(&cookies, fetched.is_https))
}

async fn findings_information_disclosure(page: &Page) -> Result<Vec<Finding>> {
    let signals = collect_disclosure(page).await?;
    let mut findings = analyze_disclosure(&signals);
    findings.extend(analyze_url_query(query_part(&signals.page_url)));
    Ok(findings)
}

/// Insecure-transport check: HTTP page, HTTP form actions, passwords in
/// cleartext, mixed content. `Fail` on `High`+, `Pass` when clean.
pub async fn check_insecure_transport(page: &Page) -> Result<CheckResult> {
    Ok(check_result(
        "Insecure transport",
        &findings_insecure_transport(page).await?,
    ))
}

/// Form-data-protection check: GET with sensitive fields, missing CSRF
/// token, autocomplete left on, unsafe new-tab targets.
pub async fn check_form_data_protection(page: &Page) -> Result<CheckResult> {
    Ok(check_result(
        "Form data protection",
        &findings_form_data_protection(page).await?,
    ))
}

/// Security-headers check: CSP (missing or permissive), framing
/// protection, HSTS, nosniff, Referrer-Policy, server banners. Missing
/// or weak headers are hardening gaps (`Warn`).
pub async fn check_security_headers(page: &Page) -> Result<CheckResult> {
    Ok(check_result(
        "Security headers",
        &findings_security_headers(page).await?,
    ))
}

/// Cookie-security check: `Secure`, `HttpOnly`, `SameSite`, and
/// `__Secure`-/`__Host`-prefix contracts on every `Set-Cookie`.
pub async fn check_cookie_security(page: &Page) -> Result<CheckResult> {
    Ok(check_result(
        "Cookie security",
        &findings_cookie_security(page).await?,
    ))
}

/// Information-disclosure check: verbose errors, secret-looking HTML
/// comments, and sensitive page-URL query parameters.
pub async fn check_information_disclosure(page: &Page) -> Result<CheckResult> {
    Ok(check_result(
        "Information disclosure",
        &findings_information_disclosure(page).await?,
    ))
}

/// One named finding-collection run: errors and hangs become an
/// explanatory `Warn` instead of erasing the other checks.
async fn gather(
    name: &'static str,
    fut: impl std::future::Future<Output = Result<Vec<Finding>>>,
    min: Option<Severity>,
) -> CheckResult {
    match tokio::time::timeout(Duration::from_secs(30), fut).await {
        Ok(Ok(findings)) => check_result(name, &filter_min(&findings, min)),
        Ok(Err(e)) => CheckResult {
            name: name.to_string(),
            status: Status::Warn,
            detail: format!("{name}: check did not complete: {e:#}"),
            screenshot: None,
        },
        Err(_) => CheckResult {
            name: name.to_string(),
            status: Status::Warn,
            detail: format!("{name}: check did not complete: timed out after 30s"),
            screenshot: None,
        },
    }
}

/// Runs all vulnerability checks, converting errors into `Warn`
/// results (same policy as [`crate::checks`] timeout handling: one bad
/// check must not erase the others).
pub async fn run_vuln_checks(page: &Page) -> Vec<CheckResult> {
    run_vuln_checks_filtered(page, None).await
}

/// [`run_vuln_checks`], keeping only findings at or above `min`
/// severity — the triage view behind `vuln --min-severity`.
pub async fn run_vuln_checks_filtered(page: &Page, min: Option<Severity>) -> Vec<CheckResult> {
    vec![
        gather("Insecure transport", findings_insecure_transport(page), min).await,
        gather(
            "Form data protection",
            findings_form_data_protection(page),
            min,
        )
        .await,
        gather("Security headers", findings_security_headers(page), min).await,
        gather("Cookie security", findings_cookie_security(page), min).await,
        gather(
            "Information disclosure",
            findings_information_disclosure(page),
            min,
        )
        .await,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect()
    }

    fn finding(severity: Severity) -> Finding {
        Finding::new(
            ID_VERBOSE_ERRORS,
            severity,
            "CWE-209",
            Confidence::Medium,
            "t",
            "d",
            "r",
        )
    }

    #[test]
    fn http_page_with_password_is_critical() {
        let sig = TransportSignals {
            page_url: "http://city.gov/apply".into(),
            form_actions: vec!["http://city.gov/submit".into()],
            has_password: true,
            http_subresources: 0,
        };
        let findings = analyze_transport(&sig);
        assert!(findings.iter().any(|f| f.id == ID_HTTP_PAGE));
        assert!(findings.iter().any(|f| f.id == ID_HTTP_FORM_ACTION));
        assert!(
            findings
                .iter()
                .any(|f| f.id == ID_PASSWORD_OVER_HTTP && f.severity == Severity::Critical)
        );
        assert_eq!(findings[0].cwe, "CWE-319");
        assert_eq!(findings[0].confidence, Confidence::High);
        assert_eq!(verdict(&findings), Status::Fail);
    }

    #[test]
    fn clean_https_page_passes_transport() {
        let sig = TransportSignals {
            page_url: "https://city.gov/apply".into(),
            form_actions: vec!["https://city.gov/submit".into()],
            has_password: true,
            http_subresources: 0,
        };
        assert!(analyze_transport(&sig).is_empty());
        assert_eq!(verdict(&analyze_transport(&sig)), Status::Pass);
    }

    #[test]
    fn mixed_content_is_a_medium_warn() {
        let sig = TransportSignals {
            page_url: "https://city.gov/apply".into(),
            form_actions: vec!["https://city.gov/submit".into()],
            has_password: false,
            http_subresources: 3,
        };
        let findings = analyze_transport(&sig);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].id, ID_MIXED_CONTENT);
        assert_eq!(verdict(&findings), Status::Warn);
    }

    #[test]
    fn get_with_sensitive_and_missing_token_are_flagged() {
        let sig = FormIntegritySignals {
            get_with_sensitive: true,
            post_without_token: true,
            sensitive_autocomplete_on: 2,
            unsafe_blank_targets: 1,
        };
        let findings = analyze_form_integrity(&sig);
        let ids: Vec<&str> = findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&ID_GET_WITH_SENSITIVE));
        assert!(ids.contains(&ID_MISSING_CSRF_TOKEN));
        assert!(ids.contains(&ID_SENSITIVE_AUTOCOMPLETE));
        assert!(ids.contains(&ID_UNSAFE_BLANK_TARGET));
        // Worst is Medium -> Warn, never Fail for this check family.
        assert_eq!(verdict(&findings), Status::Warn);
    }

    #[test]
    fn full_security_headers_pass() {
        let h = headers(&[
            ("content-security-policy", "default-src 'self'"),
            ("x-frame-options", "SAMEORIGIN"),
            ("strict-transport-security", "max-age=31536000"),
            ("x-content-type-options", "nosniff"),
            ("referrer-policy", "strict-origin-when-cross-origin"),
        ]);
        assert!(analyze_headers(&h, true).is_empty());
    }

    #[test]
    fn missing_headers_are_each_flagged() {
        let findings = analyze_headers(&headers(&[]), true);
        let ids: Vec<&str> = findings.iter().map(|f| f.id).collect();
        for expected in [
            ID_MISSING_CSP,
            ID_MISSING_FRAMING_PROTECTION,
            ID_MISSING_HSTS,
            ID_MISSING_NOSNIFF,
            ID_REFERRER_POLICY,
        ] {
            assert!(ids.contains(&expected), "missing {expected}");
        }
        assert_eq!(verdict(&findings), Status::Warn);
    }

    #[test]
    fn csp_frame_ancestors_counts_as_framing_protection() {
        let h = headers(&[(
            "content-security-policy",
            "default-src 'self'; frame-ancestors 'none'",
        )]);
        let findings = analyze_headers(&h, false);
        assert!(
            !findings
                .iter()
                .any(|f| f.id == ID_MISSING_FRAMING_PROTECTION)
        );
        // HSTS is meaningless over HTTP, so it must not fire there.
        assert!(!findings.iter().any(|f| f.id == ID_MISSING_HSTS));
    }

    #[test]
    fn permissive_csp_directives_are_flagged_not_passed() {
        let h = headers(&[(
            "content-security-policy",
            "default-src 'self'; script-src 'self' 'unsafe-inline' 'unsafe-eval'",
        )]);
        let findings = analyze_headers(&h, false);
        let weak = findings.iter().find(|f| f.id == ID_WEAK_CSP);
        assert!(weak.is_some(), "expected {ID_WEAK_CSP}");
        assert!(weak.expect("weak csp").detail.contains("unsafe-inline"));
        assert!(
            !findings.iter().any(|f| f.id == ID_MISSING_CSP),
            "a present CSP must not also count as missing"
        );
    }

    #[test]
    fn unsafe_referrer_policy_is_worse_than_missing() {
        let findings = analyze_headers(&headers(&[("referrer-policy", "unsafe-url")]), false);
        let found = findings
            .iter()
            .find(|f| f.id == ID_REFERRER_POLICY)
            .expect("referrer finding");
        assert_eq!(found.severity, Severity::Medium);
    }

    #[test]
    fn disclosure_flags_verbose_errors_and_secret_comments() {
        let findings = analyze_disclosure(&DisclosureSignals {
            verbose_errors: true,
            sensitive_comments: true,
            page_url: String::new(),
        });
        assert_eq!(findings.len(), 2);
        assert_eq!(verdict(&findings), Status::Warn);
        let clean = analyze_disclosure(&DisclosureSignals::default());
        assert!(clean.is_empty());
        assert_eq!(verdict(&clean), Status::Pass);
    }

    #[test]
    fn verdict_escalates_on_high_and_critical() {
        let low = finding(Severity::Low);
        let high = finding(Severity::High);
        assert_eq!(verdict(std::slice::from_ref(&low)), Status::Warn);
        assert_eq!(verdict(&[low, high]), Status::Fail);
    }

    #[test]
    fn severity_parses_for_min_severity_filtering() {
        assert_eq!("high".parse::<Severity>(), Ok(Severity::High));
        assert_eq!("CRITICAL".parse::<Severity>(), Ok(Severity::Critical));
        assert!("extreme".parse::<Severity>().is_err());
        let findings = vec![finding(Severity::Low), finding(Severity::High)];
        assert_eq!(filter_min(&findings, Some(Severity::High)).len(), 1);
        assert_eq!(filter_min(&findings, None).len(), 2);
    }

    #[test]
    fn set_cookie_parsing_keeps_flags_and_drops_values() {
        let c =
            parse_set_cookie("sid=abc123; Path=/; Secure; HttpOnly; SameSite=Lax").expect("parses");
        assert_eq!(c.name, "sid");
        assert!(c.secure && c.httponly);
        assert_eq!(c.samesite.as_deref(), Some("Lax"));
        assert!(parse_set_cookie("").is_none());
        assert!(parse_set_cookie("=oops").is_none());
        let prefixed = parse_set_cookie("__Host-id=1; Secure; Path=/").expect("parses");
        assert_eq!(prefixed.path.as_deref(), Some("/"));
    }

    #[test]
    fn insecure_cookies_flagged_but_quiet_on_http() {
        let cookies = vec![
            parse_set_cookie("sid=1; Path=/").expect("parses"),
            parse_set_cookie("prefs=1; Path=/; Secure; HttpOnly; SameSite=Strict").expect("parses"),
        ];
        let https_findings = analyze_cookies(&cookies, true);
        let ids: Vec<&str> = https_findings.iter().map(|f| f.id).collect();
        assert!(ids.contains(&ID_COOKIE_NO_SECURE));
        assert!(ids.contains(&ID_COOKIE_NO_HTTPONLY));
        assert!(ids.contains(&ID_COOKIE_SAMESITE));
        // Over plain HTTP the Secure finding would just repeat the
        // transport verdict, so it stays silent there.
        let http_findings = analyze_cookies(&cookies, false);
        assert!(!http_findings.iter().any(|f| f.id == ID_COOKIE_NO_SECURE));
        assert!(analyze_cookies(&[], true).is_empty());
    }

    #[test]
    fn samesite_none_without_secure_and_bad_prefixes_flagged() {
        let cookies = vec![
            parse_set_cookie("x=1; SameSite=None").expect("parses"),
            parse_set_cookie("__Host-id=1; Path=/").expect("parses"),
        ];
        let findings = analyze_cookies(&cookies, true);
        assert!(
            findings
                .iter()
                .any(|f| f.id == ID_COOKIE_SAMESITE && f.severity == Severity::Medium)
        );
        assert!(findings.iter().any(|f| f.title.contains("prefix")));
    }

    #[test]
    fn sensitive_query_keys_flagged_by_family() {
        let findings = analyze_url_query("page=2&token=abc&password=hunter2");
        assert!(
            findings
                .iter()
                .any(|f| f.id == ID_SENSITIVE_QUERY && f.severity == Severity::High)
        );
        assert!(
            findings
                .iter()
                .any(|f| f.id == ID_SENSITIVE_QUERY && f.severity == Severity::Medium)
        );
        assert!(analyze_url_query("page=2&q=permit").is_empty());
        assert!(analyze_url_query("").is_empty());
        // Encoded keys do not slip past the match.
        assert!(!analyze_url_query("%70assword=x").is_empty());
    }

    #[test]
    fn percent_decoding_handles_utf8_and_junk() {
        // Multi-byte UTF-8 must come back as real characters, not one
        // char per byte (mojibake); invalid escapes pass through so
        // matching never panics or misfires on hostile input.
        assert_eq!(percent_decode("%E2%82%AC"), "€");
        assert_eq!(percent_decode("%41BC"), "ABC");
        assert_eq!(percent_decode("a+b"), "a b");
        assert_eq!(percent_decode("%zz%2"), "%zz%2");
        assert_eq!(percent_decode("%"), "%");
    }

    #[test]
    fn version_banners_flagged_bare_names_are_not() {
        let flagged = analyze_server_banner(&headers(&[
            ("server", "Apache/2.4.52 (Ubuntu)"),
            ("x-powered-by", "PHP/8.1.0"),
        ]));
        assert_eq!(flagged.len(), 1);
        assert_eq!(flagged[0].id, ID_SERVER_BANNER);
        assert!(analyze_server_banner(&headers(&[("server", "cloudflare")])).is_empty());
        assert!(analyze_server_banner(&headers(&[])).is_empty());
    }

    #[test]
    fn structured_jar_cookies_win_over_set_cookie_parsing() {
        // The jar is authoritative; the raw header is only parsed when
        // capture provided no jar cookies.
        let fetched = FetchedHeaders {
            final_url: "https://city.gov/".into(),
            is_https: true,
            headers: HashMap::new(),
            set_cookies: vec!["sid=1; Path=/".into()],
            structured_cookies: vec![
                parse_set_cookie("sid=1; Secure; HttpOnly; SameSite=Lax").expect("parses"),
            ],
        };
        let cookies = cookies_for_analysis(&fetched);
        assert_eq!(cookies.len(), 1);
        assert!(cookies[0].secure);
    }

    #[test]
    fn newline_joined_set_cookies_split_into_individual_cookies() {
        let fetched = FetchedHeaders {
            final_url: "https://city.gov/".into(),
            is_https: true,
            headers: HashMap::new(),
            set_cookies: vec!["a=1; Secure\nb=2; HttpOnly".into()],
            structured_cookies: vec![],
        };
        let cookies = cookies_for_analysis(&fetched);
        assert_eq!(cookies.len(), 2);
    }

    #[tokio::test]
    async fn seeded_browser_capture_serves_headers_without_network() {
        // Seeding then fetching the same URL must hit the cache — this
        // uses a `.invalid` URL that could never resolve, so any real
        // network attempt would fail the test.
        let url = "https://seed-capture.invalid/apply";
        let mut headers = HashMap::new();
        headers.insert(
            "content-security-policy".to_string(),
            "default-src 'self'".to_string(),
        );
        headers.insert("x-frame-options".to_string(), "SAMEORIGIN".to_string());
        seed_capture(&CapturedDocument {
            final_url: url.to_string(),
            headers,
            set_cookies: vec![],
            cookies: vec![CapturedCookie {
                name: "sid".to_string(),
                secure: true,
                httponly: true,
                samesite: Some("Lax".to_string()),
                path: "/".to_string(),
                domain: "seed-capture.invalid".to_string(),
            }],
        });
        let fetched = fetch_headers(url).await.expect("cache hit, no network");
        assert!(fetched.is_https);
        assert_eq!(fetched.structured_cookies.len(), 1);
        assert!(analyze_cookies(&cookies_for_analysis(&fetched), true).is_empty());
    }
}
