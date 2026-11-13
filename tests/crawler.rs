use std::path::Path;

use localith::config::parse_options_with;
use localith::crawler::report::{
    build_crawl_report, build_markdown_report, compute_crawl_totals, BuildCrawlReportParams,
};
use localith::crawler::state::{
    init_crawl_state, load_saved_crawl_data, QueueItem, SavedCrawlData,
};
use localith::crawler::url_pattern::{
    compute_pattern_groups, extract_url_pattern, is_dynamic_pattern, is_id_like_segment,
    UrlPatternRegistry,
};
use localith::crawler::{
    calculate_wait_time, create_auth_skipped_page, create_error_page, create_pattern_skipped_page,
    create_run_directory, detect_auth_wall, extract_in_scope_urls, get_next_queue_items,
    get_page_artifact_paths, get_page_prefix, safe_slug,
};
use localith::types::{
    AuthOptions, CrawlOptions, CrawlReport, CrawledPage, ExtractedLink, Heading, PageExtraction,
    PageStatus, PatternCount,
};
use localith::util::{escape_cell, format_duration, is_in_scope, normalize_url};
use url::Url;

fn mock_options() -> CrawlOptions {
    CrawlOptions {
        start_url: "https://example.com".to_string(),
        max_depth: 3,
        max_pages: 10,
        max_pages_per_pattern: 3,
        delay_ms: 100,
        page_timeout_ms: 1000,
        max_snapshot_chars: 5000,
        max_page_text_chars: 5000,
        generate_knowledge: false,
        include_subdomains: false,
        respect_robots: true,
        fresh: false,
        auth: AuthOptions {
            enabled: true,
            email: Some("user@example.com".to_string()),
            password: Some("pw".to_string()),
        },
        ollama_url: "http://localhost:11434".to_string(),
        ollama_model: "llama3".to_string(),
        screenshot: false,
    }
}

fn link(href: &str, text: &str) -> ExtractedLink {
    ExtractedLink {
        href: href.to_string(),
        text: text.to_string(),
    }
}

fn page(url: &str, depth: u32, status: PageStatus) -> CrawledPage {
    CrawledPage {
        url: url.to_string(),
        final_url: url.to_string(),
        depth,
        status,
        ..CrawledPage::default()
    }
}

fn item(url: &str, depth: u32) -> QueueItem {
    QueueItem {
        url: url.to_string(),
        depth,
        pattern: None,
    }
}

fn args(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| value.to_string()).collect()
}

// --- URL patterns -----------------------------------------------------------

#[test]
fn is_id_like_segment_identifies_ids_and_keeps_static_slugs() {
    for dynamic in [
        "ISS-GPDPFI",
        "ISS-J74ENW",
        "ISS-FOTF0M",
        "ISS-HJEG0K",
        "ISS-XFB21G",
        "12345",
        "550e8400-e29b-41d4-a716-446655440000",
        "507f1f77bcf86cd799439011",
        "issue-123",
        "ticket_9824",
        "c7a8b9",
        "john.doe@example.com",
    ] {
        assert!(is_id_like_segment(dynamic), "{dynamic} should be dynamic");
    }
    for fixed in [
        "report-issues",
        "contact-us",
        "about-company",
        "dashboard",
        "settings",
        "users",
        "",
    ] {
        assert!(!is_id_like_segment(fixed), "{fixed} should be static");
    }
}

#[test]
fn extract_url_pattern_abstracts_dynamic_segments() {
    let pattern1 =
        extract_url_pattern("https://admin.dev.frf.app.fanzeal.com/report-issues/ISS-GPDPFI");
    let pattern2 =
        extract_url_pattern("https://admin.dev.frf.app.fanzeal.com/report-issues/ISS-J74ENW");
    let fixed = extract_url_pattern("https://admin.dev.frf.app.fanzeal.com/report-issues");

    assert_eq!(
        pattern1,
        "https://admin.dev.frf.app.fanzeal.com/report-issues/{{id}}"
    );
    assert_eq!(pattern1, pattern2);
    assert_eq!(fixed, "https://admin.dev.frf.app.fanzeal.com/report-issues");
    assert!(is_dynamic_pattern(&pattern1));
    assert!(!is_dynamic_pattern(&fixed));
    assert_eq!(
        extract_url_pattern("https://example.com"),
        "https://example.com/"
    );
    assert_eq!(extract_url_pattern("not a url"), "not a url");
}

#[test]
fn extract_url_pattern_keeps_static_query_values_sorted() {
    assert_eq!(
        extract_url_pattern("https://example.com/list?sort=name&filter=open"),
        "https://example.com/list?filter=open&sort=name"
    );
    assert_eq!(
        extract_url_pattern("https://example.com/items/42?tab=info"),
        "https://example.com/items/{{id}}?tab=info"
    );
}

#[test]
fn registry_enforces_sample_limit_only_at_depth_above_one() {
    let mut registry = UrlPatternRegistry::new(3);
    let urls: Vec<String> = [
        "ISS-GPDPFI",
        "ISS-J74ENW",
        "ISS-FOTF0M",
        "ISS-HJEG0K",
        "ISS-XFB21G",
    ]
    .iter()
    .map(|id| format!("https://admin.dev.frf.app.fanzeal.com/report-issues/{id}"))
    .collect();

    assert!(!registry.is_pattern_limit_reached(&urls[0], 1));
    for url in &urls[..3] {
        assert!(!registry.is_pattern_limit_reached(url, 2));
        registry.record_crawled(url, 2);
    }
    assert!(registry.is_pattern_limit_reached(&urls[3], 2));
    assert!(registry.is_pattern_limit_reached(&urls[4], 2));
    assert!(!registry.is_pattern_limit_reached(&urls[4], 1));
    assert!(!registry
        .is_pattern_limit_reached("https://admin.dev.frf.app.fanzeal.com/users/USR-001", 2));
    assert_eq!(
        registry.count("https://admin.dev.frf.app.fanzeal.com/report-issues/{{id}}"),
        3
    );
}

#[test]
fn compute_pattern_groups_counts_crawled_and_skipped() {
    let pattern = "https://admin.dev.frf.app.fanzeal.com/report-issues/{{id}}";
    let mut pages = Vec::new();
    for name in ["u1", "u2", "u3"] {
        pages.push(CrawledPage {
            pattern: Some(pattern.to_string()),
            ..page(name, 2, PageStatus::Ok)
        });
    }
    for name in ["u4", "u5"] {
        pages.push(CrawledPage {
            pattern: Some(pattern.to_string()),
            error: Some("Pattern limit reached".to_string()),
            ..page(name, 2, PageStatus::Skipped)
        });
    }
    pages.push(page("no-pattern", 1, PageStatus::Ok));

    let groups = compute_pattern_groups(&pages);
    assert_eq!(groups.len(), 1);
    assert_eq!(
        groups[pattern],
        PatternCount {
            crawled: 3,
            skipped: 2
        }
    );
}

// --- Pure crawl helpers -----------------------------------------------------

#[test]
fn normalize_url_cleans_links() {
    assert_eq!(
        normalize_url(
            "/post/?utm_source=x&b=2&a=1&fbclid=abc#top",
            Some("https://example.com/blog")
        )
        .as_deref(),
        Some("https://example.com/post?a=1&b=2")
    );
    assert_eq!(
        normalize_url("https://Example.com", None).as_deref(),
        Some("https://example.com/")
    );
    assert_eq!(
        normalize_url("javascript:void(0)", Some("https://example.com")),
        None
    );
    assert_eq!(normalize_url("mailto:a@b.c", None), None);
    assert_eq!(normalize_url("https://user:pw@example.com/", None), None);
    assert_eq!(normalize_url("relative-without-base", None), None);
}

#[test]
fn is_in_scope_checks_host_and_subdomains() {
    let start = Url::parse("https://example.com").unwrap();
    assert!(is_in_scope("https://example.com/a", &start, false));
    assert!(!is_in_scope("https://blog.example.com/a", &start, false));
    assert!(is_in_scope("https://blog.example.com/a", &start, true));
    assert!(!is_in_scope("https://notexample.com/a", &start, true));
    assert!(!is_in_scope("ftp://example.com/a", &start, true));
}

#[test]
fn calculate_wait_time_returns_remaining_delay() {
    assert_eq!(calculate_wait_time(1000, 5000, 5400), 600);
    assert_eq!(calculate_wait_time(1000, 0, 5000), 0);
    assert_eq!(calculate_wait_time(1000, 5000, 7000), 0);
}

#[test]
fn page_prefix_and_artifact_paths() {
    assert_eq!(get_page_prefix(1), "page-0001");
    assert_eq!(get_page_prefix(42), "page-0042");
    assert_eq!(get_page_prefix(9999), "page-9999");

    let pages_dir = Path::new("tmp").join("reports").join("pages");
    let paths = get_page_artifact_paths(&pages_dir, "page-0001");
    assert_eq!(
        paths.snapshot_path,
        pages_dir.join("page-0001.snapshot.txt")
    );
    assert_eq!(paths.data_path, pages_dir.join("page-0001.json"));
    assert_eq!(paths.screenshot_path, pages_dir.join("page-0001.png"));
}

#[test]
fn extract_in_scope_urls_normalizes_and_filters() {
    let start = Url::parse("https://example.com").unwrap();
    let links = vec![
        link("/post-1", "Post 1"),
        link("https://example.com/post-2?utm_source=twitter", "Post 2"),
        link("/post-1", "Duplicate Post 1"),
        link("https://other.com/about", "External"),
        link("javascript:void(0)", "JS"),
    ];
    assert_eq!(
        extract_in_scope_urls(&links, "https://example.com/blog", &start, false),
        vec!["https://example.com/post-1", "https://example.com/post-2"]
    );
}

#[test]
fn get_next_queue_items_skips_known_urls_and_increments_depth() {
    let start = Url::parse("https://example.com").unwrap();
    let links = vec![link("/page-a", "A"), link("/page-b", "B")];
    let next = get_next_queue_items(&links, "https://example.com", &start, false, 1, |url| {
        url == "https://example.com/page-a"
    });
    assert_eq!(
        next,
        vec![QueueItem {
            url: "https://example.com/page-b".to_string(),
            depth: 2,
            pattern: Some("https://example.com/page-b".to_string()),
        }]
    );
}

#[test]
fn compute_crawl_totals_counts_statuses() {
    let pages = vec![
        page("https://example.com", 0, PageStatus::Ok),
        page("https://example.com/bad", 1, PageStatus::Error),
        page("https://example.com/disallowed", 1, PageStatus::Skipped),
    ];
    let totals = compute_crawl_totals(&pages, 10, 3, 1, 0, 2);
    assert_eq!(totals.discovered, 10);
    assert_eq!(totals.visited, 3);
    assert_eq!(totals.succeeded, 1);
    assert_eq!(totals.failed, 1);
    assert_eq!(totals.skipped_by_robots, 1);
    assert_eq!(totals.skipped_by_auth, 0);
    assert_eq!(totals.skipped_by_pattern, Some(2));
}

#[test]
fn escape_cell_and_format_duration() {
    assert_eq!(escape_cell("hello | world"), "hello \\| world");
    assert_eq!(escape_cell("line1\nline2"), "line1 line2");
    assert_eq!(escape_cell(&"x".repeat(400)).len(), 300);
    assert_eq!(format_duration(500), "500ms");
    assert_eq!(format_duration(1500), "1.5s");
    assert_eq!(format_duration(12345), "12.3s");
}

#[test]
fn safe_slug_is_stable_and_filesystem_safe() {
    let slug = safe_slug("https://example.com/foo");
    assert_eq!(slug, safe_slug("https://example.com/foo"));
    assert_ne!(slug, safe_slug("https://example.com/bar"));
    assert!(slug
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'));
    // Same value the TypeScript version produced, so existing report folders are reused.
    assert_eq!(
        safe_slug("https://quotes.toscrape.com/"),
        "quotes-toscrape-com-caf2b786"
    );

    let reports = Path::new("reports");
    assert_eq!(
        create_run_directory(reports, "https://example.com"),
        create_run_directory(reports, "https://example.com")
    );
}

#[test]
fn detect_auth_wall_uses_url_or_password_field_with_wording() {
    let plain = PageExtraction {
        title: "Welcome".to_string(),
        ..PageExtraction::default()
    };
    assert!(!detect_auth_wall(&plain, "https://example.com/home"));
    assert!(detect_auth_wall(&plain, "https://example.com/login"));
    assert!(detect_auth_wall(
        &plain,
        "https://example.com/account/SignIn/"
    ));
    assert!(!detect_auth_wall(&plain, "https://example.com/authors"));

    let form = PageExtraction {
        headings: vec![Heading {
            tag: "h1".to_string(),
            text: "Please sign in".to_string(),
        }],
        password_fields: 1,
        ..PageExtraction::default()
    };
    assert!(detect_auth_wall(&form, "https://example.com/home"));

    let wording_only = PageExtraction {
        text: "Sign in to see more".to_string(),
        ..PageExtraction::default()
    };
    assert!(!detect_auth_wall(&wording_only, "https://example.com/home"));
}

// --- Crawl state ------------------------------------------------------------

fn previous_report(pages: Vec<CrawledPage>) -> CrawlReport {
    serde_json::from_value(serde_json::json!({
        "startUrl": "https://example.com",
        "startedAt": "2026-11-01T00:00:00.000Z",
        "finishedAt": "2026-11-01T00:01:00.000Z",
        "settings": {
            "maxDepth": 3, "maxPages": 10, "delayMs": 100, "includeSubdomains": false,
            "respectRobots": true, "auth": { "enabled": true }, "ollamaModel": "llama3"
        },
        "totals": {
            "discovered": 3, "visited": 1, "succeeded": 1, "failed": 0,
            "skippedByRobots": 0, "skippedByAuth": 0
        },
        "pages": pages
    }))
    .unwrap()
}

#[test]
fn init_crawl_state_starts_fresh_without_a_report() {
    let state = init_crawl_state(&mock_options(), None);
    assert!(!state.is_resumed);
    assert!(state.pages.is_empty());
    assert_eq!(state.page_number, 0);
    assert_eq!(state.queue, vec![item("https://example.com", 0)]);
    assert!(state.visited.is_empty());

    let empty = SavedCrawlData::default();
    assert!(!init_crawl_state(&mock_options(), Some(&empty)).is_resumed);
}

#[test]
fn init_crawl_state_rebuilds_queue_from_crawled_links() {
    let home = CrawledPage {
        title: Some("Home".to_string()),
        links: Some(vec![
            link("https://example.com/about", "About"),
            link("https://example.com/contact", "Contact"),
            link("https://external.com/page", "External"),
        ]),
        crawled_at: "2026-11-01T00:00:30.000Z".to_string(),
        ..page("https://example.com", 0, PageStatus::Ok)
    };
    let saved = SavedCrawlData {
        report: Some(previous_report(vec![home])),
        queue: None,
    };

    let state = init_crawl_state(&mock_options(), Some(&saved));
    assert!(state.is_resumed);
    assert_eq!(state.pages.len(), 1);
    assert_eq!(state.page_number, 1);
    assert!(state.visited.contains("https://example.com"));
    assert_eq!(state.started_at, "2026-11-01T00:00:00.000Z");
    assert_eq!(
        state.queue,
        vec![
            item("https://example.com/about", 1),
            item("https://example.com/contact", 1)
        ]
    );
}

#[test]
fn init_crawl_state_uses_saved_queue_without_visited_items() {
    let saved = SavedCrawlData {
        report: Some(previous_report(vec![
            page("https://example.com", 0, PageStatus::Ok),
            page("https://example.com/about", 1, PageStatus::Ok),
        ])),
        queue: Some(vec![
            item("https://example.com/about", 1),
            item("https://example.com/team", 2),
            item("https://example.com/pricing", 1),
        ]),
    };

    let state = init_crawl_state(&mock_options(), Some(&saved));
    assert!(state.is_resumed);
    assert_eq!(state.page_number, 2);
    assert_eq!(
        state.queue,
        vec![
            item("https://example.com/team", 2),
            item("https://example.com/pricing", 1)
        ]
    );
}

#[test]
fn saved_crawl_data_round_trips_through_report_files() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_saved_crawl_data(dir.path()).report.is_none());

    let options = mock_options();
    let pages = vec![
        CrawledPage {
            title: Some("Home | Site".to_string()),
            ..page("https://example.com/", 0, PageStatus::Ok)
        },
        create_error_page("https://example.com/broken", 1, "timeout"),
        create_pattern_skipped_page(
            "https://example.com/items/9",
            2,
            "https://example.com/items/{{id}}",
        ),
        create_auth_skipped_page(
            "https://example.com/admin",
            "https://example.com/login",
            1,
            "Auth wall",
            true,
        ),
    ];
    let report = build_crawl_report(BuildCrawlReportParams {
        options: &options,
        pages,
        started_at: "2026-11-01T00:00:00.000Z".to_string(),
        finished_at: "2026-11-01T00:01:00.000Z".to_string(),
        effective_delay: 900,
        discovered_count: 6,
        visited_count: 4,
        skipped_by_robots: 0,
        skipped_by_auth: 1,
        skipped_by_pattern: 1,
    });

    let json = serde_json::to_value(&report).unwrap();
    assert_eq!(json["settings"]["maxPagesPerPattern"], 3);
    assert_eq!(json["totals"]["skippedByPattern"], 1);
    assert_eq!(json["pages"][0]["status"], "ok");
    assert_eq!(json["pages"][3]["authWall"], true);
    assert_eq!(json["pages"][3]["authHandled"], true);
    assert!(json["pages"][1].get("authWall").is_none());
    assert_eq!(
        json["patternGroups"]["https://example.com/items/{{id}}"]["skipped"],
        1
    );

    let markdown = build_markdown_report(&report);
    assert!(markdown.contains("# Website Scan Report"));
    assert!(markdown.contains("| 1 | 0 | ok | Home \\| Site | https://example.com/ |"));
    assert!(markdown.contains("[pattern group] https://example.com/items/{{id}}"));
    assert!(markdown.contains("## Auth walls"));
    assert!(markdown.contains("- **AI model:** disabled"));

    std::fs::write(
        dir.path().join("report.json"),
        serde_json::to_string_pretty(&report).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("queue.json"),
        r#"[{"url":"https://example.com/next","depth":1}]"#,
    )
    .unwrap();
    let saved = load_saved_crawl_data(dir.path());
    assert_eq!(saved.report.as_ref().unwrap().pages.len(), 4);
    assert_eq!(
        saved.queue.as_ref().unwrap()[0],
        item("https://example.com/next", 1)
    );

    let state = init_crawl_state(&options, Some(&saved));
    assert_eq!(state.skipped_by_auth, 1);
    assert_eq!(state.skipped_by_pattern, 1);
    assert_eq!(
        state.queue.len(),
        2,
        "start URL was never visited, so it is queued first"
    );
    assert_eq!(state.queue[0], item("https://example.com", 0));
}

// --- Command-line options ---------------------------------------------------

#[test]
fn parse_options_reads_flags_and_environment() {
    let no_env = |_: &str| None;
    let options = parse_options_with(
        &args(&[
            "--depth",
            "2",
            "https://example.com/docs",
            "--max-pages=5",
            "--fresh",
            "--knowledge",
        ]),
        &no_env,
    )
    .unwrap()
    .unwrap();
    assert_eq!(options.start_url, "https://example.com/docs");
    assert_eq!(options.max_depth, 2);
    assert_eq!(options.max_pages, 5);
    assert_eq!(options.max_pages_per_pattern, 3);
    assert_eq!(options.delay_ms, 900);
    assert!(options.fresh && options.generate_knowledge && options.respect_robots);
    assert_eq!(options.ollama_model, "qwen3:4b");

    let env = |name: &str| match name {
        "CRAWL_DEPTH" => Some("1".to_string()),
        "RESPECT_ROBOTS" => Some("false".to_string()),
        "OLLAMA_URL" => Some("http://ollama:11434/".to_string()),
        "INCLUDE_SUBDOMAINS" => Some("TRUE".to_string()),
        _ => None,
    };
    let from_env = parse_options_with(&args(&["https://example.com"]), &env)
        .unwrap()
        .unwrap();
    assert_eq!(from_env.max_depth, 1);
    assert!(!from_env.respect_robots);
    assert!(from_env.include_subdomains);
    assert_eq!(from_env.ollama_url, "http://ollama:11434");
    assert_eq!(from_env.start_url, "https://example.com/");
}

#[test]
fn parse_options_rejects_bad_input() {
    let no_env = |_: &str| None;
    let error = |values: &[&str]| {
        parse_options_with(&args(values), &no_env)
            .unwrap_err()
            .to_string()
    };

    assert!(error(&[]).contains("Please provide a starting URL"));
    assert!(error(&["not-a-url"]).contains("Invalid URL"));
    assert!(error(&["ftp://example.com"]).contains("Only http:// and https://"));
    assert!(error(&["https://example.com", "--depth", "11"]).contains("--depth"));
    assert!(error(&["https://example.com", "--depth", "1.5"]).contains("--depth"));
    assert!(error(&["https://example.com", "--max-pages", "0"]).contains("--max-pages"));
    assert!(error(&["https://example.com", "--delay-ms", "abc"]).contains("--delay-ms"));
    assert!(error(&["https://example.com", "--auth"]).contains("--auth requires credentials"));
    assert!(parse_options_with(&args(&["--help"]), &no_env)
        .unwrap()
        .is_none());
}
