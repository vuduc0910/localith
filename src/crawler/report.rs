//! Builds the crawl report (JSON structure and Markdown rendering).

use crate::crawler::url_pattern::compute_pattern_groups;
use crate::types::{
    AuthSetting, CrawlOptions, CrawlReport, CrawlTotals, CrawledPage, PageStatus, ReportSettings,
};
use crate::util::escape_cell;

pub struct BuildCrawlReportParams<'a> {
    pub options: &'a CrawlOptions,
    pub pages: Vec<CrawledPage>,
    pub started_at: String,
    pub finished_at: String,
    pub effective_delay: u64,
    pub discovered_count: usize,
    pub visited_count: usize,
    pub skipped_by_robots: usize,
    pub skipped_by_auth: usize,
    pub skipped_by_pattern: usize,
}

pub fn compute_crawl_totals(
    pages: &[CrawledPage],
    discovered_count: usize,
    visited_count: usize,
    skipped_by_robots: usize,
    skipped_by_auth: usize,
    skipped_by_pattern: usize,
) -> CrawlTotals {
    CrawlTotals {
        discovered: discovered_count,
        visited: visited_count,
        succeeded: pages.iter().filter(|p| p.status == PageStatus::Ok).count(),
        failed: pages
            .iter()
            .filter(|p| p.status == PageStatus::Error)
            .count(),
        skipped_by_robots,
        skipped_by_auth,
        skipped_by_pattern: Some(skipped_by_pattern),
    }
}

pub fn build_crawl_report(params: BuildCrawlReportParams<'_>) -> CrawlReport {
    let options = params.options;
    CrawlReport {
        start_url: options.start_url.clone(),
        started_at: params.started_at,
        finished_at: params.finished_at,
        settings: ReportSettings {
            max_depth: options.max_depth,
            max_pages: options.max_pages,
            max_pages_per_pattern: Some(options.max_pages_per_pattern),
            delay_ms: params.effective_delay,
            include_subdomains: options.include_subdomains,
            respect_robots: options.respect_robots,
            auth: AuthSetting {
                enabled: options.auth.enabled,
            },
            generate_knowledge: Some(options.generate_knowledge),
            ollama_model: options.ollama_model.clone(),
        },
        totals: Some(compute_crawl_totals(
            &params.pages,
            params.discovered_count,
            params.visited_count,
            params.skipped_by_robots,
            params.skipped_by_auth,
            params.skipped_by_pattern,
        )),
        pattern_groups: Some(compute_pattern_groups(&params.pages)),
        pages: params.pages,
    }
}

fn or_dash(items: &[String]) -> String {
    if items.is_empty() {
        "—".to_string()
    } else {
        items.join(", ")
    }
}

pub fn build_markdown_report(report: &CrawlReport) -> String {
    let settings = &report.settings;
    let totals = report.totals.clone().unwrap_or_default();
    let mut lines: Vec<String> = vec![
        "# Website Scan Report".into(),
        String::new(),
        format!("- **Start URL:** {}", report.start_url),
        format!("- **Started:** {}", report.started_at),
        format!("- **Finished:** {}", report.finished_at),
        format!("- **Depth limit:** {}", settings.max_depth),
        format!("- **Page cap:** {}", settings.max_pages),
        format!(
            "- **Pattern cap:** {} samples per dynamic pattern at depth > 1",
            settings.max_pages_per_pattern.unwrap_or(3)
        ),
        format!(
            "- **AI model:** {}",
            if settings.generate_knowledge == Some(true) {
                settings.ollama_model.as_str()
            } else {
                "disabled"
            }
        ),
        format!(
            "- **robots.txt:** {}",
            if settings.respect_robots {
                "respected"
            } else {
                "ignored"
            }
        ),
        format!(
            "- **Auth wall handling:** {}",
            if settings.auth.enabled {
                "enabled"
            } else {
                "disabled"
            }
        ),
        String::new(),
        "## Summary".into(),
        String::new(),
        format!("- Discovered URLs: {}", totals.discovered),
        format!("- Visited URLs: {}", totals.visited),
        format!("- Successful pages: {}", totals.succeeded),
        format!("- Failed pages: {}", totals.failed),
        format!("- Skipped by robots.txt: {}", totals.skipped_by_robots),
        format!("- Skipped by auth wall: {}", totals.skipped_by_auth),
        format!(
            "- Skipped by pattern limit: {}",
            totals.skipped_by_pattern.unwrap_or(0)
        ),
        String::new(),
        "## Pages".into(),
        String::new(),
        "| # | Depth | Status | Title / Detail | URL |".into(),
        "|---:|---:|---|---|---|".into(),
    ];

    for (index, page) in report.pages.iter().enumerate() {
        let title = page.title.as_deref().filter(|t| !t.is_empty());
        let detail = match (title, page.pattern.as_deref(), page.status) {
            (Some(title), _, _) => title.to_string(),
            (None, Some(pattern), PageStatus::Skipped) if !pattern.is_empty() => {
                format!("[pattern group] {pattern}")
            }
            _ => page.error.clone().unwrap_or_default(),
        };
        lines.push(format!(
            "| {} | {} | {} | {} | {} |",
            index + 1,
            page.depth,
            page.status.as_str(),
            escape_cell(&detail),
            escape_cell(&page.final_url)
        ));
    }

    if let Some(groups) = report.pattern_groups.as_ref().filter(|g| !g.is_empty()) {
        lines.extend([
            String::new(),
            "## Pattern Groups".into(),
            String::new(),
            "| Pattern | Crawled Samples | Skipped URLs |".into(),
            "|---|---:|---:|".into(),
        ]);
        for (pattern, counts) in groups {
            lines.push(format!(
                "| `{}` | {} | {} |",
                escape_cell(pattern),
                counts.crawled,
                counts.skipped
            ));
        }
    }

    lines.extend([String::new(), "## AI Analysis".into(), String::new()]);
    for page in &report.pages {
        let Some(analysis) = &page.analysis else {
            continue;
        };
        let heading = page
            .title
            .as_deref()
            .filter(|t| !t.is_empty())
            .unwrap_or(&page.final_url);
        lines.extend([
            format!("### {heading}"),
            String::new(),
            format!("- **URL:** {}", page.final_url),
            format!("- **Type:** {}", analysis.page_type),
            format!("- **Summary:** {}", analysis.summary),
            format!("- **Topics:** {}", or_dash(&analysis.key_topics)),
            format!("- **Entities:** {}", or_dash(&analysis.important_entities)),
            "- **Recommendations:**".into(),
        ]);
        if analysis.recommendations.is_empty() {
            lines.push("  - —".into());
        } else {
            lines.extend(
                analysis
                    .recommendations
                    .iter()
                    .map(|item| format!("  - {item}")),
            );
        }
        lines.push(String::new());
    }

    let auth_walls: Vec<&CrawledPage> = report
        .pages
        .iter()
        .filter(|page| page.auth_wall == Some(true))
        .collect();
    if !auth_walls.is_empty() {
        lines.extend([
            "## Auth walls".into(),
            String::new(),
            "| URL | Status | Sign-in attempted |".into(),
            "|---|---|---|".into(),
        ]);
        for page in auth_walls {
            lines.push(format!(
                "| {} | {} | {} |",
                escape_cell(&page.final_url),
                page.status.as_str(),
                if page.auth_handled == Some(true) {
                    "yes"
                } else {
                    "no"
                }
            ));
        }
        lines.push(String::new());
    }

    lines.extend(
        [
            "## Files",
            "",
            "- `report.json`: machine-readable crawl report",
            "- `queue.json`: pending queue of uncrawled URLs for resuming scans",
            "- `auth-state.json`: saved browser cookies and session state (when authenticated)",
            "- `pages/*.json`: page data and full accessibility snapshot",
            "- `pages/*.snapshot.txt`: raw accessibility snapshots",
            "",
            "_Generated by Local Website Scanner._",
            "",
        ]
        .map(String::from),
    );

    lines.join("\n")
}
