//! Interactive line-based REPL.

use crate::agent::{Agent, Outcome};
use crate::config;
use crate::ui;
use anyhow::Result;
use rustyline::error::ReadlineError;
use std::time::Instant;

const HELP: &str = "\
/model [id]   show or switch model
/models       list free OpenRouter models with tool support
/undo         revert file changes from the last turn
/verify       run the project's checks (and fix failures)
/clear        start a new conversation
/cost         token usage this session
/exit         quit
End a line with \\ to continue on the next line. Ctrl-C cancels a running request.";

pub fn banner(agent: &Agent) {
    let s = &agent.settings;
    let verify = agent.verify_cmd().unwrap_or_else(|| ui::dim("none detected"));
    ui::banner(&[("model", s.model.clone()), ("dir", ui::tilde(&s.root)), ("verify", verify)]);
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

pub async fn run(agent: &mut Agent) -> Result<()> {
    banner(agent);
    println!("{}", ui::dim("  /help for commands · ctrl-c cancels a request\n"));
    let mut rl = rustyline::DefaultEditor::new()?;
    let history = config::config_dir().map(|d| d.join("history"));
    if let Some(h) = &history {
        let _ = rl.load_history(h);
    }

    loop {
        let mut input = String::new();
        let mut prompt = "❯ ";
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

        if let Some(cmd) = input.strip_prefix('/') {
            let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
            match name {
                "exit" | "quit" | "q" => return save(&mut rl, &history),
                "help" | "h" => println!("{HELP}"),
                "clear" => {
                    agent.reset();
                    println!("{}", ui::dim("conversation cleared"));
                }
                "model" if arg.is_empty() => println!("{}", agent.client.model),
                "model" => {
                    agent.client.model = arg.to_string();
                    agent.settings.model = arg.to_string();
                    println!("{}", ui::dim(&format!("model set to {arg}")));
                }
                "models" => match agent.client.list_free_tool_models().await {
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
            continue;
        }

        println!();
        if let Err(e) = turn(agent, input).await {
            ui::error(&format!("{e:#}"));
        }
        println!();
    }
}

fn save(rl: &mut rustyline::DefaultEditor, history: &Option<std::path::PathBuf>) -> Result<()> {
    if let Some(h) = history {
        if let Some(d) = h.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let _ = rl.save_history(h);
    }
    Ok(())
}
