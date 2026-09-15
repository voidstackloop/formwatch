//! Environment self-check (`formwatch doctor`).
//!
//! A deployment that fails at 2am because the history directory isn't
//! writable, or because Chrome can't be found, is worse than one that
//! fails fast. `doctor` performs the cheap, local checks a run depends
//! on and reports each one — no network, no browser launch.

use std::path::{Path, PathBuf};

/// One environment check and its outcome.
pub struct Check {
    /// Short label (e.g. "Chrome").
    pub name: &'static str,
    /// Whether this check passed. Only a `false` here makes `doctor` exit
    /// non-zero.
    pub ok: bool,
    /// Human-readable explanation, including the remedy when `ok` is
    /// false.
    pub detail: String,
}

/// Inputs `doctor` needs to check the environment.
pub struct Context<'a> {
    /// The effective history directory.
    pub history_dir: &'a Path,
    /// The effective audit-log path, if one is configured.
    pub audit_log: Option<&'a Path>,
    /// The effective proxy URL, if one is configured.
    pub proxy: Option<&'a str>,
    /// Whether the authorized-use notice has been acknowledged.
    pub accepted: bool,
}

fn check(name: &'static str, ok: bool, detail: impl Into<String>) -> Check {
    Check {
        name,
        ok,
        detail: detail.into(),
    }
}

/// Probes whether `dir` can be created and written to, cleaning up after
/// itself. Pure enough to unit-test against temp directories.
fn write_probe(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let probe = dir.join(".formwatch-doctor-probe");
    std::fs::write(&probe, b"ok")?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

/// Runs every environment check.
pub fn run(ctx: &Context) -> Vec<Check> {
    let mut checks = Vec::new();

    // Chrome: a system install, a previously cached formwatch build, or
    // neither (formwatch will download one on first run) — never fatal.
    let detection = chromiumoxide::detection::DetectionOptions::default();
    match chromiumoxide::detection::default_executable(detection) {
        Ok(path) => checks.push(check(
            "Chrome",
            true,
            format!("system Chrome/Chromium at {}", path.display()),
        )),
        Err(_) if crate::browser::has_cached_chrome() => {
            let where_ = crate::browser::chrome_cache_dir()
                .map(|d| d.display().to_string())
                .unwrap_or_else(|| "the formwatch cache".to_string());
            checks.push(check(
                "Chrome",
                true,
                format!("using the cached Chrome build in {where_}"),
            ));
        }
        Err(_) => checks.push(check(
            "Chrome",
            true,
            "no Chrome found; one will be downloaded on first run",
        )),
    }

    match write_probe(ctx.history_dir) {
        Ok(()) => checks.push(check(
            "History directory",
            true,
            format!("{} is writable", ctx.history_dir.display()),
        )),
        Err(e) => checks.push(check(
            "History directory",
            false,
            format!("{} is not writable: {e}", ctx.history_dir.display()),
        )),
    }

    match ctx.audit_log {
        Some(path) => {
            // If the log file already exists, probe *it*: its parent can
            // be writable while the file itself is read-only, in which case
            // every later audit::append would fail despite a green doctor.
            let probe = if path.exists() {
                std::fs::OpenOptions::new()
                    .append(true)
                    .open(path)
                    .map(|_| ())
            } else {
                let parent: PathBuf = path
                    .parent()
                    .filter(|p| !p.as_os_str().is_empty())
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from("."));
                write_probe(&parent)
            };
            match probe {
                Ok(()) => checks.push(check(
                    "Audit log",
                    true,
                    format!("{} is writable", path.display()),
                )),
                Err(e) => checks.push(check(
                    "Audit log",
                    false,
                    format!("{} is not writable: {e}", path.display()),
                )),
            }
        }
        None => checks.push(check("Audit log", true, "not configured")),
    }

    checks.push(check(
        "Proxy",
        true,
        match ctx.proxy {
            Some(proxy) => format!("configured: {proxy}"),
            None => "not configured (direct connection)".to_string(),
        },
    ));

    checks.push(check(
        "Authorized use",
        true,
        if ctx.accepted {
            "notice acknowledged".to_string()
        } else {
            "notice not acknowledged; --submit is disabled until it is".to_string()
        },
    ));

    checks
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_probe_succeeds_and_cleans_up() {
        let dir = std::env::temp_dir().join("formwatch-test-doctor-probe");
        let _ = std::fs::remove_dir_all(&dir);
        write_probe(&dir).expect("probe");
        assert!(dir.exists());
        assert!(!dir.join(".formwatch-doctor-probe").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_probe_fails_when_a_parent_is_a_file() {
        let base = std::env::temp_dir().join("formwatch-test-doctor-probe-fail");
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).expect("mkdir");
        let file = base.join("not-a-dir");
        std::fs::write(&file, b"x").expect("write file");

        // `file/child` cannot be created because `file` is not a directory.
        assert!(write_probe(&file.join("child")).is_err());

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn run_reports_the_authorized_use_state() {
        let dir = std::env::temp_dir().join("formwatch-test-doctor-run");
        let _ = std::fs::remove_dir_all(&dir);
        let checks = run(&Context {
            history_dir: &dir,
            audit_log: None,
            proxy: None,
            accepted: false,
        });
        let legal = checks
            .iter()
            .find(|c| c.name == "Authorized use")
            .expect("legal check");
        assert!(legal.detail.contains("not acknowledged"));
        let history = checks
            .iter()
            .find(|c| c.name == "History directory")
            .expect("history check");
        assert!(history.ok);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
