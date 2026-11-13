//! Groups URLs that differ only by identifiers into one pattern, e.g.
//! `/report-issues/ISS-GPDPFI` -> `/report-issues/{{id}}`.

use std::collections::HashMap;
use std::sync::LazyLock;

use indexmap::IndexMap;
use regex::Regex;
use url::Url;

use crate::types::{CrawledPage, PageStatus, PatternCount};
use crate::util::normalize_url;

pub const ID_PLACEHOLDER: &str = "{{id}}";

fn regex(pattern: &str) -> Regex {
    Regex::new(pattern).expect("valid built-in regex")
}

static EMAIL: LazyLock<Regex> = LazyLock::new(|| {
    regex(
        r"^[a-zA-Z0-9.!#$%&'*+/=?^_`{|}~-]+@[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?(?:\.[a-zA-Z0-9](?:[a-zA-Z0-9-]*[a-zA-Z0-9])?)+$",
    )
});
static NUMBER: LazyLock<Regex> = LazyLock::new(|| regex(r"^\d+$"));
static UUID: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$"));
static HEX_HASH: LazyLock<Regex> = LazyLock::new(|| regex(r"(?i)^[0-9a-f]{16,64}$"));
static UPPER_KEY: LazyLock<Regex> = LazyLock::new(|| regex(r"^[A-Z]{2,10}-[A-Z0-9]+$"));
static PREFIXED_TOKEN: LazyLock<Regex> =
    LazyLock::new(|| regex(r"^[a-zA-Z]{2,10}[-_][a-zA-Z0-9]*\d[a-zA-Z0-9]*$"));
static NUMERIC_PREFIX_SLUG: LazyLock<Regex> = LazyLock::new(|| regex(r"^\d+-[a-zA-Z0-9-]+$"));
static NUMERIC_SUFFIX_SLUG: LazyLock<Regex> = LazyLock::new(|| regex(r"^[a-zA-Z0-9-]+-\d+$"));
static ID_QUERY_KEY: LazyLock<Regex> =
    LazyLock::new(|| regex(r"(?i)^(id|uuid|key|code|page|offset)$"));

/// Whether a path segment looks like an identifier, token or other dynamic value.
pub fn is_id_like_segment(segment: &str) -> bool {
    if segment.is_empty() {
        return false;
    }
    if EMAIL.is_match(segment)
        || NUMBER.is_match(segment)
        || UUID.is_match(segment)
        || HEX_HASH.is_match(segment)
        || UPPER_KEY.is_match(segment)
        || PREFIXED_TOKEN.is_match(segment)
        || NUMERIC_PREFIX_SLUG.is_match(segment)
        || NUMERIC_SUFFIX_SLUG.is_match(segment)
    {
        return true;
    }

    // Short token mixing letters and digits (e.g. "c7a8b9", "J74ENW").
    let length = segment.chars().count();
    (6..=32).contains(&length)
        && segment.chars().any(|c| c.is_ascii_digit())
        && segment.chars().any(|c| c.is_ascii_alphabetic())
}

/// Replaces dynamic path segments and query values with `{{id}}`.
pub fn extract_url_pattern(url_str: &str) -> String {
    let normalized = normalize_url(url_str, None).unwrap_or_else(|| url_str.to_string());
    let Ok(url) = Url::parse(&normalized) else {
        return url_str.to_string();
    };

    let segments: Vec<&str> = url
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(|segment| {
            if is_id_like_segment(segment) {
                ID_PLACEHOLDER
            } else {
                segment
            }
        })
        .collect();
    let path = format!("/{}", segments.join("/"));

    // ?id=123 -> ?id={{id}}; a repeated key keeps its first position and last value.
    let mut params: Vec<(String, String)> = Vec::new();
    for (key, value) in url.query_pairs() {
        let value = if is_id_like_segment(&value) || ID_QUERY_KEY.is_match(&key) {
            ID_PLACEHOLDER.to_string()
        } else {
            value.into_owned()
        };
        match params.iter_mut().find(|(existing, _)| *existing == key) {
            Some(entry) => entry.1 = value,
            None => params.push((key.into_owned(), value)),
        }
    }
    params.sort_by(|a, b| a.0.cmp(&b.0));

    let search = if params.is_empty() {
        String::new()
    } else {
        let encoded = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(params)
            .finish();
        format!("?{encoded}")
    };

    format!("{}{path}{search}", url.origin().ascii_serialization())
}

pub fn is_dynamic_pattern(pattern: &str) -> bool {
    pattern.contains(ID_PLACEHOLDER)
}

/// Counts crawled and pattern-skipped pages per pattern, in first-seen order.
pub fn compute_pattern_groups(pages: &[CrawledPage]) -> IndexMap<String, PatternCount> {
    let mut groups: IndexMap<String, PatternCount> = IndexMap::new();
    for page in pages {
        let Some(pattern) = page.pattern.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        let group = groups.entry(pattern.to_string()).or_default();
        match page.status {
            PageStatus::Ok => group.crawled += 1,
            PageStatus::Skipped if page.error.as_deref().is_some_and(|e| e.contains("Pattern")) => {
                group.skipped += 1;
            }
            _ => {}
        }
    }
    groups
}

/// Tracks how many pages of each dynamic pattern were crawled and enforces the sample cap.
#[derive(Debug, Clone)]
pub struct UrlPatternRegistry {
    counts: HashMap<String, usize>,
    max_per_pattern: usize,
}

impl Default for UrlPatternRegistry {
    fn default() -> Self {
        Self::new(3)
    }
}

impl UrlPatternRegistry {
    pub fn new(max_per_pattern: usize) -> Self {
        Self {
            counts: HashMap::new(),
            max_per_pattern,
        }
    }

    pub fn pattern(&self, url: &str) -> String {
        extract_url_pattern(url)
    }

    /// The cap only applies at depth > 1 and to patterns with a dynamic part.
    pub fn is_pattern_limit_reached(&self, url: &str, depth: u32) -> bool {
        if depth <= 1 {
            return false;
        }
        let pattern = self.pattern(url);
        is_dynamic_pattern(&pattern) && self.count(&pattern) >= self.max_per_pattern
    }

    pub fn record_crawled(&mut self, url: &str, depth: u32) -> String {
        let pattern = self.pattern(url);
        if depth > 1 && is_dynamic_pattern(&pattern) {
            *self.counts.entry(pattern.clone()).or_insert(0) += 1;
        }
        pattern
    }

    pub fn count(&self, pattern: &str) -> usize {
        self.counts.get(pattern).copied().unwrap_or(0)
    }
}
