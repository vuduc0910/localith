//! Minimal robots.txt support: Allow/Disallow with `*` and `$`, and Crawl-delay.

use std::collections::HashMap;
use std::time::Duration;

use url::Url;

const USER_AGENT: &str = "LocalWebsiteScanner/1.0 (+local website audit)";
const AGENT_TOKEN: &str = "localwebsitescanner";

#[derive(Debug, Clone)]
struct Rule {
    pattern: String,
    allow: bool,
}

#[derive(Debug, Clone, Default)]
struct Group {
    rules: Vec<Rule>,
    crawl_delay: Option<f64>,
}

/// What the site's robots.txt lets this crawler do.
#[derive(Debug, Clone, Default)]
pub struct RobotsPolicy {
    origin: Option<String>,
    group: Group,
    pub crawl_delay_ms: u64,
}

impl RobotsPolicy {
    /// A policy that allows everything (robots.txt ignored, missing or unreachable).
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Parses robots.txt text fetched from `robots_url`.
    pub fn parse(robots_url: &str, text: &str) -> Self {
        let mut groups: HashMap<String, Group> = HashMap::new();
        let mut current_agents: Vec<String> = Vec::new();
        let mut last_was_agent = false;

        for line in text.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((field, value)) = line.split_once(':') else {
                continue;
            };
            let field = field.trim().to_lowercase();
            let value = value.trim();

            match field.as_str() {
                "user-agent" => {
                    if !last_was_agent {
                        current_agents.clear();
                    }
                    let agent = value.to_lowercase();
                    groups.entry(agent.clone()).or_default();
                    current_agents.push(agent);
                    last_was_agent = true;
                }
                "allow" | "disallow" => {
                    last_was_agent = false;
                    if value.is_empty() {
                        continue;
                    }
                    for agent in &current_agents {
                        groups.entry(agent.clone()).or_default().rules.push(Rule {
                            pattern: value.to_string(),
                            allow: field == "allow",
                        });
                    }
                }
                "crawl-delay" => {
                    last_was_agent = false;
                    if let Ok(delay) = value.parse::<f64>() {
                        for agent in &current_agents {
                            groups.entry(agent.clone()).or_default().crawl_delay = Some(delay);
                        }
                    }
                }
                _ => last_was_agent = false,
            }
        }

        let group = groups
            .remove(AGENT_TOKEN)
            .or_else(|| groups.remove("*"))
            .unwrap_or_default();
        let crawl_delay_ms = group
            .crawl_delay
            .filter(|delay| delay.is_finite() && *delay > 0.0)
            .map(|delay| (delay * 1000.0) as u64)
            .unwrap_or(0);

        Self {
            origin: Url::parse(robots_url)
                .ok()
                .map(|url| url.origin().ascii_serialization()),
            group,
            crawl_delay_ms,
        }
    }

    pub fn is_allowed(&self, url: &str) -> bool {
        let Ok(parsed) = Url::parse(url) else {
            return true;
        };
        // Rules only apply to the origin the robots.txt was served from.
        if self.origin.as_deref() != Some(parsed.origin().ascii_serialization().as_str()) {
            return true;
        }

        let mut target = parsed.path().to_string();
        if let Some(query) = parsed.query() {
            target.push('?');
            target.push_str(query);
        }

        // The longest matching rule wins; Allow wins a tie.
        let mut best: Option<&Rule> = None;
        for rule in &self.group.rules {
            if !pattern_matches(&rule.pattern, &target) {
                continue;
            }
            let better = match best {
                None => true,
                Some(current) => {
                    rule.pattern.len() > current.pattern.len()
                        || (rule.pattern.len() == current.pattern.len() && rule.allow)
                }
            };
            if better {
                best = Some(rule);
            }
        }
        best.is_none_or(|rule| rule.allow)
    }
}

/// Matches a robots.txt path pattern (`*` = any run of characters, trailing `$` = end of URL).
fn pattern_matches(pattern: &str, target: &str) -> bool {
    let (pattern, anchored) = match pattern.strip_suffix('$') {
        Some(stripped) => (stripped, true),
        None => (pattern, false),
    };
    let parts: Vec<&str> = pattern.split('*').collect();

    let Some(mut rest) = target.strip_prefix(parts[0]) else {
        return false;
    };
    if parts.len() == 1 {
        return !anchored || rest.is_empty();
    }

    let last = parts[parts.len() - 1];
    for part in &parts[1..parts.len() - 1] {
        match rest.find(part) {
            Some(position) => rest = &rest[position + part.len()..],
            None => return false,
        }
    }
    if anchored {
        rest.ends_with(last)
    } else {
        rest.contains(last)
    }
}

/// Downloads and parses robots.txt for the site of `start_url`.
/// Any failure results in an allow-all policy.
pub async fn load_robots_policy(start_url: &str) -> RobotsPolicy {
    let Ok(parsed) = Url::parse(start_url) else {
        return RobotsPolicy::allow_all();
    };
    let robots_url = format!("{}/robots.txt", parsed.origin().ascii_serialization());

    let fetch = async {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .user_agent(USER_AGENT)
            .build()
            .ok()?;
        let response = client.get(&robots_url).send().await.ok()?;
        if !response.status().is_success() {
            return None;
        }
        response.text().await.ok()
    };

    match fetch.await {
        Some(text) => RobotsPolicy::parse(&robots_url, &text),
        None => RobotsPolicy::allow_all(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROBOTS: &str = "
User-agent: *
Disallow: /private
Allow: /private/open
Disallow: /*.pdf$
Crawl-delay: 2

User-agent: OtherBot
Disallow: /
";

    #[test]
    fn applies_longest_match_and_wildcards() {
        let policy = RobotsPolicy::parse("https://example.com/robots.txt", ROBOTS);
        assert!(policy.is_allowed("https://example.com/"));
        assert!(!policy.is_allowed("https://example.com/private/data"));
        assert!(policy.is_allowed("https://example.com/private/open/page"));
        assert!(!policy.is_allowed("https://example.com/files/report.pdf"));
        assert!(policy.is_allowed("https://example.com/files/report.pdf?download=1"));
        assert_eq!(policy.crawl_delay_ms, 2000);
    }

    #[test]
    fn ignores_other_origins_and_prefers_own_group() {
        let policy = RobotsPolicy::parse("https://example.com/robots.txt", ROBOTS);
        assert!(policy.is_allowed("https://other.example.com/private"));

        let own = RobotsPolicy::parse(
            "https://example.com/robots.txt",
            "User-agent: *\nDisallow: /\n\nUser-agent: LocalWebsiteScanner\nDisallow: /admin\n",
        );
        assert!(own.is_allowed("https://example.com/shop"));
        assert!(!own.is_allowed("https://example.com/admin/users"));
    }

    #[test]
    fn allow_all_allows_everything() {
        let policy = RobotsPolicy::allow_all();
        assert!(policy.is_allowed("https://example.com/anything"));
        assert_eq!(policy.crawl_delay_ms, 0);
    }
}
