//! The model's task checklist (`todo_write`). The whole list is replaced on each call.

use crate::ui;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Pending,
    InProgress,
    Completed,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Todo {
    pub content: String,
    pub status: Status,
}

/// Parse `{"todos": [...]}` leniently: models vary in key names and status spellings.
pub fn parse(args: &Value) -> Result<Vec<Todo>> {
    let Some(items) = args.get("todos").or_else(|| args.get("items")).and_then(Value::as_array) else {
        bail!("expected {{\"todos\": [{{\"content\": \"...\", \"status\": \"pending|in_progress|completed\"}}]}}");
    };
    let todos: Vec<Todo> = items
        .iter()
        .filter_map(|it| {
            let (content, status) = match it {
                Value::String(s) => (s.as_str(), ""),
                Value::Object(o) => (
                    ["content", "text", "task", "title", "description"]
                        .iter()
                        .find_map(|k| o.get(*k).and_then(Value::as_str))?,
                    o.get("status").or_else(|| o.get("state")).and_then(Value::as_str).unwrap_or(""),
                ),
                _ => return None,
            };
            let status = match status.to_ascii_lowercase().replace(['-', ' '], "_").as_str() {
                "completed" | "complete" | "done" | "finished" | "x" => Status::Completed,
                "in_progress" | "active" | "doing" | "working" | "current" | "started" => Status::InProgress,
                _ => Status::Pending,
            };
            let content = content.trim();
            (!content.is_empty()).then(|| Todo { content: content.to_string(), status })
        })
        .collect();
    if todos.len() > 50 {
        bail!("too many todos ({}); keep the list under 50 items", todos.len());
    }
    Ok(todos)
}

pub fn progress(todos: &[Todo]) -> String {
    let done = todos.iter().filter(|t| t.status == Status::Completed).count();
    format!("{done}/{}", todos.len())
}

pub fn render(todos: &[Todo]) {
    if todos.is_empty() {
        println!("  {}", ui::dim("└ (empty)"));
        return;
    }
    for t in todos {
        let line = match t.status {
            Status::Completed => format!("{} {}", ui::green("✓"), ui::dim(&t.content)),
            Status::InProgress => format!("{} {}", ui::accent("▸"), ui::bold(&t.content)),
            Status::Pending => format!("{} {}", ui::dim("○"), t.content),
        };
        println!("  {line}");
    }
}

/// Short confirmation returned to the model.
pub fn for_model(todos: &[Todo]) -> String {
    let current = todos.iter().find(|t| t.status == Status::InProgress);
    let mut s = format!("Todo list saved ({} completed).", progress(todos));
    match current {
        Some(t) => s.push_str(&format!(" In progress: {}", t.content)),
        None if todos.iter().any(|t| t.status == Status::Pending) => {
            s.push_str(" Mark the next item in_progress when you start it.")
        }
        None => {}
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn lenient_parsing() {
        let t = parse(&json!({"todos":[
            {"content":"read code","status":"done"},
            {"task":"fix bug","status":"in-progress"},
            "write test",
            {"content":"  ","status":"pending"}
        ]}))
        .unwrap();
        assert_eq!(t.len(), 3);
        assert_eq!(t[0].status, Status::Completed);
        assert_eq!(t[1], Todo { content: "fix bug".into(), status: Status::InProgress });
        assert_eq!(t[2].status, Status::Pending);
        assert_eq!(progress(&t), "1/3");
        assert!(for_model(&t).contains("In progress: fix bug"));
        assert!(parse(&json!({"nope":1})).is_err());
    }
}
