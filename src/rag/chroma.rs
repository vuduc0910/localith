//! Small client for the ChromaDB HTTP API (v2).

use std::sync::LazyLock;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use url::Url;

use crate::types::{ChromaConnectionOptions, KnowledgeChunk};
use crate::util::env_var;

const DEFAULT_TENANT: &str = "default_tenant";
const DEFAULT_DATABASE: &str = "default_database";

static INVALID_NAME_CHARS: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[^a-z0-9_-]+").expect("valid built-in regex"));
static UNDERSCORE_RUN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"_+").expect("valid built-in regex"));

fn is_name_edge(c: char) -> bool {
    !(c.is_ascii_lowercase() || c.is_ascii_digit())
}

/// Turns any string into a valid ChromaDB collection name: 3-63 characters,
/// only `[a-z0-9_-]`, starting and ending with a letter or digit.
pub fn sanitize_collection_name(name: Option<&str>) -> String {
    const FALLBACK: &str = "app_knowledge_base";
    let Some(name) = name.filter(|name| !name.is_empty()) else {
        return FALLBACK.to_string();
    };

    let lowered = name.trim().to_lowercase();
    let cleaned = INVALID_NAME_CHARS.replace_all(&lowered, "_");
    let cleaned = UNDERSCORE_RUN.replace_all(&cleaned, "_");
    let mut cleaned = cleaned.trim_matches(is_name_edge).to_string();

    if cleaned.len() < 3 {
        cleaned = format!("{cleaned}_rag_kb")
            .trim_start_matches(is_name_edge)
            .to_string();
    }
    if cleaned.len() > 63 {
        cleaned = cleaned[..63].trim_end_matches(is_name_edge).to_string();
    }
    if cleaned.is_empty() {
        FALLBACK.to_string()
    } else {
        cleaned
    }
}

/// Resolved ChromaDB connection settings.
#[derive(Debug, Clone, PartialEq)]
pub struct ChromaArgs {
    pub host: String,
    pub port: u16,
    pub ssl: bool,
    pub tenant: Option<String>,
    pub database: Option<String>,
}

/// Combines explicit options with CHROMA_* environment variables.
pub fn parse_chroma_options(options: &ChromaConnectionOptions) -> ChromaArgs {
    parse_chroma_options_with(options, &env_var)
}

pub fn parse_chroma_options_with(
    options: &ChromaConnectionOptions,
    env: &dyn Fn(&str) -> Option<String>,
) -> ChromaArgs {
    let filled = |value: &Option<String>| value.clone().filter(|v| !v.is_empty());

    let mut host = filled(&options.chroma_host)
        .or_else(|| env("CHROMA_HOST"))
        .unwrap_or_else(|| "127.0.0.1".to_string());
    let mut port = options
        .chroma_port
        .or_else(|| env("CHROMA_PORT").and_then(|raw| raw.trim().parse().ok()))
        .unwrap_or(8000);
    let mut ssl = options
        .chroma_ssl
        .unwrap_or_else(|| env("CHROMA_SSL").as_deref() == Some("true"));

    // A full URL wins over the separate host/port/ssl settings.
    let target_url = filled(&options.chroma_url).or_else(|| env("CHROMA_URL"));
    if let Some(parsed) = target_url.and_then(|raw| Url::parse(&raw).ok()) {
        if let Some(url_host) = parsed.host_str() {
            host = url_host.to_string();
            ssl = parsed.scheme() == "https";
            port = parsed.port().unwrap_or(if ssl { 443 } else { 80 });
        }
    }

    ChromaArgs {
        host,
        port,
        ssl,
        tenant: filled(&options.chroma_tenant).or_else(|| env("CHROMA_TENANT")),
        database: filled(&options.chroma_database).or_else(|| env("CHROMA_DATABASE")),
    }
}

#[derive(Debug, Clone, Deserialize)]
pub struct Collection {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub metadata: Option<Value>,
}

/// Raw result of a nearest-neighbour query for one query vector.
#[derive(Debug, Clone, Default)]
pub struct QueryMatches {
    pub ids: Vec<String>,
    pub documents: Vec<Option<String>>,
    pub metadatas: Vec<Option<serde_json::Map<String, Value>>>,
    pub distances: Vec<Option<f64>>,
}

#[derive(Debug, Clone)]
pub struct ChromaClient {
    api: String,
    collections_url: String,
    http: reqwest::Client,
}

impl ChromaClient {
    pub fn new(options: &ChromaConnectionOptions) -> Self {
        Self::from_args(&parse_chroma_options(options))
    }

    pub fn from_args(args: &ChromaArgs) -> Self {
        let scheme = if args.ssl { "https" } else { "http" };
        let api = format!("{scheme}://{}:{}/api/v2", args.host, args.port);
        let collections_url = format!(
            "{api}/tenants/{}/databases/{}/collections",
            args.tenant.as_deref().unwrap_or(DEFAULT_TENANT),
            args.database.as_deref().unwrap_or(DEFAULT_DATABASE)
        );
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(120))
            .build()
            .unwrap_or_default();
        Self {
            api,
            collections_url,
            http,
        }
    }

    async fn read<T: serde::de::DeserializeOwned>(request: reqwest::RequestBuilder) -> Result<T> {
        let response = request.send().await.context("Could not reach ChromaDB")?;
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        if !status.is_success() {
            let detail = serde_json::from_str::<Value>(&body)
                .ok()
                .and_then(|value| {
                    value
                        .get("message")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .unwrap_or_else(|| body.trim().to_string());
            bail!("ChromaDB returned {status}: {detail}");
        }
        serde_json::from_str(&body).context("Unexpected response from ChromaDB")
    }

    pub async fn version(&self) -> Result<String> {
        let request = self
            .http
            .get(format!("{}/version", self.api))
            .timeout(Duration::from_secs(3));
        let version: Value = Self::read(request).await?;
        Ok(version
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| version.to_string()))
    }

    pub async fn list_collections(&self) -> Result<Vec<Collection>> {
        Self::read(self.http.get(&self.collections_url)).await
    }

    pub async fn get_collection(&self, name: &str) -> Result<Collection> {
        Self::read(self.http.get(format!("{}/{name}", self.collections_url))).await
    }

    pub async fn get_or_create_collection(
        &self,
        name: &str,
        metadata: Value,
    ) -> Result<Collection> {
        let body = json!({ "name": name, "metadata": metadata, "get_or_create": true });
        Self::read(self.http.post(&self.collections_url).json(&body)).await
    }

    pub async fn upsert(
        &self,
        collection: &Collection,
        chunks: &[KnowledgeChunk],
        embeddings: &[Vec<f32>],
    ) -> Result<()> {
        let body = json!({
            "ids": chunks.iter().map(|chunk| &chunk.id).collect::<Vec<_>>(),
            "embeddings": embeddings,
            "metadatas": chunks.iter().map(|chunk| &chunk.metadata).collect::<Vec<_>>(),
            "documents": chunks.iter().map(|chunk| &chunk.document).collect::<Vec<_>>()
        });
        let url = format!("{}/{}/upsert", self.collections_url, collection.id);
        let _: Value = Self::read(self.http.post(url).json(&body)).await?;
        Ok(())
    }

    pub async fn query(
        &self,
        collection: &Collection,
        embedding: &[f32],
        n_results: usize,
    ) -> Result<QueryMatches> {
        /// One row per query vector; ChromaDB sends `null` for parts it did not include.
        type Rows<T> = Option<Vec<Vec<Option<T>>>>;

        #[derive(Deserialize, Default)]
        #[serde(default)]
        struct QueryResponse {
            ids: Vec<Vec<String>>,
            documents: Rows<String>,
            metadatas: Rows<serde_json::Map<String, Value>>,
            distances: Rows<f64>,
        }

        let body = json!({
            "query_embeddings": [embedding],
            "n_results": n_results,
            "include": ["documents", "metadatas", "distances"]
        });
        let url = format!("{}/{}/query", self.collections_url, collection.id);
        let response: QueryResponse = Self::read(self.http.post(url).json(&body)).await?;

        fn first<T>(rows: Rows<T>) -> Vec<Option<T>> {
            rows.and_then(|rows| rows.into_iter().next())
                .unwrap_or_default()
        }
        Ok(QueryMatches {
            ids: response.ids.into_iter().next().unwrap_or_default(),
            documents: first(response.documents),
            metadatas: first(response.metadatas),
            distances: first(response.distances),
        })
    }
}
