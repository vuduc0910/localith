//! Retrieval: index knowledge into ChromaDB and answer questions from it.

pub mod chroma;
pub mod chunks;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use serde_json::{json, Value};
use url::Url;

use crate::config;
use crate::ollama::{ChatSettings, Ollama};
use crate::types::{
    CrawlKnowledgeResult, IndexKnowledgeOptions, IndexKnowledgeResult, RagSearchResultItem,
    SearchRagOptions, SearchRagResult,
};
use crate::util::{find_latest_run_dir, now_iso, read_json_file};

use chroma::{sanitize_collection_name, ChromaClient};
use chunks::build_knowledge_chunks;

const EMBED_BATCH_SIZE: usize = 20;
const UPSERT_BATCH_SIZE: usize = 100;

/// Finds the knowledge to index: inline, an explicit file, a run directory,
/// next to a report, or the newest run under `reports/`.
pub async fn resolve_knowledge(
    options: &IndexKnowledgeOptions,
    reports_dir: &Path,
) -> Result<(CrawlKnowledgeResult, Option<PathBuf>)> {
    let mut run_dir = options.run_dir.as_ref().map(PathBuf::from);
    if let Some(knowledge) = &options.knowledge {
        return Ok((knowledge.clone(), run_dir));
    }

    let mut knowledge_path = options
        .knowledge_path
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| run_dir.as_ref().map(|dir| dir.join("knowledge.json")));

    if knowledge_path.is_none() {
        let beside_report = options
            .report_path
            .as_ref()
            .and_then(|report| Path::new(report).parent().map(Path::to_path_buf))
            .filter(|dir| dir.join("knowledge.json").is_file());
        let found = beside_report.or_else(|| find_latest_run_dir(reports_dir, "knowledge.json"));
        if let Some(dir) = found {
            knowledge_path = Some(dir.join("knowledge.json"));
            run_dir = Some(dir);
        }
    }

    let Some(knowledge_path) = knowledge_path.filter(|path| path.is_file()) else {
        bail!("knowledge.json file not found. Run a scan with --knowledge or execute: localith analyze");
    };
    Ok((read_json_file(&knowledge_path).await?, run_dir))
}

/// Embeds the knowledge with Ollama and upserts it into a ChromaDB collection.
pub async fn index_knowledge_to_chroma(
    options: &IndexKnowledgeOptions,
    reports_dir: &Path,
) -> Result<IndexKnowledgeResult> {
    let (knowledge, run_dir) = resolve_knowledge(options, reports_dir).await?;

    let raw_name = options
        .collection_name
        .clone()
        .filter(|name| !name.is_empty())
        .or_else(|| {
            let dir = run_dir.as_ref()?;
            Some(dir.file_name()?.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| match Url::parse(&knowledge.start_url) {
            Ok(url) => {
                let host: String = url
                    .host_str()
                    .unwrap_or_default()
                    .chars()
                    .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
                    .collect();
                format!("{host}_kb")
            }
            Err(_) => "website_knowledge_base".to_string(),
        });
    let collection_name = sanitize_collection_name(Some(&raw_name));

    let ollama = Ollama::new(options.ollama_url.as_deref());
    let embed_model = config::embed_model(options.embed_model.as_deref());

    let chunks = build_knowledge_chunks(&knowledge);
    let texts: Vec<String> = chunks.iter().map(|chunk| chunk.document.clone()).collect();
    let embeddings = ollama.embed(&embed_model, &texts, EMBED_BATCH_SIZE).await?;

    let chroma = ChromaClient::new(&options.chroma);
    let collection = chroma
        .get_or_create_collection(
            &collection_name,
            json!({
                "description": format!("RAG Knowledge Base for {}", knowledge.start_url),
                "generatedAt": now_iso(),
                "embedModel": embed_model,
                "startUrl": knowledge.start_url,
                "totalScreens": knowledge.screens.len()
            }),
        )
        .await?;

    for (chunk_batch, embedding_batch) in chunks
        .chunks(UPSERT_BATCH_SIZE)
        .zip(embeddings.chunks(UPSERT_BATCH_SIZE))
    {
        chroma
            .upsert(&collection, chunk_batch, embedding_batch)
            .await?;
    }

    Ok(IndexKnowledgeResult {
        collection_name,
        database_name: options
            .database_name
            .clone()
            .or_else(|| options.chroma.chroma_database.clone()),
        total_chunks: chunks.len(),
        screens_count: knowledge.screens.len(),
        embed_model,
        indexed_at: now_iso(),
    })
}

fn round4(value: f64) -> f64 {
    (value * 10_000.0).round() / 10_000.0
}

/// Finds the chunks closest to the question and, unless disabled, lets the model answer from them.
pub async fn search_rag(options: &SearchRagOptions) -> Result<SearchRagResult> {
    let query = options.query.trim();
    if query.is_empty() {
        bail!("Search query cannot be empty.");
    }

    let collection_name = sanitize_collection_name(options.collection_name.as_deref());
    let n_results = options.n_results.filter(|n| *n > 0).unwrap_or(5);
    let embed_model = config::embed_model(options.embed_model.as_deref());
    let ollama = Ollama::new(options.ollama_url.as_deref());
    let chat_model = config::ollama_model(options.ollama_model.as_deref());

    let chroma = ChromaClient::new(&options.chroma);
    let collection = chroma.get_collection(&collection_name).await.map_err(|err| {
        anyhow!(
            "ChromaDB collection \"{collection_name}\" not found. Did you run the indexing step first? Error: {err:#}"
        )
    })?;

    let query_vector = ollama
        .embed(&embed_model, &[query.to_string()], EMBED_BATCH_SIZE)
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("Failed to generate embedding vector for search query."))?;

    let matches = chroma.query(&collection, &query_vector, n_results).await?;

    let mut items: Vec<RagSearchResultItem> = Vec::with_capacity(matches.ids.len());
    for (index, id) in matches.ids.into_iter().enumerate() {
        let metadata = matches
            .metadatas
            .get(index)
            .cloned()
            .flatten()
            .unwrap_or_default();
        let distance = matches.distances.get(index).copied().flatten();
        let text = |key: &str| {
            metadata
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_string)
        };

        items.push(RagSearchResultItem {
            id,
            screen_id: text("screenId"),
            screen_name: text("screenName"),
            screen_type: text("screenType"),
            pattern: text("pattern"),
            chunk_type: text("chunkType"),
            url: text("url"),
            document: matches
                .documents
                .get(index)
                .cloned()
                .flatten()
                .unwrap_or_default(),
            score: distance.map(|d| round4(1.0 - d).max(0.0)),
            distance: distance.map(round4),
            metadata,
        });
    }

    let answer = if options.generate_answer == Some(false) {
        None
    } else {
        Some(synthesize_rag_answer(&ollama, &chat_model, query, &items).await)
    };

    Ok(SearchRagResult {
        query: query.to_string(),
        collection_name,
        results_count: items.len(),
        results: items,
        answer,
    })
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

pub fn build_answer_prompt(query: &str, items: &[RagSearchResultItem]) -> String {
    let context = items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            format!(
                "[Source {}: {} ({}) - Type: {}]\n{}",
                index + 1,
                non_empty(&item.screen_name).unwrap_or("Screen"),
                non_empty(&item.pattern)
                    .or(non_empty(&item.url))
                    .unwrap_or("N/A"),
                non_empty(&item.screen_type).unwrap_or("page"),
                item.document
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n---\n\n");

    format!(
        "You are an expert AI knowledge assistant for this web application.
Answer the user's question clearly, step-by-step, and concisely based ONLY on the following application screens and workflows context.
Always cite the relevant screen names and route URLs where the user can accomplish their task.
If the context does not contain enough information, explain what is available and clarify what was not found in the scanned knowledge base.

Context from Knowledge Base:
{context}

User Question:
{query}

Answer:"
    )
}

/// Asks the model to answer `query` using only the retrieved chunks.
pub async fn synthesize_rag_answer(
    ollama: &Ollama,
    model: &str,
    query: &str,
    items: &[RagSearchResultItem],
) -> String {
    if items.is_empty() {
        return "No relevant screens or workflows were found in the knowledge base to answer this question."
            .to_string();
    }

    let settings = ChatSettings {
        json: false,
        temperature: 0.2,
        num_ctx: None,
    };
    match ollama
        .chat_text(model, &build_answer_prompt(query, items), settings)
        .await
    {
        Ok(answer) => answer.trim().to_string(),
        Err(err) => format!(
            "Retrieved {} relevant screen(s), but could not generate AI synthesis answer: {err:#}",
            items.len()
        ),
    }
}
