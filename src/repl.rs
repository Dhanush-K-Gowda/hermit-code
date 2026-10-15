//! Interactive line-based REPL.

use crate::agent::{Agent, Outcome};
use crate::complete::SlashHelper;
use crate::config;
use crate::image::{self, Image};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use crate::session::{self, Session};
use crate::tools::memory;
use crate::ui;
use anyhow::Result;
use rustyline::error::ReadlineError;
use std::time::{Duration, Instant};

/// Clipboard images pasted with Ctrl+V, waiting for the line to be submitted.
#[derive(Clone, Default)]
pub struct Clipboard {
    pending: Arc<Mutex<Vec<(usize, Image)>>>,
    next_id: Arc<AtomicUsize>,
}

impl Clipboard {
    fn next(&self) -> usize {
        self.next_id.fetch_add(1, Ordering::Relaxed) + 1
    }
}

/// Ctrl+V: if the clipboard holds an image, stash it and insert an `[Image #n]` placeholder.
/// Otherwise fall through to the default key behavior.
struct PasteImage(Clipboard);

impl rustyline::ConditionalEventHandler for PasteImage {
    fn handle(&self, _: &rustyline::Event, _: rustyline::RepeatCount, _: bool, _: &rustyline::EventContext) -> Option<rustyline::Cmd> {
        let img = image::from_clipboard().ok().flatten()?;
        let id = self.0.next();
        self.0.pending.lock().ok()?.push((id, img));
        Some(rustyline::Cmd::Insert(1, format!("[Image #{id}]")))
    }
}

/// Resolve attachments in a submitted line: image file paths (typed or dragged in) become
/// `[Image #n]` placeholders, and every placeholder still present is attached in order.
pub fn attach(input: &str, root: &Path, clipboard: &Clipboard) -> (String, Vec<String>) {
    let mut text = input.to_string();
    let mut loaded: Vec<(usize, Image, String)> = Vec::new();
    for (range, path) in image::find_paths(input, root).into_iter().rev() {
        match image::load(&path) {
            Ok(img) => {
                let id = clipboard.next();
                text.replace_range(range, &format!("[Image #{id}]"));
                let name = path.file_name().map_or(String::new(), |n| n.to_string_lossy().into_owned());
                loaded.push((id, img, name));
            }
            Err(e) => ui::error(&format!("{e:#}")),
        }
    }
    let pasted = clipboard.pending.lock().map(|mut p| std::mem::take(&mut *p)).unwrap_or_default();
    loaded.extend(pasted.into_iter().map(|(id, img)| (id, img, "pasted".to_string())));

    let mut found: Vec<(usize, usize)> = loaded
        .iter()
        .enumerate()
        .filter_map(|(i, (id, _, _))| text.find(&format!("[Image #{id}]")).map(|pos| (pos, i)))
        .collect();
    found.sort();
    let mut images = Vec::new();
    for (_, i) in found {
        let (id, img, name) = &loaded[i];
        println!("  {} {} {}", ui::accent("⎘"), ui::bold(&format!("Image #{id}")), ui::dim(&format!("{name} · {}", img.info)));
        images.push(img.data_url.clone());
    }
    (text, images)
}

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
pub async fn turn(agent: &mut Agent, input: &str, images: Vec<String>) -> Result<Outcome> {
    let start = Instant::now();
    let calls = agent.model_calls;
    let tokens = agent.prompt_tokens + agent.completion_tokens;
    let res = agent.run_turn_with_images(input, images).await;
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
    let clipboard = Clipboard::default();
    rl.bind_sequence(
        rustyline::KeyEvent::ctrl('V'),
        rustyline::EventHandler::Conditional(Box::new(PasteImage(clipboard.clone()))),
    );
    let history = config::config_dir().map(|d| d.join("history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }

    // Ctrl-C at the prompt once shows a hint; twice in a row exits.
    let mut interrupted = false;
    loop {
        let mut input = String::new();
        let mut prompt = if agent.plan_mode { "plan ❯ " } else { "❯ " };
        loop {
            match rl.readline(&ui::accent(prompt)) {
                Ok(line) => {
                    interrupted = false;
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
                    if interrupted {
                        return goodbye(agent, &mut rl, &history);
                    }
                    interrupted = true;
                    println!("{}", ui::dim("(press Ctrl-C again to exit)"));
                    input.clear();
                    break;
                }
                Err(ReadlineError::Eof) => return goodbye(agent, &mut rl, &history),
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
                    let (task, images) = attach(task, &agent.settings.root, &clipboard);
                    run_input(agent, &mut rl, &task, images).await;
                    agent.save_session();
                }
            }
            println!();
            continue;
        }

        if let Some(cmd) = input.strip_prefix('/') {
            let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
            match name {
                "exit" | "quit" | "q" => return goodbye(agent, &mut rl, &history),
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

        let (input, images) = attach(input, &agent.settings.root, &clipboard);
        run_input(agent, &mut rl, &input, images).await;
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
            replay(agent);
            if !agent.tools.todos.is_empty() {
                ui::tool_header("Todo", &crate::tools::todo::progress(&agent.tools.todos));
                crate::tools::todo::render(&agent.tools.todos);
            }
            ui::success(&format!("resumed “{title}” ({n} messages)"));
        }
        Err(e) => ui::error(&format!("{e:#}")),
    }
}

/// Print a resumed conversation the way it looked live: user prompts, rendered replies,
/// and compact tool lines. Harness-injected messages get short labels.
fn replay(agent: &Agent) {
    use crate::llm::Role;
    let root = &agent.settings.root;
    let msgs = &agent.messages[1..];
    let result_of = |id: &str| msgs.iter().find(|m| m.role == Role::Tool && m.tool_call_id.as_deref() == Some(id)).and_then(|m| m.content.as_deref());
    println!("{}", ui::dim("── earlier in this session ──"));
    for m in msgs {
        let text = m.content.as_deref().unwrap_or("");
        match m.role {
            Role::User => {
                println!();
                if let Some(rest) = text.strip_prefix("(For context, I ran `") {
                    let cmd = rest.split('`').next().unwrap_or("");
                    let status = text.lines().nth(1).unwrap_or("").trim_matches(['[', ']']);
                    println!("{} {}", ui::accent("❯"), ui::dim(&format!("!{cmd}")));
                    ui::tool_result(status);
                } else if let Some(rest) = text.strip_prefix("Automatic verification `") {
                    ui::tool_header("Verify", rest.split('`').next().unwrap_or(""));
                    ui::tool_error("failed — the model was asked to fix it");
                } else if text.starts_with("[Summary of the earlier conversation") {
                    println!("{}", ui::dim("… earlier conversation was summarized to save context"));
                } else if text.starts_with("(Note: ") {
                    println!("{}", ui::dim(text));
                } else {
                    let (shown, plan) = match text.find("\n\n[Plan mode:") {
                        Some(i) => (&text[..i], true),
                        None => (text, false),
                    };
                    let prompt = if plan { "plan ❯" } else { "❯" };
                    println!("{} {}", ui::accent(prompt), shown);
                    println!();
                }
            }
            Role::Assistant => {
                if !text.trim().is_empty() {
                    let mut md = crate::markdown::Renderer::new();
                    md.push(text.trim());
                    md.finish();
                }
                for c in &m.tool_calls {
                    crate::tools::replay(root, &c.function.name, &c.function.arguments, result_of(&c.id));
                }
            }
            Role::Tool | Role::System => {}
        }
    }
    println!();
    println!("{}", ui::dim("── end of history ──"));
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
async fn run_input(agent: &mut Agent, rl: &mut Editor, input: &str, mut images: Vec<String>) {
    let mut input = input.to_string();
    loop {
        println!();
        match turn(agent, &input, std::mem::take(&mut images)).await {
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

/// Save everything and print how to resume this exact session.
fn goodbye(agent: &Agent, rl: &mut Editor, history: &Option<std::path::PathBuf>) -> Result<()> {
    agent.save_session();
    resume_hint(agent);
    save(rl, history)
}

/// "Resume this session with: hermit --resume <id>" (only if there is something to resume).
/// The id alone is enough: resuming switches to the session's project folder.
pub fn resume_hint(agent: &Agent) {
    if agent.sessions_dir.is_none() || agent.to_session().is_none() {
        return;
    }
    println!();
    println!("{} {}", ui::dim("Resume this session with:"), ui::bold(&format!("hermit --resume {}", agent.session_id)));
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
