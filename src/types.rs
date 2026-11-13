//! Data shapes shared by the crawler, the knowledge engine, RAG and the HTTP API.
//!
//! Field names are serialized in camelCase so reports written by earlier
//! (TypeScript) versions of Localith stay readable and the web UI is unchanged.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Default)]
pub struct AuthOptions {
    pub enabled: bool,
    pub email: Option<String>,
    pub password: Option<String>,
}

#[derive(Debug, Clone)]
pub struct CrawlOptions {
    pub start_url: String,
    pub max_depth: u32,
    pub max_pages: usize,
    pub max_pages_per_pattern: usize,
    pub delay_ms: u64,
    pub page_timeout_ms: u64,
    pub max_snapshot_chars: usize,
    pub max_page_text_chars: usize,
    pub generate_knowledge: bool,
    pub include_subdomains: bool,
    pub respect_robots: bool,
    pub fresh: bool,
    pub auth: AuthOptions,
    pub ollama_url: String,
    pub ollama_model: String,
    pub screenshot: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ExtractedLink {
    #[serde(default)]
    pub href: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Heading {
    #[serde(default)]
    pub tag: String,
    #[serde(default)]
    pub text: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PageExtraction {
    pub title: String,
    pub description: String,
    pub canonical: String,
    pub headings: Vec<Heading>,
    pub links: Vec<ExtractedLink>,
    pub text: String,
    pub password_fields: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AiAnalysis {
    pub summary: String,
    pub page_type: String,
    pub key_topics: Vec<String>,
    pub important_entities: Vec<String>,
    pub recommendations: Vec<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PageStatus {
    #[default]
    Ok,
    Error,
    Skipped,
}

impl PageStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            PageStatus::Ok => "ok",
            PageStatus::Error => "error",
            PageStatus::Skipped => "skipped",
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrawledPage {
    pub url: String,
    pub final_url: String,
    pub depth: u32,
    pub status: PageStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub canonical: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headings: Option<Vec<Heading>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links: Option<Vec<ExtractedLink>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub links_found: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub analysis: Option<AiAnalysis>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_wall: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_handled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    /// Accessibility snapshot when a caller passes a report inline (never written to report.json).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub snapshot: Option<String>,
    pub crawled_at: String,
}

impl CrawledPage {
    /// The URL the browser ended up on, falling back to the requested URL.
    pub fn effective_url(&self) -> &str {
        if self.final_url.is_empty() {
            &self.url
        } else {
            &self.final_url
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct AuthSetting {
    pub enabled: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ReportSettings {
    pub max_depth: u32,
    pub max_pages: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_pages_per_pattern: Option<usize>,
    pub delay_ms: u64,
    pub include_subdomains: bool,
    pub respect_robots: bool,
    pub auth: AuthSetting,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generate_knowledge: Option<bool>,
    pub ollama_model: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrawlTotals {
    pub discovered: usize,
    pub visited: usize,
    pub succeeded: usize,
    pub failed: usize,
    pub skipped_by_robots: usize,
    pub skipped_by_auth: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub skipped_by_pattern: Option<usize>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PatternCount {
    pub crawled: usize,
    pub skipped: usize,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrawlReport {
    pub start_url: String,
    pub started_at: String,
    pub finished_at: String,
    pub settings: ReportSettings,
    /// Absent in hand-written or partial reports; counters are then derived from the pages.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub totals: Option<CrawlTotals>,
    pub pages: Vec<CrawledPage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern_groups: Option<IndexMap<String, PatternCount>>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ScreenKnowledge {
    pub screen_id: String,
    pub pattern: String,
    pub screen_name: String,
    pub screen_type: String,
    pub is_pattern_group: bool,
    pub sample_urls: Vec<String>,
    pub summary: String,
    pub purpose: String,
    pub key_workflows: Vec<String>,
    pub data_entities: Vec<String>,
    pub ui_components: Vec<String>,
    pub keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub is_fallback: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_file: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct CrawlKnowledgeResult {
    pub start_url: String,
    pub generated_at: String,
    pub ollama_model: String,
    pub total_screens: usize,
    pub total_pattern_groups: usize,
    pub screens: Vec<ScreenKnowledge>,
    pub global_keywords: Vec<String>,
    pub site_summary: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct KnowledgeChunk {
    pub id: String,
    pub document: String,
    pub metadata: IndexMap<String, Value>,
}

/// Where ChromaDB lives. Unset fields fall back to CHROMA_* environment variables.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ChromaConnectionOptions {
    pub chroma_url: Option<String>,
    pub chroma_host: Option<String>,
    pub chroma_port: Option<u16>,
    pub chroma_ssl: Option<bool>,
    #[serde(alias = "tenant")]
    pub chroma_tenant: Option<String>,
    #[serde(alias = "database")]
    pub chroma_database: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct IndexKnowledgeOptions {
    pub collection_name: Option<String>,
    pub database_name: Option<String>,
    pub knowledge: Option<CrawlKnowledgeResult>,
    pub knowledge_path: Option<String>,
    pub run_dir: Option<String>,
    pub report_path: Option<String>,
    pub embed_model: Option<String>,
    pub ollama_url: Option<String>,
    #[serde(flatten)]
    pub chroma: ChromaConnectionOptions,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IndexKnowledgeResult {
    pub collection_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub database_name: Option<String>,
    pub total_chunks: usize,
    pub screens_count: usize,
    pub embed_model: String,
    pub indexed_at: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SearchRagOptions {
    pub query: String,
    pub collection_name: Option<String>,
    pub database_name: Option<String>,
    pub n_results: Option<usize>,
    pub generate_answer: Option<bool>,
    pub embed_model: Option<String>,
    pub ollama_model: Option<String>,
    pub ollama_url: Option<String>,
    #[serde(flatten)]
    pub chroma: ChromaConnectionOptions,
}

#[derive(Debug, Clone, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RagSearchResultItem {
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub screen_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pattern: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chunk_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    pub document: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub score: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub distance: Option<f64>,
    pub metadata: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SearchRagResult {
    pub query: String,
    pub collection_name: String,
    pub results_count: usize,
    pub results: Vec<RagSearchResultItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub answer: Option<String>,
}
