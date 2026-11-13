//! HTTP API and web UI.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::response::Response;
use axum::Router;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::config;
use crate::knowledge::{generate_crawl_knowledge, GenerateKnowledgeOptions};
use crate::ollama::Ollama;
use crate::rag::chroma::ChromaClient;
use crate::rag::{index_knowledge_to_chroma, search_rag};
use crate::types::{ChromaConnectionOptions, CrawlReport, IndexKnowledgeOptions, SearchRagOptions};
use crate::util::{find_latest_run_dir, read_json_file};

const UI_HTML: &str = include_str!("../public/index.html");
const MAX_BODY_BYTES: usize = 50 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct AppState {
    pub reports_dir: PathBuf,
}

/// A response with a specific status; anything else becomes a 500.
struct ApiError {
    status: StatusCode,
    body: Value,
}

impl ApiError {
    fn new(status: StatusCode, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": message.into() }),
        }
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(err: anyhow::Error) -> Self {
        eprintln!("Server error: {err:#}");
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            body: json!({ "error": "Internal server error", "message": format!("{err:#}") }),
        }
    }
}

type ApiResult = std::result::Result<Response, ApiError>;

/// Maps a report id from the URL to its directory under `reports_dir`.
/// Returns `None` for ids that could escape that directory.
pub fn resolve_report_dir(reports_dir: &Path, report_id: &str) -> Option<PathBuf> {
    let valid = !report_id.is_empty()
        && report_id != "."
        && report_id != ".."
        && report_id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    valid.then(|| reports_dir.join(report_id))
}

fn with_cors(mut response: Response) -> Response {
    let headers = response.headers_mut();
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_ORIGIN,
        HeaderValue::from_static("*"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_METHODS,
        HeaderValue::from_static("GET, POST, OPTIONS"),
    );
    headers.insert(
        header::ACCESS_CONTROL_ALLOW_HEADERS,
        HeaderValue::from_static("Content-Type, Authorization"),
    );
    response
}

fn json_response(status: StatusCode, data: &Value) -> Response {
    let body = serde_json::to_string_pretty(data).unwrap_or_else(|_| "{}".to_string());
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/json"),
    );
    with_cors(response)
}

fn ok(data: Value) -> ApiResult {
    Ok(json_response(StatusCode::OK, &data))
}

fn ui_response() -> Response {
    let mut response = Response::new(Body::from(UI_HTML));
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/html; charset=utf-8"),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    response
}

async fn parse_body<T: DeserializeOwned + Default>(body: Body) -> std::result::Result<T, ApiError> {
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::new(StatusCode::PAYLOAD_TOO_LARGE, "Request body too large."))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(T::default());
    }
    serde_json::from_slice(&bytes).map_err(|err| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("Invalid JSON in request body: {err}"),
        )
    })
}

fn query_param(uri: &axum::http::Uri, names: &[&str]) -> Option<String> {
    let query = uri.query().unwrap_or("");
    names.iter().find_map(|name| {
        url::form_urlencoded::parse(query.as_bytes())
            .find(|(key, value)| key == name && !value.is_empty())
            .map(|(_, value)| value.into_owned())
    })
}

fn decode_path_segment(segment: &str) -> String {
    percent_encoding::percent_decode_str(segment)
        .decode_utf8_lossy()
        .into_owned()
}

fn api_index() -> Value {
    let endpoint = |method: &str, path: &str, description: &str| json!({ "method": method, "path": path, "description": description });
    json!({
        "name": "Local Website Scanner - AI Knowledge & RAG Engine",
        "description": "Consumes crawl results to produce knowledge and powers RAG search using ChromaDB and Ollama nomic-embed-text.",
        "endpoints": [
            endpoint("POST", "/api/knowledge", "Generate screen knowledge and keywords from crawl result."),
            endpoint("POST", "/api/analyze", "Alias for /api/knowledge."),
            endpoint("GET", "/ui", "Web UI: browse reports, screen map, and ask questions."),
            endpoint("GET", "/api/reports", "List available crawl reports."),
            endpoint("GET", "/api/reports/:id", "Get crawl settings, totals and page list for a report run."),
            endpoint("GET", "/api/reports/:id/knowledge", "Get saved knowledge for a specific report run."),
            endpoint("POST", "/api/reports/:id/knowledge", "Generate knowledge for a specific report run."),
            endpoint("POST", "/api/rag/index", "Index knowledge.json into ChromaDB using nomic-embed-text."),
            endpoint("POST", "/api/rag/search", "Search ChromaDB with question query and return AI answer + matching screens."),
            endpoint("GET", "/api/rag/search", "Query RAG database via query parameters (?q=...&collection=...)."),
            endpoint("GET", "/api/rag/collections", "List ChromaDB collections."),
            endpoint("GET", "/health", "Check server, Ollama, and ChromaDB status.")
        ]
    })
}

async fn health() -> ApiResult {
    let ollama = Ollama::new(None);
    let chroma = ChromaClient::new(&ChromaConnectionOptions::default());
    let (ollama_connected, chroma_version) = tokio::join!(ollama.is_reachable(), chroma.version());
    ok(json!({
        "status": "ok",
        "ollamaUrl": ollama.base_url(),
        "ollamaConnected": ollama_connected,
        "chromaConnected": chroma_version.is_ok(),
        "defaultModel": config::ollama_model(None),
        "defaultEmbedModel": config::embed_model(None)
    }))
}

async fn list_reports(reports_dir: &Path) -> ApiResult {
    let mut reports: Vec<Value> = Vec::new();
    let mut names: Vec<String> = match std::fs::read_dir(reports_dir) {
        Ok(entries) => entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().join("report.json").is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect(),
        Err(_) => Vec::new(),
    };
    names.sort();

    for name in names {
        let dir = reports_dir.join(&name);
        let has_knowledge = dir.join("knowledge.json").is_file();
        match read_json_file::<CrawlReport>(&dir.join("report.json")).await {
            Ok(report) => reports.push(json!({
                "id": name,
                "startUrl": report.start_url,
                "startedAt": report.started_at,
                "finishedAt": report.finished_at,
                "pagesCount": report.pages.len(),
                "hasKnowledge": has_knowledge
            })),
            Err(_) => reports.push(json!({ "id": name, "hasKnowledge": has_knowledge })),
        }
    }
    ok(json!({ "reports": reports }))
}

/// Resolves `:id` to an existing report directory, or the matching 400/404.
fn existing_report_dir(
    reports_dir: &Path,
    report_id: &str,
) -> std::result::Result<PathBuf, ApiError> {
    let dir = resolve_report_dir(reports_dir, report_id)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Invalid report id."))?;
    if !dir.join("report.json").is_file() {
        return Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("Report \"{report_id}\" not found."),
        ));
    }
    Ok(dir)
}

async fn report_overview(reports_dir: &Path, report_id: &str) -> ApiResult {
    let dir = existing_report_dir(reports_dir, report_id)?;
    let report: CrawlReport = read_json_file(&dir.join("report.json")).await?;
    let pages: Vec<Value> = report
        .pages
        .iter()
        .map(|page| {
            json!({
                "url": page.url,
                "finalUrl": page.final_url,
                "depth": page.depth,
                "status": page.status,
                "title": page.title,
                "pattern": page.pattern
            })
        })
        .collect();
    ok(json!({
        "id": report_id,
        "startUrl": report.start_url,
        "startedAt": report.started_at,
        "finishedAt": report.finished_at,
        "settings": report.settings,
        "totals": report.totals,
        "hasKnowledge": dir.join("knowledge.json").is_file(),
        "pages": pages
    }))
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct KnowledgePayload {
    report: Option<CrawlReport>,
    report_path: Option<String>,
    run_dir: Option<String>,
    ollama_url: Option<String>,
    ollama_model: Option<String>,
    force_reanalyze: Option<bool>,
    force: Option<bool>,
}

impl KnowledgePayload {
    fn force(&self) -> bool {
        self.force_reanalyze.or(self.force).unwrap_or(false)
    }
}

async fn report_knowledge(
    reports_dir: &Path,
    report_id: &str,
    method: &Method,
    body: Body,
) -> ApiResult {
    let dir = existing_report_dir(reports_dir, report_id)?;

    if method == Method::GET {
        let knowledge_path = dir.join("knowledge.json");
        if !knowledge_path.is_file() {
            return Err(ApiError::new(
                StatusCode::NOT_FOUND,
                format!(
                    "Knowledge not yet generated for report \"{report_id}\". POST to /api/reports/{report_id}/knowledge to generate."
                ),
            ));
        }
        return ok(read_json_file::<Value>(&knowledge_path).await?);
    }

    let payload: KnowledgePayload = parse_body(body).await?;
    let knowledge = generate_crawl_knowledge(GenerateKnowledgeOptions {
        report_path: Some(dir.join("report.json")),
        run_dir: Some(dir),
        ollama_url: payload.ollama_url.clone(),
        ollama_model: payload.ollama_model.clone(),
        force_reanalyze: payload.force(),
        report: None,
    })
    .await?;
    ok(json!({ "success": true, "reportId": report_id, "data": knowledge }))
}

async fn generate_knowledge(reports_dir: &Path, body: Body) -> ApiResult {
    let payload: KnowledgePayload = parse_body(body).await?;
    let force = payload.force();
    let mut run_dir = payload.run_dir.map(PathBuf::from);
    let report_path = payload.report_path.map(PathBuf::from);

    if payload.report.is_none() && report_path.is_none() && run_dir.is_none() {
        run_dir = find_latest_run_dir(reports_dir, "report.json");
        if run_dir.is_none() {
            return Err(ApiError::new(
                StatusCode::BAD_REQUEST,
                "No crawl result provided and no existing reports found in ./reports. Run a crawl first or provide 'report' in request body.",
            ));
        }
    }

    let target = run_dir
        .as_ref()
        .or(report_path.as_ref())
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "in-memory report".to_string());
    println!("\n🧠 Processing Knowledge & Keywords request for {target}...");

    let result = generate_crawl_knowledge(GenerateKnowledgeOptions {
        report: payload.report,
        report_path,
        run_dir,
        ollama_url: payload.ollama_url,
        ollama_model: payload.ollama_model,
        force_reanalyze: force,
    })
    .await?;

    println!(
        "   ✓ Knowledge generated: {} screens ({} pattern groups), {} global keywords.",
        result.total_screens,
        result.total_pattern_groups,
        result.global_keywords.len()
    );
    ok(json!({ "success": true, "data": result }))
}

async fn rag_index(reports_dir: &Path, body: Body) -> ApiResult {
    let options: IndexKnowledgeOptions = parse_body(body).await?;
    println!(
        "\n📚 Processing RAG index request for collection \"{}\"...",
        options.collection_name.as_deref().unwrap_or("(auto)")
    );
    let result = index_knowledge_to_chroma(&options, reports_dir).await?;
    println!(
        "   ✓ Indexed {} chunks into collection \"{}\".",
        result.total_chunks, result.collection_name
    );
    ok(json!({ "success": true, "data": result }))
}

async fn rag_search(options: SearchRagOptions) -> ApiResult {
    println!(
        "\n🔎 RAG search: \"{}\" on collection \"{}\"...",
        options.query,
        options.collection_name.as_deref().unwrap_or("(default)")
    );
    let result = search_rag(&options).await?;
    ok(json!({ "success": true, "data": result }))
}

async fn rag_collections() -> ApiResult {
    let chroma = ChromaClient::new(&ChromaConnectionOptions::default());
    match chroma.list_collections().await {
        Ok(collections) => ok(json!({
            "success": true,
            "count": collections.len(),
            "collections": collections
                .iter()
                .map(|c| json!({ "name": c.name, "id": c.id, "metadata": c.metadata }))
                .collect::<Vec<_>>()
        })),
        Err(err) => Ok(json_response(
            StatusCode::BAD_GATEWAY,
            &json!({ "error": "Failed to connect to ChromaDB server", "message": format!("{err:#}") }),
        )),
    }
}

async fn route(state: &AppState, request: Request) -> ApiResult {
    let (parts, body) = request.into_parts();
    let method = &parts.method;
    let path = parts.uri.path();
    let reports_dir = state.reports_dir.as_path();
    let is_get = method == Method::GET;
    let is_post = method == Method::POST;

    // Web UI: /ui always, and / when a browser asks for HTML.
    let wants_html = parts
        .headers
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|accept| accept.contains("text/html"));
    if is_get && (path == "/ui" || path == "/ui/" || (path == "/" && wants_html)) {
        return Ok(ui_response());
    }

    if let Some(rest) = path.strip_prefix("/api/reports/") {
        let segments: Vec<&str> = rest.split('/').collect();
        match segments.as_slice() {
            [id] if is_get => return report_overview(reports_dir, &decode_path_segment(id)).await,
            [id, "knowledge"] if is_get || is_post => {
                return report_knowledge(reports_dir, &decode_path_segment(id), method, body).await
            }
            _ => {}
        }
    }

    match (path, is_get, is_post) {
        ("/", true, _) => ok(api_index()),
        ("/health", true, _) => health().await,
        ("/api/reports", true, _) => list_reports(reports_dir).await,
        ("/api/knowledge" | "/api/analyze", _, true) => generate_knowledge(reports_dir, body).await,
        ("/api/rag/index", _, true) => rag_index(reports_dir, body).await,
        ("/api/rag/search" | "/api/rag/query", _, true) => {
            let options: SearchRagOptions = parse_body(body).await?;
            if options.query.is_empty() {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Query is required in request body: { query: 'how can I create a report?' }",
                ));
            }
            rag_search(options).await
        }
        ("/api/rag/search", true, _) => {
            let Some(query) = query_param(&parts.uri, &["q", "query"]) else {
                return Err(ApiError::new(
                    StatusCode::BAD_REQUEST,
                    "Missing required query parameter 'q' or 'query'. Example: /api/rag/search?q=how+can+I+create+a+report",
                ));
            };
            rag_search(SearchRagOptions {
                query,
                collection_name: query_param(&parts.uri, &["collection", "collectionName"]),
                n_results: query_param(&parts.uri, &["n", "nResults"]).and_then(|n| n.parse().ok()),
                generate_answer: Some(
                    query_param(&parts.uri, &["generateAnswer"]).as_deref() != Some("false"),
                ),
                ..SearchRagOptions::default()
            })
            .await
        }
        ("/api/rag/collections", true, _) => rag_collections().await,
        _ => Err(ApiError::new(
            StatusCode::NOT_FOUND,
            format!("Endpoint not found: {method} {path}"),
        )),
    }
}

async fn handle(State(state): State<Arc<AppState>>, request: Request) -> Response {
    if request.method() == Method::OPTIONS {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NO_CONTENT;
        return with_cors(response);
    }
    match route(&state, request).await {
        Ok(response) => response,
        Err(err) => json_response(err.status, &err.body),
    }
}

/// The API as a router; `reports_dir` is where crawl runs are stored.
pub fn app(reports_dir: PathBuf) -> Router {
    Router::new()
        .fallback(handle)
        .with_state(Arc::new(AppState { reports_dir }))
}

/// Serves the API and web UI until Ctrl+C.
pub async fn serve() -> Result<()> {
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|raw| raw.trim().parse().ok())
        .unwrap_or(3000);
    // Local only by default: the API has no authentication.
    let host = std::env::var("HOST")
        .ok()
        .filter(|h| !h.is_empty())
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let reports_dir = std::env::current_dir()?.join("reports");

    let listener = tokio::net::TcpListener::bind((host.as_str(), port))
        .await
        .map_err(|err| anyhow!("Could not listen on {host}:{port}: {err}"))?;

    println!(
        "\n🚀 Local Website Scanner - AI Knowledge & RAG Server running on http://localhost:{port}"
    );
    println!("   Web UI                 -> http://localhost:{port}/ui");
    println!("   POST /api/knowledge    -> Generate knowledge from crawl results");
    println!("   POST /api/rag/index    -> Index knowledge into ChromaDB with nomic-embed-text");
    println!("   POST /api/rag/search   -> Semantic search & AI answers from ChromaDB");
    println!("   GET  /api/rag/search   -> Query RAG via GET (?q=...)");
    println!("   GET  /api/reports      -> List all crawl reports");
    println!("   GET  /health           -> Check server, Ollama, and ChromaDB status\n");

    axum::serve(listener, app(reports_dir))
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    Ok(())
}

/// Resolves on Ctrl+C, or on SIGTERM (what `docker stop` sends).
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        match signal(SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                _ = ctrl_c => {}
                _ = terminate.recv() => {}
            },
            Err(_) => ctrl_c.await,
        }
    }
    #[cfg(not(unix))]
    ctrl_c.await;
}
