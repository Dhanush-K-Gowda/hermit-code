//! The agent loop: model ↔ tools, then verify-and-fix until the project's checks pass.

use crate::config::Settings;
use crate::context;
use crate::llm::{Client, Message};
use crate::tools::{self, shell, ToolCtx, UndoLog};
use crate::{ui, verify};
use anyhow::Result;
use serde_json::Value;
use std::future::Future;
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
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
}

const VERIFY_TIMEOUT: Duration = Duration::from_secs(900);
const KEEP_RECENT: usize = 8;

/// Run `f` unless Ctrl-C arrives first.
async fn cancellable<F: Future>(f: F) -> Option<F::Output> {
    tokio::select! {
        r = f => Some(r),
        _ = tokio::signal::ctrl_c() => None,
    }
}

impl Agent {
    pub fn new(settings: Settings) -> Result<Self> {
        let mut client = Client::new(&settings.base_url, settings.api_key.clone(), settings.model.clone())?;
        client.fallback_models = settings.fallback_models.clone();
        client.temperature = settings.temperature;
        client.max_tokens = settings.max_tokens;
        let tools = ToolCtx {
            root: settings.root.clone(),
            yolo: settings.yolo,
            allow: settings.allow_commands.clone(),
            command_timeout: Duration::from_secs(settings.command_timeout_secs),
            undo: UndoLog::default(),
            changed: false,
        };
        let mut a = Agent {
            client,
            settings,
            messages: Vec::new(),
            tools,
            tool_defs: tools::definitions(),
            prompt_tokens: 0,
            completion_tokens: 0,
        };
        a.reset();
        Ok(a)
    }

    /// Clear the conversation, keeping only a fresh system prompt.
    pub fn reset(&mut self) {
        self.messages.clear();
        self.messages.push(Message::system(self.system_prompt()));
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
             - Make focused changes with edit_file (exact, unique `old` text copied from read_file output, without the line-number prefix). Use write_file only for new files or full rewrites.\n\
             - Use run_command to build, test and inspect. Commands must be non-interactive.\n\
             - After editing, build and run the relevant tests yourself and fix any failures before finishing.\n\
             - Paths are relative to the workspace root. Do not touch files outside it.\n\
             - When done, reply with a short summary of what changed and how it was verified. Do not call tools in that final reply.\n\
             - Be concise.\n\n{verify}\n",
            os = std::env::consts::OS,
        );
        for name in ["HERMIT.md", "AGENTS.md", "CLAUDE.md"] {
            if let Ok(s) = std::fs::read_to_string(self.settings.root.join(name)) {
                p.push_str(&format!("\nProject instructions from {name}:\n{}\n", context::truncate_middle(&s, 8 * 1024, 0)));
                break;
            }
        }
        p
    }

    /// Handle one user request end to end, including verification.
    pub async fn run_turn(&mut self, input: &str) -> Result<Outcome> {
        self.tools.undo.begin_turn();
        self.tools.changed = false;
        self.messages.push(Message::user(input));
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
            ui::tool_header("verify", &format!("$ {cmd}"));
            let Some(out) = cancellable(shell::run(&self.settings.root, &cmd, VERIFY_TIMEOUT, false)).await else {
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
                println!("    {}", ui::dim(l));
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

    /// Model ↔ tool loop until the model stops calling tools.
    async fn steps(&mut self) -> Result<Outcome> {
        for _ in 0..self.settings.max_steps {
            if context::compact(&mut self.messages, self.settings.context_budget_tokens, KEEP_RECENT) {
                ui::notice("context compacted");
            }
            let mut printed = false;
            let mut on_text = |t: &str| {
                ui::stream_text(t);
                printed = true;
            };
            let res = cancellable(self.client.chat(&self.messages, &self.tool_defs, &mut on_text)).await;
            if printed {
                println!();
            }
            let Some(res) = res else {
                ui::notice("cancelled");
                return Ok(Outcome::Cancelled);
            };
            let c = res?;
            if let Some(u) = c.usage {
                self.prompt_tokens += u.prompt_tokens;
                self.completion_tokens += u.completion_tokens;
            }
            let calls = c.message.tool_calls.clone();
            if c.message.content.is_none() && calls.is_empty() {
                ui::notice(&format!("{} returned an empty response", c.model));
                return Ok(Outcome::Done);
            }
            self.messages.push(c.message);
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
