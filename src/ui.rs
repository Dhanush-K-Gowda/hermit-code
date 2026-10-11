//! Terminal output: plain ANSI, no dependencies. Colors are disabled when stdout isn't a TTY
//! or NO_COLOR is set; the spinner only runs on a TTY.

use std::cell::Cell;
use std::future::Future;
use std::io::{IsTerminal, Write};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

fn color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
}

fn truecolor() -> bool {
    static T: OnceLock<bool> = OnceLock::new();
    *T.get_or_init(|| std::env::var("COLORTERM").is_ok_and(|v| v.contains("truecolor") || v.contains("24bit")))
}

fn paint(code: &str, s: &str) -> String {
    if color() {
        format!("\x1b[{code}m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

pub fn dim(s: &str) -> String {
    paint("2", s)
}
pub fn bold(s: &str) -> String {
    paint("1", s)
}
pub fn red(s: &str) -> String {
    paint("31", s)
}
pub fn green(s: &str) -> String {
    paint("32", s)
}
pub fn yellow(s: &str) -> String {
    paint("33", s)
}
pub fn cyan(s: &str) -> String {
    paint("36", s)
}
/// Hermit-crab coral.
pub fn accent(s: &str) -> String {
    paint("38;5;209", s)
}

// ── banner ──────────────────────────────────────────────────────────────────

const WORDMARK: [&str; 2] = [
    "█ █ █▀▀ █▀█ █▀▄▀█ █ ▀█▀",
    "█▀█ ██▄ █▀▄ █ ▀ █ █  █ ",
];
const GRAD_FROM: (f32, f32, f32) = (255.0, 166.0, 98.0);
const GRAD_TO: (f32, f32, f32) = (236.0, 72.0, 153.0);

fn fg(r: f32, g: f32, b: f32) -> String {
    if truecolor() {
        format!("38;2;{};{};{}", r as u8, g as u8, b as u8)
    } else {
        // Nearest xterm-256 color cube entry.
        let q = |v: f32| (v / 255.0 * 5.0).round() as u8;
        format!("38;5;{}", 16 + 36 * q(r) + 6 * q(g) + q(b))
    }
}

fn gradient(line: &str) -> String {
    if !color() {
        return line.to_string();
    }
    let n = (line.chars().count().max(2) - 1) as f32;
    let mut out = String::with_capacity(line.len() * 12);
    for (i, ch) in line.chars().enumerate() {
        if ch == ' ' {
            out.push(' ');
            continue;
        }
        let t = i as f32 / n;
        let lerp = |a: f32, b: f32| a + (b - a) * t;
        out.push_str(&format!(
            "\x1b[1;{}m{ch}",
            fg(lerp(GRAD_FROM.0, GRAD_TO.0), lerp(GRAD_FROM.1, GRAD_TO.1), lerp(GRAD_FROM.2, GRAD_TO.2))
        ));
    }
    out.push_str("\x1b[0m");
    out
}

/// Replace the home directory prefix with `~`.
pub fn tilde(p: &std::path::Path) -> String {
    match dirs::home_dir().and_then(|h| p.strip_prefix(h).ok().map(|r| r.display().to_string())) {
        Some(r) if r.is_empty() => "~".into(),
        Some(r) => format!("~/{r}"),
        None => p.display().to_string(),
    }
}

pub fn banner(rows: &[(&str, String)]) {
    println!();
    println!("  {}  {}", gradient(WORDMARK[0]), dim(concat!("v", env!("CARGO_PKG_VERSION"))));
    println!("  {}  {}", gradient(WORDMARK[1]), dim("small, fast coding agent"));
    println!();
    for (k, v) in rows {
        println!("  {} {}", dim(&format!("{k:<7}")), v);
    }
}

// ── streaming + status ──────────────────────────────────────────────────────

pub fn clear_line() {
    if color() {
        let mut out = std::io::stdout().lock();
        let _ = out.write_all(b"\r\x1b[2K");
        let _ = out.flush();
    }
}

/// Marks the start of a model reply.
pub fn assistant_start() {
    clear_line();
    stream_text(&format!("{} ", accent("◆")));
}

pub fn stream_text(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

/// Show a spinner with elapsed time while `fut` runs. It stops drawing once `stop` is set
/// (e.g. when streamed text starts) and clears its line when `fut` completes.
pub async fn spin<F: Future>(label: &str, stop: &Cell<bool>, fut: F) -> F::Output {
    if !color() {
        return fut.await;
    }
    const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
    let start = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_millis(80));
    let mut frame = 0;
    let mut drawn = false;
    tokio::pin!(fut);
    loop {
        tokio::select! {
            biased;
            r = &mut fut => {
                if drawn && !stop.get() {
                    clear_line();
                }
                return r;
            }
            _ = tick.tick() => {
                if stop.get() {
                    continue;
                }
                let secs = start.elapsed().as_secs();
                let time = if secs > 0 { format!(" {secs}s") } else { String::new() };
                stream_text(&format!("\r\x1b[2K{} {}", accent(FRAMES[frame % FRAMES.len()]), dim(&format!("{label}{time}"))));
                frame += 1;
                drawn = true;
            }
        }
    }
}

pub fn notice(s: &str) {
    clear_line();
    eprintln!("{}", yellow(&format!("! {s}")));
}

pub fn error(s: &str) {
    clear_line();
    eprintln!("{}", red(&format!("✗ {s}")));
}

pub fn success(s: &str) {
    println!("  {}", green(&format!("✓ {s}")));
}

pub fn tool_header(verb: &str, summary: &str) {
    println!("{} {} {}", accent("●"), bold(verb), dim(summary));
}

pub fn tool_result(s: &str) {
    println!("  {}", dim(&format!("└ {s}")));
}

/// Prefixes each line of streamed command output with a dim gutter.
#[derive(Default)]
pub struct Gutter {
    mid_line: bool,
}

impl Gutter {
    pub fn write(&mut self, chunk: &str) {
        let mut out = String::with_capacity(chunk.len() + 32);
        for line in chunk.split_inclusive('\n') {
            if !self.mid_line {
                out.push_str(&dim("  │ "));
            }
            out.push_str(&dim(line));
            self.mid_line = !line.ends_with('\n');
        }
        stream_text(&out);
    }

    pub fn finish(&mut self) {
        if self.mid_line {
            stream_text("\n");
            self.mid_line = false;
        }
    }
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1000 {
        format!("{:.1}k", n as f64 / 1000.0)
    } else {
        n.to_string()
    }
}

/// One-line summary printed after each turn.
pub fn footer(steps: usize, tokens: u64, elapsed: Duration) {
    let mut parts = vec![format!("{steps} step{}", if steps == 1 { "" } else { "s" })];
    if tokens > 0 {
        parts.push(format!("{} tokens", fmt_tokens(tokens)));
    }
    parts.push(format!("{:.1}s", elapsed.as_secs_f64()));
    println!("{}", dim(&format!("── {}", parts.join(" · "))));
}

/// Print a colored unified diff, capped at `MAX_LINES` lines.
pub fn diff(path: &str, old: &str, new: &str) {
    const MAX_LINES: usize = 120;
    let d = similar::TextDiff::from_lines(old, new);
    let (mut adds, mut dels) = (0, 0);
    for op in d.ops() {
        for c in d.iter_changes(op) {
            match c.tag() {
                similar::ChangeTag::Insert => adds += 1,
                similar::ChangeTag::Delete => dels += 1,
                similar::ChangeTag::Equal => {}
            }
        }
    }
    println!("  {} {} {}", dim("└"), path, dim(&format!("{} {}", green(&format!("+{adds}")), red(&format!("-{dels}")))));
    let text = d.unified_diff().context_radius(2).to_string();
    let mut lines = text.lines();
    for line in lines.by_ref().take(MAX_LINES) {
        let l = if line.starts_with('+') {
            green(line)
        } else if line.starts_with('-') {
            red(line)
        } else if line.starts_with("@@") {
            cyan(line)
        } else {
            dim(line)
        };
        println!("    {l}");
    }
    let rest = lines.count();
    if rest > 0 {
        println!("    {}", dim(&format!("… {rest} more diff lines")));
    }
}
