//! Slash-command table, inline hints and Tab completion for the REPL.

use rustyline::completion::{Completer, Pair};
use rustyline::highlight::Highlighter;
use rustyline::hint::{Hint, Hinter};
use rustyline::validate::Validator;
use rustyline::{Context, Helper};
use std::borrow::Cow;

/// (name, argument placeholder, description). The single source for /help, hints and completion.
pub const COMMANDS: &[(&str, &str, &str)] = &[
    ("help", "", "show commands"),
    ("plan", "[task|off]", "read-only planning; approve the plan before any changes"),
    ("model", "[id]", "show or switch model"),
    ("models", "[all]", "list free OpenRouter models (native tool support, or all)"),
    ("todo", "", "show the agent's task list"),
    ("remember", "<fact>", "save a note to this project's HERMIT.md"),
    ("memory", "", "show saved memory"),
    ("sessions", "", "list saved sessions for this directory"),
    ("resume", "[n|id]", "resume a session (default: the latest)"),
    ("compact", "", "summarize older context now"),
    ("undo", "", "revert file changes from the last turn"),
    ("verify", "", "run the project's checks (and fix failures)"),
    ("clear", "", "start a new conversation"),
    ("cost", "", "token usage this session"),
    ("exit", "", "quit"),
];

pub fn help_text() -> String {
    let mut s = String::from("!<command>        run a shell command yourself (output is shared with the model)\n");
    for (name, args, desc) in COMMANDS {
        let usage = if args.is_empty() { format!("/{name}") } else { format!("/{name} {args}") };
        s.push_str(&format!("{usage:<17} {desc}\n"));
    }
    s.push_str("Tab completes commands; → accepts the gray suggestion.\n");
    s.push_str("End a line with \\ to continue on the next line. Ctrl-C cancels a running request.");
    s
}

fn matching(prefix: &str) -> Vec<&'static (&'static str, &'static str, &'static str)> {
    COMMANDS.iter().filter(|(n, _, _)| n.starts_with(prefix)).collect()
}

/// Inline suggestion for `line` (cursor at end): (text to show, text inserted on accept).
pub fn hint_for(line: &str) -> Option<(String, Option<String>)> {
    let rest = line.strip_prefix('/')?;
    if rest.is_empty() {
        let names: Vec<&str> = COMMANDS.iter().map(|c| c.0).collect();
        return Some((names.join(" · "), None));
    }
    if let Some((name, arg)) = rest.split_once(' ') {
        // After a full command: show its argument placeholder until something is typed.
        let (_, args, _) = COMMANDS.iter().find(|c| c.0 == name)?;
        return (arg.is_empty() && !args.is_empty()).then(|| (args.to_string(), None));
    }
    let m = matching(rest);
    match m.as_slice() {
        [] => Some(("  unknown command (Tab to list)".into(), None)),
        [(name, args, desc)] => {
            let tail = &name[rest.len()..];
            let space = if args.is_empty() { "" } else { " " };
            let shown = if args.is_empty() { format!("{tail}  {desc}") } else { format!("{tail} {args}  {desc}") };
            Some((shown, Some(format!("{tail}{space}"))))
        }
        many => {
            // Exact match that is also a prefix of others (e.g. /model vs /models).
            if let Some((name, args, desc)) = many.iter().find(|c| c.0 == rest) {
                let others: Vec<&str> = many.iter().map(|c| c.0).filter(|n| n != name).collect();
                let a = if args.is_empty() { String::new() } else { format!(" {args}") };
                return Some((format!("{a}  {desc}  (also: {})", others.join(", ")), None));
            }
            let common = common_prefix(many.iter().map(|c| c.0));
            let tail = &common[rest.len()..];
            let names: Vec<&str> = many.iter().map(|c| c.0).collect();
            let list = format!("  {}", names.join(" · "));
            if tail.is_empty() { Some((list, None)) } else { Some((format!("{tail}{list}"), Some(tail.to_string()))) }
        }
    }
}

fn common_prefix<'a>(mut names: impl Iterator<Item = &'a str>) -> String {
    let first = names.next().unwrap_or("").to_string();
    names.fold(first, |acc, n| acc.chars().zip(n.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a).collect())
}

/// Tab candidates: (start offset, [(display, replacement)]).
pub fn completions(line: &str, models: &[String]) -> (usize, Vec<(String, String)>) {
    let Some(rest) = line.strip_prefix('/') else { return (0, Vec::new()) };
    if let Some((name, arg)) = rest.split_once(' ') {
        let start = 1 + name.len() + 1;
        let opts: Vec<String> = match name {
            "model" => models.to_vec(),
            "plan" => vec!["off".into(), "on".into()],
            "models" => vec!["all".into()],
            _ => Vec::new(),
        };
        let c = opts.into_iter().filter(|o| o.starts_with(arg)).map(|o| (o.clone(), o)).collect();
        return (start, c);
    }
    let c = matching(rest)
        .into_iter()
        .map(|(n, args, _)| (format!("/{n}"), format!("/{n}{}", if args.is_empty() { "" } else { " " })))
        .collect();
    (0, c)
}

/// rustyline helper wiring the functions above into the line editor.
#[derive(Default)]
pub struct SlashHelper {
    pub models: Vec<String>,
}

pub struct SlashHint {
    display: String,
    completion: Option<String>,
}

impl Hint for SlashHint {
    fn display(&self) -> &str {
        &self.display
    }
    fn completion(&self) -> Option<&str> {
        self.completion.as_deref()
    }
}

impl Hinter for SlashHelper {
    type Hint = SlashHint;
    fn hint(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> Option<SlashHint> {
        if pos < line.len() {
            return None;
        }
        hint_for(line).map(|(display, completion)| SlashHint { display, completion })
    }
}

impl Completer for SlashHelper {
    type Candidate = Pair;
    fn complete(&self, line: &str, pos: usize, _ctx: &Context<'_>) -> rustyline::Result<(usize, Vec<Pair>)> {
        let (start, c) = completions(&line[..pos], &self.models);
        Ok((start, c.into_iter().map(|(display, replacement)| Pair { display, replacement }).collect()))
    }
}

impl Highlighter for SlashHelper {
    fn highlight_hint<'h>(&self, hint: &'h str) -> Cow<'h, str> {
        Cow::Owned(crate::ui::dim(hint))
    }
}

impl Validator for SlashHelper {}
impl Helper for SlashHelper {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hints() {
        assert!(hint_for("/").unwrap().0.starts_with("help · plan · model"));
        assert_eq!(hint_for("/p").unwrap().1.as_deref(), Some("lan "));
        assert!(hint_for("/p").unwrap().0.starts_with("lan [task|off]  read-only planning"));
        assert_eq!(hint_for("/und").unwrap().1.as_deref(), Some("o"));
        // Ambiguous: /m → model, models, memory — nothing to auto-insert.
        let (shown, ins) = hint_for("/m").unwrap();
        assert_eq!(ins, None);
        assert!(shown.contains("model · models · memory"));
        // Shared prefix gets completed: /mode → model…
        assert_eq!(hint_for("/mode").unwrap().1.as_deref(), Some("l"));
        assert!(hint_for("/model").unwrap().0.contains("(also: models)"));
        assert_eq!(hint_for("/plan ").unwrap().0, "[task|off]");
        assert_eq!(hint_for("/plan x"), None);
        assert!(hint_for("/zzz").unwrap().0.contains("unknown"));
        assert_eq!(hint_for("hello"), None);
    }

    #[test]
    fn tab_completions() {
        let (s, c) = completions("/re", &[]);
        assert_eq!(s, 0);
        assert_eq!(c.iter().map(|x| x.1.as_str()).collect::<Vec<_>>(), ["/remember ", "/resume "]);
        assert_eq!(completions("/", &[]).1.len(), COMMANDS.len());
        let models = vec!["a/x:free".to_string(), "b/y:free".to_string()];
        let (s, c) = completions("/model b", &models);
        assert_eq!(s, 7);
        assert_eq!(c, [("b/y:free".to_string(), "b/y:free".to_string())]);
        assert!(help_text().contains("/plan [task|off]"));
    }
}
