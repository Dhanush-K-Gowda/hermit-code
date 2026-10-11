//! Streaming OpenAI-compatible chat-completions client with retry and model fallback.

use super::sse::{Accumulator, SseParser};
use super::types::{Message, Usage};
use anyhow::{anyhow, bail, Context, Result};
use futures_util::StreamExt;
use serde_json::{json, Value};
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
        })
    }

    /// Send the conversation and stream the reply. `on_text` receives text deltas as they arrive.
    pub async fn chat(
        &self,
        messages: &[Message],
        tools: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Completion> {
        let models = std::iter::once(&self.model).chain(self.fallback_models.iter());
        let mut last_err = anyhow!("no model configured");
        for (i, model) in models.enumerate() {
            if i > 0 {
                crate::ui::notice(&format!("falling back to model {model}"));
            }
            match self.chat_with_model(model, messages, tools, on_text).await {
                Ok(c) => return Ok(c),
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
        tools: &Value,
        on_text: &mut dyn FnMut(&str),
    ) -> Result<Completion> {
        let mut body = json!({
            "model": model,
            "messages": messages,
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if tools.as_array().is_some_and(|t| !t.is_empty()) {
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
                Ok(r) if r.status().is_success() => break res.unwrap(),
                Ok(r) => (Some(r.status()), retry_after_secs(r)),
                Err(e) if e.is_connect() || e.is_timeout() => (None, None),
                Err(_) => return Err(res.unwrap_err()).context("request failed"),
            };
            let retryable = status.is_none_or(|s| s.as_u16() == 429 || s.is_server_error());
            if !retryable {
                let text = res.unwrap().text().await.unwrap_or_default();
                bail!("{} from {model}: {}", status.unwrap(), short(&text));
            }
            attempt += 1;
            if attempt > self.retries {
                let what = status.map_or("connection error".to_string(), |s| s.to_string());
                return Err(anyhow!(Retryable(format!("{what} from {model} after {} retries", self.retries))));
            }
            let wait = retry_after.unwrap_or(1 << (attempt - 1)).min(30);
            crate::ui::notice(&format!(
                "{} — retrying {model} in {wait}s ({attempt}/{})",
                status.map_or("connection error".to_string(), |s| s.to_string()),
                self.retries
            ));
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

    /// List free models on OpenRouter that support tool calling.
    pub async fn list_free_tool_models(&self) -> Result<Vec<String>> {
        let v: Value = self.http.get(format!("{}/models", self.base_url)).send().await?.json().await?;
        let mut out: Vec<String> = v["data"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter(|m| {
                        m["id"].as_str().is_some_and(|id| id.ends_with(":free"))
                            && m["supported_parameters"]
                                .as_array()
                                .is_some_and(|p| p.iter().any(|x| x == "tools"))
                    })
                    .filter_map(|m| m["id"].as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        out.sort();
        Ok(out)
    }
}

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
