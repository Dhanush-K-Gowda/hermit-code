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

    let mut agent = Agent::new(s)?;
    match cli.prompt {
        Some(p) => {
            hermit::repl::banner(&agent);
            println!();
            let outcome = agent.run_turn(&p).await?;
            Ok(match outcome {
                Outcome::Done | Outcome::Verified => 0,
                Outcome::Cancelled => 130,
                Outcome::VerifyFailed | Outcome::StepLimit => 2,
            })
        }
        None => {
            hermit::repl::run(&mut agent).await?;
            Ok(0)
        }
    }
}
