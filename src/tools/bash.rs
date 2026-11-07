//! `bash`: run a shell command in the working directory.

use std::{process::Stdio, time::Duration};

use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, opt_u64, schema, str_arg};
use crate::llm::{BoxFuture, ToolSpec};

pub struct Bash;

impl Tool for Bash {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "bash".into(),
            description: "Run a command with `bash -c` in the working directory; returns stdout, stderr and the \
                          exit code. Non-interactive (stdin is closed). The process group is killed on timeout \
                          or interruption."
                .into(),
            parameters: schema(
                json!({
                    "command": {"type": "string"},
                    "timeout_secs": {"type": "integer", "description": "Override the default timeout"}
                }),
                &["command"],
            ),
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let command = match str_arg(&args, "command") {
                Ok(c) => c,
                Err(e) => return ToolOutput::err(e),
            };
            let timeout = match opt_u64(&args, "timeout_secs") {
                Ok(t) => t.map_or(ctx.bash_timeout, Duration::from_secs),
                Err(e) => return ToolOutput::err(e),
            };
            run_command(&command, ctx, timeout).await
        })
    }
}

async fn run_command(command: &str, ctx: &ToolCtx, timeout: Duration) -> ToolOutput {
    let spawn = |shell: &str| {
        let mut cmd = tokio::process::Command::new(shell);
        cmd.arg("-c")
            .arg(command)
            .current_dir(&ctx.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        #[cfg(unix)]
        cmd.process_group(0);
        cmd.spawn()
    };
    let child = match spawn("bash") {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => spawn("sh"),
        other => other,
    };
    let child = match child {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("failed to spawn shell: {e}")),
    };
    let pid = child.id();
    let result = tokio::select! {
        r = child.wait_with_output() => r,
        _ = tokio::time::sleep(timeout) => {
            kill_group(pid);
            return ToolOutput::err(format!("command timed out after {}s", timeout.as_secs()));
        }
        _ = ctx.cancel.cancelled() => {
            kill_group(pid);
            return ToolOutput::err("command interrupted by user");
        }
    };
    match result {
        Err(e) => ToolOutput::err(format!("waiting for command: {e}")),
        Ok(out) => {
            let mut text = String::from_utf8_lossy(&out.stdout).into_owned();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("[stderr]\n");
                text.push_str(&stderr);
            }
            let ok = out.status.success();
            if !ok {
                if !text.is_empty() && !text.ends_with('\n') {
                    text.push('\n');
                }
                match out.status.code() {
                    Some(c) => text.push_str(&format!("[exit code {c}]")),
                    None => text.push_str("[terminated by signal]"),
                }
            }
            if text.trim().is_empty() {
                text = "(no output)".into();
            }
            ToolOutput { content: text, is_error: !ok }
        }
    }
}

/// Kills the whole process group so background children of the command die too.
fn kill_group(pid: Option<u32>) {
    #[cfg(unix)]
    if let Some(pid) = pid {
        // SAFETY: kill(2) with a negative pid signals the process group we created via process_group(0).
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    let _ = pid;
}
