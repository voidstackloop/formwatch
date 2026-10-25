//! Append-only audit logging.
//!
//! Enterprise deployments often need a tamper-evident record of *what
//! formwatch did* — which URLs it touched, when, who ran it, and what the
//! outcome was — separate from the rich result history. Each completed run
//! is appended as one JSON line (JSON Lines), deliberately **without**
//! screenshots or other captured page content, so the audit trail itself
//! carries no personal data.
//!
//! ## Advanced format (v2)
//!
//! Entries are hash-chained: every line carries `prev_hash` (the
//! `entry_hash` of the line before it, or `"GENESIS"` for the first) and
//! `entry_hash` (SHA-256 over the canonical JSON of the entry with
//! `entry_hash` itself removed). `audit verify` recomputes the chain, so
//! a deleted or edited line breaks verification. Legacy v1 lines (no
//! hashes, written by older formwatch builds) verify as chain links via
//! the SHA-256 of their raw text — the chain picks up from there instead
//! of failing on old data.

use crate::checks::Status;
use crate::error::Result;
use crate::history::RunResult;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Version of the audit-line schema written by this build.
pub const AUDIT_SCHEMA_VERSION: u32 = 2;

/// Value used as `prev_hash` for the first entry in a log.
pub const GENESIS_HASH: &str = "GENESIS";

/// Who/what ran formwatch and how — attached to every audit entry so a
/// trail can answer "who touched this form, from where, with what flags".
#[derive(Debug, Clone)]
pub struct AuditContext {
    /// Which subcommand produced the run (`test` / `monitor`).
    pub command: String,
    /// Human that (probably) ran it: `$USER`/`$USERNAME`/`$LOGNAME`.
    pub actor: String,
    /// Machine it ran on: `$HOSTNAME` or `/etc/hostname`.
    pub hostname: String,
    /// Wall-clock time the run took, in milliseconds.
    pub duration_ms: u64,
    /// Whether `--submit` was on for this run.
    pub allow_submit: bool,
    /// Effective `--fail-on` policy (`fail` / `warn`).
    pub fail_on: String,
}

impl AuditContext {
    /// Captures the ambient actor/hostname plus caller-supplied run facts.
    pub fn capture(command: &str, duration_ms: u64, allow_submit: bool, fail_on: &str) -> Self {
        Self {
            command: command.to_string(),
            actor: current_actor(),
            hostname: current_hostname(),
            duration_ms,
            allow_submit,
            fail_on: fail_on.to_string(),
        }
    }

    /// A context for code paths that don't track timing/flags (tests,
    /// legacy callers). Chain and enrichment still apply.
    pub fn unknown() -> Self {
        Self::capture("unknown", 0, false, "fail")
    }
}

impl Default for AuditContext {
    fn default() -> Self {
        Self::unknown()
    }
}

/// Best-effort actor detection without extra dependencies.
fn current_actor() -> String {
    for key in ["USER", "USERNAME", "LOGNAME"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    "unknown".to_string()
}

/// Best-effort hostname detection without extra dependencies.
fn current_hostname() -> String {
    for key in ["HOSTNAME", "HOST", "COMPUTERNAME"] {
        if let Ok(v) = std::env::var(key) {
            let v = v.trim().to_string();
            if !v.is_empty() {
                return v;
            }
        }
    }
    if let Ok(v) = std::fs::read_to_string("/etc/hostname") {
        let v = v.trim().to_string();
        if !v.is_empty() {
            return v;
        }
    }
    "unknown".to_string()
}

/// Per-run outcome tallies, so dashboards can aggregate without joining
/// against history.
#[derive(Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct AuditCounts {
    /// How many checks passed in this run.
    pub pass: usize,
    /// How many checks warned in this run.
    pub warn: usize,
    /// How many checks failed in this run.
    pub fail: usize,
    /// Total checks in this run.
    pub total: usize,
}

impl AuditCounts {
    /// Tallies the check statuses in `run`.
    pub fn of(run: &RunResult) -> Self {
        let mut c = Self::default();
        for check in &run.checks {
            match check.status {
                Status::Pass => c.pass += 1,
                Status::Warn => c.warn += 1,
                Status::Fail => c.fail += 1,
            }
        }
        c.total = run.checks.len();
        c
    }

    /// Worst status as a lowercase outcome label (`fail`/`warn`/`pass`).
    pub fn outcome(&self) -> &'static str {
        if self.fail > 0 {
            "fail"
        } else if self.warn > 0 {
            "warn"
        } else {
            "pass"
        }
    }
}

/// The on-disk audit entry (v2). `entry_hash` is always computed last
/// and excluded from its own hash input.
#[derive(Serialize, Deserialize, Debug, Clone)]
struct AuditEntry {
    audit_version: u32,
    event: String,
    timestamp: i64,
    name: String,
    url: String,
    command: String,
    actor: String,
    hostname: String,
    formwatch_version: String,
    duration_ms: u64,
    allow_submit: bool,
    fail_on: String,
    counts: AuditCounts,
    outcome: String,
    checks: Vec<AuditCheckOwned>,
    prev_hash: String,
    #[serde(default)]
    entry_hash: String,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
struct AuditCheckOwned {
    name: String,
    status: Status,
}

/// SHA-256 hex of `text`.
pub fn sha256_hex(text: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(text.as_bytes());
    hex_of(&hasher.finalize())
}

fn hex_of(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Canonical bytes hashed for `entry_hash`: the entry serialized with
/// `entry_hash` set to empty. Field order is struct order (serde_json
/// preserves it), so recomputation is deterministic.
fn hash_input(entry: &AuditEntry) -> String {
    let mut clone = entry.clone();
    clone.entry_hash = String::new();
    serde_json::to_string(&clone).unwrap_or_default()
}

/// Computes `entry_hash` for an entry whose `prev_hash` is already set.
fn compute_entry_hash(entry: &AuditEntry) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash_input(entry).as_bytes());
    hex_of(&hasher.finalize())
}

/// Renders one audit line for `run` (no trailing newline) — pure, so it
/// can be tested without touching the filesystem.
///
/// Legacy shape (v1): timestamp, name, url, checks only. Kept so existing
/// consumers and the unit test below keep working; new code should prefer
/// [`line_for_with_context`].
pub fn line_for(run: &RunResult) -> Result<String> {
    let entry = serde_json::json!({
        "timestamp": run.timestamp,
        "name": run.name,
        "url": run.url,
        "checks": run.checks.iter().map(|c| serde_json::json!({
            "name": c.name,
            "status": c.status,
        })).collect::<Vec<_>>(),
    });
    Ok(serde_json::to_string(&entry)?)
}

/// Renders one v2 audit line for `run` under `ctx`, chained after
/// `prev_hash`. Pure and filesystem-free.
pub fn line_for_with_context(
    run: &RunResult,
    ctx: &AuditContext,
    prev_hash: &str,
) -> Result<String> {
    let counts = AuditCounts::of(run);
    let outcome = counts.outcome().to_string();
    let mut entry = AuditEntry {
        audit_version: AUDIT_SCHEMA_VERSION,
        event: "run_completed".to_string(),
        timestamp: run.timestamp,
        name: run.name.clone(),
        url: run.url.clone(),
        command: ctx.command.clone(),
        actor: ctx.actor.clone(),
        hostname: ctx.hostname.clone(),
        formwatch_version: env!("CARGO_PKG_VERSION").to_string(),
        duration_ms: ctx.duration_ms,
        allow_submit: ctx.allow_submit,
        fail_on: ctx.fail_on.clone(),
        counts,
        outcome,
        checks: run
            .checks
            .iter()
            .map(|c| AuditCheckOwned {
                name: c.name.clone(),
                status: c.status,
            })
            .collect(),
        prev_hash: prev_hash.to_string(),
        entry_hash: String::new(),
    };
    entry.entry_hash = compute_entry_hash(&entry);
    Ok(serde_json::to_string(&entry)?)
}

/// Reads the `entry_hash` (v2) or raw-line hash (v1 legacy) of the last
/// non-empty line in `path`. Missing/empty file yields [`GENESIS_HASH`].
fn prev_hash_for(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else {
        return GENESIS_HASH.to_string();
    };
    let Some(last) = text.lines().rev().find(|l| !l.trim().is_empty()) else {
        return GENESIS_HASH.to_string();
    };
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(last)
        && let Some(h) = v.get("entry_hash").and_then(|h| h.as_str())
        && !h.is_empty()
    {
        return h.to_string();
    }
    // Legacy v1 line or otherwise unhash-chained: chain on its content
    // hash so old and new entries stay in one verifiable sequence.
    sha256_hex(last)
}

/// Appends one audit line for `run` to `path`, creating parent
/// directories as needed. Append mode means concurrent processes (or
/// successive runs) never clobber one another. Uses an `unknown`
/// context; callers that track timing/flags should use
/// [`append_with_context`].
pub fn append(path: &Path, run: &RunResult) -> Result<()> {
    append_with_context(path, run, &AuditContext::unknown())?;
    Ok(())
}

/// How long a lock directory may sit untouched before it is treated as
/// left behind by a crashed holder and stolen. Long enough that no
/// healthy append (milliseconds of hashing and one write) ever trips
/// it, short enough that a crash doesn't wedge the log for long.
const LOCK_STALE_AFTER: Duration = Duration::from_secs(60);

/// Cross-process mutual exclusion around the read-hash-append sequence
/// in [`append_with_context`].
///
/// Append mode alone only guarantees bytes don't clobber: two writers
/// appending at once both read the *same* `prev_hash`, both link to it,
/// and the second entry fails verification — a forked chain from a
/// workflow this module otherwise blesses (a nightly cron `monitor`
/// overlapping a `watch` process on the same log, or two shards sharing
/// one `--audit-log`). The lock makes the whole read-link-append
/// sequence atomic across processes (and threads).
///
/// Implemented as a lock *directory* (`<log>.lock/`): directory creation
/// is atomic on every platform std supports — it either creates or fails
/// with `AlreadyExists`, which is the entire protocol. Steady-state cost
/// is two syscalls per append (create + remove).
fn with_audit_lock<T>(path: &Path, f: impl FnOnce() -> Result<T>) -> Result<T> {
    use std::io::ErrorKind;
    let mut lock = path.as_os_str().to_owned();
    lock.push(".lock");
    let lock = PathBuf::from(lock);
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match std::fs::create_dir(&lock) {
            Ok(()) => break,
            Err(e) if e.kind() == ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&lock)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().unwrap_or_default() > LOCK_STALE_AFTER)
                    .unwrap_or(false);
                if stale {
                    // Best-effort steal: if the holder is actually alive
                    // it recreates the directory and we simply wait again.
                    let _ = std::fs::remove_dir(&lock);
                    continue;
                }
                if Instant::now() >= deadline {
                    return Err(crate::error::Error::Audit(format!(
                        "timed out waiting for audit lock {} (another formwatch may be appending; retry the run)",
                        lock.display()
                    )));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                return Err(crate::error::Error::Audit(format!(
                    "could not take audit lock {}: {e}",
                    lock.display()
                )));
            }
        }
    }
    let result = f();
    // Always release, even when the append itself failed — a poisoned
    // lock would serialize every future append into the steal timeout.
    let _ = std::fs::remove_dir(&lock);
    result
}

/// [`append`] with full run context. Returns the new entry's hash.
pub fn append_with_context(path: &Path, run: &RunResult, ctx: &AuditContext) -> Result<String> {
    // Parent first: the lock directory itself lives beside the log, so
    // it cannot be created until the parent exists. (create_dir_all is
    // idempotent, so racing creators here are harmless.)
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    with_audit_lock(path, || {
        let prev = prev_hash_for(path);
        let line = line_for_with_context(run, ctx, &prev)?;
        let hash: String = serde_json::from_str::<serde_json::Value>(&line)?
            .get("entry_hash")
            .and_then(|h| h.as_str())
            .unwrap_or_default()
            .to_string();
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        writeln!(file, "{line}")?;
        Ok(hash)
    })
}

/// What [`verify`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Total non-empty lines examined.
    pub entries: usize,
    /// How many are v2 (hash-chained) entries.
    pub v2_entries: usize,
    /// How many are legacy v1 lines (no hashes, content-hashed in place).
    pub legacy_entries: usize,
    /// Whether the whole chain recomputes cleanly.
    pub ok: bool,
    /// 1-based line number of the first break, if any.
    pub broken_at: Option<usize>,
    /// Human-readable reason for the break, if any.
    pub reason: Option<String>,
}

/// Verifies the hash chain in `path`. Missing file verifies as empty
/// (0 entries, ok). A corrupt line or a `prev_hash`/`entry_hash` mismatch
/// fails with the offending 1-based line number.
pub fn verify(path: &Path) -> Result<VerifyReport> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(VerifyReport {
            entries: 0,
            v2_entries: 0,
            legacy_entries: 0,
            ok: true,
            broken_at: None,
            reason: None,
        });
    };
    let mut expected_prev = GENESIS_HASH.to_string();
    let mut entries = 0usize;
    let mut v2_entries = 0usize;
    let mut legacy_entries = 0usize;
    for (idx, line) in text.lines().filter(|l| !l.trim().is_empty()).enumerate() {
        let lineno = idx + 1;
        entries += 1;
        let value: serde_json::Value = serde_json::from_str(line).map_err(|e| {
            crate::error::Error::Audit(format!("line {lineno} is not valid JSON: {e}"))
        })?;
        let is_v2 = value
            .get("entry_hash")
            .and_then(|h| h.as_str())
            .is_some_and(|h| !h.is_empty());
        if !is_v2 {
            // Legacy line: it carries no hashes of its own, so it cannot
            // break the chain — it just moves the expected link forward
            // to its own content hash.
            legacy_entries += 1;
            expected_prev = sha256_hex(line);
            continue;
        }
        v2_entries += 1;
        let mut entry: AuditEntry = serde_json::from_str(line).map_err(|e| {
            crate::error::Error::Audit(format!("line {lineno} is not a valid audit entry: {e}"))
        })?;
        if entry.prev_hash != expected_prev {
            return Ok(VerifyReport {
                entries,
                v2_entries,
                legacy_entries,
                ok: false,
                broken_at: Some(lineno),
                reason: Some(format!(
                    "line {lineno}: prev_hash mismatch (log was edited, truncated, or reordered)"
                )),
            });
        }
        let stored = std::mem::take(&mut entry.entry_hash);
        let recomputed = compute_entry_hash(&entry);
        if stored != recomputed {
            return Ok(VerifyReport {
                entries,
                v2_entries,
                legacy_entries,
                ok: false,
                broken_at: Some(lineno),
                reason: Some(format!(
                    "line {lineno}: entry_hash mismatch (line was modified after writing)"
                )),
            });
        }
        expected_prev = stored;
    }
    Ok(VerifyReport {
        entries,
        v2_entries,
        legacy_entries,
        ok: true,
        broken_at: None,
        reason: None,
    })
}

/// Returns the last `n` audit entries as parsed JSON (oldest first).
/// Missing file yields an empty vec.
pub fn tail(path: &Path, n: usize) -> Result<Vec<serde_json::Value>> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Ok(vec![]);
    };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    let start = lines.len().saturating_sub(n.max(1));
    let mut out = Vec::new();
    for line in &lines[start..] {
        out.push(serde_json::from_str(line)?);
    }
    Ok(out)
}

/// Counts (pass/warn/fail/total) across the last `n` entries' embedded
/// tallies — a cheap fleet-wide summary without touching history.
pub fn tail_counts(path: &Path, n: usize) -> Result<AuditCounts> {
    let mut total = AuditCounts::default();
    for v in tail(path, n)? {
        if let Some(c) = v.get("counts") {
            total.pass += c.get("pass").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            total.warn += c.get("warn").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            total.fail += c.get("fail").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            total.total += c.get("total").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
        } else if let Some(checks) = v.get("checks").and_then(|c| c.as_array()) {
            // Legacy v1 line: derive counts from the checks array.
            for c in checks {
                match c.get("status").and_then(|s| s.as_str()).unwrap_or("") {
                    "Pass" => total.pass += 1,
                    "Warn" => total.warn += 1,
                    "Fail" => total.fail += 1,
                    _ => {}
                }
                total.total += 1;
            }
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::checks::CheckResult;

    fn run() -> RunResult {
        RunResult {
            schema_version: crate::history::SCHEMA_VERSION,
            name: "Permit".into(),
            url: "https://city.gov/permit".into(),
            timestamp: 42,
            checks: vec![
                CheckResult {
                    name: "Accessibility".into(),
                    status: Status::Fail,
                    detail: "2 violations".into(),
                    screenshot: Some("data:image/png;base64,AAAA".into()),
                },
                CheckResult {
                    name: "Mobile usability".into(),
                    status: Status::Pass,
                    detail: "ok".into(),
                    screenshot: None,
                },
            ],
        }
    }

    fn ctx() -> AuditContext {
        AuditContext {
            command: "test".into(),
            actor: "tester".into(),
            hostname: "ci-1".into(),
            duration_ms: 1234,
            allow_submit: false,
            fail_on: "fail".into(),
        }
    }

    #[test]
    fn audit_line_records_verdicts_but_never_screenshots() {
        let line = line_for(&run()).expect("render");
        assert!(line.contains("\"url\":\"https://city.gov/permit\""));
        assert!(line.contains("\"status\":\"Fail\""));
        assert!(line.contains("\"status\":\"Pass\""));
        assert!(
            !line.contains("data:image"),
            "the audit trail must not carry captured page content: {line}"
        );
    }

    #[test]
    fn append_writes_one_json_line_and_creates_parents() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-append");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("audit.jsonl");

        append(&path, &run()).expect("append");
        append(&path, &run()).expect("append again");

        let text = std::fs::read_to_string(&path).expect("read");
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "one line per run: {text}");
        assert!(serde_json::from_str::<serde_json::Value>(lines[0]).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn concurrent_appends_keep_one_verifiable_chain() {
        // The read-hash-append sequence used to race: two writers both
        // read the same prev_hash and forked the chain, failing later
        // verification. The lock must serialize them (this exercises the
        // same path cross-process writers take — a nightly cron `monitor`
        // overlapping `watch` on one --audit-log).
        use std::sync::Barrier;
        let dir = std::env::temp_dir().join("formwatch-test-audit-concurrent");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        let threads = 8;
        let per_thread = 10;
        let barrier = Barrier::new(threads);
        std::thread::scope(|s| {
            for _ in 0..threads {
                s.spawn(|| {
                    barrier.wait();
                    for _ in 0..per_thread {
                        append(&path, &run()).expect("concurrent append");
                    }
                });
            }
        });
        let report = verify(&path).expect("verify");
        assert!(report.ok, "chain must verify: {:?}", report.reason);
        assert_eq!(report.entries, threads * per_thread);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn v2_entry_carries_context_counts_and_hashes() {
        let line = line_for_with_context(&run(), &ctx(), GENESIS_HASH).expect("render");
        assert!(!line.contains("data:image"), "no page content: {line}");
        let v: serde_json::Value = serde_json::from_str(&line).expect("json");
        assert_eq!(v["audit_version"], 2);
        assert_eq!(v["event"], "run_completed");
        assert_eq!(v["command"], "test");
        assert_eq!(v["actor"], "tester");
        assert_eq!(v["hostname"], "ci-1");
        assert_eq!(v["counts"]["fail"], 1);
        assert_eq!(v["counts"]["pass"], 1);
        assert_eq!(v["outcome"], "fail");
        assert_eq!(v["prev_hash"], GENESIS_HASH);
        assert!(v["entry_hash"].as_str().is_some_and(|h| !h.is_empty()));
    }

    #[test]
    fn chained_appends_verify_cleanly() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-chain");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        append_with_context(&path, &run(), &ctx()).expect("append 1");
        append_with_context(&path, &run(), &ctx()).expect("append 2");
        let report = verify(&path).expect("verify");
        assert!(report.ok, "chain should verify: {report:?}");
        assert_eq!(report.entries, 2);
        assert_eq!(report.v2_entries, 2);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tampering_breaks_verification() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-tamper");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        append_with_context(&path, &run(), &ctx()).expect("append 1");
        append_with_context(&path, &run(), &ctx()).expect("append 2");
        // Modify the first line's outcome in place.
        let text = std::fs::read_to_string(&path).expect("read");
        let mut lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();
        lines[0] = lines[0].replace("\"outcome\":\"fail\"", "\"outcome\":\"pass\"");
        std::fs::write(&path, lines.join("\n") + "\n").expect("rewrite");
        let report = verify(&path).expect("verify");
        assert!(!report.ok, "tampered chain must not verify");
        assert_eq!(report.broken_at, Some(1));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn legacy_v1_lines_chain_forward() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-legacy");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        // A v1 line (no hashes) followed by a v2 line must still verify.
        append(&path, &run()).expect("legacy append");
        // Legacy `append` now writes v2, so simulate v1 by writing line_for directly.
        std::fs::write(&path, line_for(&run()).expect("v1") + "\n").expect("v1 line");
        append_with_context(&path, &run(), &ctx()).expect("v2 append");
        let report = verify(&path).expect("verify");
        assert!(report.ok, "mixed chain should verify: {report:?}");
        assert!(report.legacy_entries >= 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn tail_returns_the_most_recent_entries() {
        let dir = std::env::temp_dir().join("formwatch-test-audit-tail");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("audit.jsonl");
        for _ in 0..3 {
            append_with_context(&path, &run(), &ctx()).expect("append");
        }
        let last_two = tail(&path, 2).expect("tail");
        assert_eq!(last_two.len(), 2);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
