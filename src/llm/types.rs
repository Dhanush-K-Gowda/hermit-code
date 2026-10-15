use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    #[default]
    User,
    Assistant,
    Tool,
}

/// One chat message. Serialized in the OpenAI-compatible wire format: `content` is a plain
/// string, or an array of text and `image_url` parts when the message carries images.
#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(from = "WireMessage", into = "WireMessage")]
pub struct Message {
    pub role: Role,
    pub content: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub tool_call_id: Option<String>,
    /// Attached images as `data:` URLs.
    pub images: Vec<String>,
}

/// Rough token cost of one image, for budgeting (actual cost varies by provider and size).
pub const IMAGE_TOKENS: usize = 1500;

impl Message {
    pub fn system(s: impl Into<String>) -> Self {
        Self::text(Role::System, s)
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self::text(Role::User, s)
    }
    pub fn user_with_images(s: impl Into<String>, images: Vec<String>) -> Self {
        Self { images, ..Self::text(Role::User, s) }
    }
    pub fn tool(id: impl Into<String>, s: impl Into<String>) -> Self {
        Self { tool_call_id: Some(id.into()), ..Self::text(Role::Tool, s) }
    }
    pub fn assistant(content: Option<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self { role: Role::Assistant, content, tool_calls, ..Self::default() }
    }
    fn text(role: Role, s: impl Into<String>) -> Self {
        Self { role, content: Some(s.into()), ..Self::default() }
    }

    /// Rough size of this message in characters, used for token estimation.
    /// Images count as a fixed token cost rather than their base64 length.
    pub fn char_len(&self) -> usize {
        self.content.as_ref().map_or(0, |c| c.len())
            + self
                .tool_calls
                .iter()
                .map(|t| t.function.name.len() + t.function.arguments.len() + t.id.len())
                .sum::<usize>()
            + self.images.len() * IMAGE_TOKENS * 4
            + 16
    }

    /// Replace attached images with a short note (keeps the text, frees the data).
    pub fn drop_images(&mut self, note: &str) {
        if self.images.is_empty() {
            return;
        }
        let n = self.images.len();
        self.images.clear();
        let c = self.content.get_or_insert_with(String::new);
        c.push_str(&format!("\n[{n} image{} {note}]", if n == 1 { "" } else { "s" }));
    }
}

#[derive(Serialize, Deserialize)]
struct WireMessage {
    role: Role,
    /// Always serialized (as `null` when absent); some providers reject a missing key.
    content: Option<WireContent>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum WireContent {
    Text(String),
    Parts(Vec<Part>),
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Part {
    Text { text: String },
    ImageUrl { image_url: ImageUrl },
}

#[derive(Serialize, Deserialize)]
struct ImageUrl {
    url: String,
}

impl From<Message> for WireMessage {
    fn from(m: Message) -> Self {
        let content = if m.images.is_empty() {
            m.content.map(WireContent::Text)
        } else {
            let mut parts: Vec<Part> = m.content.into_iter().filter(|t| !t.is_empty()).map(|text| Part::Text { text }).collect();
            parts.extend(m.images.into_iter().map(|url| Part::ImageUrl { image_url: ImageUrl { url } }));
            Some(WireContent::Parts(parts))
        };
        WireMessage { role: m.role, content, tool_calls: m.tool_calls, tool_call_id: m.tool_call_id }
    }
}

impl From<WireMessage> for Message {
    fn from(w: WireMessage) -> Self {
        let (content, images) = match w.content {
            None => (None, Vec::new()),
            Some(WireContent::Text(t)) => (Some(t), Vec::new()),
            Some(WireContent::Parts(parts)) => {
                let mut text = String::new();
                let mut images = Vec::new();
                for p in parts {
                    match p {
                        Part::Text { text: t } => text.push_str(&t),
                        Part::ImageUrl { image_url } => images.push(image_url.url),
                    }
                }
                ((!text.is_empty()).then_some(text), images)
            }
        };
        Message { role: w.role, content, tool_calls: w.tool_calls, tool_call_id: w.tool_call_id, images }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type", default = "function_kind")]
    pub kind: String,
    pub function: FunctionCall,
}

fn function_kind() -> String {
    "function".into()
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Deserialize, Clone, Copy, Debug, Default)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_round_trips() {
        let plain = Message::user("hi");
        assert_eq!(serde_json::to_value(&plain).unwrap(), serde_json::json!({"role":"user","content":"hi"}));

        let img = Message::user_with_images("look", vec!["data:image/png;base64,AAA".into()]);
        let v = serde_json::to_value(&img).unwrap();
        assert_eq!(v["content"][0], serde_json::json!({"type":"text","text":"look"}));
        assert_eq!(v["content"][1], serde_json::json!({"type":"image_url","image_url":{"url":"data:image/png;base64,AAA"}}));
        let back: Message = serde_json::from_value(v).unwrap();
        assert_eq!(back.content.as_deref(), Some("look"));
        assert_eq!(back.images.len(), 1);

        let none: Message = serde_json::from_value(serde_json::json!({"role":"assistant","content":null})).unwrap();
        assert!(none.content.is_none());
        assert_eq!(serde_json::to_value(&none).unwrap()["content"], serde_json::Value::Null);
    }

    #[test]
    fn images_count_as_fixed_tokens_and_can_be_dropped() {
        let mut m = Message::user_with_images("x", vec!["data:image/png;base64,".to_string() + &"A".repeat(1_000_000)]);
        assert!(m.char_len() < 10_000);
        m.drop_images("omitted");
        assert!(m.images.is_empty());
        assert_eq!(m.content.as_deref(), Some("x\n[1 image omitted]"));
    }
}
