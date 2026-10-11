//! gitignore-aware glob and grep.

use crate::safety::{confine, rel};
use anyhow::{Context, Result};
use std::io::{BufRead, BufReader, Read};
use std::path::Path;

const MAX_RESULTS: usize = 300;

fn walker(dir: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(dir).hidden(true).git_ignore(true).require_git(false).build()
}

pub fn glob(root: &Path, pattern: &str, path: Option<&str>) -> Result<String> {
    let dir = confine(root, path.unwrap_or("."))?;
    let m = globset::GlobBuilder::new(pattern)
        .literal_separator(false)
        .build()
        .context("invalid glob pattern")?
        .compile_matcher();
    let mut out = Vec::new();
    let mut total = 0;
    for e in walker(&dir).flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let r = e.path().strip_prefix(&dir).unwrap_or(e.path());
        if m.is_match(r) {
            total += 1;
            if out.len() < MAX_RESULTS {
                out.push(rel(root, e.path()));
            }
        }
    }
    out.sort();
    let mut s = out.join("\n");
    if total > out.len() {
        s.push_str(&format!("\n… {} more files", total - out.len()));
    }
    Ok(if s.is_empty() { "no files matched".into() } else { s })
}

pub fn grep(root: &Path, pattern: &str, path: Option<&str>, file_glob: Option<&str>, ignore_case: bool) -> Result<String> {
    let re = regex::RegexBuilder::new(pattern)
        .case_insensitive(ignore_case)
        .size_limit(1 << 20)
        .build()
        .context("invalid regex")?;
    let start = confine(root, path.unwrap_or("."))?;
    let filter = file_glob
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .context("invalid glob")?;

    let mut out = String::new();
    let mut hits = 0usize;
    let mut buf = Vec::new();
    'files: for e in walker(&start).flatten() {
        if !e.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if let Some(f) = &filter {
            let name = e.path().file_name().map(Path::new).unwrap_or(e.path());
            if !f.is_match(name) && !f.is_match(e.path().strip_prefix(root).unwrap_or(e.path())) {
                continue;
            }
        }
        let Ok(file) = std::fs::File::open(e.path()) else { continue };
        let mut reader = BufReader::new(file);
        // Skip binaries: NUL byte in the first block.
        if reader.fill_buf().map_or(true, |b| b.iter().take(8000).any(|&c| c == 0)) {
            continue;
        }
        let display = rel(root, e.path());
        let mut n = 0;
        loop {
            buf.clear();
            match reader.by_ref().take(1 << 20).read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            n += 1;
            let line = String::from_utf8_lossy(&buf);
            let line = line.trim_end_matches(['\n', '\r']);
            if re.is_match(line) {
                hits += 1;
                if hits > MAX_RESULTS {
                    break 'files;
                }
                let shown: String = line.chars().take(300).collect();
                out.push_str(&format!("{display}:{n}: {shown}\n"));
            }
        }
    }
    if hits > MAX_RESULTS {
        out.push_str(&format!("… stopped after {MAX_RESULTS} matches; narrow the search"));
    }
    Ok(if out.is_empty() { "no matches".into() } else { out })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grep_and_glob_respect_gitignore() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::write(root.join(".gitignore"), "target\n").unwrap();
        std::fs::write(root.join("src/lib.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        std::fs::write(root.join("target/gen.rs"), "fn alpha() {}\n").unwrap();

        let g = grep(&root, "fn a", None, None, false).unwrap();
        assert_eq!(g.trim(), "src/lib.rs:1: fn alpha() {}");
        let g = grep(&root, "BETA", None, Some("*.rs"), true).unwrap();
        assert!(g.contains("src/lib.rs:2"));

        let f = glob(&root, "**/*.rs", None).unwrap();
        assert_eq!(f, "src/lib.rs");
    }
}
