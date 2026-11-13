use std::path::Path;

use anyhow::{Context, Result};
use chrono::{SecondsFormat, Utc};
use serde::de::DeserializeOwned;
use serde::Serialize;
use url::Url;

const TRACKING_PARAMS: &[&str] = &[
    "fbclid", "gclid", "dclid", "msclkid", "mc_cid", "mc_eid", "ref", "ref_src", "source", "igshid",
];

/// Current time as an ISO-8601 UTC timestamp with milliseconds (`2026-01-31T12:00:00.000Z`).
pub fn now_iso() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn env_var(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|value| !value.is_empty())
}

pub fn trim_trailing_slash(value: &str) -> String {
    value.strip_suffix('/').unwrap_or(value).to_string()
}

/// Canonical form of a link: http(s) only, no fragment, no tracking parameters,
/// no trailing slash, query parameters sorted. Returns `None` for unusable links.
pub fn normalize_url(raw: &str, base: Option<&str>) -> Option<String> {
    let mut url = match base {
        Some(base) => Url::parse(base).ok()?.join(raw).ok()?,
        None => Url::parse(raw).ok()?,
    };
    if !matches!(url.scheme(), "http" | "https") {
        return None;
    }
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }

    url.set_fragment(None);

    if url.query().is_some() {
        let mut pairs: Vec<(String, String)> = url
            .query_pairs()
            .filter(|(key, _)| {
                let key = key.to_lowercase();
                !key.starts_with("utm_") && !TRACKING_PARAMS.contains(&key.as_str())
            })
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        if pairs.is_empty() {
            url.set_query(None);
        } else {
            url.query_pairs_mut().clear().extend_pairs(pairs);
        }
    }

    // Treat /path and /path/ as the same page.
    let path = url.path().to_string();
    if path != "/" && path.ends_with('/') {
        url.set_path(path.trim_end_matches('/'));
    }

    Some(url.to_string())
}

pub fn is_in_scope(candidate: &str, start: &Url, include_subdomains: bool) -> bool {
    let Ok(url) = Url::parse(candidate) else {
        return false;
    };
    if !matches!(url.scheme(), "http" | "https") {
        return false;
    }
    let (Some(start_host), Some(candidate_host)) = (start.host_str(), url.host_str()) else {
        return false;
    };
    let start_host = start_host.to_lowercase();
    let candidate_host = candidate_host.to_lowercase();

    candidate_host == start_host
        || (include_subdomains && candidate_host.ends_with(&format!(".{start_host}")))
}

pub async fn write_json<T: Serialize + ?Sized>(path: &Path, data: &T) -> Result<()> {
    let text = serde_json::to_string_pretty(data)?;
    tokio::fs::write(path, text)
        .await
        .with_context(|| format!("Could not write {}", path.display()))
}

pub async fn read_json_file<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let raw = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("Could not read {}", path.display()))?;
    serde_json::from_str(&raw).with_context(|| format!("Invalid JSON in {}", path.display()))
}

/// Path of `file` relative to `run_dir`, always with forward slashes.
pub fn relative_report_path(run_dir: &Path, file: &Path) -> String {
    let relative = file.strip_prefix(run_dir).unwrap_or(file);
    relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
}

/// The first `max_chars` characters of `value`.
pub fn truncate_chars(value: &str, max_chars: usize) -> &str {
    match value.char_indices().nth(max_chars) {
        Some((index, _)) => &value[..index],
        None => value,
    }
}

/// Makes a value safe for a Markdown table cell.
pub fn escape_cell(value: &str) -> String {
    let escaped = value
        .replace('|', "\\|")
        .replace("\r\n", " ")
        .replace('\n', " ");
    truncate_chars(&escaped, 300).to_string()
}

pub fn format_duration(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

/// Newest sub-directory of `reports_dir` that contains `marker_file`.
pub fn find_latest_run_dir(reports_dir: &Path, marker_file: &str) -> Option<std::path::PathBuf> {
    let entries = std::fs::read_dir(reports_dir).ok()?;
    entries
        .filter_map(|entry| entry.ok())
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| {
            let modified = std::fs::metadata(entry.path().join(marker_file))
                .ok()?
                .modified()
                .ok()?;
            Some((modified, entry.path()))
        })
        .max_by_key(|(modified, _)| *modified)
        .map(|(_, path)| path)
}
