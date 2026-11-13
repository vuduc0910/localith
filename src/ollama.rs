//! Small client for the Ollama HTTP API (chat and embeddings).

use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::config;

#[derive(Debug, Clone)]
pub struct Ollama {
    base_url: String,
    http: reqwest::Client,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ChatSettings {
    /// Ask Ollama to constrain the reply to valid JSON.
    pub json: bool,
    pub temperature: f64,
    pub num_ctx: Option<u32>,
}

impl Ollama {
    /// Client for `url`, or for OLLAMA_URL / the local default when `None`.
    pub fn new(url: Option<&str>) -> Self {
        // No overall timeout: generation on CPU-only machines can take many minutes.
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .unwrap_or_default();
        Self {
            base_url: config::ollama_url(url),
            http,
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Whether the Ollama server answers at all.
    pub async fn is_reachable(&self) -> bool {
        self.http
            .get(format!("{}/api/tags", self.base_url))
            .timeout(Duration::from_secs(3))
            .send()
            .await
            .is_ok_and(|response| response.status().is_success())
    }

    /// Sends one user message and returns the complete reply text.
    ///
    /// The reply is streamed and collected, so the connection stays active
    /// however long the model takes.
    pub async fn chat_text(
        &self,
        model: &str,
        prompt: &str,
        settings: ChatSettings,
    ) -> Result<String> {
        let mut options = json!({ "temperature": settings.temperature });
        if let Some(num_ctx) = settings.num_ctx {
            options["num_ctx"] = json!(num_ctx);
        }
        let mut body = json!({
            "model": model,
            "messages": [{ "role": "user", "content": prompt }],
            "stream": true,
            "options": options
        });
        if settings.json {
            body["format"] = json!("json");
        }

        let mut response = self
            .http
            .post(format!("{}/api/chat", self.base_url))
            .json(&body)
            .send()
            .await
            .with_context(|| format!("Could not reach Ollama at {}", self.base_url))?;

        if !response.status().is_success() {
            let status = response.status();
            let detail = response.text().await.unwrap_or_default();
            bail!("Ollama chat failed ({status}): {}", error_text(&detail));
        }

        let mut content = String::new();
        let mut buffer: Vec<u8> = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .context("Ollama stream interrupted")?
        {
            buffer.extend_from_slice(&chunk);
            while let Some(newline) = buffer.iter().position(|byte| *byte == b'\n') {
                let line: Vec<u8> = buffer.drain(..=newline).collect();
                append_chat_line(&line, &mut content)?;
            }
        }
        append_chat_line(&buffer, &mut content)?;
        Ok(content)
    }

    /// Embeds `texts` in batches and returns one vector per text, in order.
    pub async fn embed(
        &self,
        model: &str,
        texts: &[String],
        batch_size: usize,
    ) -> Result<Vec<Vec<f32>>> {
        let mut all: Vec<Vec<f32>> = Vec::with_capacity(texts.len());
        for batch in texts.chunks(batch_size.max(1)) {
            let embeddings = self.embed_batch(model, batch).await.map_err(|err| {
                anyhow!(
                    "Failed to generate embeddings with Ollama model \"{model}\": {err:#}. Run: ollama pull {model}"
                )
            })?;
            all.extend(embeddings);
        }
        Ok(all)
    }

    async fn embed_batch(&self, model: &str, batch: &[String]) -> Result<Vec<Vec<f32>>> {
        let response = self
            .http
            .post(format!("{}/api/embed", self.base_url))
            .json(&json!({ "model": model, "input": batch }))
            .send()
            .await
            .with_context(|| format!("Could not reach Ollama at {}", self.base_url))?;

        let status = response.status();
        let text = response.text().await.unwrap_or_default();
        if !status.is_success() {
            bail!("{status}: {}", error_text(&text));
        }

        #[derive(serde::Deserialize)]
        struct EmbedResponse {
            #[serde(default)]
            embeddings: Vec<Vec<f32>>,
        }
        let parsed: EmbedResponse =
            serde_json::from_str(&text).context("Invalid embed response")?;
        if parsed.embeddings.len() != batch.len() || parsed.embeddings.is_empty() {
            bail!("Ollama returned empty embeddings.");
        }
        Ok(parsed.embeddings)
    }
}

/// Adds the text of one streamed chat line to `content`.
fn append_chat_line(line: &[u8], content: &mut String) -> Result<()> {
    let line = String::from_utf8_lossy(line);
    let line = line.trim();
    if line.is_empty() {
        return Ok(());
    }
    let value: Value = serde_json::from_str(line).context("Invalid line in Ollama stream")?;
    if let Some(error) = value.get("error").and_then(Value::as_str) {
        bail!("Ollama error: {error}");
    }
    if let Some(text) = value.pointer("/message/content").and_then(Value::as_str) {
        content.push_str(text);
    }
    Ok(())
}

/// The `error` field of an Ollama error body, or the body itself.
fn error_text(body: &str) -> String {
    serde_json::from_str::<Value>(body)
        .ok()
        .and_then(|value| {
            value
                .get("error")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| body.trim().to_string())
}
