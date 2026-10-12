//! Streaming OpenAI-compatible chat-completions client with retry and model fallback.

use super::sse::{Accumulator, SseParser};
use super::types::{Message, Usage};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::cell::Cell;
use std::time::Duration;

pub struct Completion {
    pub message: Message,
    pub usage: Option<Usage>,
    pub model: String,
}

pub struct Client {
    http: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    pub model: String,
    pub fallback_models: Vec<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Retries per model on 429/5xx before falling back to the next model.
    pub retries: u32,
    /// Index into [model, fallbacks…] to try first. Moves to a fallback that worked after
    /// the preferred model kept failing (e.g. rate limits), so later calls skip the busy one.
    preferred: Cell<usize>,
}

impl Client {
    pub fn new(base_url: &str, api_key: Option<String>, model: String) -> Result<Self> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .read_timeout(Duration::from_secs(180))
            .pool_max_idle_per_host(1)
            .build()?;
        Ok(Self {
            http,
            base_url: base_url.trim_end_matches('/').to_string(),
            api_key,
            model,
            fallback_models: Vec::new(),
            temperature: None,
            max_tokens: None,
            retries: 3,
            preferred: Cell::new(0),
        })
    }

    /// Switch the primary model and forget any sticky fallback.
    pub fn set_model(&mut self, model: &str) {
        self.model = model.to_string();
        self.preferred.set(0);
    }

    /// The model requests currently go to first.
    pub fn active_model(&self) -> &str {
        self.chain().nth(self.preferred.get()).unwrap_or(&self.model)
    }

    fn chain(&self) -> impl Iterator<Item = &String> {
        std::iter::once(&self.model).chain(self.fallback_models.iter())
    }

    /// Send the conversation and stream the reply. `on_text` receives text deltas as they arrive.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: Option<&Value>,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Completion> {
        let models: Vec<&String> = self.chain().collect();
        let start = self.preferred.get().min(models.len() - 1);
        let mut last_err = anyhow!("no model configured");
        for k in 0..models.len() {
            let i = (start + k) % models.len();
            let model = models[i];
            match self.chat_with_model(model, messages, tools, on_text).await {
                Ok(c) => {
                    if i != start {
                        self.preferred.set(i);
                        crate::ui::notice(&format!(
                            "{} is busy; using {model} for the rest of this session (/model to change)",
                            models[start]
                        ));
                    }
                    return Ok(c);
                }
                Err(e) if e.downcast_ref::<Retryable>().is_some() => last_err = e,
                Err(e) => return Err(e),
            }
        }
        Err(last_err)
    }

    async fn chat_with_model(
        &self,
        model: &str,
        messages: &[Message],
        tools: Option<&Value>,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Completion> {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if let Some(tools) = tools.filter(|t| t.as_array().is_some_and(|a| !a.is_empty())) {
            body["tools"] = tools.clone();
            body["tool_choice"] = json!("auto");
        }
        if let Some(t) = self.temperature {
            body["temperature"] = json!(t);
        }
        if let Some(m) = self.max_tokens {
            body["max_tokens"] = json!(m);
        }
        let body = serde_json::to_vec(&body)?;

        let mut attempt = 0;
        let resp = loop {
            let mut req = self
                .http
                .post(format!("{}/chat/completions", self.base_url))
                .header("content-type", "application/json")
                .header("HTTP-Referer", "https://github.com/Dhanush-K-Gowda/hermit-code")
                .header("X-Title", "hermit")
                .body(body.clone());
            if let Some(k) = &self.api_key {
                req = req.bearer_auth(k);
            }
            let res = req.send().await;
            let (status, retry_after) = match &res {
                Ok(r) if r.status().is_success() => {
                    crate::ui::clear_status();
                    break res.unwrap();
                }
                Ok(r) => (Some(r.status()), retry_after_secs(r)),
                Err(e) if e.is_connect() || e.is_timeout() => (None, None),
                Err(_) => return Err(res.unwrap_err()).context("request failed"),
            };
            let retryable = status.is_none_or(|s| s.as_u16() == 429 || s.is_server_error());
            if !retryable {
                let text = res.unwrap().text().await.unwrap_or_default();
                let code = status.unwrap().as_u16();
                if tools.is_some() && super::text_tools::is_tools_unsupported(code, &text) {
                    return Err(anyhow!(ToolsUnsupported(model.to_string())));
                }
                bail!("{} from {model}: {}", status.unwrap(), short(&text));
            }
            attempt += 1;
            if attempt > self.retries {
                let what = status.map_or("connection error".to_string(), |s| s.to_string());
                return Err(anyhow!(Retryable(format!("{what} from {model} after {} retries", self.retries))));
            }
            let wait = retry_after.unwrap_or(1 << (attempt - 1)).min(30);
            let why = match status {
                Some(s) if s.as_u16() == 429 => "rate limited".to_string(),
                Some(s) => s.to_string(),
                None => "connection error".to_string(),
            };
            crate::ui::status(&format!("{model} {why} · retrying in {wait}s ({attempt}/{})", self.retries));
            tokio::time::sleep(Duration::from_secs(wait)).await;
        };

        let mut parser = SseParser::default();
        let mut acc = Accumulator::default();
        let mut stream = resp.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("stream interrupted")?;
            parser.feed(&chunk, |data| {
                if let Some(t) = acc.apply(data)? {
                    on_text(&t);
                }
                Ok(())
            })?;
            if acc.done {
                break;
            }
        }
        let usage = acc.usage;
        Ok(Completion { message: acc.into_message(), usage, model: model.to_string() })
    }

    /// List free models on OpenRouter; with `tools_only`, only those with native tool calling.
    pub async fn list_free_models(&self, tools_only: bool) -> Result<Vec<String>> {
        let v: Value = self.http.get(format!("{}/models", self.base_url)).send().await?.json().await?;
        let mut out: Vec<String> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|m| {
                        m["id"].as_str().is_some_and(|id| id.ends_with(":free"))
                            && (!tools_only
                                || m["supported_parameters"]
                                    .as_array()
                                    .is_some_and(|p| p.iter().any(|x| x == "tools")))
                    })
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }
}

/// The model rejected native tool definitions; switch to text-based tool calls.
#[derive(Debug)]
pub struct ToolsUnsupported(pub String);
impl std::fmt::Display for ToolsUnsupported {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} does not support native tool calling", self.0)
    }
}
impl std::error::Error for ToolsUnsupported {}

/// Marker error: this model failed transiently, try the next one.
#[derive(Debug)]
struct Retryable(String);
impl std::fmt::Display for Retryable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Retryable {}

fn retry_after_secs(r: &reqwest::Response) -> Option<u64> {
    r.headers().get("retry-after")?.to_str().ok()?.trim().parse().ok()
}

fn short(s: &str) -> &str {
    let end = s.char_indices().nth(500).map_or(s.len(), |(i, _)| i);
    &s[..end]
}
