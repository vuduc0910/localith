use anyhow::{anyhow, bail, Result};
use url::Url;

use crate::types::{AuthOptions, CrawlOptions};
use crate::util::{env_var, trim_trailing_slash};

pub const DEFAULT_OLLAMA_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_OLLAMA_MODEL: &str = "qwen3:4b";
pub const DEFAULT_EMBED_MODEL: &str = "nomic-embed-text";

pub const SCAN_HELP: &str = "
Local Website Scanner

Usage:
  localith scan <url> [options]

Options:
  --depth <n>             Maximum crawl depth (default: 3)
  --max-pages <n>         Maximum pages to visit (default: 200)
  --max-pattern-pages <n> Maximum pages per dynamic pattern at depth > 1 (default: 3)
  --fresh                 Start a fresh scan, ignoring previous run data and saved queue
  --delay-ms <n>          Delay between pages in milliseconds (default: 900)
  --knowledge             Generate screen & pattern knowledge + keywords with Ollama
  --include-subdomains    Include subdomains of the starting host
  --ignore-robots         Ignore robots.txt (not recommended)
  --auth                  Detect login walls and sign in using AUTH_EMAIL/AUTH_PASSWORD
  --auth-email <e>        Login email (overrides AUTH_EMAIL)
  --auth-password <p>     Login password (overrides AUTH_PASSWORD)
  --screenshot            Capture screenshots of each page
  --help                  Show this help

Examples:
  localith scan https://example.com
  localith scan https://example.com --depth 3 --max-pages 100
";

pub fn ollama_url(explicit: Option<&str>) -> String {
    let raw = explicit
        .map(str::to_string)
        .or_else(|| env_var("OLLAMA_URL"))
        .unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string());
    trim_trailing_slash(&raw)
}

pub fn ollama_model(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| env_var("OLLAMA_MODEL"))
        .unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string())
}

pub fn embed_model(explicit: Option<&str>) -> String {
    explicit
        .map(str::to_string)
        .or_else(|| env_var("OLLAMA_EMBED_MODEL"))
        .unwrap_or_else(|| DEFAULT_EMBED_MODEL.to_string())
}

/// Parses `scan` arguments. Returns `Ok(None)` when help was requested.
pub fn parse_options(args: &[String]) -> Result<Option<CrawlOptions>> {
    parse_options_with(args, &env_var)
}

/// Same as [`parse_options`] with an explicit environment lookup (used by tests).
pub fn parse_options_with(
    args: &[String],
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<CrawlOptions>> {
    if has_flag(args, "--help") || has_flag(args, "-h") {
        println!("{SCAN_HELP}");
        return Ok(None);
    }

    const VALUE_OPTIONS: &[&str] = &[
        "--depth",
        "--max-pages",
        "--max-pattern-pages",
        "--delay-ms",
        "--auth-email",
        "--auth-password",
    ];

    let mut start_url: Option<&str> = None;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if VALUE_OPTIONS.contains(&arg) {
            index += 2;
            continue;
        }
        if arg.starts_with("--") {
            index += 1;
            continue;
        }
        start_url = Some(arg);
        break;
    }

    let Some(start_url) = start_url else {
        println!("{SCAN_HELP}");
        bail!("Please provide a starting URL.");
    };

    let parsed = Url::parse(start_url).map_err(|_| anyhow!("Invalid URL: {start_url}"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("Only http:// and https:// URLs are supported.");
    }

    let setting = |flag: &str, env_name: &str, fallback: f64| -> Option<f64> {
        match read_flag(args, flag).or_else(|| env(env_name)) {
            Some(raw) => raw.trim().parse::<f64>().ok(),
            None => Some(fallback),
        }
    };
    let integer_in = |value: Option<f64>, min: f64, max: f64| -> Option<u64> {
        value
            .filter(|v| v.fract() == 0.0 && *v >= min && *v <= max)
            .map(|v| v as u64)
    };

    let max_depth = integer_in(setting("--depth", "CRAWL_DEPTH", 3.0), 0.0, 10.0)
        .ok_or_else(|| anyhow!("--depth must be an integer from 0 to 10."))?;
    let max_pages = integer_in(setting("--max-pages", "MAX_PAGES", 200.0), 1.0, 10000.0)
        .ok_or_else(|| anyhow!("--max-pages must be an integer from 1 to 10000."))?;
    let max_pages_per_pattern = integer_in(
        setting("--max-pattern-pages", "MAX_PAGES_PER_PATTERN", 3.0),
        1.0,
        1000.0,
    )
    .ok_or_else(|| anyhow!("--max-pattern-pages must be an integer from 1 to 1000."))?;
    let delay_ms = setting("--delay-ms", "CRAWL_DELAY_MS", 900.0)
        .filter(|v| v.is_finite() && (0.0..=60000.0).contains(v))
        .ok_or_else(|| anyhow!("--delay-ms must be between 0 and 60000."))?;

    let include_subdomains = has_flag(args, "--include-subdomains")
        || env("INCLUDE_SUBDOMAINS").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let ignore_robots = has_flag(args, "--ignore-robots");

    let auth_enabled = has_flag(args, "--auth");
    let auth_email = read_flag(args, "--auth-email").or_else(|| env("AUTH_EMAIL"));
    let auth_password = read_flag(args, "--auth-password").or_else(|| env("AUTH_PASSWORD"));
    if auth_enabled && (auth_email.is_none() || auth_password.is_none()) {
        bail!(
            "--auth requires credentials. Set AUTH_EMAIL and AUTH_PASSWORD in .env, or pass --auth-email and --auth-password."
        );
    }

    let env_number = |name: &str, fallback: u64| -> u64 {
        env(name)
            .and_then(|raw| raw.trim().parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
            .map(|v| v as u64)
            .unwrap_or(fallback)
    };

    Ok(Some(CrawlOptions {
        start_url: parsed.to_string(),
        max_depth: max_depth as u32,
        max_pages: max_pages as usize,
        max_pages_per_pattern: max_pages_per_pattern as usize,
        delay_ms: delay_ms as u64,
        page_timeout_ms: env_number("PAGE_TIMEOUT_MS", 30000),
        max_snapshot_chars: env_number("MAX_SNAPSHOT_CHARS", 18000) as usize,
        max_page_text_chars: env_number("MAX_PAGE_TEXT_CHARS", 16000) as usize,
        screenshot: has_flag(args, "--screenshot"),
        generate_knowledge: has_flag(args, "--knowledge"),
        include_subdomains,
        respect_robots: !ignore_robots && env("RESPECT_ROBOTS").as_deref() != Some("false"),
        fresh: has_flag(args, "--fresh"),
        auth: AuthOptions {
            enabled: auth_enabled,
            email: auth_email,
            password: auth_password,
        },
        ollama_url: trim_trailing_slash(
            &env("OLLAMA_URL").unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string()),
        ),
        ollama_model: env("OLLAMA_MODEL").unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
    }))
}

/// Value of `--name value` or `--name=value`.
pub fn read_flag(args: &[String], name: &str) -> Option<String> {
    if let Some(position) = args.iter().position(|arg| arg == name) {
        return args.get(position + 1).cloned();
    }
    let prefix = format!("{name}=");
    args.iter()
        .find_map(|arg| arg.strip_prefix(&prefix).map(str::to_string))
}

pub fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|arg| arg == name)
}
