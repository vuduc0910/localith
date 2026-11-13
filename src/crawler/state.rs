//! Crawl queue state, including resuming a previous run from its report and queue files.

use std::collections::{HashSet, VecDeque};
use std::path::Path;

use serde::{Deserialize, Serialize};
use url::Url;

use crate::crawler::extract_in_scope_urls;
use crate::types::{CrawlOptions, CrawlReport, CrawledPage, PageStatus};
use crate::util::now_iso;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct QueueItem {
    pub url: String,
    pub depth: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct SavedCrawlData {
    pub report: Option<CrawlReport>,
    pub queue: Option<Vec<QueueItem>>,
}

#[derive(Debug, Clone)]
pub struct CrawlState {
    pub queue: VecDeque<QueueItem>,
    pub queued: HashSet<String>,
    pub visited: HashSet<String>,
    pub pages: Vec<CrawledPage>,
    pub page_number: usize,
    pub skipped_by_robots: usize,
    pub skipped_by_auth: usize,
    pub skipped_by_pattern: usize,
    pub started_at: String,
    pub is_resumed: bool,
}

/// Reads report.json and queue.json from an existing run directory; unreadable files are ignored.
pub fn load_saved_crawl_data(run_dir: &Path) -> SavedCrawlData {
    fn read<T: serde::de::DeserializeOwned>(path: &Path) -> Option<T> {
        serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
    }
    SavedCrawlData {
        report: read(&run_dir.join("report.json")),
        queue: read(&run_dir.join("queue.json")),
    }
}

/// Starts a new crawl, or continues a previous one: crawled pages are kept and
/// the URLs that were still pending are queued again.
pub fn init_crawl_state(options: &CrawlOptions, saved: Option<&SavedCrawlData>) -> CrawlState {
    let Some(report) = saved.and_then(|saved| saved.report.as_ref()) else {
        return CrawlState {
            queue: VecDeque::from([QueueItem {
                url: options.start_url.clone(),
                depth: 0,
                pattern: None,
            }]),
            queued: HashSet::from([options.start_url.clone()]),
            visited: HashSet::new(),
            pages: Vec::new(),
            page_number: 0,
            skipped_by_robots: 0,
            skipped_by_auth: 0,
            skipped_by_pattern: 0,
            started_at: now_iso(),
            is_resumed: false,
        };
    };

    let pages = report.pages.clone();
    let mut visited: HashSet<String> = HashSet::new();
    for page in &pages {
        for url in [&page.url, &page.final_url] {
            if !url.is_empty() {
                visited.insert(url.clone());
            }
        }
    }

    let mut queue: VecDeque<QueueItem> = VecDeque::new();
    let mut queued = visited.clone();
    let saved_queue = saved
        .and_then(|saved| saved.queue.as_ref())
        .filter(|queue| !queue.is_empty());

    if let Some(saved_queue) = saved_queue {
        for item in saved_queue {
            if queued.insert(item.url.clone()) {
                queue.push_back(item.clone());
            }
        }
    } else if let Ok(start) = Url::parse(&options.start_url) {
        // No saved queue: rebuild it from the links of the crawled pages.
        for page in &pages {
            let Some(links) = page.links.as_ref() else {
                continue;
            };
            if page.depth >= options.max_depth || page.status != PageStatus::Ok {
                continue;
            }
            let found = extract_in_scope_urls(
                links,
                page.effective_url(),
                &start,
                options.include_subdomains,
            );
            for url in found {
                if queued.insert(url.clone()) {
                    queue.push_back(QueueItem {
                        url,
                        depth: page.depth + 1,
                        pattern: None,
                    });
                }
            }
        }
    }

    if queued.insert(options.start_url.clone()) {
        queue.push_front(QueueItem {
            url: options.start_url.clone(),
            depth: 0,
            pattern: None,
        });
    }

    let skipped = |predicate: &dyn Fn(&CrawledPage) -> bool| {
        pages
            .iter()
            .filter(|page| page.status == PageStatus::Skipped && predicate(page))
            .count()
    };
    let error_contains = |page: &CrawledPage, needle: &str| {
        page.error.as_deref().is_some_and(|e| e.contains(needle))
    };
    let totals = report.totals.as_ref();

    CrawlState {
        skipped_by_robots: totals
            .map(|t| t.skipped_by_robots)
            .unwrap_or_else(|| skipped(&|page| error_contains(page, "robots"))),
        skipped_by_auth: totals
            .map(|t| t.skipped_by_auth)
            .unwrap_or_else(|| skipped(&|page| page.auth_wall == Some(true))),
        skipped_by_pattern: totals
            .and_then(|t| t.skipped_by_pattern)
            .unwrap_or_else(|| skipped(&|page| error_contains(page, "Pattern"))),
        page_number: pages.len(),
        started_at: if report.started_at.is_empty() {
            now_iso()
        } else {
            report.started_at.clone()
        },
        queue,
        queued,
        visited,
        pages,
        is_resumed: true,
    }
}
