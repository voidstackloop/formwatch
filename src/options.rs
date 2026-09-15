//! Options that shape how a single run behaves.
//!
//! Kept separate from [`crate::config`] (which is serde-backed file/env
//! configuration) because these are resolved from several sources (CLI,
//! config file, environment) in `main` and then threaded through the
//! check engine and runner as plain, serialization-free values.

use crate::llm::LlmOptions;
use std::time::Duration;

/// How a single form's run should behave.
#[derive(Debug, Clone)]
pub struct RunOptions {
    /// Actually click the final submit (a real POST). Gated upstream by
    /// the authorized-use acknowledgement in [`crate::legal`].
    pub allow_submit: bool,
    /// Seconds to idle before the input-persistence check.
    pub wait_secs: u64,
    /// Whether non-Pass checks should capture a screenshot. Disable for
    /// privacy-sensitive deployments that must not store page contents.
    pub screenshots: bool,
    /// Base ceiling for a single check; the wizard and persistence checks
    /// scale this for their own legitimately-longer runs.
    pub check_timeout: Duration,
    /// Optional LLM-powered semantic checks (`--llm`).
    pub llm: Option<LlmOptions>,
}

impl Default for RunOptions {
    fn default() -> Self {
        Self {
            allow_submit: false,
            wait_secs: 5,
            screenshots: true,
            check_timeout: Duration::from_secs(20),
            llm: None,
        }
    }
}

impl RunOptions {
    /// The ceiling for walking a multi-step wizard. A form can legitimately
    /// have several steps, each waiting on its own transition, so the
    /// wizard gets a multiple of the base check timeout (3x, preserving
    /// the original 20s base -> 60s wizard relationship).
    pub fn wizard_timeout(&self) -> Duration {
        // `Duration * u32` panics on overflow; a huge `--check-timeout-secs`
        // must saturate, not take down the process.
        self.check_timeout.saturating_mul(3)
    }

    /// The ceiling for the input-persistence check: its own user-requested
    /// `--wait` plus room for the surrounding DOM work.
    pub fn persistence_timeout(&self) -> Duration {
        Duration::from_secs(self.wait_secs.saturating_add(self.check_timeout.as_secs()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_the_documented_cli_defaults() {
        let opts = RunOptions::default();
        assert!(!opts.allow_submit);
        assert_eq!(opts.wait_secs, 5);
        assert!(opts.screenshots);
        assert_eq!(opts.check_timeout, Duration::from_secs(20));
    }

    #[test]
    fn wizard_and_persistence_timeouts_scale_with_the_base() {
        let opts = RunOptions {
            wait_secs: 30,
            check_timeout: Duration::from_secs(20),
            ..RunOptions::default()
        };
        assert_eq!(opts.wizard_timeout(), Duration::from_secs(60));
        assert_eq!(opts.persistence_timeout(), Duration::from_secs(50));
    }

    #[test]
    fn an_enormous_timeout_saturates_instead_of_panicking() {
        let opts = RunOptions {
            check_timeout: Duration::from_secs(u64::MAX),
            ..RunOptions::default()
        };
        // Must not panic; the exact saturated value is not important.
        let _ = opts.wizard_timeout();
        let _ = opts.persistence_timeout();
    }
}
