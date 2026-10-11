use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One chat message in the OpenAI-compatible wire format.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct Message {
    pub role: Role,
    /// Always serialized (as `null` when absent); some providers reject a missing key.
    pub content: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn system(s: impl Into<String>) -> Self {
        Self::text(Role::System, s)
    }
    pub fn user(s: impl Into<String>) -> Self {
        Self::text(Role::User, s)
    }
    pub fn tool(id: impl Into<String>, s: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Some(s.into()),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }
    fn text(role: Role, s: impl Into<String>) -> Self {
        Self {
            role,
            content: Some(s.into()),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Rough size of this message in characters, used for token estimation.
    pub fn char_len(&self) -> usize {
        self.content.as_ref().map_or(0, |c| c.len())
            + self
                .tool_calls
                .iter()
                .map(|t| t.function.name.len() + t.function.arguments.len() + t.id.len())
                .sum::<usize>()
            + 16
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
