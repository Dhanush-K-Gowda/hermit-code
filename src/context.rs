//! Output capping and context-window budgeting.

use crate::llm::{Message, Role};
use std::collections::VecDeque;

pub const HEAD: usize = 8 * 1024;
pub const TAIL: usize = 8 * 1024;

/// Keeps the first `head` and last `tail` bytes of an unbounded stream in bounded memory.
pub struct CappedBuf {
    head: Vec<u8>,
    tail: VecDeque<u8>,
    head_cap: usize,
    tail_cap: usize,
    total: usize,
}

impl CappedBuf {
    pub fn new(head_cap: usize, tail_cap: usize) -> Self {
        Self { head: Vec::new(), tail: VecDeque::new(), head_cap, tail_cap, total: 0 }
    }

    pub fn push(&mut self, mut bytes: &[u8]) {
        self.total += bytes.len();
        let room = self.head_cap.saturating_sub(self.head.len());
        let n = room.min(bytes.len());
        self.head.extend_from_slice(&bytes[..n]);
        bytes = &bytes[n..];
        if bytes.len() >= self.tail_cap {
            self.tail.clear();
            self.tail.extend(&bytes[bytes.len() - self.tail_cap..]);
        } else {
            let overflow = (self.tail.len() + bytes.len()).saturating_sub(self.tail_cap);
            self.tail.drain(..overflow);
            self.tail.extend(bytes);
        }
    }

    pub fn finish(self) -> String {
        let kept = self.head.len() + self.tail.len();
        let mut out = String::from_utf8_lossy(&self.head).into_owned();
        if self.total > kept {
            out.push_str(&format!("\n…[{} bytes truncated]…\n", self.total - kept));
        }
        let (a, b) = self.tail.as_slices();
        let mut t = Vec::with_capacity(a.len() + b.len());
        t.extend_from_slice(a);
        t.extend_from_slice(b);
        out.push_str(&String::from_utf8_lossy(&t));
        out
    }
}

/// Truncate a string to `head` + `tail` bytes (on char boundaries) with a marker in between.
pub fn truncate_middle(s: &str, head: usize, tail: usize) -> String {
    if s.len() <= head + tail {
        return s.to_string();
    }
    let mut h = head;
    while !s.is_char_boundary(h) {
        h -= 1;
    }
    let mut t = s.len() - tail;
    while !s.is_char_boundary(t) {
        t += 1;
    }
    format!("{}\n…[{} bytes truncated]…\n{}", &s[..h], t - h, &s[t..])
}

pub fn estimate_tokens(messages: &[Message]) -> usize {
    messages.iter().map(Message::char_len).sum::<usize>() / 4
}

/// Shrink old content until the conversation fits `budget` tokens.
/// The system prompt and the most recent `keep_recent` messages are never touched.
pub fn compact(messages: &mut [Message], budget: usize, keep_recent: usize) -> bool {
    let mut tokens = estimate_tokens(messages);
    if tokens <= budget {
        return false;
    }
    let end = messages.len().saturating_sub(keep_recent);
    // Pass 1: old tool outputs. Pass 2: old long user/assistant text.
    for pass in 0..2 {
        for m in messages[..end].iter_mut() {
            if tokens <= budget {
                return true;
            }
            let eligible = match pass {
                0 => m.role == Role::Tool,
                _ => matches!(m.role, Role::User | Role::Assistant),
            };
            let Some(c) = m.content.as_mut().filter(|c| eligible && c.len() > 400) else {
                continue;
            };
            let before = c.len();
            *c = if pass == 0 {
                format!("[old tool output elided: {before} bytes]")
            } else {
                truncate_middle(c, 150, 150)
            };
            tokens = tokens.saturating_sub((before - c.len()) / 4);
        }
    }
    true
}

/// Where to split history for summarization: messages `[1..split]` get summarized and
/// `[split..]` (roughly the last `keep_tokens`) are kept verbatim. The split never lands on a
/// `tool` message, so every kept tool result still follows the call that produced it.
pub fn summary_split(messages: &[Message], keep_tokens: usize) -> Option<usize> {
    if messages.len() < 4 {
        return None;
    }
    let mut acc = 0;
    let mut split = messages.len();
    while split > 1 && acc < keep_tokens {
        split -= 1;
        acc += messages[split].char_len() / 4;
    }
    while split < messages.len() && messages[split].role == Role::Tool {
        split += 1;
    }
    // Need at least two old messages to be worth a model call, and something left to keep.
    (split > 2 && split < messages.len()).then_some(split)
}

/// Render messages as a compact transcript for the summarizer, bounded to `max_chars`.
pub fn transcript(messages: &[Message], max_chars: usize) -> String {
    let mut out = String::new();
    for m in messages {
        let who = match m.role {
            Role::System => continue,
            Role::User => "USER",
            Role::Assistant => "ASSISTANT",
            Role::Tool => "TOOL RESULT",
        };
        let limit = if m.role == Role::Tool { 1500 } else { 6000 };
        out.push_str(&format!("### {who}\n"));
        if let Some(c) = &m.content {
            out.push_str(&truncate_middle(c, limit / 2, limit / 2));
            out.push('\n');
        }
        for c in &m.tool_calls {
            out.push_str(&format!("[called {} {}]\n", c.function.name, truncate_middle(&c.function.arguments, 600, 200)));
        }
        out.push('\n');
    }
    truncate_middle(&out, max_chars / 3, max_chars * 2 / 3)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capped_buf_keeps_head_and_tail() {
        let mut b = CappedBuf::new(4, 4);
        b.push(b"abc");
        b.push(b"defghij");
        b.push(b"kl");
        let s = b.finish();
        assert!(s.starts_with("abcd"));
        assert!(s.ends_with("ijkl"));
        assert!(s.contains("[4 bytes truncated]"));
    }

    #[test]
    fn capped_buf_small_is_verbatim() {
        let mut b = CappedBuf::new(8, 8);
        b.push(b"hello");
        assert_eq!(b.finish(), "hello");
    }

    #[test]
    fn truncate_middle_respects_char_boundaries() {
        let s = "é".repeat(100);
        let t = truncate_middle(&s, 5, 5);
        assert!(t.contains("truncated"));
        assert_eq!(truncate_middle("short", 5, 5), "short");
    }

    #[test]
    fn compact_elides_old_tool_output_first() {
        let mut msgs = vec![
            Message::system("sys"),
            Message::user("q"),
            Message::tool("1", "x".repeat(10_000)),
            Message::user("recent ".repeat(10)),
        ];
        assert!(compact(&mut msgs, 500, 1));
        assert!(msgs[2].content.as_ref().unwrap().starts_with("[old tool output elided"));
        assert!(estimate_tokens(&msgs) <= 500);
    }

    #[test]
    fn summary_split_skips_tool_messages() {
        let mut msgs = vec![Message::system("s"), Message::user("a".repeat(4000))];
        msgs.push(Message::user("b".repeat(4000)));
        let mut call = Message::user("");
        call.role = Role::Assistant;
        call.content = None;
        call.tool_calls.push(crate::llm::ToolCall {
            id: "1".into(),
            kind: "function".into(),
            function: crate::llm::types::FunctionCall { name: "read_file".into(), arguments: "{}".into() },
        });
        msgs.push(call);
        msgs.push(Message::tool("1", "c".repeat(400)));
        msgs.push(Message::tool("1", "d".repeat(400)));
        // Keeping ~150 tokens lands inside the tool results; the split must move past them.
        let split = summary_split(&msgs, 150);
        assert!(split.is_none_or(|s| msgs[s].role != Role::Tool));
        let split = summary_split(&msgs, 210).unwrap();
        assert_eq!(msgs[split].role, Role::Assistant);
        let t = transcript(&msgs[1..split], 10_000);
        assert!(t.contains("### USER"));
        assert!(!t.contains("### TOOL"));
    }
}
