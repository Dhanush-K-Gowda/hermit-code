//! Tool registry: static enum dispatch, JSON schemas, and undo snapshots.

pub mod fs;
pub mod memory;
pub mod search;
pub mod shell;
pub mod todo;

use crate::safety::{self, Approval};
use crate::ui;
use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tool {
    ReadFile,
    WriteFile,
    EditFile,
    ListDir,
    Glob,
    Grep,
    RunCommand,
    TodoWrite,
    Remember,
}

impl Tool {
    pub const ALL: [Tool; 9] = [
        Tool::ReadFile,
        Tool::WriteFile,
        Tool::EditFile,
        Tool::ListDir,
        Tool::Glob,
        Tool::Grep,
        Tool::RunCommand,
        Tool::TodoWrite,
        Tool::Remember,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Tool::ReadFile => "read_file",
            Tool::WriteFile => "write_file",
            Tool::EditFile => "edit_file",
            Tool::ListDir => "list_dir",
            Tool::Glob => "glob",
            Tool::Grep => "grep",
            Tool::RunCommand => "run_command",
            Tool::TodoWrite => "todo_write",
            Tool::Remember => "remember",
        }
    }

    /// Tools that change files; unavailable in plan mode.
    pub fn mutates(self) -> bool {
        matches!(self, Tool::WriteFile | Tool::EditFile | Tool::Remember)
    }

    pub fn from_name(n: &str) -> Option<Tool> {
        Tool::ALL.into_iter().find(|t| t.name() == n)
    }

    fn schema(self) -> (&'static str, Value) {
        match self {
            Tool::ReadFile => (
                "Read a text file. Returns numbered lines. Use offset/limit for large files.",
                json!({"type":"object","properties":{
                    "path":{"type":"string","description":"Path relative to the workspace root"},
                    "offset":{"type":"integer","description":"1-based line to start from"},
                    "limit":{"type":"integer","description":"Max lines to return (default 2000)"}},
                    "required":["path"]}),
            ),
            Tool::WriteFile => (
                "Create a file or overwrite it entirely. Prefer edit_file for changes to existing files.",
                json!({"type":"object","properties":{
                    "path":{"type":"string"},
                    "content":{"type":"string","description":"Full file content"}},
                    "required":["path","content"]}),
            ),
            Tool::EditFile => (
                "Replace an exact string in a file. `old` must match exactly once (including whitespace) unless replace_all is true. Read the file first.",
                json!({"type":"object","properties":{
                    "path":{"type":"string"},
                    "old":{"type":"string","description":"Exact existing text to replace"},
                    "new":{"type":"string","description":"Replacement text"},
                    "replace_all":{"type":"boolean","description":"Replace every occurrence"}},
                    "required":["path","old","new"]}),
            ),
            Tool::ListDir => (
                "List entries of a directory (non-recursive). Directories end with '/'.",
                json!({"type":"object","properties":{
                    "path":{"type":"string","description":"Directory, default '.'"}}}),
            ),
            Tool::Glob => (
                "Find files by glob pattern (e.g. '**/*.rs'), respecting .gitignore.",
                json!({"type":"object","properties":{
                    "pattern":{"type":"string"},
                    "path":{"type":"string","description":"Directory to search, default '.'"}},
                    "required":["pattern"]}),
            ),
            Tool::Grep => (
                "Search file contents with a regex, respecting .gitignore. Returns file:line: text.",
                json!({"type":"object","properties":{
                    "pattern":{"type":"string","description":"Rust regex syntax"},
                    "path":{"type":"string","description":"Directory or file, default '.'"},
                    "glob":{"type":"string","description":"Only files matching this glob, e.g. '*.ts'"},
                    "ignore_case":{"type":"boolean"}},
                    "required":["pattern"]}),
            ),
            Tool::RunCommand => (
                "Run a shell command (sh -c) in the workspace root. Returns exit code and combined stdout/stderr (truncated if long). Use for builds, tests, git, etc. Non-interactive only.",
                json!({"type":"object","properties":{
                    "command":{"type":"string"},
                    "timeout_secs":{"type":"integer","description":"Default 120"}},
                    "required":["command"]}),
            ),
            Tool::TodoWrite => (
                "Create or update your task checklist for multi-step work. Send the FULL list each time (it replaces the previous one). Keep exactly one item in_progress while working, and mark items completed as soon as they're done.",
                json!({"type":"object","properties":{
                    "todos":{"type":"array","items":{"type":"object","properties":{
                        "content":{"type":"string","description":"Short imperative description"},
                        "status":{"type":"string","enum":["pending","in_progress","completed"]}},
                        "required":["content","status"]}}},
                    "required":["todos"]}),
            ),
            Tool::Remember => (
                "Save a durable fact for future sessions (build/test commands, conventions, gotchas, user preferences). One short sentence. Don't save task-specific or temporary details.",
                json!({"type":"object","properties":{
                    "fact":{"type":"string"},
                    "scope":{"type":"string","enum":["project","user"],"description":"project (default): this repo's HERMIT.md. user: preferences that apply to every project."}},
                    "required":["fact"]}),
            ),
        }
    }
}

/// All tool definitions in OpenAI `tools` format.
pub fn definitions() -> Value {
    definitions_for(false)
}

/// Tool definitions; in plan mode only the read-only tools are offered.
pub fn definitions_for(plan: bool) -> Value {
    Value::Array(
        Tool::ALL
            .into_iter()
            .filter(|t| !(plan && t.mutates()))
            .map(|t| {
                let (desc, params) = t.schema();
                json!({"type":"function","function":{"name":t.name(),"description":desc,"parameters":params}})
            })
            .collect(),
    )
}

/// Original file contents captured before the first edit in each turn.
#[derive(Default)]
pub struct UndoLog {
    turns: Vec<HashMap<PathBuf, Option<String>>>,
}

impl UndoLog {
    const MAX_TURNS: usize = 10;

    pub fn begin_turn(&mut self) {
        if self.turns.last().is_some_and(|t| t.is_empty()) {
            return;
        }
        self.turns.push(HashMap::new());
        if self.turns.len() > Self::MAX_TURNS {
            self.turns.remove(0);
        }
    }

    fn record(&mut self, path: &Path) {
        if self.turns.is_empty() {
            self.turns.push(HashMap::new());
        }
        let turn = self.turns.last_mut().unwrap();
        turn.entry(path.to_path_buf()).or_insert_with(|| std::fs::read_to_string(path).ok());
    }

    /// Restore the most recent turn that changed files. Returns the restored paths.
    pub fn undo(&mut self) -> Result<Vec<PathBuf>> {
        while let Some(turn) = self.turns.pop() {
            if turn.is_empty() {
                continue;
            }
            let mut paths = Vec::new();
            for (p, old) in turn {
                match old {
                    Some(c) => std::fs::write(&p, c)?,
                    None => {
                        let _ = std::fs::remove_file(&p);
                    }
                }
                paths.push(p);
            }
            return Ok(paths);
        }
        Ok(Vec::new())
    }
}

pub struct ToolCtx {
    pub root: PathBuf,
    pub yolo: bool,
    pub allow: Vec<String>,
    pub command_timeout: Duration,
    pub undo: UndoLog,
    /// Set when a tool modified files during the current turn.
    pub changed: bool,
    pub todos: Vec<todo::Todo>,
    /// Directory for user-scope memory (~/.config/hermit).
    pub user_dir: Option<PathBuf>,
    /// Read-only mode: file changes are refused, only plan-safe commands run.
    pub plan_mode: bool,
}

impl ToolCtx {
    pub fn new(root: PathBuf) -> Self {
        ToolCtx {
            root,
            yolo: false,
            allow: Vec::new(),
            command_timeout: Duration::from_secs(120),
            undo: UndoLog::default(),
            changed: false,
            todos: Vec::new(),
            user_dir: None,
            plan_mode: false,
        }
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
}
#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}
#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old: String,
    new: String,
    #[serde(default)]
    replace_all: bool,
}
#[derive(Deserialize)]
struct PathArg {
    path: Option<String>,
}
#[derive(Deserialize)]
struct GlobArgs {
    pattern: String,
    path: Option<String>,
}
#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    path: Option<String>,
    glob: Option<String>,
    #[serde(default)]
    ignore_case: bool,
}
#[derive(Deserialize)]
struct RememberArgs {
    fact: String,
    scope: Option<String>,
}
#[derive(Deserialize)]
struct CmdArgs {
    command: String,
    timeout_secs: Option<u64>,
}

fn parse<T: for<'de> Deserialize<'de>>(args: &str) -> Result<T> {
    serde_json::from_str(args).map_err(|e| anyhow!("invalid arguments: {e}. Arguments must be a JSON object matching the tool schema."))
}

/// Execute a tool call. Errors are returned as text for the model, never propagated.
pub async fn execute(ctx: &mut ToolCtx, name: &str, args: &str) -> String {
    match execute_inner(ctx, name, args).await {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("Error: {e:#}");
            println!("  {}", ui::red(&format!("└ {msg}")));
            msg
        }
    }
}

async fn execute_inner(ctx: &mut ToolCtx, name: &str, args: &str) -> Result<String> {
    let tool = Tool::from_name(name).ok_or_else(|| {
        anyhow!("unknown tool `{name}`. Available: {}", Tool::ALL.map(Tool::name).join(", "))
    })?;
    if ctx.plan_mode && tool.mutates() {
        anyhow::bail!("plan mode is read-only: `{name}` is unavailable. Describe the change in your plan instead.");
    }
    let root = ctx.root.clone();
    match tool {
        Tool::ReadFile => {
            let a: ReadArgs = parse(args)?;
            ui::tool_header("Read", &a.path);
            let out = fs::read_file(&root, &a.path, a.offset, a.limit)?;
            ui::tool_result(&count(&out, "line"));
            Ok(out)
        }
        Tool::WriteFile => {
            let a: WriteArgs = parse(args)?;
            ui::tool_header("Write", &a.path);
            ctx.undo.record(&safety::confine(&root, &a.path)?);
            let (rel, old) = fs::write_file(&root, &a.path, &a.content)?;
            ctx.changed = true;
            ui::diff(&rel, old.as_deref().unwrap_or(""), &a.content);
            Ok(format!("{} {rel} ({} bytes)", if old.is_some() { "Overwrote" } else { "Created" }, a.content.len()))
        }
        Tool::EditFile => {
            let a: EditArgs = parse(args)?;
            ui::tool_header("Edit", &a.path);
            let p = safety::confine(&root, &a.path)?;
            if p.is_file() {
                ctx.undo.record(&p);
            }
            let (rel, before, after) = fs::edit_file(&root, &a.path, &a.old, &a.new, a.replace_all)?;
            ctx.changed = true;
            ui::diff(&rel, &before, &after);
            Ok(format!("Edited {rel}"))
        }
        Tool::ListDir => {
            let a: PathArg = parse(args)?;
            let p = a.path.as_deref().unwrap_or(".");
            ui::tool_header("List", p);
            let out = fs::list_dir(&root, p)?;
            ui::tool_result(&count(&out, "entry"));
            Ok(out)
        }
        Tool::Glob => {
            let a: GlobArgs = parse(args)?;
            ui::tool_header("Glob", &a.pattern);
            let out = search::glob(&root, &a.pattern, a.path.as_deref())?;
            ui::tool_result(&count(&out, "file"));
            Ok(out)
        }
        Tool::Grep => {
            let a: GrepArgs = parse(args)?;
            ui::tool_header("Grep", &a.pattern);
            let out = search::grep(&root, &a.pattern, a.path.as_deref(), a.glob.as_deref(), a.ignore_case)?;
            ui::tool_result(&count(&out, "match"));
            Ok(out)
        }
        Tool::TodoWrite => {
            let v: Value = parse(args)?;
            ctx.todos = todo::parse(&v)?;
            ui::tool_header("Todo", &todo::progress(&ctx.todos));
            todo::render(&ctx.todos);
            Ok(todo::for_model(&ctx.todos))
        }
        Tool::Remember => {
            let a: RememberArgs = parse(args)?;
            ui::tool_header("Remember", &a.fact);
            let (path, label) = match a.scope.as_deref() {
                Some("user") => match &ctx.user_dir {
                    Some(d) => (d.join(memory::FILE), format!("~/.config/hermit/{}", memory::FILE)),
                    None => anyhow::bail!("no user config directory available"),
                },
                _ => (root.join(memory::FILE), memory::FILE.to_string()),
            };
            let added = memory::remember(&path, &a.fact)?;
            ui::tool_result(&if added { format!("saved to {label}") } else { "already remembered".into() });
            Ok(if added { format!("Saved to {label}.") } else { "Already in memory.".into() })
        }
        Tool::RunCommand => {
            let a: CmdArgs = parse(args)?;
            ui::tool_header("Run", &a.command);
            if ctx.plan_mode && !safety::is_plan_safe(&a.command) {
                ui::tool_result("skipped: not read-only (plan mode)");
                return Ok("Plan mode is read-only and this command isn't on the read-only allowlist. Describe it in your plan instead.".into());
            }
            if !ctx.yolo && !safety::is_allowed(&a.command, &ctx.allow) {
                match safety::ask(&a.command) {
                    Approval::Yes => {}
                    Approval::Always => ctx.allow.push(a.command.trim().to_string()),
                    Approval::No => return Ok("The user declined to run this command. Ask them or try a different approach.".into()),
                }
            }
            let timeout = a.timeout_secs.map_or(ctx.command_timeout, Duration::from_secs);
            let out = shell::run(&root, &a.command, timeout, true).await?;
            let status = out.for_model();
            let first = status.lines().next().unwrap_or("");
            let first = first.trim_matches(['[', ']']);
            if out.success() {
                ui::tool_result(first);
            } else {
                println!("  {}", ui::red(&format!("└ {first}")));
            }
            // Anything beyond the read/build/test allowlist (sed -i, codegen, …) may have changed files.
            ctx.changed |= !safety::is_allowed(&a.command, &[]);
            Ok(status)
        }
    }
}

/// "12 lines", "1 match", "no matches" — a short result summary for the terminal.
fn count(out: &str, noun: &str) -> String {
    let first = out.lines().next().unwrap_or("");
    if first.starts_with("no ") || first.starts_with('(') {
        return first.trim_matches(['(', ')']).to_string();
    }
    let n = out.lines().filter(|l| !l.starts_with('…')).count();
    let plural = match noun {
        "entry" => "entries",
        "match" => "matches",
        _ => "",
    };
    match (n, plural) {
        (1, _) => format!("1 {noun}"),
        (_, "") => format!("{n} {noun}s"),
        (_, p) => format!("{n} {p}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_cover_all_tools() {
        let d = definitions();
        assert_eq!(d.as_array().unwrap().len(), Tool::ALL.len());
        for t in Tool::ALL {
            assert_eq!(Tool::from_name(t.name()), Some(t));
        }
    }

    #[tokio::test]
    async fn undo_restores_and_deletes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "orig").unwrap();
        let mut ctx = ToolCtx { yolo: true, ..ToolCtx::new(root.clone()) };
        ctx.undo.begin_turn();
        execute(&mut ctx, "edit_file", r#"{"path":"a.txt","old":"orig","new":"changed"}"#).await;
        execute(&mut ctx, "write_file", r#"{"path":"b.txt","content":"new"}"#).await;
        assert!(ctx.changed);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "changed");
        let restored = ctx.undo.undo().unwrap();
        assert_eq!(restored.len(), 2);
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "orig");
        assert!(!root.join("b.txt").exists());
    }

    #[tokio::test]
    async fn todo_and_remember_tools() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        let mut ctx = ToolCtx { user_dir: Some(root.join("user")), ..ToolCtx::new(root.clone()) };
        let r = execute(&mut ctx, "todo_write", r#"{"todos":[{"content":"a","status":"completed"},{"content":"b","status":"in_progress"}]}"#).await;
        assert!(r.starts_with("Todo list saved (1/2"), "{r}");
        assert_eq!(ctx.todos.len(), 2);
        assert!(!ctx.changed);

        let r = execute(&mut ctx, "remember", r#"{"fact":"tests: cargo test -q"}"#).await;
        assert_eq!(r, "Saved to HERMIT.md.");
        assert!(std::fs::read_to_string(root.join("HERMIT.md")).unwrap().contains("- tests: cargo test -q"));
        execute(&mut ctx, "remember", r#"{"fact":"prefers tabs","scope":"user"}"#).await;
        assert!(root.join("user/HERMIT.md").exists());
        assert!(!ctx.changed);
    }

    #[tokio::test]
    async fn plan_mode_blocks_changes() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        std::fs::write(root.join("a.txt"), "x").unwrap();
        let mut ctx = ToolCtx { plan_mode: true, ..ToolCtx::new(root.clone()) };
        let r = execute(&mut ctx, "write_file", r#"{"path":"a.txt","content":"y"}"#).await;
        assert!(r.contains("plan mode is read-only"));
        let r = execute(&mut ctx, "run_command", r#"{"command":"touch b.txt"}"#).await;
        assert!(r.starts_with("Plan mode is read-only"));
        assert!(!root.join("b.txt").exists());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "x");
        assert!(execute(&mut ctx, "read_file", r#"{"path":"a.txt"}"#).await.contains("x"));
        let names: Vec<_> = definitions_for(true).as_array().unwrap().iter().map(|d| d["function"]["name"].as_str().unwrap().to_string()).collect();
        assert!(!names.iter().any(|n| n == "edit_file" || n == "write_file" || n == "remember"));
        assert!(names.iter().any(|n| n == "todo_write"));
    }

    #[tokio::test]
    async fn bad_args_return_error_text() {
        let d = tempfile::tempdir().unwrap();
        let mut ctx = ToolCtx { yolo: true, ..ToolCtx::new(d.path().canonicalize().unwrap()) };
        assert!(execute(&mut ctx, "read_file", "not json").await.starts_with("Error: invalid arguments"));
        assert!(execute(&mut ctx, "nope", "{}").await.starts_with("Error: unknown tool"));
    }
}
