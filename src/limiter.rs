//! Per-host request pacing.
//!
//! `monitor` already bounds global concurrency and can globally space
//! requests with `--delay-ms`, but neither keeps formwatch from hammering
//! a *single* host with several simultaneous page loads (the default
//! concurrency is 4, and a `forms.yml` often lists many forms on one
//! domain). [`HostPacer`] enforces a minimum interval between the *starts*
//! of requests to the same host, while letting different hosts run in
//! parallel.
//!
//! It reserves each future time slot **before** sleeping, so concurrent
//! callers for the same host are correctly serialized rather than all
//! reading the same "last request" value and then all sleeping the same
//! amount.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Enforces a minimum interval between request starts per host.
pub struct HostPacer {
    interval: Duration,
    next_allowed: Mutex<HashMap<String, Instant>>,
}

impl HostPacer {
    /// A pacer that spaces consecutive starts on the same host by at
    /// least `interval`. An interval of zero disables pacing entirely.
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_allowed: Mutex::new(HashMap::new()),
        }
    }

    /// Whether this pacer does anything at all.
    pub fn is_enabled(&self) -> bool {
        !self.interval.is_zero()
    }

    /// Waits until it's this host's turn. A no-op when the interval is
    /// zero. Never holds the lock across the sleep.
    pub async fn wait_turn(&self, host: &str) {
        if self.interval.is_zero() {
            return;
        }
        let now = Instant::now();
        let scheduled = {
            let mut map = self.next_allowed.lock().expect("host pacer mutex poisoned");
            let entry = map.entry(host.to_string()).or_insert(now);
            let scheduled = (*entry).max(now);
            *entry = scheduled + self.interval;
            scheduled
        };
        let delay = scheduled.saturating_duration_since(Instant::now());
        if !delay.is_zero() {
            tokio::time::sleep(delay).await;
        }
    }
}

/// Extracts the host from a URL for pacing purposes, without pulling in a
/// full URL parser. Returns `None` for values with no recognizable host
/// (e.g. a `file://` fixture or a malformed string), which callers treat
/// as "no pacing".
pub fn host_of(url: &str) -> Option<String> {
    let rest = match url.split_once("://") {
        Some((_, rest)) => rest,
        // No scheme: treat anything before the first '/' as the host.
        None => url,
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let authority = authority.rsplit('@').next().unwrap_or(authority);
    // Strip userinfo already handled; strip a trailing `:port`.
    let host = authority.split(':').next().unwrap_or("").trim();
    if host.is_empty() {
        None
    } else {
        Some(host.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_is_extracted_from_common_url_shapes() {
        assert_eq!(
            host_of("https://city.gov/apply?x=1"),
            Some("city.gov".to_string())
        );
        assert_eq!(
            host_of("http://city.gov:8080/a/b"),
            Some("city.gov".to_string())
        );
        assert_eq!(
            host_of("https://user:pass@city.gov/a"),
            Some("city.gov".to_string())
        );
        assert_eq!(host_of("city.gov/a"), Some("city.gov".to_string()));
        assert_eq!(host_of("file:///tmp/x.html"), None);
        assert_eq!(host_of(""), None);
    }

    #[tokio::test]
    async fn zero_interval_paces_nothing() {
        let pacer = HostPacer::new(Duration::ZERO);
        assert!(!pacer.is_enabled());
        let start = Instant::now();
        for _ in 0..3 {
            pacer.wait_turn("city.gov").await;
        }
        assert!(start.elapsed() < Duration::from_millis(100));
    }

    #[tokio::test]
    async fn same_host_calls_are_spaced_out() {
        let pacer = HostPacer::new(Duration::from_millis(40));
        let start = Instant::now();
        pacer.wait_turn("city.gov").await;
        pacer.wait_turn("city.gov").await;
        pacer.wait_turn("city.gov").await;
        assert!(
            start.elapsed() >= Duration::from_millis(80),
            "three starts at 40ms apart should take at least 80ms, took {:?}",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn different_hosts_do_not_block_each_other() {
        let pacer = HostPacer::new(Duration::from_millis(200));
        let start = Instant::now();
        pacer.wait_turn("a.gov").await;
        pacer.wait_turn("b.gov").await;
        pacer.wait_turn("c.gov").await;
        assert!(
            start.elapsed() < Duration::from_millis(100),
            "different hosts must not wait on each other, took {:?}",
            start.elapsed()
        );
    }
}
