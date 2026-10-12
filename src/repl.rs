//! Interactive line-based REPL.

use crate::agent::{Agent, Outcome};
use crate::complete::SlashHelper;
use crate::config;
use crate::session::{self, Session};
use crate::tools::memory;
use crate::ui;
use anyhow::Result;
use rustyline::error::ReadlineError;
use std::time::{Duration, Instant};

/// Line editor with slash-command hints and completion.
type Editor = rustyline::Editor<SlashHelper, rustyline::history::FileHistory>;

pub fn banner(agent: &Agent) {
    let s = &agent.settings;
    let verify = agent.verify_cmd().unwrap_or_else(|| ui::dim("none detected"));
    let mut rows = vec![("model", s.model.clone()), ("dir", ui::tilde(&s.root)), ("verify", verify)];
    if agent.plan_mode {
        rows.push(("mode", ui::accent("plan (read-only)")));
    }
    ui::banner(&rows);
    println!();
    if s.yolo {
        ui::notice("yolo mode: commands run without asking");
    }
    if s.api_key.is_none() && s.base_url.contains("openrouter.ai") {
        ui::notice("OPENROUTER_API_KEY is not set — get a free key at https://openrouter.ai/keys");
    }
}

/// Run one request and print the turn footer.
pub async fn turn(agent: &mut Agent, input: &str) -> Result<Outcome> {
    let start = Instant::now();
    let calls = agent.model_calls;
    let tokens = agent.prompt_tokens + agent.completion_tokens;
    let res = agent.run_turn(input).await;
    ui::footer(
        agent.model_calls - calls,
        agent.prompt_tokens + agent.completion_tokens - tokens,
        start.elapsed(),
    );
    res
}

pub async fn run(agent: &mut Agent, resume_arg: Option<&str>) -> Result<()> {
    banner(agent);
    if let Some(r) = resume_arg {
        resume(agent, r);
        println!();
    }
    println!("{}", ui::dim("  type / for commands · !cmd runs a shell command · ctrl-c cancels\n"));
    let cfg = rustyline::Config::builder().completion_type(rustyline::CompletionType::List).build();
    let mut rl = Editor::with_config(cfg)?;
    let mut models = vec![agent.settings.model.clone()];
    models.extend(agent.settings.fallback_models.iter().cloned());
    rl.set_helper(Some(SlashHelper { models }));
    let history = config::config_dir().map(|d| d.join("history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }

    loop {
        let mut input = String::new();
        let mut prompt = if agent.plan_mode { "plan ❯ " } else { "❯ " };
        loop {
            match rl.readline(&ui::accent(prompt)) {
                Ok(line) => {
                    if let Some(l) = line.strip_suffix('\\') {
                        input.push_str(l);
                        input.push('\n');
                        prompt = "… ";
                        continue;
                    }
                    input.push_str(&line);
                    break;
                }
                Err(ReadlineError::Interrupted) => {
                    input.clear();
                    break;
                }
                Err(ReadlineError::Eof) => return save(&mut rl, &history),
                Err(e) => return Err(e.into()),
            }
        }
        let input = input.trim();
        if input.is_empty() {
            continue;
        }
        let _ = rl.add_history_entry(input);

        if let Some(cmd) = input.strip_prefix('!') {
            shell(agent, cmd.trim()).await;
            agent.save_session();
            println!();
            continue;
        }

        if let Some(rest) = input.strip_prefix("/plan").filter(|r| r.is_empty() || r.starts_with(' ')) {
            match rest.trim() {
                "off" => set_plan(agent, false),
                "on" => set_plan(agent, true),
                "" => set_plan(agent, !agent.plan_mode),
                task => {
                    set_plan(agent, true);
                    run_input(agent, &mut rl, task).await;
                    agent.save_session();
                }
            }
            println!();
            continue;
        }

        if let Some(cmd) = input.strip_prefix('/') {
            let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
            match name {
                "exit" | "quit" | "q" => return save(&mut rl, &history),
                "help" | "h" => println!("{}", crate::complete::help_text()),
                "clear" => {
                    agent.save_session();
                    agent.reset();
                    println!("{}", ui::dim("new conversation started"));
                }
                "todo" | "todos" => {
                    ui::tool_header("Todo", &crate::tools::todo::progress(&agent.tools.todos));
                    crate::tools::todo::render(&agent.tools.todos);
                }
                "remember" if arg.is_empty() => ui::error("usage: /remember <fact>"),
                "remember" => {
                    let path = agent.settings.root.join(memory::FILE);
                    match memory::remember(&path, arg) {
                        Ok(true) => {
                            ui::success(&format!("saved to {}", memory::FILE));
                            agent.messages.push(crate::llm::Message::user(format!("(Note: I saved this to project memory: {arg})")));
                        }
                        Ok(false) => println!("{}", ui::dim("already remembered")),
                        Err(e) => ui::error(&format!("{e:#}")),
                    }
                }
                "memory" => show_memory(agent),
                "sessions" => list_sessions(agent),
                "resume" => resume(agent, arg),
                "compact" => {
                    let before = crate::context::estimate_tokens(&agent.messages);
                    agent.compact(true).await;
                    if crate::context::estimate_tokens(&agent.messages) >= before {
                        println!("{}", ui::dim("nothing to compact yet"));
                    }
                }
                "model" if arg.is_empty() => {
                    let active = agent.client.active_model();
                    if active == agent.client.model {
                        println!("{active}");
                    } else {
                        println!("{active} {}", ui::dim(&format!("(fallback; {} was busy)", agent.client.model)));
                    }
                }
                "model" => {
                    agent.set_model(arg);
                    println!("{}", ui::dim(&format!("model set to {arg}")));
                }
                "models" => match agent.client.list_free_models(arg != "all").await {
                    Ok(ms) => ms.iter().for_each(|m| println!("  {m}")),
                    Err(e) => ui::error(&format!("{e:#}")),
                },
                "undo" => match agent.tools.undo.undo() {
                    Ok(p) if p.is_empty() => println!("{}", ui::dim("nothing to undo")),
                    Ok(p) => {
                        for f in p {
                            println!("  {} {}", ui::yellow("restored"), crate::safety::rel(&agent.settings.root, &f));
                        }
                        agent.messages.push(crate::llm::Message::user(
                            "(Note: the user reverted your file changes from the previous turn with /undo.)",
                        ));
                    }
                    Err(e) => ui::error(&format!("{e:#}")),
                },
                "verify" => {
                    if let Err(e) = agent.verify_and_fix().await {
                        ui::error(&format!("{e:#}"));
                    }
                }
                "cost" => println!(
                    "{}",
                    ui::dim(&format!(
                        "prompt {} · completion {} tokens · context ≈{} tokens",
                        agent.prompt_tokens,
                        agent.completion_tokens,
                        crate::context::estimate_tokens(&agent.messages)
                    ))
                ),
                _ => ui::error(&format!("unknown command /{name} (try /help)")),
            }
            agent.save_session();
            continue;
        }

        run_input(agent, &mut rl, input).await;
        agent.save_session();
        println!();
    }
}

/// `!cmd`: run a command the user typed. No approval needed; output goes into the
/// conversation so the model can see it on the next turn.
async fn shell(agent: &mut Agent, cmd: &str) {
    if cmd.is_empty() {
        return;
    }
    ui::tool_header("Shell", cmd);
    let run = crate::tools::shell::run(&agent.settings.root, cmd, USER_CMD_TIMEOUT, true);
    let out = tokio::select! {
        r = run => r,
        _ = tokio::signal::ctrl_c() => {
            ui::notice("command cancelled");
            return;
        }
    };
    match out {
        Ok(o) => {
            let status = o.for_model();
            let first = status.lines().next().unwrap_or("").trim_matches(['[', ']']);
            if o.success() {
                ui::tool_result(first);
            } else {
                println!("  {}", ui::red(&format!("└ {first}")));
            }
            agent.messages.push(crate::llm::Message::user(format!(
                "(For context, I ran `{cmd}` in the workspace myself. No action needed unless I ask.)\n{status}"
            )));
        }
        Err(e) => ui::error(&format!("{e:#}")),
    }
}

/// Long enough for builds; Ctrl-C stops it sooner.
const USER_CMD_TIMEOUT: Duration = Duration::from_secs(1800);

fn show_memory(agent: &Agent) {
    let mut any = false;
    let user = config::config_dir().map(|d| d.join(memory::FILE));
    for (label, path) in [("user", user), ("project", Some(agent.settings.root.join(memory::FILE)))] {
        let Some(path) = path else { continue };
        if let Ok(s) = std::fs::read_to_string(&path) {
            any = true;
            println!("{} {}", ui::accent("●"), ui::bold(&format!("{label} memory")) + &ui::dim(&format!(" {}", ui::tilde(&path))));
            for l in s.lines().filter(|l| l.starts_with("- ")) {
                println!("  {l}");
            }
        }
    }
    if !any {
        println!("{}", ui::dim("no memory saved yet — use /remember <fact>, or the agent will save useful facts itself"));
    }
}

fn list_sessions(agent: &Agent) {
    let Some(dir) = &agent.sessions_dir else { return };
    let list = session::list(dir, &agent.settings.root);
    if list.is_empty() {
        println!("{}", ui::dim("no saved sessions for this directory"));
        return;
    }
    for (i, m) in list.iter().take(15).enumerate() {
        let current = if m.id == agent.session_id { ui::accent(" ← current") } else { String::new() };
        println!("  {} {}  {}{}", ui::dim(&format!("{:>2}.", i + 1)), m.title, ui::dim(&format!("{} · {}", session::age(m.updated), m.id)), current);
    }
    println!("{}", ui::dim("  /resume <n> to continue one"));
}

/// Load a saved session into the agent and show a short recap.
pub fn resume(agent: &mut Agent, arg: &str) {
    let Some(dir) = agent.sessions_dir.clone() else { return };
    let loaded = session::resolve(&dir, &agent.settings.root, arg).and_then(|id| Session::load(&dir, &id));
    match loaded {
        Ok(s) => {
            agent.save_session();
            let title = s.title.clone();
            let n = s.messages.len();
            agent.resume(s);
            ui::success(&format!("resumed “{title}” ({n} messages)"));
            if let Some(last) = agent.messages.iter().rev().find(|m| m.role == crate::llm::Role::Assistant).and_then(|m| m.content.as_deref()) {
                let snippet: String = last.chars().take(300).collect();
                println!("  {}", ui::dim(&format!("last reply: {}", snippet.replace('\n', " "))));
            }
            if !agent.tools.todos.is_empty() {
                crate::tools::todo::render(&agent.tools.todos);
            }
        }
        Err(e) => ui::error(&format!("{e:#}")),
    }
}

fn set_plan(agent: &mut Agent, on: bool) {
    agent.set_plan_mode(on);
    if on {
        println!("{}", ui::accent("plan mode on: read-only. Describe what you want; you'll approve the plan before any changes."));
    } else {
        println!("{}", ui::dim("plan mode off"));
    }
}

/// Run a request. In plan mode, ask for approval afterwards: `y` implements the plan,
/// `n` keeps planning, anything else is feedback for a revised plan.
async fn run_input(agent: &mut Agent, rl: &mut Editor, input: &str) {
    let mut input = input.to_string();
    loop {
        println!();
        match turn(agent, &input).await {
            Err(e) => return ui::error(&format!("{e:#}")),
            Ok(Outcome::Done) if agent.plan_mode => {}
            Ok(_) => return,
        }
        println!();
        let question = format!("{} {} ", ui::accent("approve plan?"), ui::dim("[y] implement · [n] keep planning · or type feedback ›"));
        let answer = rl.readline(&question).map(|l| l.trim().to_string()).unwrap_or_default();
        match answer.to_ascii_lowercase().as_str() {
            "y" | "yes" => {
                agent.set_plan_mode(false);
                ui::success("plan approved, implementing");
                input = "The plan is approved. Implement it now, step by step, then verify it works.".into();
            }
            "" | "n" | "no" => {
                println!("{}", ui::dim("staying in plan mode. Keep refining, or /plan off to leave."));
                return;
            }
            _ => input = answer,
        }
    }
}

fn save(rl: &mut Editor, history: &Option<std::path::PathBuf>) -> Result<()> {
    if let Some(h) = history {
        if let Some(d) = h.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = rl.save_history(h);
    }
    Ok(())
}
