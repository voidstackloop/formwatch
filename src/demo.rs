//! Bundled demo site (`formwatch demo`).
//!
//! Serves a small, deliberately varied set of forms so people can see what
//! formwatch catches without pointing it at a real (and probably
//! unauthorized) website. The pages are compiled into the binary with
//! `include_str!`, so `cargo install` gets them too — there are no files to
//! copy and no `robots.txt` to worry about.
//!
//! The server is read-only and local-only by default, exactly like
//! [`crate::serve`], and exists purely as a target for humans and for
//! formwatch itself.

use anyhow::{Context, Result};
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use std::net::SocketAddr;
use tokio::net::TcpListener;

const HTML: &str = "text/html; charset=utf-8";
const CSS: &str = "text/css; charset=utf-8";
const TEXT: &str = "text/plain; charset=utf-8";

struct Page {
    path: &'static str,
    content_type: &'static str,
    body: &'static str,
}

/// Builds a [`Page`] from the file of the same name under `demo-sites/`.
macro_rules! page {
    ($path:literal) => {
        Page {
            path: $path,
            content_type: HTML,
            body: include_str!(concat!("../demo-sites", $path)),
        }
    };
}

const INDEX: &str = include_str!("../demo-sites/index.html");

const PAGES: &[Page] = &[
    Page {
        path: "/",
        content_type: HTML,
        body: INDEX,
    },
    Page {
        path: "/index.html",
        content_type: HTML,
        body: INDEX,
    },
    Page {
        path: "/demo.css",
        content_type: CSS,
        body: include_str!("../demo-sites/demo.css"),
    },
    // The clean baseline and the original headline demos.
    page!("/good-form.html"),
    page!("/missing-labels.html"),
    page!("/document-upload.html"),
    page!("/no-autocomplete.html"),
    page!("/tiny-targets.html"),
    page!("/lost-input.html"),
    page!("/multi-step.html"),
    page!("/captcha.html"),
    page!("/vague-errors.html"),
    // Accessibility.
    page!("/low-contrast.html"),
    page!("/missing-alt.html"),
    page!("/no-landmarks.html"),
    // Mobile usability.
    page!("/horizontal-overflow.html"),
    // Validation.
    page!("/aria-required-only.html"),
    page!("/no-required-fields.html"),
    page!("/checkbox-required-first.html"),
    // Required documents.
    page!("/good-upload.html"),
    page!("/no-format-guidance.html"),
    page!("/documents-prose-no-upload.html"),
    // Input persistence.
    page!("/session-timeout.html"),
    page!("/no-text-field.html"),
    // Submission flow.
    page!("/slow-step.html"),
    page!("/keyup-next.html"),
    page!("/icon-only-next.html"),
    page!("/no-submit-control.html"),
    page!("/shadow-form.html"),
    page!("/nested-shadow-form.html"),
    page!("/closed-shadow.html"),
    page!("/multi-form.html"),
    page!("/rtl-arabic.html"),
    // Bot protection.
    page!("/hcaptcha.html"),
    page!("/turnstile.html"),
    page!("/cloudflare-challenge.html"),
    page!("/generic-botcheck.html"),
    // LLM wording demos.
    page!("/instructions-unclear.html"),
];

/// Resolves a request to a `(status, content-type, body)` triple. Separate
/// from the socket plumbing so it can be unit-tested directly.
fn route(method: &Method, path: &str) -> (StatusCode, &'static str, &'static str) {
    if method != Method::GET {
        return (StatusCode::METHOD_NOT_ALLOWED, TEXT, "method not allowed\n");
    }
    match PAGES.iter().find(|p| p.path == path) {
        Some(page) => (StatusCode::OK, page.content_type, page.body),
        None => (StatusCode::NOT_FOUND, TEXT, "not found\n"),
    }
}

async fn handle(req: Request<Incoming>) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let (status, content_type, body) = route(req.method(), req.uri().path());
    let response = Response::builder()
        .status(status)
        .header("content-type", content_type)
        .header("cache-control", "no-store")
        .body(Full::new(Bytes::from(body)))
        .unwrap_or_else(|_| Response::new(Full::new(Bytes::new())));
    Ok(response)
}

/// Serves the demo site on `addr` until interrupted (Ctrl-C).
pub async fn run(addr: SocketAddr) -> Result<()> {
    let listener = TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    let local = listener.local_addr().context("reading local address")?;
    println!("formwatch demo site on http://{local}");
    println!();
    for page in PAGES {
        if page.path.ends_with(".html") && page.path != "/index.html" {
            println!("  http://{local}{}", page.path);
        }
    }
    println!();
    println!("  formwatch test http://{local}/good-form.html");
    println!("  formwatch monitor demo-sites/forms.yml   (with this running)");
    println!();
    println!("Press Ctrl-C to stop.");

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(e) => { tracing::warn!(error = %e, "accept failed"); continue; }
                };
                tokio::spawn(async move {
                    let io = TokioIo::new(stream);
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(io, service_fn(handle))
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

    #[test]
    fn known_pages_resolve_with_a_content_type() {
        for path in [
            "/",
            "/index.html",
            "/demo.css",
            "/good-form.html",
            "/lost-input.html",
            "/multi-step.html",
            "/closed-shadow.html",
            "/cloudflare-challenge.html",
        ] {
            let (status, content_type, body) = route(&Method::GET, path);
            assert_eq!(status, StatusCode::OK, "{path}");
            assert!(!body.is_empty(), "{path}");
            if path.ends_with(".css") {
                assert!(content_type.starts_with("text/css"), "{path}");
            } else {
                assert!(content_type.starts_with("text/html"), "{path}");
            }
        }
    }

    #[test]
    fn unknown_and_non_get_are_rejected() {
        let (status, _, _) = route(&Method::GET, "/nope.html");
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (status, _, _) = route(&Method::POST, "/good-form.html");
        assert_eq!(status, StatusCode::METHOD_NOT_ALLOWED);
    }

    #[test]
    fn index_links_to_every_demo_page() {
        for page in PAGES {
            if page.content_type == HTML
                && page.path.ends_with(".html")
                && page.path != "/"
                && page.path != "/index.html"
            {
                assert!(
                    INDEX.contains(page.path),
                    "index does not link to {}",
                    page.path
                );
            }
        }
    }
}
