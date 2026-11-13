//! Command-line entry points: scan, serve, analyze, index, search.

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

use crate::browser::Browser;
use crate::config::{self, parse_options};
use crate::crawler::crawl;
use crate::knowledge::{generate_crawl_knowledge, GenerateKnowledgeOptions};
use crate::rag::chroma::ChromaClient;
use crate::rag::{index_knowledge_to_chroma, search_rag};
use crate::types::{ChromaConnectionOptions, IndexKnowledgeOptions, SearchRagOptions};
use crate::util::find_latest_run_dir;

pub const HELP: &str = "
Localith - local-first website crawler and knowledge engine

Usage:
  localith <command> [options]

Commands:
  scan <url> [options]        Crawl a website with agent-browser
  analyze [run-dir] [--force] Generate screen knowledge for a crawl with Ollama
  index [run-dir|file.json]   Index knowledge into ChromaDB
                              [--collection <name>] [--embed-model <model>]
  search \"<question>\"         Ask a question against the indexed knowledge
                              [--collection <name>] [--limit <n>] [--no-answer]
  serve                       Start the HTTP API and web UI (PORT, default 3000)
  help                        Show this help

Run `localith scan --help` for crawl options.
";

fn reports_dir() -> Result<PathBuf> {
    Ok(std::env::current_dir()?.join("reports"))
}

pub async fn run(args: &[String]) -> Result<()> {
    let Some((command, rest)) = args.split_first() else {
        println!("{HELP}");
        return Ok(());
    };

    match command.as_str() {
        "scan" => scan(rest).await,
        "analyze" => analyze(rest).await,
        "index" | "rag:index" => index(rest).await,
        "search" | "rag:search" => search(rest).await,
        "serve" => crate::server::serve().await,
        "help" | "--help" | "-h" => {
            println!("{HELP}");
            Ok(())
        }
        "--version" | "-V" => {
            println!("localith {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        other => {
            println!("{HELP}");
            bail!("Unknown command: {other}");
        }
    }
}

async fn scan(args: &[String]) -> Result<()> {
    let Some(options) = parse_options(args)? else {
        return Ok(());
    };

    let browser = Browser::new()?;
    browser.check_installed().await.context(
        "Could not run agent-browser. Try: npm install -g agent-browser && agent-browser install",
    )?;

    crawl(&options, browser).await?;
    Ok(())
}

async fn analyze(args: &[String]) -> Result<()> {
    let force = args.iter().any(|arg| arg == "--force" || arg == "-f");
    let explicit = args
        .iter()
        .find(|arg| !arg.starts_with('-'))
        .map(PathBuf::from);

    let run_dir = match explicit {
        Some(dir) => dir,
        None => find_latest_run_dir(&reports_dir()?, "report.json").context(
            "No crawl reports found in reports/. Run a scan first with: localith scan <url>",
        )?,
    };

    println!("\n🧠 AI Knowledge & Keywords Engine");
    println!("   Target Report: {}", run_dir.display());
    println!(
        "   Mode: {}",
        if force {
            "Force re-analysis (--force enabled)"
        } else {
            "Incremental (reuses previously saved screens)"
        }
    );

    let result = generate_crawl_knowledge(GenerateKnowledgeOptions {
        run_dir: Some(run_dir.clone()),
        force_reanalyze: force,
        ..GenerateKnowledgeOptions::default()
    })
    .await?;

    let keywords: Vec<&str> = result
        .global_keywords
        .iter()
        .take(10)
        .map(String::as_str)
        .collect();
    println!("\n✅ Knowledge generated successfully!");
    println!(
        "   Total Screens: {} ({} pattern groups)",
        result.total_screens, result.total_pattern_groups
    );
    println!("   Global Keywords: {}...", keywords.join(", "));
    println!(
        "   Saved JSON: {}",
        run_dir.join("knowledge.json").display()
    );
    println!(
        "   Saved Markdown: {}\n",
        run_dir.join("knowledge.md").display()
    );
    Ok(())
}

async fn index(args: &[String]) -> Result<()> {
    let mut collection_name: Option<String> = None;
    let mut embed_model = config::embed_model(None);
    let mut target: Option<String> = None;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--collection" | "-c" => collection_name = iter.next().cloned(),
            "--embed-model" => {
                if let Some(model) = iter.next() {
                    embed_model = model.clone();
                }
            }
            other if !other.starts_with('-') => target = Some(other.to_string()),
            _ => {}
        }
    }

    println!("\n📚 ChromaDB Knowledge Indexer");
    println!("   Embedding Model: {embed_model}");
    if let Some(name) = &collection_name {
        println!("   Target Collection: {name}");
    }
    if let Some(path) = &target {
        println!("   Source Path: {path}");
    }

    let is_file = target
        .as_deref()
        .is_some_and(|path| path.ends_with(".json"));
    let options = IndexKnowledgeOptions {
        collection_name,
        embed_model: Some(embed_model),
        knowledge_path: target.clone().filter(|_| is_file),
        run_dir: target.filter(|_| !is_file),
        ..IndexKnowledgeOptions::default()
    };
    let result = index_knowledge_to_chroma(&options, &reports_dir()?)
        .await
        .context("Indexing failed")?;

    println!("\n✅ Knowledge successfully indexed into ChromaDB!");
    println!("   Collection Name: {}", result.collection_name);
    println!(
        "   Total Chunks Indexed: {} (across {} screens)",
        result.total_chunks, result.screens_count
    );
    println!("   Embedding Model: {}", result.embed_model);
    println!("   Timestamp: {}\n", result.indexed_at);
    Ok(())
}

async fn search(args: &[String]) -> Result<()> {
    let mut query: Option<String> = None;
    let mut collection_name: Option<String> = None;
    let mut n_results: usize = 5;
    let mut generate_answer = true;

    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--collection" | "-c" => collection_name = iter.next().cloned(),
            "--limit" | "-n" => {
                n_results = iter.next().and_then(|raw| raw.parse().ok()).unwrap_or(5);
            }
            "--no-answer" => generate_answer = false,
            other if !other.starts_with('-') => query = Some(other.to_string()),
            _ => {}
        }
    }

    let Some(query) = query else {
        bail!(
            "Usage: localith search \"<question>\" [--collection <name>]\nExample: localith search \"how can I create a report?\""
        );
    };

    // Without --collection, use the first collection ChromaDB reports.
    if collection_name.is_none() {
        let chroma = ChromaClient::new(&ChromaConnectionOptions::default());
        if let Ok(collections) = chroma.list_collections().await {
            collection_name = collections
                .into_iter()
                .next()
                .map(|collection| collection.name);
        }
    }

    println!("\n🔎 Querying RAG Database:");
    println!("   Question: \"{query}\"");
    println!(
        "   Collection: {}",
        collection_name.as_deref().unwrap_or("default")
    );

    let result = search_rag(&SearchRagOptions {
        query,
        collection_name,
        n_results: Some(n_results),
        generate_answer: Some(generate_answer),
        ..SearchRagOptions::default()
    })
    .await
    .context("Search failed")?;

    if let Some(answer) = result.answer.as_deref().filter(|answer| !answer.is_empty()) {
        let rule = "-".repeat(80);
        println!("\n🤖 AI Answer:\n{rule}\n{answer}\n{rule}");
    }

    println!("\n📄 Retrieved Sources ({} chunks):", result.results_count);
    for (index, item) in result.results.iter().enumerate() {
        let score = item
            .score
            .map(|score| format!(" (Match Score: {:.1}%)", score * 100.0))
            .unwrap_or_default();
        println!(
            "\n[{}] {} [{}]{score}",
            index + 1,
            item.screen_name.as_deref().unwrap_or("Screen"),
            item.chunk_type.as_deref().unwrap_or("chunk")
        );
        println!(
            "    Route: {}",
            item.pattern
                .as_deref()
                .or(item.url.as_deref())
                .unwrap_or("N/A")
        );
        if let Some(workflow) = item
            .metadata
            .get("workflow")
            .and_then(|value| value.as_str())
        {
            println!("    Workflow: {workflow}");
        }
        let snippet: Vec<&str> = item.document.lines().take(3).collect();
        println!("    Snippet:\n    {}...", snippet.join("\n    "));
    }
    println!();
    Ok(())
}
