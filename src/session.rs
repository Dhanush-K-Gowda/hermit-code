//! Persistent sessions: one JSON file per conversation in ~/.config/hermit/sessions/.

use crate::llm::{Message, Role};
use crate::tools::todo::Todo;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

const KEEP_SESSIONS: usize = 100;

#[derive(Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub root: PathBuf,
    pub model: String,
    pub title: String,
    pub created: u64,
    pub updated: u64,
    #[serde(default)]
    pub prompt_tokens: u64,
    #[serde(default)]
    pub completion_tokens: u64,
    #[serde(default)]
    pub todos: Vec<Todo>,
    /// Conversation without the system prompt (rebuilt fresh on resume).
    pub messages: Vec<Message>,
}

/// Lightweight listing entry; message bodies are skipped while parsing.
#[derive(Deserialize)]
pub struct Meta {
    pub id: String,
    pub root: PathBuf,
    pub title: String,
    pub updated: u64,
    #[serde(default)]
    pub messages: serde::de::IgnoredAny,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Sortable, unique-enough id: unix seconds + 4 hex digits.
pub fn new_id() -> String {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.subsec_nanos());
    format!("{}-{:04x}", now(), (nanos ^ std::process::id()) & 0xffff)
}

pub fn dir() -> Option<PathBuf> {
    crate::config::config_dir().map(|d| d.join("sessions"))
}

/// First user message, single-lined and shortened.
pub fn title_from(messages: &[Message]) -> String {
    let first = messages
        .iter()
        .find(|m| m.role == Role::User && m.content.as_deref().is_some_and(|c| !c.starts_with('(')))
        .and_then(|m| m.content.as_deref())
        .unwrap_or("(untitled)");
    let one: String = first.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 60 {
        format!("{}…", one.chars().take(59).collect::<String>())
    } else {
        one
    }
}

impl Session {
    pub fn save(&self, dir: &Path) -> Result<()> {
        std::fs::create_dir_all(dir)?;
        let path = dir.join(format!("{}.json", self.id));
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(self)?)?;
        std::fs::rename(&tmp, &path)?;
        Ok(())
    }

    pub fn load(dir: &Path, id: &str) -> Result<Session> {
        let path = dir.join(format!("{id}.json"));
        let data = std::fs::read(&path).with_context(|| format!("no session {id}"))?;
        serde_json::from_slice(&data).with_context(|| format!("corrupt session file {}", path.display()))
    }
}

/// Sessions for `root`, most recent first.
pub fn list(dir: &Path, root: &Path) -> Vec<Meta> {
    let mut out: Vec<Meta> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .filter_map(|e| serde_json::from_slice::<Meta>(&std::fs::read(e.path()).ok()?).ok())
        .filter(|m| m.root == root)
        .collect();
    out.sort_by(|a, b| b.updated.cmp(&a.updated).then_with(|| b.id.cmp(&a.id)));
    out
}

/// Resolve `/resume` arguments: empty = latest, a number = position in the list, else an id prefix.
pub fn resolve(dir: &Path, root: &Path, arg: &str) -> Result<String> {
    let sessions = list(dir, root);
    if sessions.is_empty() {
        bail!("no saved sessions for this directory");
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(sessions[0].id.clone());
    }
    if let Ok(n) = arg.parse::<usize>() {
        if (1..=sessions.len()).contains(&n) && arg.len() < 5 {
            return Ok(sessions[n - 1].id.clone());
        }
    }
    let matches: Vec<_> = sessions.iter().filter(|s| s.id.starts_with(arg)).collect();
    match matches.len() {
        1 => Ok(matches[0].id.clone()),
        0 => bail!("no session matching `{arg}` (see /sessions)"),
        n => bail!("`{arg}` matches {n} sessions; use more of the id"),
    }
}

/// Delete the oldest session files beyond the retention limit.
pub fn prune(dir: &Path) {
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
        .map(|e| e.path())
        .collect();
    if files.len() <= KEEP_SESSIONS {
        return;
    }
    files.sort(); // ids start with unix seconds, so name order = creation order
    for f in &files[..files.len() - KEEP_SESSIONS] {
        let _ = std::fs::remove_file(f);
    }
}

pub fn age(secs: u64) -> String {
    let d = now().saturating_sub(secs);
    match d {
        0..=59 => "just now".into(),
        60..=3599 => format!("{}m ago", d / 60),
        3600..=86_399 => format!("{}h ago", d / 3600),
        _ => format!("{}d ago", d / 86_400),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session(id: &str, root: &Path, updated: u64, first: &str) -> Session {
        Session {
            id: id.into(),
            root: root.to_path_buf(),
            model: "m".into(),
            title: first.into(),
            created: updated,
            updated,
            prompt_tokens: 1,
            completion_tokens: 2,
            todos: vec![],
            messages: vec![Message::user(first)],
        }
    }

    #[test]
    fn save_list_resolve_load() {
        let d = tempfile::tempdir().unwrap();
        let root = Path::new("/proj");
        session("100-aaaa", root, 100, "older").save(d.path()).unwrap();
        session("200-bbbb", root, 200, "newer").save(d.path()).unwrap();
        session("300-cccc", Path::new("/other"), 300, "elsewhere").save(d.path()).unwrap();

        let l = list(d.path(), root);
        assert_eq!(l.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(), ["200-bbbb", "100-aaaa"]);
        assert_eq!(resolve(d.path(), root, "").unwrap(), "200-bbbb");
        assert_eq!(resolve(d.path(), root, "2").unwrap(), "100-aaaa");
        assert_eq!(resolve(d.path(), root, "100").unwrap(), "100-aaaa");
        assert!(resolve(d.path(), root, "9").is_err());

        let s = Session::load(d.path(), "100-aaaa").unwrap();
        assert_eq!(s.messages[0].content.as_deref(), Some("older"));
        assert_eq!(s.completion_tokens, 2);
    }

    #[test]
    fn titles() {
        let m = vec![Message::user("(For context, I ran ls)"), Message::user("fix   the\nbug ".repeat(10))];
        let t = title_from(&m);
        assert!(t.starts_with("fix the bug fix"));
        assert!(t.ends_with('…'));
        assert_eq!(t.chars().count(), 60);
    }
}
