//! File tools: read, write, edit, list. All paths go through `safety::confine`.

use crate::context::truncate_middle;
use crate::safety::{confine, rel};
use anyhow::{bail, Context, Result};
use std::io::{BufRead, BufReader};
use std::path::Path;

const MAX_LINE: usize = 2000;

pub fn read_file(root: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> Result<String> {
    let p = confine(root, path)?;
    let f = std::fs::File::open(&p).with_context(|| format!("cannot open {path}"))?;
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(2000).clamp(1, 5000);
    let mut reader = BufReader::new(f);
    let mut out = String::new();
    let mut buf = Vec::new();
    let mut n = 0usize;
    let mut shown = 0usize;
    // Stream line by line so huge files never sit fully in memory.
    while reader.read_until(b'\n', &mut buf)? > 0 {
        n += 1;
        if n >= start {
            if shown == 0 && buf.iter().take(8000).any(|&b| b == 0) {
                bail!("{path} looks like a binary file");
            }
            if shown == limit {
                out.push_str(&format!("… (more lines after {}; use offset to continue)\n", n - 1));
                break;
            }
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            let line = if line.len() > MAX_LINE { truncate_middle(line, MAX_LINE / 2, 100) } else { line.to_string() };
            out.push_str(&format!("{n:>6}\t{line}\n"));
            shown += 1;
        }
        buf.clear();
    }
    if out.is_empty() {
        return Ok(if n == 0 { "(empty file)".into() } else { format!("(file has {n} lines; offset {start} is past the end)") });
    }
    Ok(truncate_middle(&out, 48 * 1024, 8 * 1024))
}

/// Returns (relative path, old content if it existed, new content).
pub fn write_file(root: &Path, path: &str, content: &str) -> Result<(String, Option<String>)> {
    let p = confine(root, path)?;
    if p.is_dir() {
        bail!("{path} is a directory");
    }
    let old = std::fs::read_to_string(&p).ok();
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&p, content).with_context(|| format!("cannot write {path}"))?;
    Ok((rel(root, &p), old))
}

/// Exact-string replace. `old` must match exactly once unless `replace_all`.
/// Returns (relative path, old content, new content).
pub fn edit_file(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> Result<(String, String, String)> {
    if old.is_empty() {
        bail!("`old` must not be empty (use write_file to create a file)");
    }
    if old == new {
        bail!("`old` and `new` are identical");
    }
    let p = confine(root, path)?;
    let before = std::fs::read_to_string(&p).with_context(|| format!("cannot read {path}"))?;
    let count = before.matches(old).count();
    let after = match count {
        0 => {
            // Common model mistake: wrong indentation or line endings. Give a useful hint.
            let hint = if before.contains(old.trim()) {
                " (a whitespace-trimmed version does match: check indentation/whitespace)"
            } else {
                ""
            };
            bail!("`old` text not found in {path}{hint}; read the file again and copy the text exactly");
        }
        1 => before.replacen(old, new, 1),
        _ if replace_all => before.replace(old, new),
        n => bail!("`old` matches {n} places in {path}; include more surrounding context to make it unique, or set replace_all"),
    };
    std::fs::write(&p, &after)?;
    Ok((rel(root, &p), before, after))
}

pub fn list_dir(root: &Path, path: &str) -> Result<String> {
    let p = confine(root, path)?;
    let mut entries: Vec<String> = std::fs::read_dir(&p)
        .with_context(|| format!("cannot list {path}"))?
        .filter_map(|e| e.ok())
        .map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if e.file_type().is_ok_and(|t| t.is_dir()) { format!("{name}/") } else { name }
        })
        .collect();
    entries.sort();
    let total = entries.len();
    entries.truncate(500);
    let mut out = entries.join("\n");
    if total > 500 {
        out.push_str(&format!("\n… {} more entries", total - 500));
    }
    if out.is_empty() {
        out = "(empty directory)".into();
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (tempfile::TempDir, std::path::PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        (d, root)
    }

    #[test]
    fn edit_unique_match() {
        let (_d, root) = setup();
        std::fs::write(root.join("a.txt"), "one\ntwo\nthree\n").unwrap();
        let (_, _, after) = edit_file(&root, "a.txt", "two", "TWO", false).unwrap();
        assert_eq!(after, "one\nTWO\nthree\n");
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), after);
    }

    #[test]
    fn edit_no_match_and_multi_match() {
        let (_d, root) = setup();
        std::fs::write(root.join("a.txt"), "x x x").unwrap();
        assert!(edit_file(&root, "a.txt", "y", "z", false).unwrap_err().to_string().contains("not found"));
        assert!(edit_file(&root, "a.txt", "x", "z", false).unwrap_err().to_string().contains("matches 3"));
        let (_, _, after) = edit_file(&root, "a.txt", "x", "z", true).unwrap();
        assert_eq!(after, "z z z");
    }

    #[test]
    fn write_creates_dirs_and_read_numbers_lines() {
        let (_d, root) = setup();
        let (rel, old) = write_file(&root, "deep/dir/f.rs", "fn a() {}\nfn b() {}\n").unwrap();
        assert_eq!(rel, "deep/dir/f.rs");
        assert!(old.is_none());
        let out = read_file(&root, "deep/dir/f.rs", Some(2), None).unwrap();
        assert_eq!(out, "     2\tfn b() {}\n");
        assert!(write_file(&root, "../escape.txt", "x").is_err());
    }
}
