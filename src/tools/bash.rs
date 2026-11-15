//! `bash`: run a shell command in the working directory, in the foreground or as a background job.

use std::{
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};

use super::{
    Tool, ToolCtx, ToolOutput,
    jobs::{self, Capture},
    opt_bool, opt_u64, schema, str_arg,
};
use crate::llm::{BoxFuture, ToolSpec};

/// `background` offers `run_in_background` and moves commands that outlive the foreground limit
/// to a job instead of killing them. Subagents get the plain tool, since their jobs would die
/// with them.
pub struct Bash {
    pub background: bool,
}

impl Tool for Bash {
    fn guidance(&self) -> Option<&'static str> {
        Some(if self.background {
            "Check the [exit code N] marker on bash results and investigate failures before moving on. Do not use bash to read, search, or edit files when a dedicated tool does it. Start servers, watchers and other long-running commands with run_in_background; you are told when a job finishes, so do not poll it with sleep. Kill jobs you no longer need with job_kill."
        } else {
            "Check the [exit code N] marker on bash results and investigate failures before moving on. Do not use bash to read, search, or edit files when a dedicated tool does it."
        })
    }

    fn spec(&self) -> ToolSpec {
        if self.background {
            return ToolSpec {
                name: "bash".into(),
                description: "Run a command with `bash -c` in the working directory; returns stdout, stderr and the \
                              exit code. Non-interactive (stdin is closed). A command still running after the \
                              foreground limit (normally 120s) keeps running as a background job; so does one started \
                              with run_in_background. Jobs write to a log; job_output reads it and job_kill stops \
                              them. Interruption kills a foreground command, not jobs."
                    .into(),
                parameters: schema(
                    json!({
                        "command": {"type": "string"},
                        "run_in_background": {"type": "boolean", "description": "Start as a background job and return at once"},
                        "timeout_secs": {"type": "integer", "description": "Kill the command after this many seconds (default: never)"}
                    }),
                    &["command"],
                ),
            };
        }
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
                Ok(t) => t.map(Duration::from_secs),
                Err(e) => return ToolOutput::err(e),
            };
            let jobs = ctx.jobs.as_ref().filter(|_| self.background);
            let background = jobs.is_some() && opt_bool(&args, "run_in_background");
            run_command(&command, ctx, timeout, jobs, background).await
        })
    }
}

/// How a foreground wait ended.
enum Waited {
    Exited(std::io::Result<std::process::ExitStatus>),
    Limit,
    Cancelled,
}

/// @param timeout kill limit from the model; without `jobs` it defaults to `ctx.bash_timeout`.
/// @param jobs when set, a command outliving `ctx.bash_timeout` becomes a job instead.
/// @param background start as a job right away.
async fn run_command(
    command: &str,
    ctx: &ToolCtx,
    timeout: Option<Duration>,
    jobs: Option<&Arc<jobs::Jobs>>,
    background: bool,
) -> ToolOutput {
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
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return ToolOutput::err(format!("failed to spawn shell: {e}")),
    };
    let started = Instant::now();
    let pid = child.id();
    let capture = Arc::new(Mutex::new(Capture::default()));
    let mut readers = Vec::new();
    if let Some(out) = child.stdout.take() {
        readers.push(jobs::read_into(out, capture.clone(), false));
    }
    if let Some(err) = child.stderr.take() {
        readers.push(jobs::read_into(err, capture.clone(), true));
    }
    let deadline = timeout.map(|t| started + t);
    if let (Some(jobs), true) = (jobs, background) {
        return match jobs.adopt(command, started, child, capture, readers, deadline) {
            Ok((id, log)) => ToolOutput::ok(format!(
                "Started background job {id}; output goes to {}. You will be told when it finishes; job_output {id} shows its output so far.",
                log.display()
            )),
            Err(e) => {
                kill_group(pid);
                ToolOutput::err(format!("could not start a background job: {e}"))
            }
        };
    }
    // With jobs, the foreground limit moves the command to the background unless the model's
    // own timeout comes first.
    let (limit, kill_at_limit) = match (jobs, timeout) {
        (Some(_), Some(t)) if t <= ctx.bash_timeout => (t, true),
        (Some(_), _) => (ctx.bash_timeout, false),
        (None, t) => (t.unwrap_or(ctx.bash_timeout), true),
    };
    let waited = tokio::select! {
        r = child.wait() => Waited::Exited(r),
        _ = tokio::time::sleep(limit) => Waited::Limit,
        _ = ctx.cancel.cancelled() => Waited::Cancelled,
    };
    let status = match waited {
        Waited::Exited(r) => r,
        Waited::Cancelled => {
            kill_group(pid);
            return ToolOutput::err("command interrupted by user");
        }
        Waited::Limit if kill_at_limit => {
            kill_group(pid);
            return ToolOutput::err(format!("command timed out after {}s", limit.as_secs()));
        }
        Waited::Limit => {
            let jobs = jobs.expect("only jobs move commands to the background");
            let so_far = {
                let c = capture.lock().unwrap_or_else(|e| e.into_inner());
                format_output(&c.stdout, &c.stderr)
            };
            return match jobs.adopt(command, started, child, capture, readers, deadline) {
                Ok((id, log)) => ToolOutput::ok(format!(
                    "Still running after {}s, so it continues as background job {id}; the rest of its output goes to {}. You will be told when it finishes.\n{}",
                    limit.as_secs(),
                    log.display(),
                    if so_far.trim().is_empty() { "(no output yet)".to_owned() } else { format!("Output so far:\n{so_far}") }
                )),
                Err(e) => {
                    kill_group(pid);
                    ToolOutput::err(format!("command timed out after {}s and could not move to the background: {e}", limit.as_secs()))
                }
            };
        }
    };
    for r in readers {
        let _ = r.await;
    }
    let status = match status {
        Ok(s) => s,
        Err(e) => return ToolOutput::err(format!("waiting for command: {e}")),
    };
    let c = capture.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = format_output(&c.stdout, &c.stderr);
    let ok = status.success();
    if !ok {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        match status.code() {
            Some(c) => text.push_str(&format!("[exit code {c}]")),
            None => text.push_str("[terminated by signal]"),
        }
    }
    if text.trim().is_empty() {
        text = "(no output)".into();
    }
    ToolOutput { content: text, is_error: !ok }
}

/// Stdout, then stderr under a `[stderr]` marker.
fn format_output(stdout: &[u8], stderr: &[u8]) -> String {
    let mut text = String::from_utf8_lossy(stdout).into_owned();
    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.trim().is_empty() {
        if !text.is_empty() && !text.ends_with('\n') {
            text.push('\n');
        }
        text.push_str("[stderr]\n");
        text.push_str(&stderr);
    }
    text
}

/// Kills the whole process group so background children of the command die too.
pub(crate) fn kill_group(pid: Option<u32>) {
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
