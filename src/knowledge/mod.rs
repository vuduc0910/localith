//! Turns a crawl report into per-screen knowledge using Ollama.

pub mod markdown;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use anyhow::{anyhow, bail, Result};
use indexmap::IndexMap;
use regex::Regex;
use serde_json::Value;
use url::Url;

use crate::config;
use crate::crawler::url_pattern::{extract_url_pattern, is_dynamic_pattern, ID_PLACEHOLDER};
use crate::ollama::{ChatSettings, Ollama};
use crate::types::{CrawlKnowledgeResult, CrawlReport, CrawledPage, PageStatus, ScreenKnowledge};
use crate::util::{now_iso, read_json_file, truncate_chars, write_json};

pub use markdown::build_knowledge_markdown;

static CODE_FENCE_START: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^```(?:json)?\s*").expect("valid built-in regex"));
static CODE_FENCE_END: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\s*```$").expect("valid built-in regex"));

#[derive(Debug, Clone, Default)]
pub struct GenerateKnowledgeOptions {
    pub ollama_url: Option<String>,
    pub ollama_model: Option<String>,
    pub run_dir: Option<PathBuf>,
    pub report: Option<CrawlReport>,
    pub report_path: Option<PathBuf>,
    pub force_reanalyze: bool,
}

/// Pages that represent the same screen: one static URL, or all URLs of a dynamic pattern.
#[derive(Debug, Clone)]
pub struct ScreenGroup {
    pub pattern: String,
    pub is_pattern_group: bool,
    pub pages: Vec<CrawledPage>,
}

/// The part of a screen's knowledge that comes from analysis (model or fallback).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScreenAnalysis {
    pub screen_name: String,
    pub screen_type: String,
    pub summary: String,
    pub purpose: String,
    pub key_workflows: Vec<String>,
    pub data_entities: Vec<String>,
    pub ui_components: Vec<String>,
    pub keywords: Vec<String>,
    pub is_fallback: bool,
}

/// Groups pages into screens, in first-seen order.
pub fn group_pages_for_knowledge(pages: &[CrawledPage]) -> Vec<ScreenGroup> {
    let mut groups: IndexMap<String, ScreenGroup> = IndexMap::new();
    for page in pages {
        let url = page.effective_url();
        let pattern = page
            .pattern
            .clone()
            .filter(|pattern| !pattern.is_empty())
            .unwrap_or_else(|| extract_url_pattern(url));
        let is_group = is_dynamic_pattern(&pattern);
        let key = if is_group { pattern } else { url.to_string() };

        groups
            .entry(key.clone())
            .or_insert_with(|| ScreenGroup {
                pattern: key,
                is_pattern_group: is_group,
                pages: Vec::new(),
            })
            .pages
            .push(page.clone());
    }
    groups.into_values().collect()
}

pub fn derive_default_name(group: &ScreenGroup) -> String {
    let Ok(parsed) = Url::parse(&group.pattern) else {
        return "Application Screen".to_string();
    };
    let last_segment = parsed
        .path()
        .split('/')
        .rfind(|segment| !segment.is_empty())
        .unwrap_or("home");
    // The URL parser percent-encodes the braces of `{{id}}`.
    let clean = last_segment
        .replace(ID_PLACEHOLDER, "")
        .replace("%7B%7Bid%7D%7D", "")
        .replace(['-', '_'], " ");
    let clean = clean.split_whitespace().collect::<Vec<_>>().join(" ");

    let mut chars = clean.chars();
    match chars.next() {
        Some(first) => format!("{}{} Screen", first.to_uppercase(), chars.as_str()),
        None => "Home Screen".to_string(),
    }
}

pub fn create_fallback_screen_analysis(group: &ScreenGroup, title: &str) -> ScreenAnalysis {
    ScreenAnalysis {
        screen_name: if title.is_empty() {
            derive_default_name(group)
        } else {
            title.to_string()
        },
        screen_type: if group.is_pattern_group {
            "detail-view"
        } else {
            "page"
        }
        .to_string(),
        summary: format!("Screen representing {}.", group.pattern),
        purpose: "Application screen interface.".to_string(),
        key_workflows: vec!["View screen information".to_string()],
        data_entities: Vec::new(),
        ui_components: Vec::new(),
        keywords: group
            .pattern
            .split(['/', '_', '-'])
            .filter(|word| word.chars().count() > 3 && !word.starts_with("http"))
            .map(str::to_string)
            .collect(),
        is_fallback: true,
    }
}

/// Reads the model's JSON reply; anything unusable becomes a fallback analysis.
pub fn parse_screen_analysis(content: &str, group: &ScreenGroup) -> ScreenAnalysis {
    let cleaned = CODE_FENCE_START.replace(content.trim(), "");
    let cleaned = CODE_FENCE_END.replace(&cleaned, "");
    let Ok(Value::Object(parsed)) = serde_json::from_str::<Value>(&cleaned) else {
        return create_fallback_screen_analysis(group, "");
    };

    let text = |key: &str| parsed.get(key).and_then(Value::as_str).map(str::to_string);
    let list = |key: &str, max: usize| -> Vec<String> {
        parsed
            .get(key)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .take(max)
                    .map(|item| match item {
                        Value::String(text) => text.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    };

    ScreenAnalysis {
        screen_name: text("screenName").unwrap_or_else(|| derive_default_name(group)),
        screen_type: text("screenType").unwrap_or_else(|| "page".to_string()),
        summary: text("summary")
            .unwrap_or_else(|| "Screen functionality summary unavailable.".to_string()),
        purpose: text("purpose").unwrap_or_else(|| "Application interface screen.".to_string()),
        key_workflows: list("keyWorkflows", 10),
        data_entities: list("dataEntities", 10),
        ui_components: list("uiComponents", 10),
        keywords: list("keywords", 20),
        is_fallback: false,
    }
}

/// Finds the accessibility snapshot of a crawled page: inline snapshot, then the
/// `.snapshot.txt` file, then the page's `.json` data file, then the page text.
pub async fn load_page_snapshot(page: &CrawledPage, run_dir: Option<&Path>) -> String {
    if let Some(snapshot) = page.snapshot.as_deref().filter(|s| !s.is_empty()) {
        return snapshot.to_string();
    }

    let first_existing = |file: &str| -> Option<PathBuf> {
        let name = Path::new(file).file_name()?;
        let mut candidates = vec![PathBuf::from(file)];
        if let Some(run_dir) = run_dir {
            candidates.push(run_dir.join(file));
            candidates.push(run_dir.join("pages").join(name));
        }
        candidates.into_iter().find(|candidate| candidate.is_file())
    };

    if let Some(path) = page.snapshot_file.as_deref().and_then(first_existing) {
        if let Ok(content) = tokio::fs::read_to_string(&path).await {
            if !content.trim().is_empty() {
                return content;
            }
        }
    }

    if let Some(path) = page.data_file.as_deref().and_then(first_existing) {
        if let Ok(data) = read_json_file::<Value>(&path).await {
            if let Some(snapshot) = data.get("snapshot").and_then(Value::as_str) {
                if !snapshot.trim().is_empty() {
                    return snapshot.to_string();
                }
            }
        }
    }

    page.text.clone().unwrap_or_default()
}

fn build_prompt(group: &ScreenGroup, titles: &str, headings: &str, snapshot: &str) -> String {
    let group_note = if group.is_pattern_group {
        "(which represents a parameterized screen template group)"
    } else {
        ""
    };
    let headings_block = if headings.is_empty() {
        String::new()
    } else {
        format!("Key Headings:\n{headings}\n")
    };
    format!(
        r#"
You are an expert software architect and product analyst. Analyze this web application screen {group_note}.

Target pattern/URL: {pattern}
Screen sample titles: {titles}
{headings_block}
Below is the accessibility tree snapshot of the screen captured by agent-browser.
It shows the real UI elements (navigation, buttons, inputs, links, forms, tables, headings, and dialogs):

```text
{snapshot}
```

Based on the agent-browser accessibility snapshot, return ONLY valid JSON matching this schema:
{{
  "screenName": "Concise human-friendly name for this screen (e.g. 'Issue Details & Triage Viewer')",
  "screenType": "Categorical screen type, e.g. 'dashboard', 'detail-view', 'list-management', 'form', 'settings', 'auth', 'analytics'",
  "summary": "2-3 sentence functional summary of what this screen does based on its UI elements",
  "purpose": "Primary user/business purpose of this screen in the application",
  "keyWorkflows": ["Specific actions/workflows a user can perform on this screen derived from buttons and inputs"],
  "dataEntities": ["Business entities, data records, or domain objects displayed or managed here"],
  "uiComponents": ["Key UI sections, widgets, tables, feeds, or control panels visible in the snapshot"],
  "keywords": ["10-15 specific domain, technical, workflow, and search keywords for this screen"]
}}
"#,
        pattern = group.pattern,
        titles = if titles.is_empty() { "(none)" } else { titles },
        snapshot = if snapshot.is_empty() {
            "(no snapshot available)"
        } else {
            snapshot
        },
    )
}

async fn analyze_screen_with_ollama(
    client: &Ollama,
    model: &str,
    group: &ScreenGroup,
    run_dir: Option<&Path>,
) -> ScreenAnalysis {
    let ok_pages: Vec<&CrawledPage> = group
        .pages
        .iter()
        .filter(|page| page.status == PageStatus::Ok)
        .collect();
    let sample_pages: Vec<&CrawledPage> = if ok_pages.is_empty() {
        group.pages.iter().take(3).collect()
    } else {
        ok_pages.into_iter().take(3).collect()
    };

    let mut titles: Vec<&str> = Vec::new();
    for title in sample_pages.iter().filter_map(|page| page.title.as_deref()) {
        if !title.is_empty() && !titles.contains(&title) {
            titles.push(title);
        }
    }
    let titles = titles.join(" | ");

    let headings = sample_pages
        .iter()
        .flat_map(|page| page.headings.iter().flatten())
        .take(15)
        .map(|heading| format!("{}: {}", heading.tag.to_uppercase(), heading.text))
        .collect::<Vec<_>>()
        .join("\n");

    let max_chars_per_sample = if sample_pages.len() > 1 { 5500 } else { 12000 };
    let mut snippets: Vec<String> = Vec::new();
    for (index, page) in sample_pages.iter().enumerate() {
        let raw = load_page_snapshot(page, run_dir).await;
        let trimmed = truncate_chars(raw.trim(), max_chars_per_sample);
        if trimmed.is_empty() {
            continue;
        }
        if sample_pages.len() > 1 {
            snippets.push(format!(
                "--- Sample {} ({}) ---\n{trimmed}",
                index + 1,
                page.effective_url()
            ));
        } else {
            snippets.push(trimmed.to_string());
        }
    }

    let prompt = build_prompt(group, &titles, &headings, &snippets.join("\n\n"));
    let settings = ChatSettings {
        json: true,
        temperature: 0.2,
        num_ctx: Some(8192),
    };
    let reply = match client.chat_text(model, &prompt, settings).await {
        Ok(content) if content.trim().is_empty() => {
            Err(anyhow!("Ollama returned an empty response."))
        }
        other => other,
    };

    match reply {
        Ok(content) => parse_screen_analysis(&content, group),
        Err(err) => {
            eprintln!(
                "  ⚠️ AI screen analysis fallback for {}: {err:#}",
                group.pattern
            );
            create_fallback_screen_analysis(group, &titles)
        }
    }
}

/// Loads screen knowledge saved by earlier runs, keyed by pattern.
pub async fn load_saved_screen_knowledge(run_dir: &Path) -> HashMap<String, ScreenKnowledge> {
    let mut saved: HashMap<String, ScreenKnowledge> = HashMap::new();
    let usable =
        |screen: &ScreenKnowledge| !screen.pattern.is_empty() && !screen.screen_name.is_empty();

    if let Ok(data) = read_json_file::<CrawlKnowledgeResult>(&run_dir.join("knowledge.json")).await
    {
        for screen in data.screens.into_iter().filter(usable) {
            saved.insert(screen.pattern.clone(), screen);
        }
    }

    // screens/*.json covers runs that stopped before knowledge.json was complete.
    if let Ok(mut entries) = tokio::fs::read_dir(run_dir.join("screens")).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
                continue;
            }
            if let Ok(screen) = read_json_file::<ScreenKnowledge>(&path).await {
                if usable(&screen) && !saved.contains_key(&screen.pattern) {
                    saved.insert(screen.pattern.clone(), screen);
                }
            }
        }
    }

    saved
}

/// Writes one finished screen plus the cumulative knowledge.json and knowledge.md.
pub async fn persist_screen_knowledge_state(
    run_dir: &Path,
    screen: &ScreenKnowledge,
    current: &CrawlKnowledgeResult,
) -> Result<()> {
    let screens_dir = run_dir.join("screens");
    tokio::fs::create_dir_all(&screens_dir).await?;
    write_json(
        &screens_dir.join(format!("{}.json", screen.screen_id)),
        screen,
    )
    .await?;
    write_json(&run_dir.join("knowledge.json"), current).await?;
    tokio::fs::write(
        run_dir.join("knowledge.md"),
        build_knowledge_markdown(current),
    )
    .await?;
    Ok(())
}

/// Keywords ordered by how many screens use them (first seen first on ties).
fn top_keywords(frequency: &IndexMap<String, usize>) -> Vec<String> {
    let mut entries: Vec<(&String, &usize)> = frequency.iter().collect();
    entries.sort_by(|a, b| b.1.cmp(a.1));
    entries
        .into_iter()
        .take(30)
        .map(|(keyword, _)| keyword.clone())
        .collect()
}

/// Analyzes every screen of a crawl with Ollama and writes knowledge.json / knowledge.md.
///
/// Each screen is saved as soon as it is done, and screens analyzed by an
/// earlier run are reused unless `force_reanalyze` is set.
pub async fn generate_crawl_knowledge(
    options: GenerateKnowledgeOptions,
) -> Result<CrawlKnowledgeResult> {
    let client = Ollama::new(options.ollama_url.as_deref());
    let model = config::ollama_model(options.ollama_model.as_deref());

    let mut run_dir = options.run_dir.clone();
    let report = match options.report {
        Some(report) => report,
        None => {
            let report_file = options
                .report_path
                .clone()
                .or_else(|| run_dir.as_ref().map(|dir| dir.join("report.json")));
            let Some(report_file) = report_file else {
                bail!("Either report, reportPath, or runDir must be provided.");
            };
            if run_dir.is_none() {
                run_dir = report_file.parent().map(Path::to_path_buf);
            }
            read_json_file::<CrawlReport>(&report_file).await?
        }
    };
    let run_dir = run_dir.as_deref();
    let writable_dir = run_dir.filter(|dir| dir.is_dir());

    let saved = match run_dir {
        Some(dir) if !options.force_reanalyze => load_saved_screen_knowledge(dir).await,
        _ => HashMap::new(),
    };
    if !saved.is_empty() {
        println!(
            "   📂 Found {} previously saved screen knowledge record(s) in {}",
            saved.len(),
            run_dir
                .map(|dir| dir.display().to_string())
                .unwrap_or_default()
        );
    }

    let groups = group_pages_for_knowledge(&report.pages);
    let mut screens: Vec<ScreenKnowledge> = Vec::new();
    let mut keyword_frequency: IndexMap<String, usize> = IndexMap::new();

    for (index, group) in groups.iter().enumerate() {
        let position = index + 1;
        let screen_id = format!("screen-{position:03}");

        let reusable = saved
            .get(&group.pattern)
            .filter(|cached| cached.is_fallback != Some(true));
        let analysis = match reusable {
            Some(cached) => {
                println!(
                    "   ⏩ [{position}/{}] Reusing saved knowledge for {} ({})",
                    groups.len(),
                    group.pattern,
                    cached.screen_name
                );
                ScreenAnalysis {
                    screen_name: cached.screen_name.clone(),
                    screen_type: cached.screen_type.clone(),
                    summary: cached.summary.clone(),
                    purpose: cached.purpose.clone(),
                    key_workflows: cached.key_workflows.clone(),
                    data_entities: cached.data_entities.clone(),
                    ui_components: cached.ui_components.clone(),
                    keywords: cached.keywords.clone(),
                    is_fallback: false,
                }
            }
            None => {
                println!(
                    "   🤖 [{position}/{}] Analyzing with Ollama: {}...",
                    groups.len(),
                    group.pattern
                );
                analyze_screen_with_ollama(&client, &model, group, run_dir).await
            }
        };

        let screen = ScreenKnowledge {
            screen_file: run_dir.map(|_| format!("screens/{screen_id}.json")),
            screen_id,
            pattern: group.pattern.clone(),
            is_pattern_group: group.is_pattern_group,
            sample_urls: group
                .pages
                .iter()
                .take(5)
                .map(|page| page.effective_url().to_string())
                .collect(),
            screen_name: analysis.screen_name,
            screen_type: analysis.screen_type,
            summary: analysis.summary,
            purpose: analysis.purpose,
            key_workflows: analysis.key_workflows,
            data_entities: analysis.data_entities,
            ui_components: analysis.ui_components,
            keywords: analysis.keywords,
            is_fallback: Some(analysis.is_fallback),
        };

        for keyword in &screen.keywords {
            let normalized = keyword.trim().to_lowercase();
            if normalized.chars().count() > 2 {
                *keyword_frequency.entry(normalized).or_insert(0) += 1;
            }
        }
        screens.push(screen);

        if let Some(dir) = writable_dir {
            let current = CrawlKnowledgeResult {
                start_url: report.start_url.clone(),
                generated_at: now_iso(),
                ollama_model: model.clone(),
                total_screens: screens.len(),
                total_pattern_groups: screens.iter().filter(|s| s.is_pattern_group).count(),
                screens: screens.clone(),
                global_keywords: top_keywords(&keyword_frequency),
                site_summary: format!(
                    "Knowledge synthesis of {} ({} of {} screens analyzed).",
                    report.start_url,
                    screens.len(),
                    groups.len()
                ),
            };
            let screen = screens.last().expect("screen was just pushed");
            persist_screen_knowledge_state(dir, screen, &current).await?;
            println!(
                "   ✓ Persisted {} ({})",
                screen.screen_id, screen.screen_name
            );
        }
    }

    let total_pattern_groups = screens.iter().filter(|s| s.is_pattern_group).count();
    let result = CrawlKnowledgeResult {
        site_summary: format!(
            "Knowledge synthesis of {} covering {} distinct functional screens ({} dynamic pattern templates).",
            report.start_url,
            screens.len(),
            total_pattern_groups
        ),
        start_url: report.start_url,
        generated_at: now_iso(),
        ollama_model: model,
        total_screens: screens.len(),
        total_pattern_groups,
        global_keywords: top_keywords(&keyword_frequency),
        screens,
    };

    if let Some(dir) = writable_dir {
        write_json(&dir.join("knowledge.json"), &result).await?;
        tokio::fs::write(dir.join("knowledge.md"), build_knowledge_markdown(&result)).await?;
    }

    Ok(result)
}
