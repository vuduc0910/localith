//! Drives the `agent-browser` CLI (one shared browser session per crawl).

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Result};
use serde::de::DeserializeOwned;
use tokio::process::Command;

use crate::types::PageExtraction;
use crate::util::{env_var, truncate_chars};

const EMAIL_SELECTOR: &str = r#"input[autocomplete="username"], input[type="email"], input[name="email"], input[name="username"], input[id="email"], input[id="username"], input[data-path="email"]"#;
const PASSWORD_SELECTOR: &str =
    r#"input[type="password"], input[name="password"], input[id="password"]"#;

/// How to start agent-browser: a program plus arguments that precede the command.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentBrowserCommand {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

/// File name of the native agent-browser binary shipped in its npm package.
pub fn native_binary_name() -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        _ => return None,
    };
    match std::env::consts::OS {
        "windows" => Some(format!("agent-browser-win32-{arch}.exe")),
        "macos" => Some(format!("agent-browser-darwin-{arch}")),
        "linux" => {
            let musl = cfg!(target_env = "musl") || Path::new("/etc/alpine-release").exists();
            let os = if musl { "linux-musl" } else { "linux" };
            Some(format!("agent-browser-{os}-{arch}"))
        }
        _ => None,
    }
}

/// Finds agent-browser.
///
/// Order: `AGENT_BROWSER_BIN`, then the native binary of a local or global npm
/// install, then `agent-browser` on PATH. The native binary is preferred over
/// npm's shims because `.cmd` shims cannot take arbitrary arguments on Windows.
pub fn resolve_agent_browser_command() -> Result<AgentBrowserCommand> {
    resolve_with(
        env_var("AGENT_BROWSER_BIN").as_deref(),
        &std::env::current_dir().unwrap_or_default(),
        &std::env::var_os("PATH")
            .map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>())
            .unwrap_or_default(),
    )
}

pub fn resolve_with(
    override_bin: Option<&str>,
    cwd: &Path,
    path_dirs: &[PathBuf],
) -> Result<AgentBrowserCommand> {
    if let Some(override_bin) = override_bin
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let lower = override_bin.to_lowercase();
        let is_script = [".js", ".cjs", ".mjs"]
            .iter()
            .any(|ext| lower.ends_with(ext));
        return Ok(if is_script {
            AgentBrowserCommand {
                program: PathBuf::from("node"),
                prefix_args: vec![override_bin.to_string()],
            }
        } else {
            AgentBrowserCommand {
                program: PathBuf::from(override_bin),
                prefix_args: Vec::new(),
            }
        });
    }

    let native = native_binary_name();
    let package_bin = |root: &Path| -> Option<PathBuf> {
        let candidate = root
            .join("node_modules")
            .join("agent-browser")
            .join("bin")
            .join(native.as_deref()?);
        candidate.is_file().then_some(candidate)
    };
    let direct = |program: PathBuf| AgentBrowserCommand {
        program,
        prefix_args: Vec::new(),
    };

    if let Some(found) = package_bin(cwd) {
        return Ok(direct(found));
    }

    for dir in path_dirs {
        // Global npm installs: <prefix>/node_modules (Windows) or <prefix>/lib/node_modules (Unix).
        let npm_roots = [dir.clone(), dir.join("..").join("lib")];
        let has_shim = ["agent-browser", "agent-browser.cmd"]
            .iter()
            .any(|name| dir.join(name).exists());
        if has_shim {
            if let Some(found) = npm_roots.iter().find_map(|root| package_bin(root)) {
                return Ok(direct(found));
            }
        }

        let executable = if cfg!(windows) {
            "agent-browser.exe"
        } else {
            "agent-browser"
        };
        let candidate = dir.join(executable);
        if candidate.is_file() {
            return Ok(direct(candidate));
        }
    }

    bail!(
        "agent-browser was not found. Install it with `npm install -g agent-browser` and run `agent-browser install`, or set AGENT_BROWSER_BIN to its executable."
    )
}

/// Result of reading the page currently shown in the browser.
#[derive(Debug, Clone)]
pub struct PageCapture {
    pub final_url: String,
    pub snapshot: String,
    pub extraction: PageExtraction,
}

#[derive(Debug, Clone, Default)]
pub struct LoginResult {
    pub filled: bool,
    pub submitted: bool,
    pub reason: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Browser {
    command: AgentBrowserCommand,
    session: Option<String>,
}

impl Browser {
    pub fn new() -> Result<Self> {
        Ok(Self {
            command: resolve_agent_browser_command()?,
            session: None,
        })
    }

    /// Names the agent-browser session so cookies and storage persist between runs.
    pub fn set_session(&mut self, session_name: &str) {
        self.session = Some(session_name.to_string());
    }

    /// Runs one agent-browser command and returns its output.
    pub async fn run(&self, args: &[&str], timeout_ms: u64) -> Result<String> {
        let name = args.first().copied().unwrap_or("");
        let mut command = Command::new(&self.command.program);
        command
            .args(&self.command.prefix_args)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(session) = &self.session {
            command.env("AGENT_BROWSER_SESSION", session);
            command.env("AGENT_BROWSER_RESTORE", "1");
        }
        #[cfg(windows)]
        command.creation_flags(0x0800_0000); // CREATE_NO_WINDOW

        let output = tokio::time::timeout(Duration::from_millis(timeout_ms), command.output())
            .await
            .map_err(|_| anyhow!("agent-browser {name} failed: timed out after {timeout_ms}ms"))?
            .map_err(|err| {
                anyhow!(
                    "agent-browser {name} failed: could not start {}: {err}",
                    self.command.program.display()
                )
            })?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        if !output.status.success() {
            let detail = [stderr.trim(), stdout.trim()]
                .into_iter()
                .filter(|part| !part.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            bail!("agent-browser {name} failed: {detail} ({})", output.status);
        }
        Ok(if stdout.is_empty() { stderr } else { stdout })
    }

    pub async fn check_installed(&self) -> Result<()> {
        self.run(&["--help"], 15_000).await.map(|_| ())
    }

    pub async fn open(&self, url: &str, timeout_ms: u64) -> Result<()> {
        self.run(&["open", url], timeout_ms).await.map(|_| ())
    }

    /// Best-effort shutdown; never fails.
    pub async fn close(&self) {
        let _ = self.run(&["close"], 15_000).await;
    }

    pub async fn save_state(&self, state_path: &Path) -> bool {
        let path = state_path.to_string_lossy();
        match self.run(&["state", "save", &path], 30_000).await {
            Ok(_) => true,
            Err(err) => {
                eprintln!("   ⚠️ Could not save browser state: {err}");
                false
            }
        }
    }

    pub async fn load_state(&self, state_path: &Path) -> bool {
        let path = state_path.to_string_lossy();
        match self.run(&["state", "load", &path], 30_000).await {
            Ok(_) => true,
            Err(err) => {
                eprintln!("   ⚠️ Could not load browser state: {err}");
                false
            }
        }
    }

    /// Selects the session and, when a saved state file exists, loads it into the browser.
    pub async fn restore_session(&mut self, session_name: &str, state_path: &Path) -> bool {
        self.set_session(session_name);
        if !state_path.exists() {
            return false;
        }
        // Start the session on a blank page, then load the saved state into it.
        if let Err(err) = self.open("about:blank", 15_000).await {
            eprintln!("   ⚠️ Could not restore browser session: {err}");
            return false;
        }
        self.load_state(state_path).await
    }

    /// Reads URL, accessibility snapshot and DOM facts of the current page.
    pub async fn capture_page(
        &self,
        max_snapshot_chars: usize,
        max_text_chars: usize,
        screenshot_path: Option<&Path>,
    ) -> Result<PageCapture> {
        // Keep CLI calls sequential: they share one agent-browser session.
        let url_output = self.run(&["get", "url"], 45_000).await?;
        let snapshot_output = self.run(&["snapshot", "-c"], 60_000).await?;

        if let Some(path) = screenshot_path {
            self.run(&["screenshot", &path.to_string_lossy()], 30_000)
                .await?;
        }

        let script = extraction_script(max_text_chars);
        let eval_output = self.run(&["eval", &script], 60_000).await?;
        let extraction: PageExtraction = parse_json_from_cli(&eval_output)?;

        Ok(PageCapture {
            final_url: url_output.trim().to_string(),
            snapshot: truncate_chars(&snapshot_output, max_snapshot_chars).to_string(),
            extraction,
        })
    }

    /// Fills the first login form on the page and submits it.
    pub async fn attempt_login(&self, email: &str, password: &str) -> LoginResult {
        let fill = async {
            self.run(&["snapshot", "-i"], 30_000).await?;
            self.run(&["fill", EMAIL_SELECTOR, email], 30_000).await?;
            self.run(&["fill", PASSWORD_SELECTOR, password], 30_000)
                .await?;
            anyhow::Ok(())
        };
        if let Err(err) = fill.await {
            return LoginResult {
                reason: Some(err.to_string()),
                ..LoginResult::default()
            };
        }

        let submit = async {
            self.run(&["press", "Enter"], 60_000).await?;
            self.run(&["wait", "2000"], 10_000).await?;
            anyhow::Ok(())
        };
        match submit.await {
            Ok(()) => LoginResult {
                filled: true,
                submitted: true,
                reason: None,
            },
            Err(err) => LoginResult {
                filled: true,
                submitted: false,
                reason: Some(err.to_string()),
            },
        }
    }
}

fn extraction_script(max_text_chars: usize) -> String {
    format!(
        r#"JSON.stringify((() => {{
      const clean = (s) => (s || "").replace(/\s+/g, " ").trim();
      return {{
        title: document.title || "",
        description: document.querySelector('meta[name="description"]')?.getAttribute("content") || "",
        canonical: document.querySelector('link[rel="canonical"]')?.href || "",
        headings: Array.from(document.querySelectorAll("h1,h2,h3"))
          .slice(0, 100)
          .map((el) => ({{ tag: el.tagName.toLowerCase(), text: clean(el.innerText || el.textContent) }}))
          .filter((item) => item.text),
        links: Array.from(document.querySelectorAll("a[href]"))
          .map((a) => ({{ href: a.href, text: clean(a.innerText || a.getAttribute("aria-label") || a.title) }}))
          .filter((item) => item.href)
          .slice(0, 1500),
        text: (document.body?.innerText || "").slice(0, {max_text_chars}),
        passwordFields: document.querySelectorAll('input[type="password"]').length
      }};
    }})())"#
    )
}

/// Parses JSON printed by `agent-browser eval`, which may be double-encoded
/// or surrounded by other output.
pub fn parse_json_from_cli<T: DeserializeOwned>(output: &str) -> Result<T> {
    let trimmed = output.trim();

    if let Ok(first) = serde_json::from_str::<serde_json::Value>(trimmed) {
        let parsed = match first {
            serde_json::Value::String(inner) => serde_json::from_str::<T>(&inner).ok(),
            other => serde_json::from_value::<T>(other).ok(),
        };
        if let Some(parsed) = parsed {
            return Ok(parsed);
        }
    }

    if let (Some(start), Some(end)) = (trimmed.find('{'), trimmed.rfind('}')) {
        if end > start {
            if let Ok(parsed) = serde_json::from_str::<T>(&trimmed[start..=end]) {
                return Ok(parsed);
            }
        }
    }

    bail!(
        "Could not parse JSON returned by agent-browser: {}",
        truncate_chars(trimmed, 500)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_double_encoded_and_wrapped_json() {
        let plain: PageExtraction =
            parse_json_from_cli(r#"{"title":"Home","passwordFields":1}"#).unwrap();
        assert_eq!(plain.title, "Home");
        assert_eq!(plain.password_fields, 1);

        let double: PageExtraction = parse_json_from_cli(r#""{\"title\":\"Quoted\"}""#).unwrap();
        assert_eq!(double.title, "Quoted");

        let wrapped: PageExtraction =
            parse_json_from_cli("✓ Done\n{\"title\":\"Wrapped\"}\n").unwrap();
        assert_eq!(wrapped.title, "Wrapped");

        assert!(parse_json_from_cli::<PageExtraction>("no json here").is_err());
    }

    #[test]
    fn override_selects_program_or_node_script() {
        let cwd = Path::new("/nonexistent");
        let exe = resolve_with(Some("/opt/agent-browser"), cwd, &[]).unwrap();
        assert_eq!(exe.program, PathBuf::from("/opt/agent-browser"));
        assert!(exe.prefix_args.is_empty());

        let script = resolve_with(Some("C:/tools/agent-browser.JS"), cwd, &[]).unwrap();
        assert_eq!(script.program, PathBuf::from("node"));
        assert_eq!(
            script.prefix_args,
            vec!["C:/tools/agent-browser.JS".to_string()]
        );
    }

    #[test]
    fn finds_native_binary_in_local_node_modules_and_reports_missing() {
        let Some(native) = native_binary_name() else {
            return;
        };
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_with(None, dir.path(), &[]).is_err());

        let bin_dir = dir.path().join("node_modules/agent-browser/bin");
        std::fs::create_dir_all(&bin_dir).unwrap();
        std::fs::write(bin_dir.join(&native), b"").unwrap();

        let found = resolve_with(None, dir.path(), &[]).unwrap();
        assert_eq!(found.program, bin_dir.join(native));
    }
}
