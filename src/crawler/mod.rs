//! The crawl loop and its pure helpers.

pub mod report;
pub mod state;
pub mod url_pattern;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use regex::Regex;
use sha1::{Digest, Sha1};
use url::Url;

use crate::browser::Browser;
use crate::knowledge::{generate_crawl_knowledge, GenerateKnowledgeOptions};
use crate::robots::{load_robots_policy, RobotsPolicy};
use crate::types::{
    CrawlOptions, CrawlReport, CrawledPage, ExtractedLink, PageExtraction, PageStatus,
};
use crate::util::{
    format_duration, is_in_scope, normalize_url, now_iso, relative_report_path, truncate_chars,
    write_json,
};

use report::{build_crawl_report, build_markdown_report, BuildCrawlReportParams};
use state::{init_crawl_state, load_saved_crawl_data, QueueItem};
use url_pattern::{extract_url_pattern, is_dynamic_pattern, UrlPatternRegistry};

/// Time given to client-rendered pages to settle before the DOM is read.
const SETTLE_MS: u64 = 5000;

static AUTH_WALL_KEYWORDS: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)sign ?in|log ?in|log ?on|sign ?on|signon|create an? account|register|access denied|unauthorized|please (sign|log) ?in|enter your (email|password)|enter password|email address")
        .expect("valid built-in regex")
});
static LOGIN_PATH: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)(^|/)(login|sign ?in|signin|auth|sign ?on|signon|register|sign ?up|signup)(/|$)",
    )
    .expect("valid built-in regex")
});

// ---------------------------------------------------------------------------
// Pure helpers
// ---------------------------------------------------------------------------

/// `<host>-<8 hex chars of sha1(url)>`: stable per start URL, safe as a folder and session name.
pub fn safe_slug(value: &str) -> String {
    let host = Url::parse(value)
        .ok()
        .and_then(|url| url.host_str().map(str::to_string))
        .unwrap_or_default();
    let mut slug = String::new();
    for c in host.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.strip_prefix('-').unwrap_or(&slug);
    let slug = slug.strip_suffix('-').unwrap_or(slug);

    let digest = Sha1::digest(value.as_bytes());
    let hash: String = digest
        .iter()
        .take(4)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("{}-{hash}", if slug.is_empty() { "website" } else { slug })
}

pub fn create_run_directory(reports_dir: &Path, start_url: &str) -> PathBuf {
    reports_dir.join(safe_slug(start_url))
}

/// Milliseconds to wait so requests stay `effective_delay` apart. `last_request_at` is 0 before the first request.
pub fn calculate_wait_time(effective_delay: u64, last_request_at: u64, now: u64) -> u64 {
    if last_request_at == 0 {
        return 0;
    }
    effective_delay.saturating_sub(now.saturating_sub(last_request_at))
}

fn now_ms() -> u64 {
    chrono::Utc::now().timestamp_millis().max(0) as u64
}

pub fn get_page_prefix(page_number: usize) -> String {
    format!("page-{page_number:04}")
}

#[derive(Debug, Clone, PartialEq)]
pub struct PageArtifactPaths {
    pub snapshot_path: PathBuf,
    pub data_path: PathBuf,
    pub screenshot_path: PathBuf,
}

pub fn get_page_artifact_paths(pages_dir: &Path, page_prefix: &str) -> PageArtifactPaths {
    PageArtifactPaths {
        snapshot_path: pages_dir.join(format!("{page_prefix}.snapshot.txt")),
        data_path: pages_dir.join(format!("{page_prefix}.json")),
        screenshot_path: pages_dir.join(format!("{page_prefix}.png")),
    }
}

/// Normalizes links and keeps the ones inside the crawl scope, without duplicates.
pub fn extract_in_scope_urls(
    links: &[ExtractedLink],
    base_url: &str,
    start_url: &Url,
    include_subdomains: bool,
) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut in_scope = Vec::new();
    for link in links {
        let Some(normalized) = normalize_url(&link.href, Some(base_url)) else {
            continue;
        };
        if is_in_scope(&normalized, start_url, include_subdomains)
            && seen.insert(normalized.clone())
        {
            in_scope.push(normalized);
        }
    }
    in_scope
}

/// Queue entries for the links of a page that are not known yet.
pub fn get_next_queue_items(
    links: &[ExtractedLink],
    base_url: &str,
    start_url: &Url,
    include_subdomains: bool,
    current_depth: u32,
    is_known: impl Fn(&str) -> bool,
) -> Vec<QueueItem> {
    extract_in_scope_urls(links, base_url, start_url, include_subdomains)
        .into_iter()
        .filter(|url| !is_known(url))
        .map(|url| QueueItem {
            pattern: Some(extract_url_pattern(&url)),
            url,
            depth: current_depth + 1,
        })
        .collect()
}

/// A page is an auth wall when its URL looks like a login route, or it has a
/// password field together with sign-in wording.
pub fn detect_auth_wall(extraction: &PageExtraction, final_url: &str) -> bool {
    let looks_like_login_url = Url::parse(final_url)
        .map(|url| LOGIN_PATH.is_match(&url.path().to_lowercase()))
        .unwrap_or(false);
    if looks_like_login_url {
        return true;
    }
    if extraction.password_fields == 0 {
        return false;
    }
    let mut text = format!("{} {}", extraction.title, extraction.description);
    for heading in &extraction.headings {
        text.push(' ');
        text.push_str(&heading.text);
    }
    text.push(' ');
    text.push_str(&extraction.text);
    AUTH_WALL_KEYWORDS.is_match(&text)
}

fn skipped_page(url: &str, final_url: &str, depth: u32, error: String) -> CrawledPage {
    CrawledPage {
        url: url.to_string(),
        final_url: final_url.to_string(),
        depth,
        status: PageStatus::Skipped,
        error: Some(error),
        crawled_at: now_iso(),
        ..CrawledPage::default()
    }
}

pub fn create_robots_skipped_page(url: &str, depth: u32) -> CrawledPage {
    skipped_page(url, url, depth, "Disallowed by robots.txt".to_string())
}

pub fn create_pattern_skipped_page(url: &str, depth: u32, pattern: &str) -> CrawledPage {
    CrawledPage {
        pattern: Some(pattern.to_string()),
        ..skipped_page(
            url,
            url,
            depth,
            format!("Pattern limit reached for {pattern}"),
        )
    }
}

pub fn create_auth_skipped_page(
    url: &str,
    final_url: &str,
    depth: u32,
    error: &str,
    auth_handled: bool,
) -> CrawledPage {
    CrawledPage {
        auth_wall: Some(true),
        auth_handled: auth_handled.then_some(true),
        ..skipped_page(url, final_url, depth, error.to_string())
    }
}

pub fn create_error_page(url: &str, depth: u32, message: &str) -> CrawledPage {
    CrawledPage {
        url: url.to_string(),
        final_url: url.to_string(),
        depth,
        status: PageStatus::Error,
        error: Some(message.to_string()),
        crawled_at: now_iso(),
        ..CrawledPage::default()
    }
}

pub struct SuccessPageParams<'a> {
    pub url: &'a str,
    pub final_url: &'a str,
    pub depth: u32,
    pub extraction: &'a PageExtraction,
    pub run_dir: &'a Path,
    pub snapshot_path: &'a Path,
    pub data_path: &'a Path,
    pub auth_wall: bool,
    pub auth_handled: bool,
    pub pattern: Option<String>,
}

pub fn create_success_page(params: SuccessPageParams<'_>) -> CrawledPage {
    let extraction = params.extraction;
    CrawledPage {
        url: params.url.to_string(),
        final_url: params.final_url.to_string(),
        depth: params.depth,
        status: PageStatus::Ok,
        title: Some(extraction.title.clone()),
        description: Some(extraction.description.clone()),
        canonical: Some(extraction.canonical.clone()),
        headings: Some(extraction.headings.clone()),
        links: Some(extraction.links.clone()),
        links_found: Some(extraction.links.len()),
        text: Some(extraction.text.clone()),
        snapshot_file: Some(relative_report_path(params.run_dir, params.snapshot_path)),
        data_file: Some(relative_report_path(params.run_dir, params.data_path)),
        auth_wall: Some(params.auth_wall),
        auth_handled: params.auth_handled.then_some(true),
        pattern: params.pattern,
        crawled_at: now_iso(),
        ..CrawledPage::default()
    }
}

// ---------------------------------------------------------------------------
// Crawl orchestration
// ---------------------------------------------------------------------------

struct AuthResolution {
    success: bool,
    auth_handled: bool,
    extraction: PageExtraction,
    final_url: String,
    snapshot: String,
}

async fn handle_auth_wall(
    browser: &Browser,
    options: &CrawlOptions,
    extraction: PageExtraction,
    final_url: String,
    snapshot: String,
) -> Result<AuthResolution> {
    println!("   🔐 Auth wall detected, attempting sign-in...");
    let login = browser
        .attempt_login(
            options.auth.email.as_deref().unwrap_or_default(),
            options.auth.password.as_deref().unwrap_or_default(),
        )
        .await;

    let mut resolution = AuthResolution {
        success: false,
        auth_handled: login.submitted,
        extraction,
        final_url,
        snapshot,
    };

    // Give the post-login navigation time to complete, then look again.
    for _ in 0..3 {
        tokio::time::sleep(Duration::from_millis(2000)).await;
        let recaptured = browser
            .capture_page(
                options.max_snapshot_chars,
                options.max_page_text_chars,
                None,
            )
            .await?;
        let new_url = normalize_url(&recaptured.final_url, None)
            .unwrap_or_else(|| resolution.final_url.clone());
        if !detect_auth_wall(&recaptured.extraction, &new_url) {
            resolution.extraction = recaptured.extraction;
            resolution.final_url = new_url;
            resolution.snapshot = recaptured.snapshot;
            resolution.success = true;
            break;
        }
    }
    Ok(resolution)
}

enum PageOutcome {
    Crawled { title: String, links: usize },
    AuthSkipped,
}

struct CrawlRun<'a> {
    options: &'a CrawlOptions,
    browser: &'a Browser,
    start: Url,
    run_dir: PathBuf,
    pages_dir: PathBuf,
    auth_state_path: PathBuf,
    queue_path: PathBuf,
    state: state::CrawlState,
    registry: UrlPatternRegistry,
}

impl CrawlRun<'_> {
    fn ok_pages(&self) -> usize {
        self.state
            .pages
            .iter()
            .filter(|p| p.status == PageStatus::Ok)
            .count()
    }

    /// Opens one page, records it and queues its links.
    async fn visit(&mut self, item: &QueueItem, last_request_at: &mut u64) -> Result<PageOutcome> {
        let options = self.options;
        let paths =
            get_page_artifact_paths(&self.pages_dir, &get_page_prefix(self.state.page_number));

        self.browser
            .open(&item.url, options.page_timeout_ms)
            .await?;
        *last_request_at = now_ms();
        tokio::time::sleep(Duration::from_millis(SETTLE_MS)).await;

        let captured = self
            .browser
            .capture_page(
                options.max_snapshot_chars,
                options.max_page_text_chars,
                options
                    .screenshot
                    .then_some(paths.screenshot_path.as_path()),
            )
            .await?;

        let mut final_url =
            normalize_url(&captured.final_url, None).unwrap_or_else(|| item.url.clone());
        let mut extraction = captured.extraction;
        let mut snapshot = captured.snapshot;
        let mut auth_wall = false;
        let mut auth_handled = false;

        if detect_auth_wall(&extraction, &final_url) {
            auth_wall = true;
            if !options.auth.enabled {
                self.state.skipped_by_auth += 1;
                self.state.pages.push(create_auth_skipped_page(
                    &item.url,
                    &final_url,
                    item.depth,
                    "Auth wall detected (re-run with --auth and AUTH_EMAIL/AUTH_PASSWORD to proceed)",
                    false,
                ));
                println!("🔐 [auth wall] {}", item.url);
                return Ok(PageOutcome::AuthSkipped);
            }

            let resolved =
                handle_auth_wall(self.browser, options, extraction, final_url, snapshot).await?;
            auth_handled = resolved.auth_handled;
            if !resolved.success {
                self.state.skipped_by_auth += 1;
                self.state.pages.push(create_auth_skipped_page(
                    &item.url,
                    &resolved.final_url,
                    item.depth,
                    "Auth wall: sign-in did not succeed",
                    auth_handled,
                ));
                println!("   ✗ Sign-in did not succeed");
                return Ok(PageOutcome::AuthSkipped);
            }

            extraction = resolved.extraction;
            final_url = resolved.final_url;
            snapshot = resolved.snapshot;
            println!("   ✓ Sign-in completed, continuing crawl");
            if self.browser.save_state(&self.auth_state_path).await {
                println!("   💾 Saved session state to auth-state.json");
            }
        }

        tokio::fs::write(&paths.snapshot_path, &snapshot)
            .await
            .with_context(|| format!("Could not write {}", paths.snapshot_path.display()))?;

        let pattern = self.registry.record_crawled(&item.url, item.depth);
        let record = create_success_page(SuccessPageParams {
            url: &item.url,
            final_url: &final_url,
            depth: item.depth,
            extraction: &extraction,
            run_dir: &self.run_dir,
            snapshot_path: &paths.snapshot_path,
            data_path: &paths.data_path,
            auth_wall,
            auth_handled,
            pattern: is_dynamic_pattern(&pattern).then_some(pattern),
        });

        let mut data = serde_json::to_value(&record)?;
        if let Some(object) = data.as_object_mut() {
            object.insert("snapshot".to_string(), serde_json::Value::String(snapshot));
        }
        write_json(&paths.data_path, &data).await?;
        self.state.pages.push(record);

        if item.depth < options.max_depth {
            let next = get_next_queue_items(
                &extraction.links,
                &final_url,
                &self.start,
                options.include_subdomains,
                item.depth,
                |url| self.state.queued.contains(url) || self.state.visited.contains(url),
            );
            for item in next {
                self.state.queued.insert(item.url.clone());
                self.state.queue.push_back(item);
            }
        }

        // Persist the queue after each page so an interrupted scan can resume.
        write_json(&self.queue_path, &self.state.queue).await?;

        Ok(PageOutcome::Crawled {
            title: extraction.title,
            links: extraction.links.len(),
        })
    }

    async fn run_loop(&mut self, robots: &RobotsPolicy, effective_delay: u64) {
        let options = self.options;
        let mut last_request_at: u64 = 0;

        while self.ok_pages() < options.max_pages {
            let Some(item) = self.state.queue.pop_front() else {
                break;
            };
            if !self.state.visited.insert(item.url.clone()) {
                continue;
            }

            if !robots.is_allowed(&item.url) {
                self.state.skipped_by_robots += 1;
                self.state
                    .pages
                    .push(create_robots_skipped_page(&item.url, item.depth));
                println!("⏭  [robots] {}", item.url);
                continue;
            }

            if self
                .registry
                .is_pattern_limit_reached(&item.url, item.depth)
            {
                let pattern = self.registry.pattern(&item.url);
                self.state.skipped_by_pattern += 1;
                self.state
                    .pages
                    .push(create_pattern_skipped_page(&item.url, item.depth, &pattern));
                println!("⏭  [pattern cap] {} -> {pattern}", item.url);
                continue;
            }

            let wait = calculate_wait_time(effective_delay, last_request_at, now_ms());
            if wait > 0 {
                tokio::time::sleep(Duration::from_millis(wait)).await;
            }

            self.state.page_number += 1;
            let page_started = Instant::now();
            println!(
                "🌐 [{}/{}] depth={} {}",
                self.ok_pages() + 1,
                options.max_pages,
                item.depth,
                item.url
            );

            match self.visit(&item, &mut last_request_at).await {
                Ok(PageOutcome::Crawled { title, links }) => println!(
                    "   ✓ {} | {links} links | {}",
                    if title.is_empty() {
                        "(no title)"
                    } else {
                        &title
                    },
                    format_duration(page_started.elapsed().as_millis() as u64)
                ),
                Ok(PageOutcome::AuthSkipped) => {}
                Err(err) => {
                    let message = format!("{err:#}");
                    println!("   ✗ {}", truncate_chars(&message, 300));
                    self.state
                        .pages
                        .push(create_error_page(&item.url, item.depth, &message));
                }
            }
        }
    }
}

/// Crawls a site into `reports/<slug>/` under the current directory and returns the report.
pub async fn crawl(options: &CrawlOptions, mut browser: Browser) -> Result<CrawlReport> {
    let start = Url::parse(&options.start_url)?;
    let reports_dir = std::env::current_dir()?.join("reports");
    let run_dir = create_run_directory(&reports_dir, &options.start_url);
    let pages_dir = run_dir.join("pages");
    tokio::fs::create_dir_all(&pages_dir).await?;

    let auth_state_path = run_dir.join("auth-state.json");
    let queue_path = run_dir.join("queue.json");

    // Continue the previous scan of this URL unless --fresh was given.
    let saved = (!options.fresh).then(|| load_saved_crawl_data(&run_dir));
    let state = init_crawl_state(options, saved.as_ref());

    let robots = if options.respect_robots {
        load_robots_policy(&options.start_url).await
    } else {
        RobotsPolicy::allow_all()
    };
    let effective_delay = options.delay_ms.max(robots.crawl_delay_ms);

    let mut registry = UrlPatternRegistry::new(options.max_pages_per_pattern);
    for page in &state.pages {
        if page.status == PageStatus::Ok {
            registry.record_crawled(&page.url, page.depth);
        }
    }

    let session_restored = browser
        .restore_session(&safe_slug(&options.start_url), &auth_state_path)
        .await;

    let already_crawled = state
        .pages
        .iter()
        .filter(|p| p.status == PageStatus::Ok)
        .count();
    println!(
        "\n🔎 Local Website Scanner{}",
        if state.is_resumed {
            " (Resuming Scan)"
        } else {
            ""
        }
    );
    println!("   Start: {}", options.start_url);
    if state.is_resumed {
        println!(
            "   Resuming: {already_crawled} previously crawled pages | Pending queue: {} URLs",
            state.queue.len()
        );
    }
    println!(
        "   Depth limit: {} | Target max pages: {} (Remaining: {})",
        options.max_depth,
        options.max_pages,
        options.max_pages.saturating_sub(already_crawled)
    );
    println!(
        "   Pattern sampling: max {} pages per dynamic pattern group at depth > 1",
        options.max_pages_per_pattern
    );
    println!(
        "   AI: {}",
        if options.generate_knowledge {
            &options.ollama_model
        } else {
            "disabled"
        }
    );
    println!(
        "   robots.txt: {}",
        if options.respect_robots {
            "respected"
        } else {
            "ignored"
        }
    );
    println!(
        "   Auth wall handling: {}",
        if options.auth.enabled {
            "enabled"
        } else {
            "disabled"
        }
    );
    if session_restored {
        println!("   Session: Restored saved authentication state (auth-state.json)");
    }
    println!("   Output: {}\n", run_dir.display());

    if already_crawled >= options.max_pages {
        println!(
            "ℹ️ Already reached limit of {} pages ({already_crawled} successful crawls recorded).",
            options.max_pages
        );
        if !state.queue.is_empty() {
            println!(
                "   There are {} pending uncrawled URLs in the queue.",
                state.queue.len()
            );
            println!("   To crawl more pages, re-run with a higher --max-pages value.\n");
        }
    }

    let mut run = CrawlRun {
        options,
        browser: &browser,
        start,
        run_dir: run_dir.clone(),
        pages_dir,
        auth_state_path: auth_state_path.clone(),
        queue_path: queue_path.clone(),
        state,
        registry,
    };
    run.run_loop(&robots, effective_delay).await;
    let state = run.state;

    if options.auth.enabled || auth_state_path.exists() {
        browser.save_state(&auth_state_path).await;
    }
    browser.close().await;

    let finished_at = now_iso();
    let report = build_crawl_report(BuildCrawlReportParams {
        options,
        pages: state.pages,
        started_at: state.started_at,
        finished_at,
        effective_delay,
        discovered_count: state.queued.len(),
        visited_count: state.visited.len(),
        skipped_by_robots: state.skipped_by_robots,
        skipped_by_auth: state.skipped_by_auth,
        skipped_by_pattern: state.skipped_by_pattern,
    });

    write_json(&run_dir.join("report.json"), &report).await?;
    tokio::fs::write(run_dir.join("report.md"), build_markdown_report(&report)).await?;
    write_json(&queue_path, &state.queue).await?;

    let totals = report.totals.clone().unwrap_or_default();
    println!("\n✅ Scan finished");
    println!("   Visited: {}", totals.visited);
    println!(
        "   Successful: {} | Failed: {} | Robots skipped: {} | Auth walls: {} | Pattern skipped: {}",
        totals.succeeded,
        totals.failed,
        totals.skipped_by_robots,
        totals.skipped_by_auth,
        totals.skipped_by_pattern.unwrap_or(0)
    );
    println!("   Discovered: {}", totals.discovered);
    if let Some(groups) = report.pattern_groups.as_ref().filter(|g| !g.is_empty()) {
        println!("   Pattern groups: {}", groups.len());
    }
    if !state.queue.is_empty() {
        println!(
            "   Pending in queue: {} not-crawled URLs",
            state.queue.len()
        );
    }
    let duration_ms = chrono::DateTime::parse_from_rfc3339(&report.finished_at)
        .ok()
        .zip(chrono::DateTime::parse_from_rfc3339(&report.started_at).ok())
        .map(|(end, begin)| (end - begin).num_milliseconds().max(0) as u64)
        .unwrap_or(0);
    println!("   Duration: {}", format_duration(duration_ms));
    println!("   Report: {}\n", run_dir.join("report.md").display());

    if options.generate_knowledge {
        println!("🧠 Generating screen & pattern knowledge with Ollama...");
        let knowledge = generate_crawl_knowledge(GenerateKnowledgeOptions {
            run_dir: Some(run_dir.clone()),
            report: Some(report.clone()),
            ollama_url: Some(options.ollama_url.clone()),
            ollama_model: Some(options.ollama_model.clone()),
            ..GenerateKnowledgeOptions::default()
        })
        .await;
        match knowledge {
            Ok(_) => println!(
                "   ✓ Knowledge report: {}\n",
                run_dir.join("knowledge.md").display()
            ),
            Err(err) => eprintln!("   ⚠️ Knowledge generation failed: {err:#}\n"),
        }
    }

    Ok(report)
}
