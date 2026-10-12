//! Minimal streaming Markdown renderer for model replies.
//!
//! Text streams to the terminal raw as it arrives; when a line completes, it is redrawn in
//! place with styling. That keeps token-by-token streaming while still rendering headings,
//! emphasis, code and lists. Without a color TTY, text passes through untouched.

use crate::ui;
use std::io::IsTerminal;

const BOLD: (&str, &str) = ("\x1b[1m", "\x1b[22m");
const ITALIC: (&str, &str) = ("\x1b[3m", "\x1b[23m");
const UNDERLINE: (&str, &str) = ("\x1b[4m", "\x1b[24m");
const CODE: (&str, &str) = ("\x1b[38;5;180m", "\x1b[39m");

fn wrap(style: (&str, &str), s: &str) -> String {
    format!("{}{s}{}", style.0, style.1)
}

/// Streams reply text and restyles each completed line.
pub struct Renderer {
    line: String,
    in_code: bool,
    first: bool,
    styled: bool,
}

impl Default for Renderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer {
    pub fn new() -> Self {
        let styled = std::io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none();
        Renderer { line: String::new(), in_code: false, first: true, styled }
    }

    fn marker(&self) -> String {
        if self.first { format!("{} ", ui::accent("◆")) } else { String::new() }
    }

    pub fn push(&mut self, text: &str) {
        let mut out = String::new();
        for seg in text.split_inclusive('\n') {
            if self.line.is_empty() {
                out.push_str(&self.marker());
            }
            let body = seg.strip_suffix('\n');
            let raw = body.unwrap_or(seg);
            out.push_str(raw);
            self.line.push_str(raw);
            if body.is_some() {
                self.end_line(&mut out);
            }
        }
        ui::stream_text(&out);
    }

    /// Flush a final line that didn't end with a newline.
    pub fn finish(&mut self) {
        if !self.line.is_empty() {
            let mut out = String::new();
            self.end_line(&mut out);
            ui::stream_text(&out);
        }
    }

    fn end_line(&mut self, out: &mut String) {
        if self.styled {
            // Move back over however many rows the raw line wrapped onto, then redraw it.
            let prefix = if self.first { 2 } else { 0 };
            let rows = (prefix + display_width(&self.line)).max(1).div_ceil(term_width());
            if rows > 1 {
                out.push_str(&format!("\x1b[{}A", rows - 1));
            }
            out.push_str("\r\x1b[J");
            out.push_str(&self.marker());
            out.push_str(&render_line(&self.line, &mut self.in_code));
        }
        out.push('\n');
        self.line.clear();
        self.first = false;
    }
}

fn term_width() -> usize {
    // SAFETY: TIOCGWINSZ only writes into the provided winsize struct.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let ok = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) } == 0;
    if ok && ws.ws_col > 0 { ws.ws_col as usize } else { 80 }
}

/// Terminal columns used by `s` (tabs to 8-column stops, common wide characters count 2).
fn display_width(s: &str) -> usize {
    let mut w = 0;
    for c in s.chars() {
        w += match c {
            '\t' => 8 - w % 8,
            c if is_wide(c) => 2,
            c if c.is_control() => 0,
            _ => 1,
        };
    }
    w
}

fn is_wide(c: char) -> bool {
    matches!(c as u32,
        0x1100..=0x115F | 0x2E80..=0xA4CF | 0xAC00..=0xD7A3 | 0xF900..=0xFAFF |
        0xFE30..=0xFE4F | 0xFF00..=0xFF60 | 0xFFE0..=0xFFE6 | 0x1F300..=0x1F64F |
        0x1F900..=0x1FAFF | 0x20000..=0x3FFFD)
}

/// Style one complete line. `in_code` tracks fenced code blocks across lines.
pub fn render_line(line: &str, in_code: &mut bool) -> String {
    let trimmed = line.trim_start();
    let indent = &line[..line.len() - trimmed.len()];

    if trimmed.starts_with("```") || trimmed.starts_with("~~~") {
        *in_code = !*in_code;
        return if *in_code {
            ui::dim(&format!("{indent}┌─ {}", trimmed[3..].trim()))
        } else {
            ui::dim(&format!("{indent}└─"))
        };
    }
    if *in_code {
        return format!("{}{}", ui::dim("│ "), wrap(CODE, line));
    }

    let hashes = trimmed.bytes().take_while(|&b| b == b'#').count();
    if (1..=6).contains(&hashes) && trimmed[hashes..].starts_with(' ') {
        let text = inline(trimmed[hashes..].trim());
        return ui::accent(&wrap(BOLD, &text));
    }

    let compact: String = trimmed.chars().filter(|c| !c.is_whitespace()).collect();
    if compact.len() >= 3 && compact.chars().all(|c| c == '-') || compact.len() >= 3 && compact.chars().all(|c| c == '*') {
        return ui::dim(&"─".repeat(40));
    }

    if let Some(rest) = trimmed.strip_prefix('>') {
        return format!("{indent}{}{}", ui::dim("│ "), wrap(ITALIC, &inline(rest.trim_start())));
    }

    for bullet in ["- ", "* ", "+ "] {
        if let Some(rest) = trimmed.strip_prefix(bullet) {
            if let Some(t) = rest.strip_prefix("[ ] ") {
                return format!("{indent}{} {}", ui::dim("○"), inline(t));
            }
            if let Some(t) = rest.strip_prefix("[x] ").or_else(|| rest.strip_prefix("[X] ")) {
                return format!("{indent}{} {}", ui::green("✓"), ui::dim(&inline(t)));
            }
            return format!("{indent}{} {}", ui::accent("•"), inline(rest));
        }
    }

    let digits = trimmed.bytes().take_while(u8::is_ascii_digit).count();
    if digits > 0 && digits < 4 {
        let after = &trimmed[digits..];
        if let Some(rest) = after.strip_prefix(". ").or_else(|| after.strip_prefix(") ")) {
            return format!("{indent}{} {}", ui::dim(&trimmed[..=digits]), inline(rest));
        }
    }

    format!("{indent}{}", inline(trimmed))
}

/// Inline styling: **bold**, __bold__, *italic*, `code`, [links](url).
pub fn inline(s: &str) -> String {
    let c: Vec<char> = s.chars().collect();
    let n = c.len();
    let mut out = String::with_capacity(s.len() + 16);
    let find = |from: usize, pat: &[char]| -> Option<usize> {
        (from..=n.saturating_sub(pat.len())).find(|&j| c[j..j + pat.len()] == *pat)
    };
    let text = |a: usize, b: usize| -> String { c[a..b].iter().collect() };
    let mut i = 0;
    while i < n {
        let ch = c[i];
        let next = c.get(i + 1).copied();
        // `code`
        if ch == '`' {
            let ticks = c[i..].iter().take_while(|&&x| x == '`').count();
            let fence: Vec<char> = vec!['`'; ticks];
            if let Some(j) = find(i + ticks, &fence) {
                out.push_str(&wrap(CODE, text(i + ticks, j).trim()));
                i = j + ticks;
                continue;
            }
        }
        // **bold** / __bold__
        if (ch == '*' || ch == '_') && next == Some(ch) {
            let pat = [ch, ch];
            if let Some(j) = find(i + 2, &pat).filter(|&j| j > i + 2 && !c[i + 2].is_whitespace()) {
                out.push_str(&wrap(BOLD, &inline(&text(i + 2, j))));
                i = j + 2;
                continue;
            }
        }
        // *italic* (not inside words, so `a*b*c` and lone asterisks stay literal)
        if ch == '*' && next.is_some_and(|x| !x.is_whitespace() && x != '*') && (i == 0 || !c[i - 1].is_alphanumeric()) {
            let close = (i + 1..n).find(|&j| {
                c[j] == '*' && !c[j - 1].is_whitespace() && c.get(j + 1).is_none_or(|x| !x.is_alphanumeric())
            });
            if let Some(j) = close {
                out.push_str(&wrap(ITALIC, &inline(&text(i + 1, j))));
                i = j + 1;
                continue;
            }
        }
        // [text](url)
        if ch == '[' {
            if let Some(mid) = find(i + 1, &[']', '(']) {
                if let Some(end) = find(mid + 2, &[')']) {
                    let label = text(i + 1, mid);
                    let url = text(mid + 2, end);
                    out.push_str(&wrap(UNDERLINE, &inline(&label)));
                    if url != label {
                        out.push_str(&ui::dim(&format!(" ({url})")));
                    }
                    i = end + 1;
                    continue;
                }
            }
        }
        out.push(ch);
        i += 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip(s: &str) -> String {
        let mut out = String::new();
        let mut it = s.chars().peekable();
        while let Some(c) = it.next() {
            if c == '\x1b' {
                for d in it.by_ref() {
                    if d.is_ascii_alphabetic() {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn inline_styles() {
        let s = inline("use **bold** and *it* with `a*b` and [docs](https://x.y)");
        assert!(s.contains("\x1b[1mbold\x1b[22m"));
        assert!(s.contains("\x1b[3mit\x1b[23m"));
        assert!(s.contains("\x1b[38;5;180ma*b\x1b[39m"));
        assert!(s.contains("\x1b[4mdocs\x1b[24m"));
        assert_eq!(strip(&s), "use bold and it with a*b and docs (https://x.y)");
    }

    #[test]
    fn inline_leaves_plain_symbols_alone() {
        for s in ["2 * 3 * 4", "snake_case_name", "a*b*c", "price: $5 **", "x [y] z", "unclosed `tick"] {
            assert_eq!(inline(s), s, "{s}");
        }
    }

    #[test]
    fn block_elements() {
        let mut code = false;
        assert_eq!(strip(&render_line("## Plan", &mut code)), "Plan");
        assert_eq!(strip(&render_line("  - item **x**", &mut code)), "  • item x");
        assert_eq!(strip(&render_line("- [x] done", &mut code)), "✓ done");
        assert_eq!(strip(&render_line("- [ ] todo", &mut code)), "○ todo");
        assert_eq!(strip(&render_line("3. third", &mut code)), "3. third");
        assert_eq!(strip(&render_line("> note", &mut code)), "│ note");
        assert_eq!(strip(&render_line("---", &mut code)), "─".repeat(40));

        assert_eq!(strip(&render_line("```rust", &mut code)), "┌─ rust");
        assert!(code);
        // Inside code blocks nothing is interpreted.
        assert_eq!(strip(&render_line("let x = **y**; // # not a heading", &mut code)), "│ let x = **y**; // # not a heading");
        assert_eq!(strip(&render_line("```", &mut code)), "└─");
        assert!(!code);
    }

    #[test]
    fn widths() {
        assert_eq!(display_width("abc"), 3);
        assert_eq!(display_width("\tx"), 9);
        assert_eq!(display_width("日本"), 4);
    }
}
