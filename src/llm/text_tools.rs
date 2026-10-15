//! Tool calling for models without native support: the model writes
//! `<tool_call>{"name": ..., "arguments": {...}}</tool_call>` blocks in plain text.
//!
//! History is always stored in the native shape (assistant `tool_calls` + `tool` messages);
//! `to_text_protocol` converts it per request, so switching modes or models mid-session works.

use super::types::{FunctionCall, Message, Role, ToolCall};
use serde_json::Value;
use std::collections::HashMap;

const OPEN: &str = "<tool_call>";
const CLOSE: &str = "</tool_call>";

/// Instructions appended to the system prompt in text mode.
pub fn instructions(defs: &Value) -> String {
    let mut s = String::from(
        "\n\n# Tools\nYou can call tools. To call one, write a block exactly like this:\n\
         <tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"src/main.rs\"}}\n</tool_call>\n\
         Rules: the block content must be one JSON object with \"name\" and \"arguments\". \
         You may write several blocks in one reply. After your tool calls, STOP and wait: \
         results arrive in the next message inside <tool_result> tags. Never invent results. \
         When the task is done, reply without any tool calls.\n\nAvailable tools:\n",
    );
    for d in defs.as_array().into_iter().flatten() {
        let f = &d["function"];
        s.push_str(&format!(
            "\n## {}\n{}\nParameters (JSON schema): {}\n",
            f["name"].as_str().unwrap_or(""),
            f["description"].as_str().unwrap_or(""),
            f["parameters"]
        ));
    }
    s
}

/// Convert canonical history into a plain-text conversation (no tool roles or tool_calls).
pub fn to_text_protocol(messages: &[Message], instructions: &str) -> Vec<Message> {
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut out: Vec<Message> = Vec::with_capacity(messages.len());
    let mut results = String::new();
    let flush = |results: &mut String, out: &mut Vec<Message>| {
        if !results.is_empty() {
            out.push(Message::user(std::mem::take(results)));
        }
    };
    for m in messages {
        match m.role {
            Role::Tool => {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                let name = names.get(id).copied().unwrap_or("tool");
                results.push_str(&format!(
                    "<tool_result name=\"{name}\">\n{}\n</tool_result>\n",
                    m.content.as_deref().unwrap_or("")
                ));
            }
            Role::System => {
                flush(&mut results, &mut out);
                let mut c = m.content.clone().unwrap_or_default();
                c.push_str(instructions);
                out.push(Message::system(c));
            }
            Role::Assistant => {
                flush(&mut results, &mut out);
                let mut c = m.content.clone().unwrap_or_default();
                for call in &m.tool_calls {
                    names.insert(&call.id, &call.function.name);
                    let args: Value = serde_json::from_str(&call.function.arguments).unwrap_or(Value::Object(Default::default()));
                    let block = serde_json::json!({"name": call.function.name, "arguments": args});
                    if !c.is_empty() {
                        c.push('\n');
                    }
                    c.push_str(&format!("{OPEN}\n{block}\n{CLOSE}"));
                }
                out.push(Message::assistant(Some(c), Vec::new()));
            }
            Role::User => {
                flush(&mut results, &mut out);
                out.push(m.clone());
            }
        }
    }
    flush(&mut results, &mut out);
    out
}

/// Extract `<tool_call>` blocks from model text. Returns (remaining text, calls).
/// Blocks that aren't valid JSON naming a tool are left in the text.
pub fn parse(content: &str, id_prefix: &str) -> (String, Vec<ToolCall>) {
    let mut text = String::new();
    let mut calls = Vec::new();
    let mut rest = content;
    while let Some(start) = rest.find(OPEN) {
        let after = &rest[start + OPEN.len()..];
        let (body, next) = match after.find(CLOSE) {
            Some(end) => (&after[..end], &after[end + CLOSE.len()..]),
            None => (after, ""), // model stopped before closing the tag
        };
        match parse_call(body) {
            Some((name, args)) => {
                text.push_str(&rest[..start]);
                calls.push(ToolCall {
                    id: format!("{id_prefix}{}", calls.len()),
                    kind: "function".into(),
                    function: FunctionCall { name, arguments: args },
                });
            }
            None => text.push_str(&rest[..start + OPEN.len() + body.len()]),
        }
        rest = next;
    }
    text.push_str(rest);
    (text.trim().to_string(), calls)
}

fn parse_call(body: &str) -> Option<(String, String)> {
    let body = body.trim();
    let body = body.strip_prefix("```json").or_else(|| body.strip_prefix("```")).unwrap_or(body);
    let body = body.strip_suffix("```").unwrap_or(body).trim();
    let v: Value = serde_json::from_str(body).ok()?;
    let name = ["name", "tool", "function"].iter().find_map(|k| v.get(*k)?.as_str())?.to_string();
    let args = ["arguments", "args", "parameters", "input"]
        .iter()
        .find_map(|k| v.get(*k))
        .cloned()
        .unwrap_or(Value::Object(Default::default()));
    // Some models send arguments as a JSON-encoded string.
    let args = match args {
        Value::String(s) => s,
        other => other.to_string(),
    };
    Some((name, args))
}

/// Hides `<tool_call>…</tool_call>` blocks from streamed text while passing the rest through.
#[derive(Default)]
pub struct StreamFilter {
    pending: String,
    hiding: bool,
}

impl StreamFilter {
    /// Feed a chunk; returns the text that is safe to display now.
    pub fn push(&mut self, chunk: &str) -> String {
        self.pending.push_str(chunk);
        let mut out = String::new();
        loop {
            if self.hiding {
                if let Some(i) = self.pending.find(CLOSE) {
                    self.pending.drain(..i + CLOSE.len());
                    self.hiding = false;
                    continue;
                }
                // Keep just enough to detect a CLOSE split across chunks.
                let keep = floor_boundary(&self.pending, self.pending.len().saturating_sub(CLOSE.len() - 1));
                self.pending.drain(..keep);
                return out;
            }
            if let Some(i) = self.pending.find(OPEN) {
                out.push_str(&self.pending[..i]);
                self.pending.drain(..i + OPEN.len());
                self.hiding = true;
                continue;
            }
            // Hold back a trailing partial "<tool_c…" that might become OPEN.
            let hold = (1..OPEN.len()).rev().find(|&k| self.pending.ends_with(&OPEN[..k])).unwrap_or(0);
            let cut = self.pending.len() - hold;
            out.push_str(&self.pending[..cut]);
            self.pending.drain(..cut);
            return out;
        }
    }

    /// Flush anything held back at the end of the stream.
    pub fn finish(&mut self) -> String {
        let rest = std::mem::take(&mut self.pending);
        if self.hiding { String::new() } else { rest }
    }
}

fn floor_boundary(s: &str, mut i: usize) -> usize {
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// Does a provider error say the model doesn't support tool calling?
pub fn is_tools_unsupported(status: u16, body: &str) -> bool {
    let b = body.to_ascii_lowercase();
    matches!(status, 400 | 404 | 422)
        && b.contains("tool")
        && ["support", "not available", "no endpoints", "not enabled"].iter().any(|k| b.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_blocks_and_keeps_text() {
        let s = "Let me look.\n<tool_call>\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.rs\"}}\n</tool_call>\n\
                 <tool_call>```json\n{\"tool\":\"grep\",\"args\":\"{\\\"pattern\\\":\\\"x\\\"}\"}\n```</tool_call>";
        let (text, calls) = parse(s, "t1_");
        assert_eq!(text, "Let me look.");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "t1_0");
        assert_eq!(calls[0].function.arguments, "{\"path\":\"a.rs\"}");
        assert_eq!(calls[1].function.name, "grep");
        assert_eq!(calls[1].function.arguments, "{\"pattern\":\"x\"}");
    }

    #[test]
    fn unclosed_and_invalid_blocks() {
        let (_, calls) = parse("<tool_call>{\"name\":\"list_dir\",\"arguments\":{}}", "x");
        assert_eq!(calls.len(), 1);
        let (text, calls) = parse("use <tool_call>not json</tool_call> tags", "x");
        assert!(calls.is_empty());
        assert!(text.contains("not json"));
    }

    #[test]
    fn stream_filter_hides_split_tags() {
        let mut f = StreamFilter::default();
        let mut shown = String::new();
        for c in ["Hello <to", "ol_call>{\"name\":", "\"x\"}</tool_", "call> world <", "b>é"] {
            shown.push_str(&f.push(c));
        }
        shown.push_str(&f.finish());
        assert_eq!(shown, "Hello  world <b>é");
    }

    #[test]
    fn history_round_trips_to_text() {
        let mut a = Message::assistant(Some("checking".into()), vec![]);
        a.tool_calls.push(ToolCall {
            id: "c1".into(),
            kind: "function".into(),
            function: FunctionCall { name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() },
        });
        let msgs = vec![Message::system("sys"), Message::user("go"), a, Message::tool("c1", "file body"), Message::user("next")];
        let t = to_text_protocol(&msgs, "\nTOOLS");
        assert_eq!(t.len(), 5);
        assert!(t[0].content.as_deref().unwrap().ends_with("TOOLS"));
        assert!(t[2].content.as_deref().unwrap().contains("<tool_call>\n{\"arguments\":{\"path\":\"a\"},\"name\":\"read_file\"}"));
        assert!(t[2].tool_calls.is_empty());
        assert_eq!(t[3].role, Role::User);
        assert!(t[3].content.as_deref().unwrap().starts_with("<tool_result name=\"read_file\">\nfile body"));
        assert!(t.iter().all(|m| m.role != Role::Tool));
    }

    #[test]
    fn detects_unsupported_errors() {
        assert!(is_tools_unsupported(404, r#"{"error":{"message":"No endpoints found that support tool use."}}"#));
        assert!(!is_tools_unsupported(401, "tool use not supported"));
        assert!(!is_tools_unsupported(400, "invalid model"));
    }
}
