mod common;

use std::sync::Arc;

use common::{chat_stream, start_mock, Call};
use localith::knowledge::{
    build_knowledge_markdown, create_fallback_screen_analysis, derive_default_name,
    generate_crawl_knowledge, group_pages_for_knowledge, load_page_snapshot,
    load_saved_screen_knowledge, parse_screen_analysis, persist_screen_knowledge_state,
    GenerateKnowledgeOptions, ScreenGroup,
};
use localith::types::{
    CrawlKnowledgeResult, CrawlReport, CrawledPage, PageStatus, ScreenKnowledge,
};

const ISSUE_PATTERN: &str = "https://admin.dev.frf.app.fanzeal.com/report-issues/{{id}}";
const DASHBOARD_URL: &str = "https://admin.dev.frf.app.fanzeal.com/dashboard";

fn page(url: &str, title: &str, pattern: Option<&str>) -> CrawledPage {
    CrawledPage {
        url: url.to_string(),
        final_url: url.to_string(),
        depth: if pattern.is_some() { 2 } else { 1 },
        status: PageStatus::Ok,
        title: Some(title.to_string()),
        pattern: pattern.map(str::to_string),
        snapshot: Some(format!("- heading \"{title}\"\n- button \"Save\"")),
        ..CrawledPage::default()
    }
}

fn sample_pages() -> Vec<CrawledPage> {
    vec![
        page(
            "https://admin.dev.frf.app.fanzeal.com/report-issues/ISS-GPDPFI",
            "Issue GPDPFI",
            Some(ISSUE_PATTERN),
        ),
        page(
            "https://admin.dev.frf.app.fanzeal.com/report-issues/ISS-J74ENW",
            "Issue J74ENW",
            Some(ISSUE_PATTERN),
        ),
        page(DASHBOARD_URL, "Dashboard", None),
    ]
}

fn sample_report() -> CrawlReport {
    CrawlReport {
        start_url: "https://admin.dev.frf.app.fanzeal.com".to_string(),
        pages: sample_pages(),
        ..CrawlReport::default()
    }
}

fn sample_screen() -> ScreenKnowledge {
    ScreenKnowledge {
        screen_id: "screen-001".to_string(),
        pattern: ISSUE_PATTERN.to_string(),
        screen_name: "Issue Details | Triage".to_string(),
        screen_type: "detail-view".to_string(),
        is_pattern_group: true,
        sample_urls: vec![
            "https://admin.dev.frf.app.fanzeal.com/report-issues/ISS-GPDPFI".to_string(),
        ],
        summary: "Shows one reported issue.".to_string(),
        purpose: "Triage issues.".to_string(),
        key_workflows: vec!["Assign the issue".to_string()],
        data_entities: vec!["Issue".to_string()],
        ui_components: vec!["Status badge".to_string()],
        keywords: vec!["issue".to_string(), "triage".to_string()],
        is_fallback: Some(false),
        screen_file: Some("screens/screen-001.json".to_string()),
    }
}

fn sample_knowledge(screens: Vec<ScreenKnowledge>) -> CrawlKnowledgeResult {
    CrawlKnowledgeResult {
        start_url: "https://admin.dev.frf.app.fanzeal.com".to_string(),
        generated_at: "2026-11-01T00:00:00.000Z".to_string(),
        ollama_model: "test-model".to_string(),
        total_screens: screens.len(),
        total_pattern_groups: screens.iter().filter(|s| s.is_pattern_group).count(),
        global_keywords: vec!["issue".to_string(), "triage".to_string()],
        site_summary: "Summary".to_string(),
        screens,
    }
}

/// A fake Ollama that answers every chat with the given analysis JSON.
fn ollama_replying(analysis: serde_json::Value) -> common::Responder {
    Arc::new(move |call: &Call| match call.path.as_str() {
        "/api/chat" => (200, chat_stream(&analysis.to_string())),
        _ => (404, "{}".to_string()),
    })
}

fn analysis_json(name: &str) -> serde_json::Value {
    serde_json::json!({
        "screenName": name,
        "screenType": "detail-view",
        "summary": "Shows details.",
        "purpose": "Lets staff review records.",
        "keyWorkflows": ["Open a record", "Change status"],
        "dataEntities": ["Issue"],
        "uiComponents": ["Table", 3],
        "keywords": ["Issue", "triage", "ab"]
    })
}

#[test]
fn groups_parameterized_pages_into_one_screen() {
    let groups = group_pages_for_knowledge(&sample_pages());
    assert_eq!(groups.len(), 2);

    assert!(groups[0].is_pattern_group);
    assert_eq!(groups[0].pattern, ISSUE_PATTERN);
    assert_eq!(groups[0].pages.len(), 2);

    assert!(!groups[1].is_pattern_group);
    assert_eq!(groups[1].pattern, DASHBOARD_URL);
    assert_eq!(groups[1].pages.len(), 1);
}

#[test]
fn groups_pages_without_a_saved_pattern_by_their_url_shape() {
    let mut pages = sample_pages();
    for page in &mut pages {
        page.pattern = None;
    }
    let groups = group_pages_for_knowledge(&pages);
    assert_eq!(groups.len(), 2);
    assert_eq!(groups[0].pattern, ISSUE_PATTERN);
}

#[test]
fn default_names_and_fallback_analysis() {
    let group = |pattern: &str, is_pattern_group: bool| ScreenGroup {
        pattern: pattern.to_string(),
        is_pattern_group,
        pages: Vec::new(),
    };
    assert_eq!(
        derive_default_name(&group("https://example.com/report-issues", false)),
        "Report issues Screen"
    );
    assert_eq!(
        derive_default_name(&group("https://example.com/", false)),
        "Home Screen"
    );
    assert_eq!(
        derive_default_name(&group("https://example.com/items/{{id}}", true)),
        "Home Screen"
    );
    assert_eq!(
        derive_default_name(&group("not a url", false)),
        "Application Screen"
    );

    let fallback = create_fallback_screen_analysis(&group(ISSUE_PATTERN, true), "");
    assert!(fallback.is_fallback);
    assert_eq!(fallback.screen_type, "detail-view");
    assert_eq!(
        fallback.keywords,
        vec![
            "admin.dev.frf.app.fanzeal.com",
            "report",
            "issues",
            "{{id}}"
        ]
    );
    assert_eq!(
        create_fallback_screen_analysis(&group(DASHBOARD_URL, false), "Dash").screen_name,
        "Dash"
    );
}

#[test]
fn parses_model_replies_and_falls_back_on_garbage() {
    let group = ScreenGroup {
        pattern: DASHBOARD_URL.to_string(),
        is_pattern_group: false,
        pages: Vec::new(),
    };

    let fenced = format!("```json\n{}\n```", analysis_json("Dashboard Home"));
    let parsed = parse_screen_analysis(&fenced, &group);
    assert_eq!(parsed.screen_name, "Dashboard Home");
    assert_eq!(parsed.ui_components, vec!["Table", "3"]);
    assert!(!parsed.is_fallback);

    let partial = parse_screen_analysis(r#"{"summary": "Only a summary"}"#, &group);
    assert_eq!(partial.screen_name, "Dashboard Screen");
    assert_eq!(partial.screen_type, "page");
    assert_eq!(partial.summary, "Only a summary");
    assert!(partial.keywords.is_empty());

    assert!(parse_screen_analysis("I cannot answer that.", &group).is_fallback);
}

#[test]
fn markdown_lists_keywords_table_and_details() {
    let markdown = build_knowledge_markdown(&sample_knowledge(vec![sample_screen()]));
    assert!(markdown.contains("# Application Screen Knowledge & Keywords Report"));
    assert!(markdown.contains("`issue` • `triage`"));
    assert!(markdown.contains("| 1 | **Issue Details \\| Triage** | detail-view |"));
    assert!(markdown.contains("### Issue Details | Triage"));
    assert!(markdown.contains("- **Pattern Group:** Yes (parameterized template)"));
    assert!(markdown.contains("**Key Workflows:**\n- Assign the issue"));
    assert!(markdown.contains("<details><summary>Sample URLs (1)</summary>"));

    let empty = build_knowledge_markdown(&sample_knowledge(Vec::new()));
    assert!(empty.contains("## Global Domain Keywords"));
}

#[tokio::test]
async fn page_snapshot_prefers_inline_then_files_then_text() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("pages")).unwrap();
    std::fs::write(
        dir.path().join("pages/page-0001.snapshot.txt"),
        "snapshot from file",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("pages/page-0002.json"),
        r#"{"snapshot":"snapshot from data file"}"#,
    )
    .unwrap();

    let base = CrawledPage {
        text: Some("plain text".to_string()),
        ..CrawledPage::default()
    };

    let inline = CrawledPage {
        snapshot: Some("inline snapshot".to_string()),
        snapshot_file: Some("pages/page-0001.snapshot.txt".to_string()),
        ..base.clone()
    };
    assert_eq!(
        load_page_snapshot(&inline, Some(dir.path())).await,
        "inline snapshot"
    );

    let from_file = CrawledPage {
        snapshot_file: Some("pages/page-0001.snapshot.txt".to_string()),
        ..base.clone()
    };
    assert_eq!(
        load_page_snapshot(&from_file, Some(dir.path())).await,
        "snapshot from file"
    );

    let from_data = CrawledPage {
        snapshot_file: Some("pages/missing.snapshot.txt".to_string()),
        data_file: Some("pages/page-0002.json".to_string()),
        ..base.clone()
    };
    assert_eq!(
        load_page_snapshot(&from_data, Some(dir.path())).await,
        "snapshot from data file"
    );

    assert_eq!(load_page_snapshot(&from_file, None).await, "plain text");
    assert_eq!(
        load_page_snapshot(&base, Some(dir.path())).await,
        "plain text"
    );
}

#[tokio::test]
async fn generates_knowledge_from_streamed_model_replies() {
    let ollama = start_mock(ollama_replying(analysis_json("Issue Viewer"))).await;
    let dir = tempfile::tempdir().unwrap();

    let result = generate_crawl_knowledge(GenerateKnowledgeOptions {
        ollama_url: Some(ollama.url.clone()),
        ollama_model: Some("test-model".to_string()),
        run_dir: Some(dir.path().to_path_buf()),
        report: Some(sample_report()),
        ..GenerateKnowledgeOptions::default()
    })
    .await
    .unwrap();

    assert_eq!(result.total_screens, 2);
    assert_eq!(result.total_pattern_groups, 1);
    assert_eq!(result.ollama_model, "test-model");
    assert_eq!(result.screens[0].screen_id, "screen-001");
    assert_eq!(result.screens[0].screen_name, "Issue Viewer");
    assert_eq!(result.screens[0].sample_urls.len(), 2);
    assert_eq!(
        result.screens[0].screen_file.as_deref(),
        Some("screens/screen-001.json")
    );
    assert_eq!(result.screens[0].is_fallback, Some(false));
    // Lower-cased, counted across screens, and words of two letters or fewer dropped.
    assert_eq!(result.global_keywords, vec!["issue", "triage"]);

    let chats = ollama.calls_to("/api/chat");
    assert_eq!(chats.len(), 2);
    assert_eq!(chats[0].body["model"], "test-model");
    assert_eq!(chats[0].body["stream"], true);
    assert_eq!(chats[0].body["format"], "json");
    assert_eq!(chats[0].body["options"]["num_ctx"], 8192);
    let prompt = chats[0].body["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains(ISSUE_PATTERN));
    assert!(prompt.contains("--- Sample 2 ("));
    assert!(prompt.contains("Issue GPDPFI | Issue J74ENW"));

    for file in [
        "knowledge.json",
        "knowledge.md",
        "screens/screen-001.json",
        "screens/screen-002.json",
    ] {
        assert!(dir.path().join(file).is_file(), "{file} should exist");
    }
    let saved: CrawlKnowledgeResult =
        serde_json::from_str(&std::fs::read_to_string(dir.path().join("knowledge.json")).unwrap())
            .unwrap();
    assert_eq!(saved.screens.len(), 2);
    assert!(saved
        .site_summary
        .contains("covering 2 distinct functional screens"));
}

#[tokio::test]
async fn falls_back_when_the_model_fails() {
    let ollama = start_mock(Arc::new(|_: &Call| {
        (500, r#"{"error":"model not found"}"#.to_string())
    }))
    .await;

    let result = generate_crawl_knowledge(GenerateKnowledgeOptions {
        ollama_url: Some(ollama.url.clone()),
        report: Some(sample_report()),
        ..GenerateKnowledgeOptions::default()
    })
    .await
    .unwrap();

    assert_eq!(result.screens.len(), 2);
    assert!(result
        .screens
        .iter()
        .all(|screen| screen.is_fallback == Some(true)));
    assert_eq!(result.screens[0].screen_name, "Issue GPDPFI | Issue J74ENW");
    assert_eq!(result.screens[0].screen_file, None);
}

#[tokio::test]
async fn persists_and_reloads_screen_knowledge() {
    let dir = tempfile::tempdir().unwrap();
    assert!(load_saved_screen_knowledge(dir.path()).await.is_empty());

    let screen = sample_screen();
    persist_screen_knowledge_state(dir.path(), &screen, &sample_knowledge(vec![screen.clone()]))
        .await
        .unwrap();

    // A screen that only exists as an individual file is picked up too.
    let extra = ScreenKnowledge {
        screen_id: "screen-002".to_string(),
        pattern: DASHBOARD_URL.to_string(),
        screen_name: "Dashboard".to_string(),
        ..ScreenKnowledge::default()
    };
    std::fs::write(
        dir.path().join("screens/screen-002.json"),
        serde_json::to_string(&extra).unwrap(),
    )
    .unwrap();
    std::fs::write(dir.path().join("screens/broken.json"), "{ not json").unwrap();

    let saved = load_saved_screen_knowledge(dir.path()).await;
    assert_eq!(saved.len(), 2);
    assert_eq!(saved[ISSUE_PATTERN], screen);
    assert_eq!(saved[DASHBOARD_URL].screen_name, "Dashboard");
    assert!(dir.path().join("knowledge.md").is_file());
}

#[tokio::test]
async fn reuses_saved_screens_and_only_analyzes_new_ones() {
    let ollama = start_mock(ollama_replying(analysis_json("Fresh Dashboard"))).await;
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("report.json"),
        serde_json::to_string(&sample_report()).unwrap(),
    )
    .unwrap();
    let screen = sample_screen();
    persist_screen_knowledge_state(dir.path(), &screen, &sample_knowledge(vec![screen.clone()]))
        .await
        .unwrap();

    let options = |force: bool| GenerateKnowledgeOptions {
        ollama_url: Some(ollama.url.clone()),
        run_dir: Some(dir.path().to_path_buf()),
        force_reanalyze: force,
        ..GenerateKnowledgeOptions::default()
    };

    let result = generate_crawl_knowledge(options(false)).await.unwrap();
    assert_eq!(
        ollama.calls_to("/api/chat").len(),
        1,
        "only the dashboard is new"
    );
    assert_eq!(result.screens[0].screen_name, "Issue Details | Triage");
    assert_eq!(result.screens[1].screen_name, "Fresh Dashboard");

    generate_crawl_knowledge(options(false)).await.unwrap();
    assert_eq!(
        ollama.calls_to("/api/chat").len(),
        1,
        "everything is cached now"
    );

    generate_crawl_knowledge(options(true)).await.unwrap();
    assert_eq!(
        ollama.calls_to("/api/chat").len(),
        3,
        "--force analyzes both screens again"
    );
}

#[tokio::test]
async fn requires_a_report_source() {
    let error = generate_crawl_knowledge(GenerateKnowledgeOptions::default())
        .await
        .unwrap_err();
    assert!(error
        .to_string()
        .contains("Either report, reportPath, or runDir must be provided."));
}
