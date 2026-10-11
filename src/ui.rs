//! Terminal output helpers. Colors are disabled when stdout isn't a TTY or NO_COLOR is set.

use std::io::{IsTerminal, Write};
use std::sync::OnceLock;

fn color() -> bool {
    static C: OnceLock<bool> = OnceLock::new();
    *C.get_or_init(|| std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none())
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

pub fn stream_text(s: &str) {
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(s.as_bytes());
    let _ = out.flush();
}

pub fn notice(s: &str) {
    eprintln!("{}", yellow(&format!("! {s}")));
}

pub fn error(s: &str) {
    eprintln!("{}", red(&format!("✗ {s}")));
}

pub fn success(s: &str) {
    println!("{}", green(&format!("✓ {s}")));
}

pub fn tool_header(name: &str, summary: &str) {
    println!("{} {}", cyan(&format!("● {name}")), dim(summary));
}

/// Print a colored unified diff, capped at `max_lines` lines.
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
    println!("  {} {}", bold(path), dim(&format!("(+{adds} -{dels})")));
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
