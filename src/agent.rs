//! The agent loop: model ↔ tools, then verify-and-fix until the project's checks pass.

use crate::config::{Settings, ToolMode};
use crate::context;
use crate::llm::text_tools::{self, StreamFilter};
use crate::llm::{Client, Message, Role, ToolsUnsupported};
use crate::markdown::Renderer;
use crate::session::{self, Session};
use crate::tools::{self, memory, shell, ToolCtx};
use crate::{ui, verify};
use anyhow::Result;
use serde_json::Value;
use std::cell::Cell;
use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Model finished; nothing needed verifying (no changes, or no check command).
    Done,
    /// Model finished and the check command passed.
    Verified,
    /// Checks still fail after all fix attempts.
    VerifyFailed,
    Cancelled,
    StepLimit,
}

pub struct Agent {
    pub client: Client,
    pub settings: Settings,
    pub messages: Vec<Message>,
    pub tools: ToolCtx,
    tool_defs: Value,
    text_instructions: String,
    plan_defs: Value,
    plan_text_instructions: String,
    /// Read-only planning: only read tools are offered and file changes are refused.
    pub plan_mode: bool,
    /// Send tools as `<tool_call>` text instead of native tool definitions.
    pub text_mode: bool,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    /// Model requests made this session.
    pub model_calls: usize,
    pub session_id: String,
    session_created: u64,
    /// Where sessions are saved; None disables persistence.
    pub sessions_dir: Option<PathBuf>,
}

const VERIFY_TIMEOUT: Duration = Duration::from_secs(900);
const KEEP_RECENT: usize = 8;

const SUMMARY_PROMPT: &str = "You compress the earlier part of a coding-agent session so the work can continue with less context. \
Write a dense summary (under 400 words) with these sections:\n\
Goal: what the user wants.\n\
Done: changes made so far (file paths and what changed).\n\
Findings: exact paths, function names, commands, error messages and decisions that matter.\n\
State: what currently works or fails.\n\
Next: remaining steps.\n\
Use only facts from the transcript. No preamble.";

/// Run `f` unless Ctrl-C arrives first.
async fn cancellable<F: Future>(f: F) -> Option<F::Output> {
    tokio::select! {
        r = f => Some(r),
        _ = tokio::signal::ctrl_c() => None,
    }
}

/// Print streamed reply text through the Markdown renderer, skipping leading whitespace.
fn show(started: &Cell<bool>, md: &mut Renderer, text: &str) {
    if text.is_empty() {
        return;
    }
    if !started.get() {
        let t = text.trim_start();
        if t.is_empty() {
            return;
        }
        started.set(true);
        ui::clear_line();
        md.push(t);
    } else {
        md.push(text);
    }
}

impl Agent {
    pub fn new(settings: Settings) -> Result<Self> {
        let mut client = Client::new(&settings.base_url, settings.api_key.clone(), settings.model.clone())?;
        client.fallback_models = settings.fallback_models.clone();
        client.temperature = settings.temperature;
        client.max_tokens = settings.max_tokens;
        let tools = ToolCtx {
            yolo: settings.yolo,
            allow: settings.allow_commands.clone(),
            command_timeout: Duration::from_secs(settings.command_timeout_secs),
            user_dir: crate::config::config_dir(),
            ..ToolCtx::new(settings.root.clone())
        };
        let tool_defs = tools::definitions();
        let plan_defs = tools::definitions_for(true);
        let mut a = Agent {
            client,
            text_mode: settings.tool_mode == ToolMode::Text,
            settings,
            messages: Vec::new(),
            tools,
            text_instructions: text_tools::instructions(&tool_defs),
            tool_defs,
            plan_text_instructions: text_tools::instructions(&plan_defs),
            plan_defs,
            plan_mode: false,
            prompt_tokens: 0,
            completion_tokens: 0,
            model_calls: 0,
            session_id: String::new(),
            session_created: 0,
            sessions_dir: session::dir(),
        };
        a.reset();
        Ok(a)
    }

    /// Start a new conversation (and a new session) with a fresh system prompt.
    pub fn reset(&mut self) {
        self.messages.clear();
        self.messages.push(Message::system(self.system_prompt()));
        self.tools.todos.clear();
        self.session_id = session::new_id();
        self.session_created = session::now();
    }

    pub fn set_model(&mut self, model: &str) {
        self.client.model = model.to_string();
        self.settings.model = model.to_string();
        self.text_mode = self.settings.tool_mode == ToolMode::Text;
    }

    pub fn set_plan_mode(&mut self, on: bool) {
        self.plan_mode = on;
        self.tools.plan_mode = on;
    }

    pub fn verify_cmd(&self) -> Option<String> {
        self.settings.verify_cmd.clone().or_else(|| verify::detect(&self.settings.root))
    }

    fn system_prompt(&self) -> String {
        let root = self.settings.root.display();
        let verify = self
            .verify_cmd()
            .map(|c| format!("After you finish, the harness automatically runs `{c}` and reports failures back to you."))
            .unwrap_or_else(|| "No automatic check command was detected; run the project's build/tests yourself.".into());
        let mut p = format!(
            "You are hermit, an autonomous coding agent working in a local repository.\n\
             Workspace root: {root}\nOS: {os}\n\n\
             How to work:\n\
             - Explore with list_dir, glob, grep and read_file before changing code. Never guess file contents.\n\
             - For tasks with 3+ steps, plan with todo_write first. Keep exactly one item in_progress and update the list as you finish items.\n\
             - Make focused changes with edit_file (exact, unique `old` text copied from read_file output, without the line-number prefix). Use write_file only for new files or full rewrites.\n\
             - Use run_command to build, test and inspect. Commands must be non-interactive.\n\
             - After editing, build and run the relevant tests yourself and fix any failures before finishing.\n\
             - When you learn something durable about this project (build/test commands, conventions, gotchas) or the user states a lasting preference, save it with remember. Don't save temporary details.\n\
             - Paths are relative to the workspace root. Do not touch files outside it.\n\
             - When done, reply with a short summary of what changed and how it was verified. Do not call tools in that final reply.\n\
             - Be concise.\n\n{verify}\n",
            os = std::env::consts::OS,
        );
        let mut sources: Vec<(String, PathBuf)> = Vec::new();
        if let Some(d) = &self.tools.user_dir {
            sources.push(("User memory (applies to all projects)".into(), d.join(memory::FILE)));
        }
        sources.push((format!("Project memory ({})", memory::FILE), self.settings.root.join(memory::FILE)));
        if let Some(name) = ["AGENTS.md", "CLAUDE.md"].into_iter().find(|n| self.settings.root.join(n).is_file()) {
            sources.push((format!("Project instructions ({name})"), self.settings.root.join(name)));
        }
        for (label, path) in sources {
            if let Ok(s) = std::fs::read_to_string(&path) {
                if !s.trim().is_empty() {
                    p.push_str(&format!("\n# {label}\n{}\n", context::truncate_middle(&s, 8 * 1024, 0)));
                }
            }
        }
        p
    }

    // ── sessions ────────────────────────────────────────────────────────────

    pub fn to_session(&self) -> Option<Session> {
        let convo = &self.messages[1..];
        if !convo.iter().any(|m| m.role == Role::User) {
            return None;
        }
        Some(Session {
            id: self.session_id.clone(),
            root: self.settings.root.clone(),
            model: self.settings.model.clone(),
            title: session::title_from(convo),
            created: self.session_created,
            updated: session::now(),
            prompt_tokens: self.prompt_tokens,
            completion_tokens: self.completion_tokens,
            todos: self.tools.todos.clone(),
            messages: convo.to_vec(),
        })
    }

    /// Persist the current conversation. Failures are reported, never fatal.
    pub fn save_session(&self) {
        let (Some(dir), Some(s)) = (&self.sessions_dir, self.to_session()) else { return };
        if let Err(e) = s.save(dir) {
            ui::notice(&format!("could not save session: {e:#}"));
        }
        session::prune(dir);
    }

    /// Replace the conversation with a saved session (system prompt is rebuilt fresh).
    pub fn resume(&mut self, s: Session) {
        self.reset();
        self.session_id = s.id;
        self.session_created = s.created;
        self.messages.extend(s.messages);
        self.tools.todos = s.todos;
        self.prompt_tokens = s.prompt_tokens;
        self.completion_tokens = s.completion_tokens;
    }

    // ── turns ───────────────────────────────────────────────────────────────

    /// Handle one user request end to end, including verification.
    pub async fn run_turn(&mut self, input: &str) -> Result<Outcome> {
        self.tools.undo.begin_turn();
        self.tools.changed = false;
        if self.plan_mode {
            self.messages.push(Message::user(format!(
                "{input}\n\n[Plan mode: investigate with read-only tools only and do not modify anything. \
                 Finish with a concise, concrete plan: the files to change, what to change in each, and how you will verify it. \
                 The user will approve the plan before you implement it.]"
            )));
        } else {
            self.messages.push(Message::user(input));
        }
        let outcome = self.steps().await?;
        if outcome != Outcome::Done || !self.tools.changed || !self.settings.verify {
            return Ok(outcome);
        }
        self.verify_and_fix().await
    }

    /// Run the check command; on failure, hand the output to the model and let it fix things.
    pub async fn verify_and_fix(&mut self) -> Result<Outcome> {
        let Some(cmd) = self.verify_cmd() else {
            ui::notice("no verify command detected (set verify_cmd in .hermit.toml)");
            return Ok(Outcome::Done);
        };
        let max = self.settings.max_fix_attempts;
        for attempt in 0..=max {
            ui::tool_header("Verify", &cmd);
            let run = shell::run(&self.settings.root, &cmd, VERIFY_TIMEOUT, false);
            let Some(out) = cancellable(ui::spin("running checks", &Cell::new(false), run)).await else {
                ui::notice("verification cancelled");
                return Ok(Outcome::Cancelled);
            };
            let out = out?;
            if out.success() {
                ui::success("checks passed");
                return Ok(Outcome::Verified);
            }
            let tail: Vec<&str> = out.output.lines().rev().take(15).collect();
            for l in tail.into_iter().rev() {
                println!("  {}", ui::dim(&format!("│ {l}")));
            }
            if attempt == max {
                ui::error(&format!("checks still failing after {max} fix attempts"));
                return Ok(Outcome::VerifyFailed);
            }
            ui::notice(&format!("checks failed — asking the model to fix (attempt {}/{max})", attempt + 1));
            self.tools.changed = false;
            self.messages.push(Message::user(format!(
                "Automatic verification `{cmd}` failed:\n\n{}\n\nFix the root cause, re-run the relevant checks, then give a short summary.",
                out.for_model()
            )));
            match self.steps().await? {
                Outcome::Done => {}
                other => return Ok(other),
            }
            if !self.tools.changed {
                ui::error("the model made no further changes; checks still failing");
                return Ok(Outcome::VerifyFailed);
            }
        }
        unreachable!()
    }

    /// Shrink the conversation when it exceeds the token budget (or always, if `force`).
    /// Older turns are summarized by the model; recent turns are kept verbatim. Falls back
    /// to eliding old tool output if summarizing isn't possible.
    pub async fn compact(&mut self, force: bool) {
        let budget = self.settings.context_budget_tokens;
        let before = context::estimate_tokens(&self.messages);
        if !force && before <= budget {
            return;
        }
        let keep = if force { budget / 5 } else { budget * 2 / 5 };
        if let Some(split) = context::summary_split(&self.messages, keep) {
            let mut req = vec![Message::system(SUMMARY_PROMPT)];
            let mut body = format!("Transcript:\n\n{}", context::transcript(&self.messages[1..split], budget * 2));
            if !self.tools.todos.is_empty() {
                body.push_str(&format!("\nCurrent todo list: {}", serde_json::to_string(&self.tools.todos).unwrap_or_default()));
            }
            req.push(Message::user(body));
            let mut quiet = |_: &str| {};
            let chat = self.client.chat(&req, None, &mut quiet);
            match cancellable(ui::spin("compacting context", &Cell::new(false), chat)).await {
                Some(Ok(c)) if c.message.content.as_deref().is_some_and(|s| !s.trim().is_empty()) => {
                    if let Some(u) = c.usage {
                        self.prompt_tokens += u.prompt_tokens;
                        self.completion_tokens += u.completion_tokens;
                    }
                    let summary = c.message.content.unwrap_or_default();
                    self.messages.splice(
                        1..split,
                        [Message::user(format!("[Summary of the earlier conversation; older messages were compacted]\n{}", summary.trim()))],
                    );
                }
                Some(Ok(_)) => ui::notice("summary was empty; trimming old tool output instead"),
                Some(Err(e)) => ui::notice(&format!("summary failed ({e:#}); trimming old tool output instead")),
                None => ui::notice("compaction cancelled; trimming old tool output instead"),
            }
        }
        context::compact(&mut self.messages, budget, KEEP_RECENT);
        let after = context::estimate_tokens(&self.messages);
        if after < before {
            ui::notice(&format!("context compacted: ~{before} → ~{after} tokens"));
        }
    }

    /// Model ↔ tool loop until the model stops calling tools.
    async fn steps(&mut self) -> Result<Outcome> {
        for _ in 0..self.settings.max_steps {
            self.compact(false).await;

            let (defs, instructions) = if self.plan_mode {
                (&self.plan_defs, &self.plan_text_instructions)
            } else {
                (&self.tool_defs, &self.text_instructions)
            };
            let converted;
            let (msgs, tool_defs) = if self.text_mode {
                converted = text_tools::to_text_protocol(&self.messages, instructions);
                (converted.as_slice(), None)
            } else {
                (self.messages.as_slice(), Some(defs))
            };

            let started = Cell::new(false);
            let mut filter = StreamFilter::default();
            let mut md = Renderer::new();
            let mut on_text = |t: &str| show(&started, &mut md, &filter.push(t));
            self.model_calls += 1;
            let chat = self.client.chat(msgs, tool_defs, &mut on_text);
            let res = cancellable(ui::spin("thinking", &started, chat)).await;
            show(&started, &mut md, &filter.finish());
            md.finish();
            let Some(res) = res else {
                ui::notice("cancelled");
                return Ok(Outcome::Cancelled);
            };
            let c = match res {
                Ok(c) => c,
                Err(e) if e.is::<ToolsUnsupported>() && self.settings.tool_mode == ToolMode::Auto && !self.text_mode => {
                    ui::notice(&format!("{e} — switching to text-based tool calls"));
                    self.text_mode = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if let Some(u) = c.usage {
                self.prompt_tokens += u.prompt_tokens;
                self.completion_tokens += u.completion_tokens;
            }
            let mut msg = c.message;
            // Text-mode calls, or a native-mode model that wrote tool calls as text anyway.
            if msg.tool_calls.is_empty() {
                if let Some(content) = msg.content.as_deref().filter(|c| c.contains("<tool_call>")) {
                    let (text, calls) = text_tools::parse(content, &format!("t{}_", self.model_calls));
                    if !calls.is_empty() {
                        msg.content = (!text.is_empty()).then_some(text);
                        msg.tool_calls = calls;
                    }
                }
            }
            if msg.content.as_deref().is_none_or(|c| c.trim().is_empty()) && msg.tool_calls.is_empty() {
                ui::notice(&format!("{} returned an empty response", c.model));
                return Ok(Outcome::Done);
            }
            let calls = msg.tool_calls.clone();
            self.messages.push(msg);
            if calls.is_empty() {
                return Ok(Outcome::Done);
            }
            for (i, call) in calls.iter().enumerate() {
                let r = cancellable(tools::execute(&mut self.tools, &call.function.name, &call.function.arguments)).await;
                match r {
                    Some(out) => self.messages.push(Message::tool(&call.id, out)),
                    None => {
                        // Every tool call needs a result to keep the conversation valid.
                        for c in &calls[i..] {
                            self.messages.push(Message::tool(&c.id, "Cancelled by the user."));
                        }
                        ui::notice("cancelled");
                        return Ok(Outcome::Cancelled);
                    }
                }
            }
        }
        ui::notice(&format!("stopped after {} steps (raise max_steps to allow more)", self.settings.max_steps));
        Ok(Outcome::StepLimit)
    }
}
