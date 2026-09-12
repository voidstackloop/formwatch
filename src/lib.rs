//! The library half of formwatch: everything the CLI (`src/main.rs`) is
//! a thin wrapper around, and what `tests/*.rs` exercise directly.
//!
//! Read these modules in the order a form actually flows through them:
//!
//! 1. [`browser`] launches headless Chrome and opens a page at a URL,
//!    downloading a Chrome build itself if none is found on `PATH`.
//! 2. [`checks`] is the check engine: [`checks::run_all`] runs every
//!    built-in check (submission flow, accessibility, mobile usability,
//!    validation errors, required documents, autofill hints, input
//!    persistence) against an open [`chromiumoxide::Page`], and
//!    [`checks::run_custom_checks`] runs any user-supplied `--checks-dir`
//!    scripts the same way.
//! 3. [`runner::run_one`] ties browser + checks + history together for a
//!    single form, converting even a page that never loaded into a
//!    proper recorded result rather than losing the run entirely.
//! 4. [`history`] persists and reloads each form's run history on disk,
//!    keyed by a collision-free encoding of its URL, and diffs
//!    consecutive runs to find what changed.
//! 5. [`report`] builds the cross-form report model (used by both the
//!    plain-text and `--html` output) from that history.
#![warn(missing_docs)]

/// Launches headless Chrome and opens pages.
pub mod browser;
/// The check engine: every built-in check, plus running `--checks-dir`
/// custom checks.
pub mod checks;
/// Persists and reloads run history on disk, and diffs consecutive runs.
pub mod history;
/// Builds the cross-form report model used by both plain-text and
/// `--html` output.
pub mod report;
/// Ties browser + checks + history together for a single form.
pub mod runner;
