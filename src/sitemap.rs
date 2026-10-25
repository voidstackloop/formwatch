//! Sitemap-based form discovery (`formwatch sitemap`).
//!
//! Answers "what forms does this site even have?" before any browser
//! launches: fetches `/sitemap.xml` (following a sitemap index one level),
//! then optionally fetches each listed page and keeps the ones containing
//! a `<form`. Everything is polite (single-threaded fetches with a short
//! timeout) and read-only — no submissions, no fuzzing.

use anyhow::{Context, Result};

/// URLs found in a sitemap XML document (both `<urlset>` and `<sitemap>`
/// index entries — the caller decides whether to follow the latter).
/// Pure: no I/O, so discovery output stays testable without the network.
pub fn parse_sitemap_xml(xml: &str) -> Vec<String> {
    let mut urls = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find("<loc>") {
        let after = &rest[start + 5..];
        if let Some(end) = after.find("</loc>") {
            let url = after[..end].trim().to_string();
            if !url.is_empty() {
                urls.push(url);
            }
            rest = &after[end + 6..];
        } else {
            break;
        }
    }
    urls
}

/// True when the page HTML contains at least one `<form` tag (case
/// insensitive). Pure string scan — good enough for triage; the browser
/// checks remain the source of truth for what actually renders.
pub fn page_has_form(html: &str) -> bool {
    html.to_ascii_lowercase().contains("<form")
}

/// Extracts absolute form `action` targets from page HTML, resolving
/// relative actions against the page URL. Pure.
pub fn extract_form_actions(html: &str, page_url: &str) -> Vec<String> {
    let lower = html.to_ascii_lowercase();
    let mut actions = Vec::new();
    // Scan tags case-insensitively on the lowercased copy but slice
    // attribute values from the original (action paths are case sensitive).
    let html_lower_bytes = lower.as_bytes();
    let orig = html.as_bytes();
    let mut i = 0;
    while i + 5 < html_lower_bytes.len() {
        if &html_lower_bytes[i..i + 5] == b"<form" {
            let end = lower[i..]
                .find('>')
                .map(|e| i + e)
                .unwrap_or(html_lower_bytes.len());
            let tag_orig = &html[i..end.min(orig.len())];
            if let Some(action) = form_action_of(tag_orig) {
                actions.push(resolve_url(page_url, &action));
            } else {
                actions.push(page_url.to_string());
            }
            i = end;
        } else {
            i += 1;
        }
    }
    actions.sort();
    actions.dedup();
    actions
}

fn form_action_of(tag: &str) -> Option<String> {
    let lower = tag.to_ascii_lowercase();
    let pos = lower.find("action")?;
    let after = &tag[pos + 6..];
    let after = after.trim_start_matches(|c: char| c.is_whitespace());
    let after = after.strip_prefix('=')?.trim_start();
    let quote = after.chars().next()?;
    if quote == '"' || quote == '\'' {
        let end = after[1..].find(quote)?;
        Some(after[1..1 + end].to_string())
    } else {
        let end = after
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(after.len());
        Some(after[..end].to_string())
    }
}

fn resolve_url(base: &str, action: &str) -> String {
    if action.starts_with("http://") || action.starts_with("https://") {
        return action.to_string();
    }
    let Some(scheme_end) = base.find("://") else {
        return action.to_string();
    };
    let origin_end = base[scheme_end + 3..]
        .find('/')
        .map(|i| i + scheme_end + 3)
        .unwrap_or(base.len());
    let origin = &base[..origin_end];
    // Protocol-relative (//other.gov/path) inherits the page's scheme.
    if let Some(rest) = action.strip_prefix("//") {
        return format!("{}://{rest}", &base[..scheme_end]);
    }
    if let Some(a) = action.strip_prefix('/') {
        return format!("{origin}/{a}");
    }
    // Relative against the page's directory, collapsing ../ and ./
    // segments so nested forms resolve to canonical URLs.
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(origin);
    let mut parts: Vec<&str> = dir[origin.len()..]
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    for seg in action.split('/') {
        match seg {
            ".." => {
                parts.pop();
            }
            "." | "" => {}
            s => parts.push(s),
        }
    }
    if parts.is_empty() {
        return format!("{origin}/");
    }
    format!("{origin}/{}", parts.join("/"))
}

/// Sitemap `loc`s listed in a site's `robots.txt` (`Sitemap:` lines).
/// Pure: feeds [`sitemap_urls_for`] without network.
pub fn parse_robots_sitemaps(robots: &str) -> Vec<String> {
    robots
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.len() >= 8 && line[..8].eq_ignore_ascii_case("sitemap:") {
                let url = line[8..].trim().to_string();
                if !url.is_empty() { Some(url) } else { None }
            } else {
                None
            }
        })
        .collect()
}

/// Candidate sitemap URLs for a `sitemap` command argument: the argument
/// itself when it already points at XML, otherwise the conventional
/// `/sitemap.xml` plus any `Sitemap:` entries from `/robots.txt` (fetched
/// best-effort — a missing robots.txt just yields the conventional URL).
/// Pure apart from the robots.txt fetch, which callers may skip by
/// passing `robots = None`.
pub fn sitemap_urls_for(arg: &str, robots: Option<&str>) -> Vec<String> {
    if arg.trim_end().ends_with(".xml") {
        return vec![arg.to_string()];
    }
    let root = arg.trim_end_matches('/');
    let mut urls = vec![format!("{root}/sitemap.xml")];
    if let Some(body) = robots {
        for s in parse_robots_sitemaps(body) {
            if !urls.contains(&s) {
                urls.push(s);
            }
        }
    }
    urls
}

/// Fetches a sitemap (following an index one level) and returns candidate
/// page URLs, capped at `max_urls`. Network: plain GET with a short
/// timeout, one request for a `<urlset>` plus one per nested sitemap.
pub async fn discover_pages(sitemap_url: &str, max_urls: usize) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let xml = client
        .get(sitemap_url)
        .send()
        .await
        .with_context(|| format!("fetching {sitemap_url}"))?
        .error_for_status()?
        .text()
        .await?;
    let mut locs = parse_sitemap_xml(&xml);
    // A sitemap index is a sitemap whose entries are all (or mostly)
    // other sitemaps — detected by content, not by the URL containing
    // "sitemap", so custom paths like /index.xml work too.
    let xml_count = locs.iter().filter(|u| u.ends_with(".xml")).count();
    if !locs.is_empty() && xml_count * 2 >= locs.len() {
        let mut expanded = Vec::new();
        for loc in locs.iter().take(10) {
            if loc.ends_with(".xml") {
                match client.get(loc).send().await {
                    Ok(resp) => {
                        if let Ok(text) = resp.text().await {
                            expanded.extend(parse_sitemap_xml(&text));
                        }
                    }
                    Err(e) => {
                        tracing::warn!(sitemap = %loc, error = %e, "nested sitemap fetch failed")
                    }
                }
            } else {
                expanded.push(loc.clone());
            }
            if expanded.len() >= max_urls {
                break;
            }
        }
        locs = expanded;
    }
    locs.truncate(max_urls);
    Ok(locs)
}

/// Discovers candidate pages from a `sitemap` command argument: either a
/// sitemap URL directly, or a site root (whose `/robots.txt` is consulted
/// best-effort for `Sitemap:` entries plus the conventional
/// `/sitemap.xml`). Tries each candidate sitemap in order until one
/// fetches, then returns its pages capped at `max_urls`.
pub async fn discover_from_arg(arg: &str, max_urls: usize) -> Result<Vec<String>> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(20))
        .build()?;
    let robots = if arg.trim_end().ends_with(".xml") {
        None
    } else {
        let root = arg.trim_end_matches('/');
        match client.get(format!("{root}/robots.txt")).send().await {
            Ok(resp) => resp.text().await.ok(),
            Err(_) => None,
        }
    };
    let mut last_err = None;
    for candidate in sitemap_urls_for(arg, robots.as_deref()) {
        match discover_pages(&candidate, max_urls).await {
            Ok(pages) => return Ok(pages),
            Err(e) => last_err = Some((candidate, e)),
        }
    }
    if let Some((candidate, e)) = last_err {
        return Err(e)
            .with_context(|| format!("no sitemap fetchable for {arg} (tried {candidate})"));
    }
    Ok(vec![])
}

/// Keeps only the pages that actually contain a `<form>`. Polite:
/// sequential GETs with a short timeout.
pub async fn filter_to_forms(pages: &[String]) -> Vec<(String, Vec<String>)> {
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .build()
    {
        Ok(c) => c,
        Err(_) => return vec![],
    };
    let mut found = Vec::new();
    for page in pages {
        let actions = match client.get(page).send().await {
            Ok(resp) => match resp.text().await {
                Ok(html) if page_has_form(&html) => extract_form_actions(&html, page),
                _ => continue,
            },
            Err(e) => {
                tracing::warn!(page, error = %e, "page fetch failed; skipping");
                continue;
            }
        };
        found.push((page.clone(), actions));
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_urlset_and_index_locs() {
        let xml = r#"<?xml version="1.0"?><urlset><url><loc>https://x.gov/a</loc></url><url><loc>https://x.gov/b</loc></url></urlset>"#;
        assert_eq!(
            parse_sitemap_xml(xml),
            vec!["https://x.gov/a".to_string(), "https://x.gov/b".to_string()]
        );
    }

    #[test]
    fn resolves_relative_protocol_relative_and_dotdot_actions() {
        assert_eq!(
            resolve_url("https://x.gov/a/b/page", "../submit"),
            "https://x.gov/a/submit"
        );
        assert_eq!(
            resolve_url("https://x.gov/a/page", "//cdn.gov/f"),
            "https://cdn.gov/f"
        );
        assert_eq!(
            resolve_url("https://x.gov/a/page", "/root"),
            "https://x.gov/root"
        );
    }

    #[test]
    fn parses_robots_sitemaps_and_builds_candidates() {
        let robots = "User-agent: *\nSitemap: https://x.gov/s.xml\nsitemap: https://x.gov/o.xml\n";
        assert_eq!(
            parse_robots_sitemaps(robots),
            vec![
                "https://x.gov/s.xml".to_string(),
                "https://x.gov/o.xml".to_string()
            ]
        );
        assert_eq!(
            sitemap_urls_for("https://x.gov/i.xml", None),
            vec!["https://x.gov/i.xml".to_string()]
        );
        let cands = sitemap_urls_for("https://x.gov", Some(robots));
        assert!(cands.contains(&"https://x.gov/sitemap.xml".to_string()));
        assert!(cands.contains(&"https://x.gov/s.xml".to_string()));
    }

    #[test]
    fn detects_and_extracts_forms() {
        let html =
            r#"<html><body><FORM action="/apply"><input></form><form><input></form></body></html>"#;
        assert!(page_has_form(html));
        let actions = extract_form_actions(html, "https://x.gov/forms");
        assert!(actions.contains(&"https://x.gov/apply".to_string()));
        assert!(actions.contains(&"https://x.gov/forms".to_string()));
        assert!(!page_has_form("<html><body>no forms</body></html>"));
    }
}
