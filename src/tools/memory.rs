//! Durable notes (`remember`): appended to HERMIT.md in the project, or in the user config dir.
//! Both files are loaded into the system prompt of every new session.

use anyhow::{bail, Result};
use std::path::Path;

pub const FILE: &str = "HERMIT.md";
const HEADER: &str = "# Hermit memory\n\nNotes saved for future sessions. Edit freely.\n\n";

/// Append `fact` as a bullet. Returns false if an identical note already exists.
pub fn remember(path: &Path, fact: &str) -> Result<bool> {
    let fact: String = fact.split_whitespace().collect::<Vec<_>>().join(" ");
    let fact = fact.trim_start_matches("- ").trim();
    if fact.is_empty() {
        bail!("nothing to remember");
    }
    if fact.len() > 500 {
        bail!("keep each note under 500 characters");
    }
    let line = format!("- {fact}");
    let mut s = std::fs::read_to_string(path).unwrap_or_default();
    if s.lines().any(|l| l.trim() == line) {
        return Ok(false);
    }
    if s.is_empty() {
        s.push_str(HEADER);
    } else if !s.ends_with('\n') {
        s.push('\n');
    }
    s.push_str(&line);
    s.push('\n');
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d)?;
    }
    std::fs::write(path, s)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_once() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join(FILE);
        assert!(remember(&p, "Run tests with `cargo test -q`\n  (fast)").unwrap());
        assert!(!remember(&p, "- Run tests with `cargo test -q` (fast)").unwrap());
        assert!(remember(&p, "Use tabs").unwrap());
        let s = std::fs::read_to_string(&p).unwrap();
        assert!(s.starts_with("# Hermit memory"));
        assert!(s.ends_with("- Run tests with `cargo test -q` (fast)\n- Use tabs\n"));
        assert!(remember(&p, "   ").is_err());
    }
}
