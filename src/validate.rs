//! Validates a `forms.yml` registry without touching the network (unless
//! asked to probe).
//!
//! Catches the mistakes that waste a whole `monitor` run: duplicate form
//! names (history and reports key on them informally), duplicate URLs
//! (two entries silently sharing one history directory), non-HTTP(S)
//! URLs, empty names, and files with no usable forms at all.

use serde::Deserialize;

/// One problem found while validating a registry file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidationIssue {
    /// `error` (blocks a useful run) or `warning` (worth fixing).
    pub level: &'static str,
    /// Human-readable description, including the file and index.
    pub message: String,
}

#[derive(Debug, Deserialize)]
struct FormsFile {
    #[serde(default)]
    forms: Vec<FormEntry>,
}

#[derive(Debug, Deserialize)]
struct FormEntry {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    url: Option<String>,
}

/// Validates the parsed content of one registry file. `origin` is used in
/// messages (usually the file path). Pure: no I/O, no network.
pub fn validate_text(origin: &str, text: &str) -> Vec<ValidationIssue> {
    let mut issues = Vec::new();
    let parsed: Result<FormsFile, _> = serde_yaml::from_str(text);
    let file = match parsed {
        Ok(f) => f,
        Err(e) => {
            return vec![ValidationIssue {
                level: "error",
                message: format!("{origin}: not valid YAML: {e}"),
            }];
        }
    };
    if file.forms.is_empty() {
        issues.push(ValidationIssue {
            level: "error",
            message: format!("{origin}: no `forms:` entries found"),
        });
        return issues;
    }
    let mut seen_names = std::collections::HashSet::new();
    let mut seen_urls = std::collections::HashSet::new();
    for (i, form) in file.forms.iter().enumerate() {
        let at = format!("{origin} forms[{i}]");
        let name = form.name.as_deref().unwrap_or("").trim();
        let url = form.url.as_deref().unwrap_or("").trim();
        if name.is_empty() {
            issues.push(ValidationIssue {
                level: "error",
                message: format!("{at}: missing `name`"),
            });
        } else if !seen_names.insert(name.to_ascii_lowercase()) {
            issues.push(ValidationIssue {
                level: "warning",
                message: format!("{at}: duplicate form name {name:?}"),
            });
        }
        if url.is_empty() {
            issues.push(ValidationIssue {
                level: "error",
                message: format!("{at}: missing `url`"),
            });
        } else if !(url.starts_with("http://") || url.starts_with("https://")) {
            issues.push(ValidationIssue {
                level: "error",
                message: format!("{at}: URL {url:?} must start with http:// or https://"),
            });
        } else {
            if !seen_urls.insert(url.to_string()) {
                issues.push(ValidationIssue {
                    level: "warning",
                    message: format!("{at}: duplicate URL {url:?} (shares one history directory)"),
                });
            }
            if url.starts_with("http://") {
                issues.push(ValidationIssue {
                    level: "warning",
                    message: format!("{at}: plain-http URL {url:?} — prefer the https:// form"),
                });
            }
        }
    }
    issues
}

/// Outcome of probing one URL (`validate --probe`). Pure data; the fetch
/// itself lives in [`probe_urls`] so the rendering stays testable.
#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// The probed URL.
    pub url: String,
    /// `true` when the URL answered any HTTP response.
    pub reachable: bool,
    /// HTTP status code, when one was received.
    pub status: Option<u16>,
}

/// Extracts the distinct HTTP(S) URLs from already-validated files, so
/// `--probe` only re-requests URLs that passed syntactic validation.
pub fn http_urls_in(paths: &[std::path::PathBuf]) -> Vec<String> {
    let mut urls = Vec::new();
    for path in paths {
        let Ok(text) = std::fs::read_to_string(path) else {
            continue;
        };
        let Ok(file): Result<FormsFile, _> = serde_yaml::from_str(&text) else {
            continue;
        };
        for form in file.forms {
            if let Some(url) = form.url.map(|u| u.trim().to_string())
                && (url.starts_with("http://") || url.starts_with("https://"))
                && !urls.contains(&url)
            {
                urls.push(url);
            }
        }
    }
    urls
}

/// Sequentially GETs each URL with a short timeout, reporting one
/// [`ProbeResult`] per URL. Sequential (not concurrent) and header-only
/// in spirit: the body is discarded unread. A connection-level failure
/// is `reachable: false`; any HTTP response (even 404/500) is reachable
/// — the registry points *somewhere* real, even if that page needs a
/// closer look from `test`.
pub async fn probe_urls(urls: &[String]) -> Vec<ProbeResult> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => {
            return urls
                .iter()
                .map(|u| ProbeResult {
                    url: u.clone(),
                    reachable: false,
                    status: None,
                })
                .collect();
        }
    };
    let mut out = Vec::new();
    for url in urls {
        match client.get(url).send().await {
            Ok(resp) => out.push(ProbeResult {
                url: url.clone(),
                reachable: true,
                status: Some(resp.status().as_u16()),
            }),
            Err(e) => {
                tracing::warn!(url, error = %e, "probe failed");
                out.push(ProbeResult {
                    url: url.clone(),
                    reachable: false,
                    status: None,
                });
            }
        }
    }
    out
}

/// Validates every file in `paths`, returning all issues in order.
pub fn validate_files(paths: &[std::path::PathBuf]) -> Vec<ValidationIssue> {
    let mut all = Vec::new();
    for path in paths {
        let origin = path.display().to_string();
        match std::fs::read_to_string(path) {
            Ok(text) => all.extend(validate_text(&origin, &text)),
            Err(e) => all.push(ValidationIssue {
                level: "error",
                message: format!("{origin}: cannot read file: {e:#}"),
            }),
        }
    }
    all
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_clean_registry() {
        let text = "forms:\n  - name: Permit\n    url: https://city.gov/permit\n";
        assert!(validate_text("f.yml", text).is_empty());
    }

    #[test]
    fn flags_missing_names_bad_urls_and_duplicates() {
        let text = "forms:\n  - name: A\n    url: https://x.gov/a\n  - name: A\n    url: https://x.gov/a\n  - name: ''\n    url: gopher://x\n  - name: B\n    url: http://x.gov/b\n";
        let issues = validate_text("f.yml", text);
        assert!(
            issues
                .iter()
                .any(|i| i.level == "error" && i.message.contains("missing `name`"))
        );
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("duplicate form name"))
        );
        assert!(issues.iter().any(|i| i.message.contains("duplicate URL")));
        assert!(
            issues
                .iter()
                .any(|i| i.message.contains("must start with http"))
        );
        assert!(issues.iter().any(|i| i.message.contains("plain-http")));
    }

    #[test]
    fn http_urls_in_collects_only_valid_distinct_urls() {
        let dir = std::env::temp_dir().join("formwatch-test-validate-urls");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("forms.yml");
        std::fs::write(
            &path,
            "forms:\n  - name: A\n    url: https://x.gov/a\n  - name: B\n    url: https://x.gov/a\n  - name: C\n    url: gopher://x\n",
        )
        .unwrap();
        let urls = http_urls_in(&[path]);
        assert_eq!(urls, vec!["https://x.gov/a".to_string()]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rejects_empty_and_invalid_yaml() {
        assert!(
            validate_text("f.yml", "forms: []")
                .iter()
                .any(|i| i.level == "error")
        );
        assert!(
            validate_text("f.yml", "{{{")
                .iter()
                .any(|i| i.level == "error")
        );
    }
}
