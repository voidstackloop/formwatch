//! Read-only HTTP service (`formwatch serve`).
//!
//! Turns the accumulated history into endpoints a monitoring system or
//! status page can consume without shelling out:
//!
//! - `GET /healthz` — liveness; always `200 ok`.
//! - `GET /readyz` — readiness; `200` if the history directory is usable,
//!   `503` otherwise.
//! - `GET /metrics` — the latest runs in Prometheus text format.
//! - `GET /api/forms` — the latest run of every known form as JSON.
//! - `GET /` — a short plain-text index of the endpoints.
//!
//! It is deliberately **read-only**: there is no endpoint that triggers a
//! run, submits a form, or accepts a URL, so exposing it never gives a
//! caller the ability to make formwatch touch a third-party site. It binds
//! to `127.0.0.1` by default.

use crate::history;
use crate::history::RunResult;
use crate::metrics;
use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const TEXT: &str = "text/plain; charset=utf-8";
const PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";
const JSON: &str = "application/json; charset=utf-8";

const INDEX: &str = "formwatch (read-only)\n\n  GET /healthz     liveness\n  GET /readyz      readiness\n  GET /metrics     Prometheus metrics\n  GET /api/forms   latest run of every form (JSON)\n  GET /api/summary aggregate counts + worst status per form (JSON)\n  GET /api/trends  pass-rate + streaks per form (JSON)\n  GET /api/flakes  currently-flapping checks per form (JSON)\n  GET /badge.svg   fleet status badge (SVG)\n";

/// Optional bearer-token guard for the data endpoints. `/healthz` and
/// `/readyz` stay open (load-balancer probes can't send secrets);
/// everything else requires `Authorization: Bearer <token>` when set.
#[derive(Debug, Clone, Default)]
pub struct ServeOptions {
    /// Expected bearer token, or `None` for unauthenticated (default).
    pub api_token: Option<String>,
}

/// Renders a shields-style SVG badge: green PASS, yellow WARN, red FAIL.
/// Pure so the route stays thin and the badge stays unit-tested.
pub fn badge_svg(runs: &[RunResult]) -> String {
    use crate::checks::Status;
    let (mut fail, mut warn) = (0, 0);
    for r in runs {
        if r.checks.iter().any(|c| c.status == Status::Fail) {
            fail += 1;
        } else if r.checks.iter().any(|c| c.status == Status::Warn) {
            warn += 1;
        }
    }
    let (label, color) = if fail > 0 {
        (format!("fail {fail}"), "#cb2431")
    } else if warn > 0 {
        (format!("warn {warn}"), "#dbab09")
    } else {
        ("pass".to_string(), "#2da44e")
    };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="122" height="20" role="img" aria-label="formwatch {label}"><rect width="122" height="20" rx="3" fill="#555"/><rect x="70" width="52" height="20" rx="3" fill="{color}"/><text x="35" y="14" fill="#fff" font-family="Verdana" font-size="11" text-anchor="middle">formwatch</text><text x="96" y="14" fill="#fff" font-family="Verdana" font-size="11" text-anchor="middle">{label}</text></svg>"##
    )
}

/// Currently-flapping checks per form for `/api/flakes`. Pure over the
/// already-loaded latest runs; per-form history is bounded the same way
/// as [`trends_json`] so one giant history can't blow up a scrape.
pub fn flakes_json(history_dir: &Path, runs: &[RunResult]) -> serde_json::Value {
    let forms: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| {
            let loaded = history::load_runs(history_dir, &r.url).unwrap_or_default();
            let considered: &[RunResult] = if loaded.len() > 20 {
                &loaded[loaded.len() - 20..]
            } else {
                &loaded
            };
            let flaky: Vec<serde_json::Value> = history::flakiness(considered)
                .into_iter()
                .filter(|f| f.is_flaky())
                .map(|f| {
                    serde_json::json!({
                        "check": f.name,
                        "transitions": f.transitions,
                        "current": f.current.label(),
                    })
                })
                .collect();
            serde_json::json!({ "name": r.name, "url": r.url, "flaky": flaky })
        })
        .collect();
    serde_json::json!({ "forms": forms })
}

/// Pass-rate + streak snapshot per form for `/api/trends`. Pure over the
/// already-loaded latest runs; per-form run counts come from a bounded
/// history load so one giant history can't blow up a scrape.
pub fn trends_json(history_dir: &Path, runs: &[RunResult]) -> serde_json::Value {
    use crate::checks::Status;
    let forms: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| {
            let loaded = history::load_runs(history_dir, &r.url).unwrap_or_default();
            let considered: &[RunResult] = if loaded.len() > 20 {
                &loaded[loaded.len() - 20..]
            } else {
                &loaded
            };
            let total = considered.len().max(1) as f64;
            let passing = considered
                .iter()
                .filter(|x| x.checks.iter().all(|c| c.status == Status::Pass))
                .count() as f64;
            serde_json::json!({
                "name": r.name,
                "url": r.url,
                "runs": considered.len(),
                "pass_rate": passing / total,
                "worst": if r.checks.iter().any(|c| c.status == Status::Fail) { "FAIL" }
                    else if r.checks.iter().any(|c| c.status == Status::Warn) { "WARN" }
                    else { "PASS" },
            })
        })
        .collect();
    serde_json::json!({ "forms": forms })
}

fn authorized(api_token: &Option<String>, req: &hyper::Request<hyper::body::Incoming>) -> bool {
    let Some(expected) = api_token.as_deref() else {
        return true;
    };
    req.headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .is_some_and(|got| subtle_eq(got, expected))
}

/// Constant-time comparison so the token can't be guessed byte-by-byte
/// off response timing.
fn subtle_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// Aggregate fleet summary for dashboards that only need counts.
/// Pure over the already-loaded latest runs so `/api/summary` stays as
/// cheap as `/api/forms` under the shared [`HistoryCache`].
pub fn summary_json(runs: &[RunResult]) -> serde_json::Value {
    use crate::checks::Status;
    let mut pass = 0usize;
    let mut warn = 0usize;
    let mut fail = 0usize;
    let forms: Vec<serde_json::Value> = runs
        .iter()
        .map(|r| {
            let (mut p, mut w, mut f) = (0, 0, 0);
            for c in &r.checks {
                match c.status {
                    Status::Pass => {
                        p += 1;
                        pass += 1;
                    }
                    Status::Warn => {
                        w += 1;
                        warn += 1;
                    }
                    Status::Fail => {
                        f += 1;
                        fail += 1;
                    }
                }
            }
            let worst = if f > 0 {
                "FAIL"
            } else if w > 0 {
                "WARN"
            } else {
                "PASS"
            };
            serde_json::json!({
                "name": r.name,
                "url": r.url,
                "timestamp": r.timestamp,
                "pass": p,
                "warn": w,
                "fail": f,
                "worst": worst,
            })
        })
        .collect();
    serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "schema_version": crate::history::SCHEMA_VERSION,
        "forms": runs.len(),
        "pass": pass,
        "warn": warn,
        "fail": fail,
        "checks": pass + warn + fail,
        "forms_detail": forms,
    })
}

/// How long a loaded [`history::all_known_forms`] result is reused before
/// the next `/metrics` or `/api/forms` request re-reads history from
/// disk. `/metrics` in particular is meant to be scraped (Prometheus,
/// `/readyz`-style liveness polling, a status page) far more often than
/// `monitor` actually writes new runs — without this, every scrape
/// re-walked the entire history directory (one file read per form) for
/// data that, almost always, hadn't changed since the last scrape a few
/// seconds earlier. Short enough that nobody scraping a monitoring
/// endpoint will notice the staleness.
const HISTORY_CACHE_TTL: Duration = Duration::from_secs(2);

/// Caches the last [`history::all_known_forms`] result for
/// [`HISTORY_CACHE_TTL`], shared across every request `serve` handles —
/// `/metrics` and `/api/forms` both read through it instead of each
/// re-scanning history independently.
struct HistoryCache {
    ttl: Duration,
    loaded: Mutex<Option<(Instant, Arc<Vec<RunResult>>)>>,
}

impl HistoryCache {
    fn new(ttl: Duration) -> Self {
        Self {
            ttl,
            loaded: Mutex::new(None),
        }
    }

    fn get_or_load(&self, history_dir: &Path) -> Result<Arc<Vec<RunResult>>> {
        let mut loaded = self.loaded.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((fetched_at, runs)) = loaded.as_ref()
            && fetched_at.elapsed() < self.ttl
        {
            return Ok(runs.clone());
        }
        let runs = Arc::new(history::all_known_forms(history_dir)?);
        *loaded = Some((Instant::now(), runs.clone()));
        Ok(runs)
    }
}

/// Routes a request to a `(status, content-type, body)` triple. Kept
/// separate from the socket plumbing so it can be unit-tested directly.
fn route_with_auth(
    method: &Method,
    path: &str,
    history_dir: &Path,
    cache: &HistoryCache,
    authed: bool,
) -> (StatusCode, &'static str, String) {
    // Data endpoints are guarded when a token is configured; the probes
    // stay open so load balancers and container orchestrators keep working.
    if !authed && !matches!(path, "/" | "/healthz" | "/readyz") {
        return (
            StatusCode::UNAUTHORIZED,
            TEXT,
            "unauthorized: missing or invalid bearer token\n".into(),
        );
    }
    route(method, path, history_dir, cache)
}

fn route(
    method: &Method,
    path: &str,
    history_dir: &Path,
    cache: &HistoryCache,
) -> (StatusCode, &'static str, String) {
    if method != Method::GET {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            TEXT,
            "method not allowed\n".into(),
        );
    }
    match path {
        "/healthz" => (StatusCode::OK, TEXT, "ok\n".into()),
        "/readyz" => match readiness(history_dir) {
            Ok(()) => (StatusCode::OK, TEXT, "ok\n".into()),
            Err(e) => (
                StatusCode::SERVICE_UNAVAILABLE,
                TEXT,
                format!("history directory not writable: {e}\n"),
            ),
        },
        "/metrics" => match cache.get_or_load(history_dir) {
            Ok(runs) => (StatusCode::OK, PROMETHEUS, metrics::render(&runs)),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error rendering metrics: {e}\n"),
            ),
        },
        "/api/summary" => match cache.get_or_load(history_dir) {
            Ok(runs) => (StatusCode::OK, JSON, summary_json(&runs).to_string()),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error reading history: {e}\n"),
            ),
        },
        "/api/forms" | "/api/runs" => match cache.get_or_load(history_dir) {
            Ok(runs) => match serde_json::to_string_pretty(&*runs) {
                Ok(body) => (StatusCode::OK, JSON, body),
                Err(e) => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    TEXT,
                    format!("error encoding runs: {e}\n"),
                ),
            },
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error reading history: {e}\n"),
            ),
        },
        "/api/flakes" => match cache.get_or_load(history_dir) {
            Ok(runs) => (
                StatusCode::OK,
                JSON,
                flakes_json(history_dir, &runs).to_string(),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error reading history: {e}\n"),
            ),
        },
        "/api/trends" => match cache.get_or_load(history_dir) {
            Ok(runs) => (
                StatusCode::OK,
                JSON,
                trends_json(history_dir, &runs).to_string(),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error reading history: {e}\n"),
            ),
        },
        "/badge.svg" => match cache.get_or_load(history_dir) {
            Ok(runs) => (
                StatusCode::OK,
                "image/svg+xml; charset=utf-8",
                badge_svg(&runs),
            ),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error reading history: {e}\n"),
            ),
        },
        "/" => (StatusCode::OK, TEXT, INDEX.into()),
        _ => (StatusCode::NOT_FOUND, TEXT, "not found\n".into()),
    }
}

/// Readiness probe. `create_dir_all` alone is not enough: it returns `Ok`
/// for an existing directory that is *not writable*, so `/readyz` used to
/// report ready for exactly the misconfiguration it claims to catch.
/// Writing (and removing) a probe file actually exercises writability.
fn readiness(history_dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(history_dir)?;
    let probe = history_dir.join(".formwatch-readyz-probe");
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

async fn handle_with(
    req: Request<Incoming>,
    history_dir: PathBuf,
    cache: Arc<HistoryCache>,
    options: Arc<ServeOptions>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let authed = authorized(&options.api_token, &req);
    let path = req.uri().path().to_string();
    let (status, content_type, body) =
        route_with_auth(req.method(), &path, &history_dir, &cache, authed);
    let response = Response::builder()
        .status(status)
        .header("content-type", content_type)
        // A monitoring endpoint should never be cached by an intermediary,
        // and its (machine-readable) bodies must never be sniffed as HTML.
        .header("cache-control", "no-store")
        .header("x-content-type-options", "nosniff")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
    Ok(response)
}

/// Serves read-only endpoints on `addr` until interrupted (Ctrl-C).
///
/// Binding `:0` lets the OS choose a port; the chosen address is printed.
pub async fn run(history_dir: PathBuf, addr: SocketAddr) -> Result<()> {
    run_with(history_dir, addr, ServeOptions::default()).await
}

/// [`run`], honoring `options` (currently just the API bearer token).
pub async fn run_with(history_dir: PathBuf, addr: SocketAddr, options: ServeOptions) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr().context("reading local address")?;
    tracing::info!(%local, "formwatch serve listening");
    println!("formwatch serving (read-only) on http://{local}");
    println!(
        "  /healthz  /readyz  /metrics  /api/forms  /api/summary  /api/trends  /api/flakes  /badge.svg"
    );

    // Bound concurrent connections and the time a client may take to send
    // its headers, so a slow or malicious client can't exhaust the process
    // with unbounded half-open connections.
    const MAX_CONNECTIONS: usize = 64;
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let cache = Arc::new(HistoryCache::new(HISTORY_CACHE_TTL));

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => {
                        tracing::warn!(error = %e, "accept failed");
                        continue;
                    }
                };
                let permit = match limit.clone().try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        tracing::warn!(%peer, "connection limit reached; refusing connection");
                        continue;
                    }
                };
                let history_dir = history_dir.clone();
                let cache = cache.clone();
                let options = Arc::new(options.clone());
                tokio::spawn(async move {
                    let _permit = permit;
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| {
                        handle_with(req, history_dir.clone(), cache.clone(), options.clone())
                    });
                    if let Err(e) =
                        hyper::server::conn::http1::Builder::new()
                            .header_read_timeout(Duration::from_secs(10))
                            .serve_connection(io, service)
                            .await
                    {
                        tracing::debug!(%peer, error = %e, "connection ended");
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\nshutting down");
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::{CheckResult, Status};
    use crate::history::{RunResult, SCHEMA_VERSION};
    fn seeded_history(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("formwatch-test-serve-{tag}"));
        let _ = std::fs::remove_dir_all(&dir);
        history::save_run(
            &dir,
            &RunResult {
                schema_version: SCHEMA_VERSION,
                name: "Permit".into(),
                url: "https://city.gov/permit".into(),
                timestamp: 42,
                checks: vec![CheckResult {
                    name: "Accessibility".into(),
                    status: Status::Fail,
                    detail: "2 violations".into(),
                    screenshot: None,
                }],
            },
        )
        .expect("save run");
        dir
    }

    /// A cache that never actually caches, so existing tests see exactly
    /// the same "always fresh" behavior `route` had before the cache
    /// existed.
    fn no_cache() -> HistoryCache {
        HistoryCache::new(Duration::ZERO)
    }

    #[test]
    fn health_and_readiness_are_ok() {
        let dir = seeded_history("health");
        let cache = no_cache();
        let (status, _, body) = route(&Method::GET, "/healthz", &dir, &cache);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");
        let (status, _, _) = route(&Method::GET, "/readyz", &dir, &cache);
        assert_eq!(status, StatusCode::OK);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn readiness_fails_when_the_history_path_is_not_a_usable_directory() {
        let base = std::env::temp_dir().join("formwatch-test-serve-readyz-file");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        // A plain file where the history dir should be: create_dir_all
        // fails, so readiness must report 503.
        let file = base.join("history");
        std::fs::write(&file, b"x").expect("write file");
        let (status, _, _) = route(&Method::GET, "/readyz", &file, &no_cache());
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn metrics_endpoint_renders_prometheus() {
        let dir = seeded_history("metrics");
        let (status, content_type, body) = route(&Method::GET, "/metrics", &dir, &no_cache());
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("version=0.0.4"));
        assert!(body.contains("formwatch_forms 1"), "{body}");
        assert!(body.contains("formwatch_check_status{"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forms_endpoint_returns_json() {
        let dir = seeded_history("forms");
        let (status, content_type, body) = route(&Method::GET, "/api/forms", &dir, &no_cache());
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("application/json"));
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value[0]["url"], "https://city.gov/permit");
        assert_eq!(value[0]["checks"][0]["status"], "Fail");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn summary_endpoint_aggregates_counts_and_worst_status() {
        let dir = seeded_history("summary");
        let (status, content_type, body) = route(&Method::GET, "/api/summary", &dir, &no_cache());
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("application/json"));
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["forms"], 1);
        assert_eq!(value["fail"], 1);
        assert_eq!(value["forms_detail"][0]["worst"], "FAIL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn badge_reflects_the_worst_status() {
        let dir = seeded_history("badge");
        let svg = badge_svg(&history::all_known_forms(&dir).unwrap());
        assert!(svg.contains("fail 1"), "{svg}");
        assert!(svg.starts_with("<svg"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn guarded_endpoints_need_the_bearer_token() {
        let dir = seeded_history("auth");
        let cache = no_cache();
        // Probes stay open; data endpoints don't.
        let (status, _, _) = route_with_auth(&Method::GET, "/healthz", &dir, &cache, false);
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = route_with_auth(&Method::GET, "/api/forms", &dir, &cache, false);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let (status, _, _) = route_with_auth(&Method::GET, "/api/forms", &dir, &cache, true);
        assert_eq!(status, StatusCode::OK);
        let (status, _, _) = route_with_auth(&Method::GET, "/badge.svg", &dir, &cache, false);
        assert_eq!(status, StatusCode::UNAUTHORIZED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn flakes_endpoint_lists_only_flapping_checks() {
        use crate::checks::CheckResult;
        let dir = std::env::temp_dir().join("formwatch-test-serve-flakes");
        let _ = std::fs::remove_dir_all(&dir);
        for (ts, st) in [(1, Status::Pass), (2, Status::Fail), (3, Status::Pass)] {
            history::save_run(
                &dir,
                &RunResult {
                    schema_version: SCHEMA_VERSION,
                    name: "Permit".into(),
                    url: "https://city.gov/permit-flaky".into(),
                    timestamp: ts,
                    checks: vec![CheckResult {
                        name: "Accessibility".into(),
                        status: st,
                        detail: String::new(),
                        screenshot: None,
                    }],
                },
            )
            .expect("save run");
        }
        let (status, _, body) = route(&Method::GET, "/api/flakes", &dir, &no_cache());
        assert_eq!(status, StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["forms"][0]["flaky"][0]["check"], "Accessibility");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn trends_endpoint_reports_pass_rate() {
        let dir = seeded_history("trends");
        let (status, _, body) = route(&Method::GET, "/api/trends", &dir, &no_cache());
        assert_eq!(status, StatusCode::OK);
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value["forms"][0]["worst"], "FAIL");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_paths_and_write_methods_are_rejected() {
        let dir = seeded_history("unknown");
        let cache = no_cache();
        let (status, _, _) = route(&Method::GET, "/nope", &dir, &cache);
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Critically, nothing that mutates is served.
        let (status, _, _) = route(&Method::POST, "/api/forms", &dir, &cache);
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn metrics_are_cached_within_the_ttl_and_refreshed_after_it_expires() {
        let dir = seeded_history("cache-ttl");
        let cache = HistoryCache::new(Duration::from_millis(50));

        let (_, _, first) = route(&Method::GET, "/metrics", &dir, &cache);
        assert!(first.contains("formwatch_forms 1"), "{first}");

        // A second form's run lands on disk, but within the TTL the
        // cached result must still be served — proving this genuinely
        // avoids re-reading history, not just that it returns correct
        // data on a cold cache.
        history::save_run(
            &dir,
            &RunResult {
                schema_version: SCHEMA_VERSION,
                name: "Renewal".into(),
                url: "https://city.gov/renewal".into(),
                timestamp: 43,
                checks: vec![CheckResult {
                    name: "Accessibility".into(),
                    status: Status::Pass,
                    detail: String::new(),
                    screenshot: None,
                }],
            },
        )
        .expect("save second run");

        let (_, _, still_cached) = route(&Method::GET, "/metrics", &dir, &cache);
        assert!(
            still_cached.contains("formwatch_forms 1"),
            "expected the cached (stale) count within the TTL, got: {still_cached}"
        );

        std::thread::sleep(Duration::from_millis(60));
        let (_, _, refreshed) = route(&Method::GET, "/metrics", &dir, &cache);
        assert!(
            refreshed.contains("formwatch_forms 2"),
            "expected a fresh read after the TTL expired, got: {refreshed}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
