//! Workspace confinement and shell-command approval.

use anyhow::{bail, Result};
use std::io::{BufRead, Write};
use std::path::{Component, Path, PathBuf};

/// Resolve `p` against `root` (which must already be canonical) and reject anything
/// that escapes it, including via `..` or symlinks. The target need not exist.
pub fn confine(root: &Path, p: &str) -> Result<PathBuf> {
    let p = p.trim();
    let joined = if Path::new(p).is_absolute() { PathBuf::from(p) } else { root.join(p) };

    // Lexically normalize `.` and `..`.
    let mut norm = PathBuf::new();
    for c in joined.components() {
        match c {
            Component::ParentDir => {
                if !norm.pop() {
                    bail!("path escapes workspace: {p}");
                }
            }
            Component::CurDir => {}
            other => norm.push(other),
        }
    }

    // Canonicalize the longest existing ancestor so symlinks are resolved.
    let mut existing = norm.as_path();
    let mut rest = Vec::new();
    while !existing.exists() {
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                rest.push(name.to_os_string());
                existing = parent;
            }
            _ => break,
        }
    }
    let mut resolved = existing.canonicalize().unwrap_or_else(|_| existing.to_path_buf());
    for name in rest.into_iter().rev() {
        resolved.push(name);
    }
    if !resolved.starts_with(root) {
        bail!("path is outside the workspace ({}): {p}", root.display());
    }
    Ok(resolved)
}

/// Path relative to root for display.
pub fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).display().to_string()
}

pub const DEFAULT_ALLOW: &[&str] = &[
    "ls", "pwd", "cat", "head", "tail", "wc", "echo", "grep", "rg", "which", "tree", "diff",
    "git status", "git diff", "git log", "git show", "git branch",
    "cargo check", "cargo build", "cargo test", "cargo clippy", "cargo fmt", "cargo tree",
    "npm test", "npm run test", "npm run build", "npm run lint", "npx tsc",
    "go build", "go test", "go vet", "gofmt",
    "pytest", "python -m pytest", "python3 -m pytest", "ruff", "mypy",
];

/// True when every `&&`/`||`/`;`/`|`-separated segment starts with an allowed prefix
/// and the command has no redirection or substitution.
pub fn is_allowed(cmd: &str, allow: &[String]) -> bool {
    let cmd = cmd.trim();
    if cmd.is_empty() || ['>', '<', '`', '\n'].iter().any(|c| cmd.contains(*c)) || cmd.contains("$(") {
        return false;
    }
    cmd.split(['|', ';', '&'])
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .all(|seg| {
            DEFAULT_ALLOW.iter().copied().chain(allow.iter().map(String::as_str)).any(|a| {
                seg == a || seg.strip_prefix(a).is_some_and(|r| r.starts_with(' '))
            })
        })
}

pub enum Approval {
    Yes,
    No,
    Always,
}

/// Ask the user on the terminal. EOF or anything unrecognized means no.
pub fn ask(cmd: &str) -> Approval {
    print!(
        "{} {}\n  {} ",
        crate::ui::yellow("? run command:"),
        crate::ui::bold(cmd),
        crate::ui::dim("[y]es / [n]o / [a]lways this session ›")
    );
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    if std::io::stdin().lock().read_line(&mut line).unwrap_or(0) == 0 {
        return Approval::No;
    }
    match line.trim().to_ascii_lowercase().as_str() {
        "y" | "yes" => Approval::Yes,
        "a" | "always" => Approval::Always,
        _ => Approval::No,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn confine_rejects_escapes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(confine(&root, "src/main.rs").unwrap().starts_with(&root));
        assert!(confine(&root, "a/../b.txt").is_ok());
        assert!(confine(&root, "../outside").is_err());
        assert!(confine(&root, "/etc/passwd").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn confine_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::os::unix::fs::symlink(outside.path(), root.join("link")).unwrap();
        assert!(confine(&root, "link/secret.txt").is_err());
    }

    #[test]
    fn allowlist() {
        let extra = vec!["make test".to_string()];
        assert!(is_allowed("cargo test", &extra));
        assert!(is_allowed("cargo test -- --nocapture && git diff", &extra));
        assert!(is_allowed("make test", &extra));
        assert!(!is_allowed("cargo testx", &extra));
        assert!(!is_allowed("rm -rf /", &extra));
        assert!(!is_allowed("ls && rm x", &extra));
        assert!(!is_allowed("cat a > b", &extra));
        assert!(!is_allowed("echo $(whoami)", &extra));
    }
}
