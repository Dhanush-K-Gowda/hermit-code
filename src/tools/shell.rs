//! Shell command execution with timeout and bounded output capture.

use crate::context::{CappedBuf, HEAD, TAIL};
use anyhow::{Context, Result};
use std::path::Path;
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;

pub struct CmdOutput {
    pub code: Option<i32>,
    pub output: String,
    pub timed_out: bool,
}

impl CmdOutput {
    pub fn success(&self) -> bool {
        self.code == Some(0) && !self.timed_out
    }

    pub fn for_model(&self) -> String {
        let status = if self.timed_out {
            "timed out".to_string()
        } else {
            match self.code {
                Some(c) => format!("exit code {c}"),
                None => "killed by signal".to_string(),
            }
        };
        let body = if self.output.trim().is_empty() { "(no output)" } else { self.output.as_str() };
        format!("[{status}]\n{body}")
    }
}

/// Run `cmd` via `sh -c` in `cwd`. stdout and stderr are merged in order.
/// When `echo` is set, output is also streamed dimmed to the terminal.
pub async fn run(cwd: &Path, cmd: &str, timeout: Duration, echo: bool) -> Result<CmdOutput> {
    let mut child = tokio::process::Command::new("sh")
        .arg("-c")
        // `exec 2>&1` merges stderr into stdout for the whole script, preserving order.
        .arg(format!("exec 2>&1\n{cmd}"))
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .context("failed to spawn sh")?;
    let mut stdout = child.stdout.take().expect("piped stdout");
    let mut cap = CappedBuf::new(HEAD, TAIL);

    let read_all = async {
        let mut chunk = [0u8; 8192];
        let mut gutter = crate::ui::Gutter::default();
        loop {
            let n = stdout.read(&mut chunk).await?;
            if n == 0 {
                break;
            }
            cap.push(&chunk[..n]);
            if echo {
                gutter.write(&String::from_utf8_lossy(&chunk[..n]));
            }
        }
        gutter.finish();
        child.wait().await
    };

    match tokio::time::timeout(timeout, read_all).await {
        Ok(status) => {
            let status = status?;
            Ok(CmdOutput { code: status.code(), output: cap.finish(), timed_out: false })
        }
        Err(_) => {
            let _ = child.kill().await;
            Ok(CmdOutput { code: None, output: cap.finish(), timed_out: true })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn merges_streams_and_reports_code() {
        let d = tempfile::tempdir().unwrap();
        let o = run(d.path(), "echo out; echo err >&2; exit 3", Duration::from_secs(5), false).await.unwrap();
        assert_eq!(o.code, Some(3));
        assert_eq!(o.output, "out\nerr\n");
        assert!(!o.success());
    }

    #[tokio::test]
    async fn times_out() {
        let d = tempfile::tempdir().unwrap();
        let o = run(d.path(), "sleep 5", Duration::from_millis(200), false).await.unwrap();
        assert!(o.timed_out);
    }
}
