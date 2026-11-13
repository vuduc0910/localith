mod common;

use std::path::Path;
use std::sync::Arc;

use common::{chat_stream, start_mock, Call, Mock};
use localith::ollama::Ollama;
use localith::rag::chroma::{parse_chroma_options_with, sanitize_collection_name};
use localith::rag::chunks::build_knowledge_chunks;
use localith::rag::{index_knowledge_to_chroma, search_rag, synthesize_rag_answer};
use localith::types::{
    ChromaConnectionOptions, CrawlKnowledgeResult, IndexKnowledgeOptions, ScreenKnowledge,
    SearchRagOptions,
};
use serde_json::json;

const COLLECTIONS: &str = "/api/v2/tenants/default_tenant/databases/default_database/collections";

fn sample_knowledge() -> CrawlKnowledgeResult {
    CrawlKnowledgeResult {
        start_url: "https://admin.dev.frf.app.fanzeal.com".to_string(),
        generated_at: "2026-11-01T00:00:00.000Z".to_string(),
        ollama_model: "test-model".to_string(),
        total_screens: 1,
        total_pattern_groups: 0,
        global_keywords: vec!["reports".to_string(), "analytics".to_string()],
        site_summary: "Admin console.".to_string(),
        screens: vec![ScreenKnowledge {
            screen_id: "screen-001".to_string(),
            pattern: "https://admin.dev.frf.app.fanzeal.com/reports".to_string(),
            screen_name: "Reports & Analytics Center".to_string(),
            screen_type: "list-management".to_string(),
            is_pattern_group: false,
            sample_urls: vec!["https://admin.dev.frf.app.fanzeal.com/reports".to_string()],
            summary: "Lists all reports.".to_string(),
            purpose: "Manage reports.".to_string(),
            key_workflows: vec![
                "Create a new report".to_string(),
                "Export report as CSV".to_string(),
                "Filter reports by date".to_string(),
            ],
            data_entities: vec!["Report".to_string()],
            ui_components: vec!["Reports table".to_string()],
            keywords: vec!["reports".to_string(), "export".to_string()],
            is_fallback: Some(false),
            screen_file: None,
        }],
    }
}

/// A fake Ollama: embeddings of `[index, 0.5]` per input, and a fixed chat answer.
async fn fake_ollama() -> Mock {
    start_mock(Arc::new(|call: &Call| match call.path.as_str() {
        "/api/embed" => {
            let count = call.body["input"].as_array().map(Vec::len).unwrap_or(0);
            let embeddings: Vec<_> = (0..count).map(|index| json!([index as f32, 0.5])).collect();
            (200, json!({ "embeddings": embeddings }).to_string())
        }
        "/api/chat" => (
            200,
            chat_stream("Open the Reports screen and press Create."),
        ),
        _ => (404, "{}".to_string()),
    }))
    .await
}

/// A fake ChromaDB with one collection (`known_kb`) that returns two matches.
async fn fake_chroma() -> Mock {
    start_mock(Arc::new(|call: &Call| {
        let collection = json!({ "id": "col-1", "name": "known_kb", "metadata": null });
        match (call.method.as_str(), call.path.as_str()) {
            ("POST", path) if path == COLLECTIONS => (
                200,
                json!({ "id": "col-1", "name": call.body["name"], "metadata": call.body["metadata"] }).to_string(),
            ),
            ("GET", path) if path == COLLECTIONS => (200, json!([collection]).to_string()),
            ("GET", path) if path.ends_with("/collections/known_kb") => (200, collection.to_string()),
            ("GET", _) => (404, json!({ "error": "NotFoundError", "message": "Collection does not exist" }).to_string()),
            ("POST", path) if path.ends_with("/col-1/upsert") => (200, "{}".to_string()),
            ("POST", path) if path.ends_with("/col-1/query") => (
                200,
                json!({
                    "ids": [["screen-001-wf-1", "screen-001-overview"]],
                    "documents": [["Action / Workflow: Create a new report", null]],
                    "metadatas": [[
                        {
                            "screenId": "screen-001",
                            "screenName": "Reports & Analytics Center",
                            "screenType": "list-management",
                            "pattern": "https://admin.dev.frf.app.fanzeal.com/reports",
                            "chunkType": "workflow",
                            "workflow": "Create a new report",
                            "workflowIndex": 1
                        },
                        null
                    ]],
                    "distances": [[0.12345678, 1.5]]
                })
                .to_string(),
            ),
            _ => (404, "{}".to_string()),
        }
    }))
    .await
}

fn chroma_at(mock: &Mock) -> ChromaConnectionOptions {
    ChromaConnectionOptions {
        chroma_url: Some(mock.url.clone()),
        ..ChromaConnectionOptions::default()
    }
}

#[test]
fn sanitizes_collection_names() {
    assert_eq!(
        sanitize_collection_name(Some("My Cool App!")),
        "my_cool_app"
    );
    assert_eq!(
        sanitize_collection_name(Some("https://admin.dev.frf.app.fanzeal.com")),
        "https_admin_dev_frf_app_fanzeal_com"
    );
    assert_eq!(sanitize_collection_name(Some("a")), "a_rag_kb");
    assert_eq!(
        sanitize_collection_name(Some("---test_collection---")),
        "test_collection"
    );
    assert_eq!(sanitize_collection_name(Some("")), "app_knowledge_base");
    assert_eq!(sanitize_collection_name(None), "app_knowledge_base");
    assert_eq!(sanitize_collection_name(Some("!!!")), "rag_kb");

    let long = sanitize_collection_name(Some(&format!("{}-tail", "x".repeat(62))));
    assert_eq!(long, "x".repeat(62));
}

#[test]
fn chroma_options_come_from_url_options_or_environment() {
    let no_env = |_: &str| None;

    let custom = parse_chroma_options_with(
        &ChromaConnectionOptions {
            chroma_url: Some("https://my-chroma.internal:9000".to_string()),
            chroma_database: Some("prod_db".to_string()),
            chroma_tenant: Some("team_a".to_string()),
            ..ChromaConnectionOptions::default()
        },
        &no_env,
    );
    assert_eq!(custom.host, "my-chroma.internal");
    assert_eq!(custom.port, 9000);
    assert!(custom.ssl);
    assert_eq!(custom.database.as_deref(), Some("prod_db"));
    assert_eq!(custom.tenant.as_deref(), Some("team_a"));

    let defaults = parse_chroma_options_with(&ChromaConnectionOptions::default(), &no_env);
    assert_eq!(
        (defaults.host.as_str(), defaults.port, defaults.ssl),
        ("127.0.0.1", 8000, false)
    );
    assert_eq!(defaults.tenant, None);

    let env = |name: &str| match name {
        "CHROMA_URL" => Some("http://chroma".to_string()),
        "CHROMA_HOST" => Some("ignored".to_string()),
        "CHROMA_TENANT" => Some("env_tenant".to_string()),
        _ => None,
    };
    let from_env = parse_chroma_options_with(&ChromaConnectionOptions::default(), &env);
    assert_eq!(
        (from_env.host.as_str(), from_env.port, from_env.ssl),
        ("chroma", 80, false)
    );
    assert_eq!(from_env.tenant.as_deref(), Some("env_tenant"));

    // The API accepts the short `tenant` / `database` keys as well.
    let aliased: ChromaConnectionOptions =
        serde_json::from_value(json!({ "tenant": "t1", "database": "d1", "chromaPort": 9001 }))
            .unwrap();
    let parsed = parse_chroma_options_with(&aliased, &no_env);
    assert_eq!(parsed.tenant.as_deref(), Some("t1"));
    assert_eq!(parsed.database.as_deref(), Some("d1"));
    assert_eq!(parsed.port, 9001);
}

#[test]
fn builds_site_screen_and_workflow_chunks() {
    let chunks = build_knowledge_chunks(&sample_knowledge());
    assert_eq!(
        chunks.len(),
        5,
        "1 site overview + 1 screen overview + 3 workflows"
    );

    let site = chunks
        .iter()
        .find(|c| c.id == "global-site-overview")
        .unwrap();
    assert_eq!(site.metadata["chunkType"], "site_overview");
    assert!(site
        .document
        .contains("- Reports & Analytics Center (list-management)"));

    let overview = chunks
        .iter()
        .find(|c| c.id == "screen-001-overview")
        .unwrap();
    assert_eq!(
        overview.metadata["screenName"],
        "Reports & Analytics Center"
    );
    assert_eq!(overview.metadata["isPatternGroup"], "false");
    assert!(overview.document.contains("Create a new report"));

    let workflows: Vec<_> = chunks
        .iter()
        .filter(|c| c.metadata["chunkType"] == "workflow")
        .collect();
    assert_eq!(workflows.len(), 3);
    assert_eq!(workflows[0].id, "screen-001-wf-1");
    assert_eq!(workflows[0].metadata["workflow"], "Create a new report");
    assert_eq!(workflows[0].metadata["workflowIndex"], 1);
    assert!(workflows[0]
        .document
        .contains("Action / Workflow: Create a new report"));
    assert!(workflows[0].document.contains("- Export report as CSV"));
    assert!(!workflows[0].document.contains("- Create a new report"));
}

#[tokio::test]
async fn embeds_texts_in_batches() {
    let ollama = fake_ollama().await;
    let client = Ollama::new(Some(&ollama.url));
    let texts: Vec<String> = (0..45).map(|index| format!("text {index}")).collect();

    let embeddings = client.embed("nomic-embed-text", &texts, 20).await.unwrap();
    assert_eq!(embeddings.len(), 45);

    let calls = ollama.calls_to("/api/embed");
    let sizes: Vec<usize> = calls
        .iter()
        .map(|c| c.body["input"].as_array().unwrap().len())
        .collect();
    assert_eq!(sizes, vec![20, 20, 5]);
    assert_eq!(calls[0].body["model"], "nomic-embed-text");

    assert!(client
        .embed("nomic-embed-text", &[], 20)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn embedding_errors_name_the_model() {
    let ollama = start_mock(Arc::new(|_: &Call| {
        (404, r#"{"error":"model \"nope\" not found"}"#.to_string())
    }))
    .await;
    let error = Ollama::new(Some(&ollama.url))
        .embed("nope", &["text".to_string()], 20)
        .await
        .unwrap_err()
        .to_string();
    assert!(error.contains("Failed to generate embeddings with Ollama model \"nope\""));
    assert!(error.contains("Run: ollama pull nope"));
}

#[tokio::test]
async fn indexes_knowledge_into_a_named_collection() {
    let ollama = fake_ollama().await;
    let chroma = fake_chroma().await;

    let result = index_knowledge_to_chroma(
        &IndexKnowledgeOptions {
            knowledge: Some(sample_knowledge()),
            ollama_url: Some(ollama.url.clone()),
            chroma: chroma_at(&chroma),
            ..IndexKnowledgeOptions::default()
        },
        Path::new("unused-reports-dir"),
    )
    .await
    .unwrap();

    assert_eq!(result.collection_name, "admin_dev_frf_app_fanzeal_com_kb");
    assert_eq!(result.total_chunks, 5);
    assert_eq!(result.screens_count, 1);
    assert_eq!(result.embed_model, "nomic-embed-text");

    let created = &chroma.calls_to("/collections")[0];
    assert_eq!(created.body["name"], "admin_dev_frf_app_fanzeal_com_kb");
    assert_eq!(created.body["get_or_create"], true);
    assert_eq!(created.body["metadata"]["totalScreens"], 1);

    let upserts = chroma.calls_to("/col-1/upsert");
    assert_eq!(upserts.len(), 1);
    let body = &upserts[0].body;
    assert_eq!(body["ids"].as_array().unwrap().len(), 5);
    assert_eq!(body["ids"][0], "global-site-overview");
    assert_eq!(body["embeddings"][4], json!([4.0, 0.5]));
    assert_eq!(body["metadatas"][1]["chunkType"], "screen_overview");
    assert!(body["documents"][2]
        .as_str()
        .unwrap()
        .contains("Create a new report"));
}

#[tokio::test]
async fn indexes_from_a_run_directory_and_reports_missing_knowledge() {
    let ollama = fake_ollama().await;
    let chroma = fake_chroma().await;
    let reports = tempfile::tempdir().unwrap();
    let run_dir = reports.path().join("Example-Site_1a2b");
    std::fs::create_dir_all(&run_dir).unwrap();

    let options = |run_dir: Option<String>| IndexKnowledgeOptions {
        run_dir,
        ollama_url: Some(ollama.url.clone()),
        chroma: chroma_at(&chroma),
        ..IndexKnowledgeOptions::default()
    };

    let missing = index_knowledge_to_chroma(&options(None), reports.path())
        .await
        .unwrap_err();
    assert!(missing
        .to_string()
        .contains("knowledge.json file not found"));

    std::fs::write(
        run_dir.join("knowledge.json"),
        serde_json::to_string(&sample_knowledge()).unwrap(),
    )
    .unwrap();

    let explicit = index_knowledge_to_chroma(
        &options(Some(run_dir.to_string_lossy().into_owned())),
        reports.path(),
    )
    .await
    .unwrap();
    assert_eq!(explicit.collection_name, "example-site_1a2b");

    let latest = index_knowledge_to_chroma(&options(None), reports.path())
        .await
        .unwrap();
    assert_eq!(latest.collection_name, "example-site_1a2b");
}

#[tokio::test]
async fn searches_chroma_and_synthesizes_an_answer() {
    let ollama = fake_ollama().await;
    let chroma = fake_chroma().await;

    let options = SearchRagOptions {
        query: "  how can I create a report?  ".to_string(),
        collection_name: Some("Known KB".to_string()),
        n_results: Some(2),
        ollama_url: Some(ollama.url.clone()),
        ollama_model: Some("test-model".to_string()),
        chroma: chroma_at(&chroma),
        ..SearchRagOptions::default()
    };
    let result = search_rag(&options).await.unwrap();

    assert_eq!(result.query, "how can I create a report?");
    assert_eq!(result.collection_name, "known_kb");
    assert_eq!(result.results_count, 2);
    assert_eq!(
        result.answer.as_deref(),
        Some("Open the Reports screen and press Create.")
    );

    let first = &result.results[0];
    assert_eq!(first.id, "screen-001-wf-1");
    assert_eq!(
        first.screen_name.as_deref(),
        Some("Reports & Analytics Center")
    );
    assert_eq!(first.chunk_type.as_deref(), Some("workflow"));
    assert_eq!(first.distance, Some(0.1235));
    assert_eq!(first.score, Some(0.8765));
    assert_eq!(first.metadata["workflow"], "Create a new report");

    let second = &result.results[1];
    assert_eq!(second.document, "");
    assert_eq!(second.screen_name, None);
    assert_eq!(second.score, Some(0.0), "scores never go below zero");

    let query = &chroma.calls_to("/col-1/query")[0].body;
    assert_eq!(query["n_results"], 2);
    assert_eq!(query["query_embeddings"], json!([[0.0, 0.5]]));
    assert_eq!(
        query["include"],
        json!(["documents", "metadatas", "distances"])
    );

    let chat = &ollama.calls_to("/api/chat")[0].body;
    assert_eq!(chat["model"], "test-model");
    assert!(chat.get("format").is_none());
    let prompt = chat["messages"][0]["content"].as_str().unwrap();
    assert!(prompt.contains("[Source 1: Reports & Analytics Center (https://admin.dev.frf.app.fanzeal.com/reports) - Type: list-management]"));
    assert!(prompt.contains("[Source 2: Screen (N/A) - Type: page]"));
    assert!(prompt.contains("User Question:\nhow can I create a report?"));

    let without_answer = search_rag(&SearchRagOptions {
        generate_answer: Some(false),
        ..options
    })
    .await
    .unwrap();
    assert_eq!(without_answer.answer, None);
    assert_eq!(ollama.calls_to("/api/chat").len(), 1);
}

#[tokio::test]
async fn search_rejects_empty_queries_and_unknown_collections() {
    let ollama = fake_ollama().await;
    let chroma = fake_chroma().await;
    let options = |query: &str, collection: &str| SearchRagOptions {
        query: query.to_string(),
        collection_name: Some(collection.to_string()),
        ollama_url: Some(ollama.url.clone()),
        chroma: chroma_at(&chroma),
        ..SearchRagOptions::default()
    };

    let empty = search_rag(&options("   ", "known_kb")).await.unwrap_err();
    assert!(empty.to_string().contains("Search query cannot be empty."));

    let unknown = search_rag(&options("anything", "missing"))
        .await
        .unwrap_err()
        .to_string();
    assert!(unknown.contains("ChromaDB collection \"missing\" not found"));
    assert!(unknown.contains("Collection does not exist"));
}

#[tokio::test]
async fn answer_synthesis_handles_no_results_and_model_failures() {
    let ollama = Ollama::new(Some("http://127.0.0.1:9"));
    let none = synthesize_rag_answer(&ollama, "model", "question", &[]).await;
    assert_eq!(
        none,
        "No relevant screens or workflows were found in the knowledge base to answer this question."
    );

    let item = localith::types::RagSearchResultItem {
        id: "x".to_string(),
        document: "doc".to_string(),
        ..Default::default()
    };
    let failed = synthesize_rag_answer(&ollama, "model", "question", &[item]).await;
    assert!(failed.starts_with(
        "Retrieved 1 relevant screen(s), but could not generate AI synthesis answer:"
    ));
}
