use anyhow::{Context, Result};
use clap::Parser;
use hermit::agent::{Agent, Outcome};
use hermit::config::Settings;
use std::path::PathBuf;

/// A small, fast coding agent. Runs an interactive REPL, or one task with -p.
#[derive(Parser)]
#[command(version)]
struct Cli {
    /// Run a single task non-interactively and exit
    #[arg(short, long)]
    prompt: Option<String>,
    /// Model id (e.g. poolside/laguna-s-2.1:free)
    #[arg(short, long)]
    model: Option<String>,
    /// Workspace directory (default: current directory)
    #[arg(short = 'C', long)]
    dir: Option<PathBuf>,
    /// Run shell commands without asking
    #[arg(long)]
    yolo: bool,
    /// Override the check command run after changes
    #[arg(long)]
    verify: Option<String>,
    /// Don't run checks after changes
    #[arg(long)]
    no_verify: bool,
    /// Max model calls per request
    #[arg(long)]
    max_steps: Option<usize>,
    /// Continue the most recent session in this directory
    #[arg(short = 'c', long = "continue")]
    continue_last: bool,
    /// Resume a saved session by id (see /sessions)
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
    /// Use text-based tool calls (for models without native tool support)
    #[arg(long)]
    text_tools: bool,
}

fn main() {
    let cli = Cli::parse();
    // A single-threaded runtime keeps the footprint small; the agent is sequential anyway.
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("tokio runtime");
    match rt.block_on(run(cli)) {
        Ok(code) => std::process::exit(code),
        Err(e) => {
            hermit::ui::error(&format!("{e:#}"));
            std::process::exit(1);
        }
    }
}

async fn run(cli: Cli) -> Result<i32> {
    let dir = match cli.dir {
        Some(d) => d,
        None => std::env::current_dir()?,
    };
    let root = dir.canonicalize().with_context(|| format!("no such directory: {}", dir.display()))?;
    let mut s = Settings::load(&root)?;
    if let Some(m) = cli.model {
        s.model = m;
    }
    if cli.verify.is_some() {
        s.verify_cmd = cli.verify;
    }
    if cli.no_verify {
        s.verify = false;
    }
    if let Some(n) = cli.max_steps {
        s.max_steps = n;
    }
    s.yolo = cli.yolo;
    if cli.text_tools {
        s.tool_mode = hermit::config::ToolMode::Text;
    }

    let mut agent = Agent::new(s)?;
    let resume_arg = cli.resume.or(cli.continue_last.then(String::new));
    match cli.prompt {
        Some(p) => {
            hermit::repl::banner(&agent);
            if let Some(r) = &resume_arg {
                hermit::repl::resume(&mut agent, r);
            }
            let outcome = hermit::repl::turn(&mut agent, &p).await;
            agent.save_session();
            let outcome = outcome?;
            Ok(match outcome {
                Outcome::Done | Outcome::Verified => 0,
                Outcome::Cancelled => 130,
                Outcome::VerifyFailed | Outcome::StepLimit => 2,
            })
        }
        None => {
            hermit::repl::run(&mut agent, resume_arg.as_deref()).await?;
            Ok(0)
        }
    }
}
