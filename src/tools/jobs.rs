//! Background shell jobs: commands started with `run_in_background`, or still running when the
//! foreground limit passes. Output goes to a log file per job. The agent hears about jobs in
//! short `<background_jobs>` notes: which are still running at the start of each turn, and each
//! job's end as soon as it finishes (mid-turn, or in a turn of its own when the session is idle).

use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use tokio::{io::AsyncReadExt, sync::Notify, task::JoinHandle};

use super::{Tool, ToolCtx, ToolOutput, opt_u64, schema};
use crate::llm::{BoxFuture, ToolSpec};

/// Opening tag of every job note; the TUI shows user messages starting with it as status lines.
pub const NOTE_TAG: &str = "<background_jobs>";
const NOTE_END: &str = "</background_jobs>";
/// Output lines quoted in a job's completion note.
const TAIL_LINES: usize = 10;
const TAIL_CHARS: usize = 1500;

/// Whether a user message is a job note written by the harness rather than by the user.
pub fn is_note(text: &str) -> bool {
    text.starts_with(NOTE_TAG)
}

/// The note without its tags, for display.
pub fn note_body(text: &str) -> &str {
    text.strip_prefix(NOTE_TAG).and_then(|t| t.strip_suffix(NOTE_END)).unwrap_or(text).trim()
}

/// Output of a running command, collected in memory until the command moves to the background,
/// then appended to the job's log.
#[derive(Default)]
pub struct Capture {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    file: Option<File>,
}

pub type SharedCapture = Arc<Mutex<Capture>>;

/// Copies `stream` into `capture` (`err` picks stderr) until it closes.
pub fn read_into<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    mut stream: R,
    capture: SharedCapture,
    err: bool,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut buf = [0u8; 8192];
        while let Ok(n) = stream.read(&mut buf).await {
            if n == 0 {
                break;
            }
            let mut c = capture.lock().unwrap_or_else(|e| e.into_inner());
            match &mut c.file {
                // A failed log write loses output, not the job.
                Some(f) => drop(f.write_all(&buf[..n])),
                None if err => c.stderr.extend_from_slice(&buf[..n]),
                None => c.stdout.extend_from_slice(&buf[..n]),
            }
        }
    })
}

struct Job {
    id: u32,
    command: String,
    started: Instant,
    log: PathBuf,
    pid: Option<u32>,
    /// Bytes of the log already returned by `job_output` (or shown when the job started).
    read: u64,
    killed: bool,
    end: Option<(String, Duration)>,
    /// Whether the agent has been told how the job ended.
    reported: bool,
}

/// The background jobs of one session.
pub struct Jobs {
    dir: PathBuf,
    table: Mutex<Vec<Job>>,
    finished: Notify,
}

impl Jobs {
    /// @param dir where job logs are written (created on first use).
    pub fn new(dir: PathBuf) -> Arc<Self> {
        Arc::new(Self { dir, table: Mutex::new(Vec::new()), finished: Notify::new() })
    }

    fn table(&self) -> std::sync::MutexGuard<'_, Vec<Job>> {
        self.table.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes over a running command: its output so far starts the log, the rest follows.
    ///
    /// @param deadline kills the job at this instant, when set.
    /// @returns the job id, log path and how many bytes the log starts with.
    pub fn adopt(
        self: &Arc<Self>,
        command: &str,
        started: Instant,
        mut child: tokio::process::Child,
        capture: SharedCapture,
        readers: Vec<JoinHandle<()>>,
        deadline: Option<Instant>,
    ) -> std::io::Result<(u32, PathBuf)> {
        std::fs::create_dir_all(&self.dir)?;
        let mut table = self.table();
        let id = table.iter().map(|j| j.id).max().unwrap_or(0) + 1;
        let log = self.dir.join(format!("{id}.log"));
        let mut file = File::create(&log)?;
        let read = {
            let mut c = capture.lock().unwrap_or_else(|e| e.into_inner());
            file.write_all(&c.stdout)?;
            if !c.stderr.is_empty() {
                if !c.stdout.is_empty() && !c.stdout.ends_with(b"\n") {
                    file.write_all(b"\n")?;
                }
                file.write_all(b"[stderr]\n")?;
                file.write_all(&c.stderr)?;
            }
            let read = file.stream_position()?;
            c.file = Some(file);
            read
        };
        let pid = child.id();
        table.push(Job { id, command: command.to_owned(), started, log: log.clone(), pid, read, killed: false, end: None, reported: false });
        drop(table);
        let jobs = self.clone();
        tokio::spawn(async move {
            let status = match deadline {
                Some(d) => tokio::select! {
                    s = child.wait() => Some(s),
                    _ = tokio::time::sleep_until(d.into()) => None,
                },
                None => Some(child.wait().await),
            };
            if status.is_none() {
                super::bash::kill_group(pid);
                let _ = child.wait().await;
            }
            for r in readers {
                let _ = r.await;
            }
            let mut table = jobs.table();
            let Some(job) = table.iter_mut().find(|j| j.id == id) else { return };
            let how = match status {
                _ if job.killed => "killed".to_owned(),
                None => "timed out".to_owned(),
                Some(Ok(s)) => s.code().map_or_else(|| "terminated by signal".to_owned(), |c| format!("exit {c}")),
                Some(Err(e)) => format!("lost ({e})"),
            };
            job.end = Some((how, job.started.elapsed()));
            drop(table);
            jobs.finished.notify_one();
        });
        Ok((id, log))
    }

    /// Resolves when a job has finished since the last call (possibly before it).
    pub async fn finished(&self) {
        self.finished.notified().await
    }

    pub fn any_running(&self) -> bool {
        self.table().iter().any(|j| j.end.is_none())
    }

    /// Whether a finished job has not been reported to the agent yet.
    pub fn has_news(&self) -> bool {
        self.table().iter().any(|j| j.end.is_some() && !j.reported)
    }

    /// Completion lines for jobs that ended since the last call, each with its output's tail.
    pub fn take_finished(&self) -> Vec<String> {
        let mut table = self.table();
        table
            .iter_mut()
            .filter(|j| j.end.is_some() && !j.reported)
            .map(|j| {
                j.reported = true;
                let (how, took) = j.end.as_ref().expect("filtered on end");
                let tail = tail(&j.log);
                let mut line = format!("job {} `{}` finished: {how} after {}.", j.id, short(&j.command), clock(*took));
                if tail.is_empty() {
                    line.push_str(" No output.");
                } else {
                    line.push_str(&format!(" Last output:\n{tail}\n(job_output {} for the rest)", j.id));
                }
                line
            })
            .collect()
    }

    /// One line naming the jobs still running and for how long, or None when none are.
    pub fn running_line(&self) -> Option<String> {
        let table = self.table();
        let running: Vec<String> = table
            .iter()
            .filter(|j| j.end.is_none())
            .map(|j| format!("job {} `{}` ({})", j.id, short(&j.command), clock(j.started.elapsed())))
            .collect();
        (!running.is_empty()).then(|| format!("Still running: {}", running.join(", ")))
    }

    /// The note for the start of a turn, or None when there is nothing to say.
    pub fn turn_note(&self) -> Option<String> {
        note(self.take_finished().into_iter().chain(self.running_line()).collect())
    }

    /// The note for jobs that finished mid-turn, or None.
    pub fn finished_note(&self) -> Option<String> {
        note(self.take_finished())
    }

    /// Kills every running job (the session is going away).
    pub fn kill_all(&self) {
        for j in self.table().iter_mut().filter(|j| j.end.is_none()) {
            j.killed = true;
            super::bash::kill_group(j.pid);
        }
    }

    fn output(&self, id: u32) -> Result<String, String> {
        let mut table = self.table();
        let job = table.iter_mut().find(|j| j.id == id).ok_or_else(|| format!("no job {id}"))?;
        let mut text = String::new();
        if let Ok(mut f) = File::open(&job.log) {
            let _ = f.seek(SeekFrom::Start(job.read));
            let mut bytes = Vec::new();
            let _ = f.read_to_end(&mut bytes);
            job.read += bytes.len() as u64;
            text = String::from_utf8_lossy(&bytes).into_owned();
        }
        let status = match &job.end {
            None => format!("job {id} running for {}", clock(job.started.elapsed())),
            Some((how, took)) => {
                job.reported = true;
                format!("job {id} finished: {how} after {}", clock(*took))
            }
        };
        let body = if text.trim().is_empty() { "(no new output)".to_owned() } else { text };
        Ok(format!("[{status}; log {}]\n{body}", job.log.display()))
    }

    fn kill(&self, id: u32) -> Result<String, String> {
        let mut table = self.table();
        let job = table.iter_mut().find(|j| j.id == id).ok_or_else(|| format!("no job {id}"))?;
        if let Some((how, _)) = &job.end {
            return Ok(format!("job {id} already finished: {how}"));
        }
        job.killed = true;
        // The agent asked for this, so it needs no completion note.
        job.reported = true;
        super::bash::kill_group(job.pid);
        Ok(format!("killed job {id}"))
    }
}

fn note(lines: Vec<String>) -> Option<String> {
    (!lines.is_empty()).then(|| format!("{NOTE_TAG}\n{}\n{NOTE_END}", lines.join("\n")))
}

/// First line of a command, shortened for notes.
fn short(command: &str) -> String {
    let first = command.lines().next().unwrap_or("").trim();
    if first.chars().count() > 60 || command.trim().lines().count() > 1 {
        format!("{}…", first.chars().take(60).collect::<String>())
    } else {
        first.to_owned()
    }
}

/// `41s`, `3m12s`, `1h04m`.
pub fn clock(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0..60 => format!("{s}s"),
        60..3600 => format!("{}m{:02}s", s / 60, s % 60),
        _ => format!("{}h{:02}m", s / 3600, s / 60 % 60),
    }
}

/// The last lines of a log, at most `TAIL_CHARS`.
fn tail(log: &Path) -> String {
    let Ok(mut f) = File::open(log) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let _ = f.seek(SeekFrom::Start(len.saturating_sub(TAIL_CHARS as u64 * 4)));
    let mut bytes = Vec::new();
    let _ = f.read_to_end(&mut bytes);
    let text = String::from_utf8_lossy(&bytes);
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let kept = lines[lines.len().saturating_sub(TAIL_LINES)..].join("\n");
    let n = kept.chars().count();
    if n > TAIL_CHARS { kept.chars().skip(n - TAIL_CHARS).collect() } else { kept }
}

fn job_id(args: &Value) -> Result<u32, String> {
    opt_u64(args, "id")?.map(|n| n as u32).ok_or_else(|| "missing required integer argument `id`".to_owned())
}

fn id_schema() -> Value {
    schema(json!({"id": {"type": "integer", "description": "Job number"}}), &["id"])
}

fn with_jobs(ctx: &ToolCtx, f: impl FnOnce(&Jobs) -> Result<String, String>) -> ToolOutput {
    match &ctx.jobs {
        Some(j) => ToolOutput::from_result(f(j)),
        None => ToolOutput::err("background jobs are not available here"),
    }
}

/// `job_output`: a background job's status and the output it wrote since the last check.
pub struct JobOutput;

impl Tool for JobOutput {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_output".into(),
            description: "Status of a background bash job and the output it produced since you last checked.".into(),
            parameters: id_schema(),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { with_jobs(ctx, |j| j.output(job_id(&args)?)) })
    }
}

/// `job_kill`: stops a background job and everything it started.
pub struct JobKill;

impl Tool for JobKill {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "job_kill".into(),
            description: "Stop a background bash job and its child processes.".into(),
            parameters: id_schema(),
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { with_jobs(ctx, |j| j.kill(job_id(&args)?)) })
    }
}
