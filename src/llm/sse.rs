//! Incremental Server-Sent Events parsing and streaming-delta accumulation.

use super::types::{FunctionCall, Message, ToolCall, Usage};
use anyhow::{bail, Result};
use serde_json::Value;

/// Splits a byte stream into SSE `data:` payloads. Holds at most one partial line.
#[derive(Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    /// Feed raw bytes; calls `on_data` for each complete `data:` payload.
    pub fn feed(&mut self, bytes: &[u8], mut on_data: impl FnMut(&str) -> Result<()>) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(pos) = self.buf[start..].iter().position(|&b| b == b'\n') {
            let end = start + pos;
            let line = &self.buf[start..end];
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            if let Some(data) = line.strip_prefix(b"data:") {
                let data = std::str::from_utf8(data).unwrap_or("").trim_start();
                if !data.is_empty() {
                    on_data(data)?;
                }
            }
            // Comments (": keep-alive"), `event:`, `id:` and blank lines are ignored.
            start = end + 1;
        }
        self.buf.drain(..start);
        Ok(())
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Accumulates streamed chat-completion chunks into one assistant message.
#[derive(Default)]
pub struct Accumulator {
    content: String,
    calls: Vec<PartialCall>,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
    pub done: bool,
}

impl Accumulator {
    /// Apply one `data:` payload. Returns any text delta for live display.
    pub fn apply(&mut self, data: &str) -> Result<Option<String>> {
        if data == "[DONE]" {
            self.done = true;
            return Ok(None);
        }
        let v: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(_) => return Ok(None), // tolerate junk/keep-alive payloads
        };
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("unknown error");
            bail!("provider error: {msg}");
        }
        if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
            self.usage = serde_json::from_value(u.clone()).ok();
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return Ok(None);
        };
        if let Some(fr) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(fr.to_string());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(None);
        };
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for (pos, c) in calls.iter().enumerate() {
                let idx = c.get("index").and_then(Value::as_u64).map_or(pos, |i| i as usize);
                while self.calls.len() <= idx {
                    self.calls.push(PartialCall::default());
                }
                let p = &mut self.calls[idx];
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    p.id.push_str(id);
                }
                if let Some(f) = c.get("function") {
                    if let Some(n) = f.get("name").and_then(Value::as_str) {
                        p.name.push_str(n);
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        p.arguments.push_str(a);
                    }
                }
            }
        }
        match delta.get("content").and_then(Value::as_str) {
            Some(t) if !t.is_empty() => {
                self.content.push_str(t);
                Ok(Some(t.to_string()))
            }
            _ => Ok(None),
        }
    }

    pub fn into_message(self) -> Message {
        let tool_calls = self
            .calls
            .into_iter()
            .filter(|c| !c.name.is_empty())
            .enumerate()
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() { format!("call_{i}") } else { c.id },
                kind: "function".into(),
                function: FunctionCall {
                    name: c.name,
                    arguments: if c.arguments.trim().is_empty() { "{}".into() } else { c.arguments },
                },
            })
            .collect();
        Message::assistant(if self.content.is_empty() { None } else { Some(self.content) }, tool_calls)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(chunks: &[&str]) -> (Message, String) {
        let mut p = SseParser::default();
        let mut acc = Accumulator::default();
        let mut text = String::new();
        for c in chunks {
            p.feed(c.as_bytes(), |d| {
                if let Some(t) = acc.apply(d)? {
                    text.push_str(&t);
                }
                Ok(())
            })
            .unwrap();
        }
        (acc.into_message(), text)
    }

    #[test]
    fn text_split_across_chunks() {
        let (m, t) = run(&[
            ": OPENROUTER PROCESSING\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "\ndata: {\"choices\":[{\"del",
            "ta\":{\"content\":\"lo\"}}]}\r\n\r\ndata: [DONE]\n\n",
        ]);
        assert_eq!(t, "Hello");
        assert_eq!(m.content.as_deref(), Some("Hello"));
        assert!(m.tool_calls.is_empty());
    }

    #[test]
    fn tool_call_deltas_merge_by_index() {
        let (m, _) = run(&[
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"a\",\"function\":{\"name\":\"read_file\",\"arguments\":\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"id\":\"b\",\"function\":{\"name\":\"grep\",\"arguments\":\"{\\\"pattern\\\"\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":\\\"x\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":1,\"function\":{\"arguments\":\":\\\"y\\\"}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":5,\"completion_tokens\":2}}\n\n",
        ]);
        assert_eq!(m.tool_calls.len(), 2);
        assert_eq!(m.tool_calls[0].function.name, "read_file");
        assert_eq!(m.tool_calls[0].function.arguments, "{\"path\":\"x\"}");
        assert_eq!(m.tool_calls[1].id, "b");
        assert_eq!(m.tool_calls[1].function.arguments, "{\"pattern\":\"y\"}");
    }

    #[test]
    fn provider_error_surfaces() {
        let mut acc = Accumulator::default();
        assert!(acc.apply("{\"error\":{\"message\":\"rate limited\"}}").is_err());
    }
}
