//! Splits knowledge into text chunks for vector search: one site overview,
//! one overview per screen, and one chunk per workflow.

use indexmap::IndexMap;
use serde_json::{json, Value};

use crate::types::{CrawlKnowledgeResult, KnowledgeChunk, ScreenKnowledge};
use crate::util::truncate_chars;

fn metadata(entries: Vec<(&str, Value)>) -> IndexMap<String, Value> {
    entries
        .into_iter()
        .map(|(key, value)| (key.to_string(), value))
        .collect()
}

fn first_keywords(keywords: &[String]) -> String {
    keywords
        .iter()
        .take(15)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ")
}

fn screen_url(screen: &ScreenKnowledge) -> &str {
    screen
        .sample_urls
        .first()
        .filter(|url| !url.is_empty())
        .unwrap_or(&screen.pattern)
}

pub fn build_site_overview_chunk(knowledge: &CrawlKnowledgeResult) -> KnowledgeChunk {
    let screen_list: Vec<String> = knowledge
        .screens
        .iter()
        .take(50)
        .map(|s| {
            format!(
                "- {} ({}): {} — {}",
                s.screen_name, s.screen_type, s.pattern, s.purpose
            )
        })
        .collect();

    let mut lines = vec![
        format!("Application Knowledge Base for: {}", knowledge.start_url),
        format!("Site Summary: {}", knowledge.site_summary),
        format!("Total Unique Screens: {}", knowledge.total_screens),
        format!("Dynamic Pattern Groups: {}", knowledge.total_pattern_groups),
        format!(
            "Global Domain Keywords: {}",
            knowledge.global_keywords.join(", ")
        ),
    ];
    if !screen_list.is_empty() {
        lines.push(format!(
            "Available Screens in Application:\n{}",
            screen_list.join("\n")
        ));
    }

    KnowledgeChunk {
        id: "global-site-overview".to_string(),
        document: lines.join("\n\n"),
        metadata: metadata(vec![
            ("screenId", json!("global")),
            ("screenName", json!("Application Overview")),
            ("screenType", json!("site-summary")),
            ("pattern", json!(knowledge.start_url)),
            ("chunkType", json!("site_overview")),
            ("url", json!(knowledge.start_url)),
            (
                "keywords",
                json!(first_keywords(&knowledge.global_keywords)),
            ),
        ]),
    }
}

pub fn build_screen_chunk(screen: &ScreenKnowledge) -> KnowledgeChunk {
    let mut lines = vec![
        format!("Screen: {}", screen.screen_name),
        format!("Screen Type: {}", screen.screen_type),
        format!("URL / Route Pattern: {}", screen.pattern),
        if screen.is_pattern_group {
            "Pattern Template: Yes (parameterized screen group)".to_string()
        } else {
            "Pattern Template: No (static screen)".to_string()
        },
        format!("Purpose: {}", screen.purpose),
        format!("Summary: {}", screen.summary),
    ];

    if !screen.key_workflows.is_empty() {
        lines.push("Key Actions & Workflows:".to_string());
        lines.extend(
            screen
                .key_workflows
                .iter()
                .map(|workflow| format!("- {workflow}")),
        );
    }
    if !screen.data_entities.is_empty() {
        lines.push(format!(
            "Data Entities & Models: {}",
            screen.data_entities.join(", ")
        ));
    }
    if !screen.ui_components.is_empty() {
        lines.push(format!(
            "UI Components & Controls: {}",
            screen.ui_components.join(", ")
        ));
    }
    if !screen.keywords.is_empty() {
        lines.push(format!("Keywords: {}", screen.keywords.join(", ")));
    }
    if !screen.sample_urls.is_empty() {
        let samples: Vec<&str> = screen
            .sample_urls
            .iter()
            .take(3)
            .map(String::as_str)
            .collect();
        lines.push(format!("Sample URLs: {}", samples.join(", ")));
    }

    KnowledgeChunk {
        id: format!("{}-overview", screen.screen_id),
        document: lines.join("\n\n"),
        metadata: metadata(vec![
            ("screenId", json!(screen.screen_id)),
            ("screenName", json!(screen.screen_name)),
            ("screenType", json!(screen.screen_type)),
            ("pattern", json!(screen.pattern)),
            (
                "isPatternGroup",
                json!(if screen.is_pattern_group {
                    "true"
                } else {
                    "false"
                }),
            ),
            ("chunkType", json!("screen_overview")),
            ("url", json!(screen_url(screen))),
            ("keywords", json!(first_keywords(&screen.keywords))),
        ]),
    }
}

/// One chunk per workflow, so questions like "how do I reset my password?" match directly.
pub fn build_workflow_chunks(screen: &ScreenKnowledge) -> Vec<KnowledgeChunk> {
    screen
        .key_workflows
        .iter()
        .enumerate()
        .map(|(index, workflow)| {
            let mut lines = vec![
                format!("Screen: {} ({})", screen.screen_name, screen.pattern),
                format!("Screen Type: {}", screen.screen_type),
                format!("Action / Workflow: {workflow}"),
                format!("How it works: {}", screen.summary),
                format!("Screen Purpose: {}", screen.purpose),
            ];
            if !screen.ui_components.is_empty() {
                lines.push(format!(
                    "Related UI Components: {}",
                    screen.ui_components.join(", ")
                ));
            }
            let others: Vec<String> = screen
                .key_workflows
                .iter()
                .filter(|other| *other != workflow)
                .take(5)
                .map(|other| format!("- {other}"))
                .collect();
            if !others.is_empty() {
                lines.push(format!(
                    "Other Actions on this Screen:\n{}",
                    others.join("\n")
                ));
            }

            KnowledgeChunk {
                id: format!("{}-wf-{}", screen.screen_id, index + 1),
                document: lines.join("\n\n"),
                metadata: metadata(vec![
                    ("screenId", json!(screen.screen_id)),
                    ("screenName", json!(screen.screen_name)),
                    ("screenType", json!(screen.screen_type)),
                    ("pattern", json!(screen.pattern)),
                    ("chunkType", json!("workflow")),
                    ("workflowIndex", json!(index + 1)),
                    ("workflow", json!(truncate_chars(workflow, 500))),
                    ("url", json!(screen_url(screen))),
                ]),
            }
        })
        .collect()
}

pub fn build_knowledge_chunks(knowledge: &CrawlKnowledgeResult) -> Vec<KnowledgeChunk> {
    let mut chunks = vec![build_site_overview_chunk(knowledge)];
    for screen in &knowledge.screens {
        chunks.push(build_screen_chunk(screen));
        chunks.extend(build_workflow_chunks(screen));
    }
    chunks
}
