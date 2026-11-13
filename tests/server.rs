use std::path::{Path, PathBuf};

use localith::server::{app, resolve_report_dir};
use serde_json::{json, Value};

/// Starts the API on a free local port, serving reports from `reports_dir`.
async fn start_server(reports_dir: PathBuf) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move {
        axum::serve(listener, app(reports_dir)).await.unwrap();
    });
    url
}

fn write_report(reports_dir: &Path, id: &str, with_knowledge: bool) {
    let dir = reports_dir.join(id);
    std::fs::create_dir_all(&dir).unwrap();
    let report = json!({
        "startUrl": "https://example.com/",
        "startedAt": "2026-11-01T00:00:00.000Z",
        "finishedAt": "2026-11-01T00:01:00.000Z",
        "settings": { "maxDepth": 2, "maxPages": 5, "ollamaModel": "qwen3:4b" },
        "totals": { "discovered": 3, "visited": 2, "succeeded": 1, "failed": 0 },
        "pages": [
            { "url": "https://example.com/", "finalUrl": "https://example.com/", "depth": 0,
              "status": "ok", "title": "Home", "text": "long page text", "crawledAt": "" },
            { "url": "https://example.com/items/7", "finalUrl": "https://example.com/items/7", "depth": 2,
              "status": "skipped", "pattern": "https://example.com/items/{{id}}", "crawledAt": "" }
        ]
    });
    std::fs::write(dir.join("report.json"), report.to_string()).unwrap();
    if with_knowledge {
        let knowledge =
            json!({ "startUrl": "https://example.com/", "screens": [], "totalScreens": 0 });
        std::fs::write(dir.join("knowledge.json"), knowledge.to_string()).unwrap();
    }
}

async fn get_json(url: &str) -> (u16, Value) {
    let response = reqwest::get(url).await.unwrap();
    let status = response.status().as_u16();
    (status, response.json().await.unwrap())
}

#[test]
fn resolve_report_dir_rejects_path_traversal() {
    let reports = Path::new("reports");
    assert_eq!(
        resolve_report_dir(reports, "books-toscrape-com-3060d727"),
        Some(reports.join("books-toscrape-com-3060d727"))
    );
    assert_eq!(
        resolve_report_dir(reports, "run_1.2"),
        Some(reports.join("run_1.2"))
    );
    for bad in [
        "..",
        ".",
        "",
        "../secret",
        "a/b",
        "a\\b",
        "C:evil",
        "name with space",
    ] {
        assert_eq!(
            resolve_report_dir(reports, bad),
            None,
            "{bad:?} must be rejected"
        );
    }
}

#[tokio::test]
async fn root_serves_api_documentation_and_cors_preflight() {
    let base = start_server(PathBuf::from("unused")).await;
    let client = reqwest::Client::new();

    let response = client.get(&base).send().await.unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "application/json");
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    let body: Value = response.json().await.unwrap();
    assert_eq!(
        body["name"],
        "Local Website Scanner - AI Knowledge & RAG Engine"
    );
    assert!(body["endpoints"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["path"] == "/ui"));

    let preflight = client
        .request(reqwest::Method::OPTIONS, format!("{base}/api/rag/search"))
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), 204);
    assert_eq!(
        preflight.headers()["access-control-allow-methods"],
        "GET, POST, OPTIONS"
    );

    let (status, body) = get_json(&format!("{base}/nope")).await;
    assert_eq!(status, 404);
    assert_eq!(body["error"], "Endpoint not found: GET /nope");
}

#[tokio::test]
async fn ui_is_served_as_html_and_root_negotiates_content() {
    let base = start_server(PathBuf::from("unused")).await;
    let client = reqwest::Client::new();

    for path in ["/ui", "/ui/"] {
        let response = client.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), 200);
        assert_eq!(
            response.headers()["content-type"],
            "text/html; charset=utf-8"
        );
        assert!(response.text().await.unwrap().contains("<!doctype html>"));
    }

    let browser = client
        .get(&base)
        .header("accept", "text/html,application/xhtml+xml")
        .send()
        .await
        .unwrap();
    assert!(browser.headers()["content-type"]
        .to_str()
        .unwrap()
        .starts_with("text/html"));

    let api_client = client
        .get(&base)
        .header("accept", "application/json")
        .send()
        .await
        .unwrap();
    assert_eq!(api_client.headers()["content-type"], "application/json");
}

#[tokio::test]
async fn report_endpoints_list_show_and_validate_ids() {
    let reports = tempfile::tempdir().unwrap();
    write_report(reports.path(), "example-com-1111aaaa", true);
    write_report(reports.path(), "other-site-2222bbbb", false);
    std::fs::create_dir_all(reports.path().join("not-a-report")).unwrap();
    let base = start_server(reports.path().to_path_buf()).await;

    let (status, list) = get_json(&format!("{base}/api/reports")).await;
    assert_eq!(status, 200);
    let items = list["reports"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["id"], "example-com-1111aaaa");
    assert_eq!(items[0]["pagesCount"], 2);
    assert_eq!(items[0]["hasKnowledge"], true);
    assert_eq!(items[1]["hasKnowledge"], false);

    let (status, overview) = get_json(&format!("{base}/api/reports/example-com-1111aaaa")).await;
    assert_eq!(status, 200);
    assert_eq!(overview["startUrl"], "https://example.com/");
    assert_eq!(overview["totals"]["visited"], 2);
    assert_eq!(overview["settings"]["maxDepth"], 2);
    assert_eq!(overview["pages"][0]["title"], "Home");
    assert_eq!(
        overview["pages"][1]["pattern"],
        "https://example.com/items/{{id}}"
    );
    assert!(
        overview["pages"][0].get("text").is_none(),
        "page bodies are not sent to the UI"
    );

    let (status, knowledge) = get_json(&format!(
        "{base}/api/reports/example-com-1111aaaa/knowledge"
    ))
    .await;
    assert_eq!(status, 200);
    assert_eq!(knowledge["totalScreens"], 0);

    let (status, missing) =
        get_json(&format!("{base}/api/reports/other-site-2222bbbb/knowledge")).await;
    assert_eq!(status, 404);
    assert!(missing["error"]
        .as_str()
        .unwrap()
        .contains("Knowledge not yet generated"));

    let (status, unknown) = get_json(&format!("{base}/api/reports/does-not-exist")).await;
    assert_eq!(status, 404);
    assert_eq!(unknown["error"], "Report \"does-not-exist\" not found.");

    for path in [
        "/api/reports/..%2F..%2Fsecret",
        "/api/reports/..%2Fother/knowledge",
        "/api/reports/a%5Cb",
    ] {
        let (status, body) = get_json(&format!("{base}{path}")).await;
        assert_eq!(status, 400, "{path}");
        assert_eq!(body["error"], "Invalid report id.");
    }

    let empty = start_server(reports.path().join("missing-dir")).await;
    let (status, list) = get_json(&format!("{empty}/api/reports")).await;
    assert_eq!(status, 200);
    assert_eq!(list["reports"], json!([]));
}

#[tokio::test]
async fn rag_and_knowledge_endpoints_validate_input() {
    let reports = tempfile::tempdir().unwrap();
    let base = start_server(reports.path().to_path_buf()).await;
    let client = reqwest::Client::new();

    let post = |path: &str, body: &str| {
        let request = client
            .post(format!("{base}{path}"))
            .header("content-type", "application/json")
            .body(body.to_string());
        async move {
            let response = request.send().await.unwrap();
            let status = response.status().as_u16();
            (status, response.json::<Value>().await.unwrap())
        }
    };

    let (status, body) = post("/api/rag/search", "{}").await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Query is required"));

    let (status, body) = post("/api/rag/query", "").await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Query is required"));

    let (status, body) = post("/api/rag/search", "{ not json").await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Invalid JSON in request body"));

    let (status, body) = get_json(&format!("{base}/api/rag/search")).await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("Missing required query parameter"));

    let (status, body) = post("/api/knowledge", "{}").await;
    assert_eq!(status, 400);
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("No crawl result provided"));

    let (status, body) = post("/api/rag/index", "{}").await;
    assert_eq!(status, 500);
    assert_eq!(body["error"], "Internal server error");
    assert!(body["message"]
        .as_str()
        .unwrap()
        .contains("knowledge.json file not found"));
}
