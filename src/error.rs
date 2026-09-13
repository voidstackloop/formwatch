//! A typed error for formwatch's library surface.
//!
//! The original code used `anyhow::Error` throughout, which is a fine
//! choice for a binary but unhelpful for embedders: a downstream caller
//! can't match on *why* something failed, only print it. This module adds
//! a `thiserror`-derived [`Error`] so new (and progressively, existing)
//! fallible APIs can return structured errors, while
//! [`Error::Other`] wraps an `anyhow::Error` so the two interop freely
//! and nothing had to be rewritten in one risky sweep.

use thiserror::Error;

/// The library's structured error type.
#[derive(Debug, Error)]
pub enum Error {
    /// A configuration file or environment variable was invalid.
    #[error("configuration error: {0}")]
    Config(String),
    /// An action was refused because the operator hadn't acknowledged the
    /// authorized-use notice.
    #[error("authorization error: {0}")]
    Unauthorized(String),
    /// A regression notification couldn't be delivered.
    #[error("notification error: {0}")]
    Notify(String),
    /// An LLM-backed semantic check failed (configuration, transport, or an
    /// unparseable reply).
    #[error("LLM error: {0}")]
    Llm(String),
    /// An I/O operation failed.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// JSON (de)serialization failed.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// YAML (de)serialization failed.
    #[error(transparent)]
    Yaml(#[from] serde_yaml::Error),
    /// An HTTP request (e.g. a webhook delivery) failed.
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Any other error, carried through from the existing `anyhow`-based
    /// code so the two error styles compose without a rewrite.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Convenience alias for `Result<T, formwatch::error::Error>`.
pub type Result<T> = std::result::Result<T, Error>;
