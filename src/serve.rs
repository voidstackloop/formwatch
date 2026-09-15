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
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;

const TEXT: &str = "text/plain; charset=utf-8";
const PROMETHEUS: &str = "text/plain; version=0.0.4; charset=utf-8";
const JSON: &str = "application/json; charset=utf-8";

const INDEX: &str = "formwatch (read-only)\n\n  GET /healthz    liveness\n  GET /readyz     readiness\n  GET /metrics    Prometheus metrics\n  GET /api/forms  latest run of every form (JSON)\n";

/// Routes a request to a `(status, content-type, body)` triple. Kept
/// separate from the socket plumbing so it can be unit-tested directly.
fn route(method: &Method, path: &str, history_dir: &Path) -> (StatusCode, &'static str, String) {
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
        "/metrics" => match history::all_known_forms(history_dir) {
            Ok(runs) => (StatusCode::OK, PROMETHEUS, metrics::render(&runs)),
            Err(e) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                TEXT,
                format!("error rendering metrics: {e}\n"),
            ),
        },
        "/api/forms" | "/api/runs" => match history::all_known_forms(history_dir) {
            Ok(runs) => match serde_json::to_string_pretty(&runs) {
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

async fn handle(
    req: Request<Incoming>,
    history_dir: PathBuf,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let path = req.uri().path().to_string();
    let (status, content_type, body) = route(req.method(), &path, &history_dir);
    let response = Response::builder()
        .status(status)
        .header("content-type", content_type)
        // A monitoring endpoint should never be cached by an intermediary.
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
    Ok(response)
}

/// Serves read-only endpoints on `addr` until interrupted (Ctrl-C).
///
/// Binding `:0` lets the OS choose a port; the chosen address is printed.
pub async fn run(history_dir: PathBuf, addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr().context("reading local address")?;
    tracing::info!(%local, "formwatch serve listening");
    println!("formwatch serving (read-only) on http://{local}");
    println!("  /healthz  /readyz  /metrics  /api/forms");

    // Bound concurrent connections and the time a client may take to send
    // its headers, so a slow or malicious client can't exhaust the process
    // with unbounded half-open connections.
    const MAX_CONNECTIONS: usize = 64;
    let limit = Arc::new(Semaphore::new(MAX_CONNECTIONS));

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
                tokio::spawn(async move {
                    let _permit = permit;
                    let io = TokioIo::new(stream);
                    let service = service_fn(move |req| handle(req, history_dir.clone()));
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

    #[test]
    fn health_and_readiness_are_ok() {
        let dir = seeded_history("health");
        let (status, _, body) = route(&Method::GET, "/healthz", &dir);
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, "ok\n");
        let (status, _, _) = route(&Method::GET, "/readyz", &dir);
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
        let (status, _, _) = route(&Method::GET, "/readyz", &file);
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn metrics_endpoint_renders_prometheus() {
        let dir = seeded_history("metrics");
        let (status, content_type, body) = route(&Method::GET, "/metrics", &dir);
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("version=0.0.4"));
        assert!(body.contains("formwatch_forms 1"), "{body}");
        assert!(body.contains("formwatch_check_status{"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn forms_endpoint_returns_json() {
        let dir = seeded_history("forms");
        let (status, content_type, body) = route(&Method::GET, "/api/forms", &dir);
        assert_eq!(status, StatusCode::OK);
        assert!(content_type.contains("application/json"));
        let value: serde_json::Value = serde_json::from_str(&body).expect("valid JSON");
        assert_eq!(value[0]["url"], "https://city.gov/permit");
        assert_eq!(value[0]["checks"][0]["status"], "Fail");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn unknown_paths_and_write_methods_are_rejected() {
        let dir = seeded_history("unknown");
        let (status, _, _) = route(&Method::GET, "/nope", &dir);
        assert_eq!(status, StatusCode::NOT_FOUND);
        // Critically, nothing that mutates is served.
        let (status, _, _) = route(&Method::POST, "/api/forms", &dir);
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
