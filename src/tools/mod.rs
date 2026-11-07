//! Tool registry and built-in tools.

pub mod bash;
pub mod fs;
pub mod search;
pub mod task;

use std::{
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    agent::EventSink,
    llm::{BoxFuture, ToolSpec},
};

/// Result of one tool call. `content` is exactly what the model sees.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn ok(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn err(message: impl std::fmt::Display) -> Self {
        Self { content: format!("Error: {message}"), is_error: true }
    }

    pub fn from_result(r: Result<String, String>) -> Self {
        r.map_or_else(Self::err, Self::ok)
    }
}

/// Per-call execution context.
#[derive(Clone)]
pub struct ToolCtx {
    pub cwd: PathBuf,
    /// Cancelled when the user interrupts the turn.
    pub cancel: CancellationToken,
    /// Event sink of the agent running this tool (subagents forward through it).
    pub sink: EventSink,
    pub call_id: String,
    pub bash_timeout: Duration,
}

pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;

    /// Whether this tool may run concurrently with other parallel-safe calls of one response.
    fn parallel_safe(&self) -> bool {
        false
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput>;
}

/// Name-indexed tool set offered to one agent.
#[derive(Clone, Default)]
pub struct Registry {
    tools: Vec<(String, Arc<dyn Tool>)>,
}

impl Registry {
    /// Adds or replaces the tool with the same name.
    pub fn add(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.spec().name;
        self.tools.retain(|(n, _)| *n != name);
        self.tools.push((name, tool));
    }

    pub fn get(&self, name: &str) -> Option<&Arc<dyn Tool>> {
        self.tools.iter().find(|(n, _)| n == name).map(|(_, t)| t)
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools.iter().map(|(_, t)| t.spec()).collect()
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.tools.iter().map(|(n, _)| n.as_str())
    }
}

/// File, shell and search tools (everything except `task`).
pub fn builtin() -> Registry {
    let mut r = Registry::default();
    r.add(Arc::new(fs::ReadFile));
    r.add(Arc::new(fs::WriteFile));
    r.add(Arc::new(fs::EditFile));
    r.add(Arc::new(bash::Bash));
    r.add(Arc::new(search::Glob));
    r.add(Arc::new(search::Grep));
    r
}

/// Resolves a model-supplied path against `cwd`, expanding a leading `~/`.
pub fn resolve_path(cwd: &Path, p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/")
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(rest);
    }
    let path = Path::new(p);
    if path.is_absolute() { path.to_path_buf() } else { cwd.join(path) }
}

pub(crate) fn str_arg(args: &Value, key: &str) -> Result<String, String> {
    args.get(key)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("missing required string argument `{key}`"))
}

pub(crate) fn opt_str(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_owned)
}

pub(crate) fn opt_u64(args: &Value, key: &str) -> Result<Option<u64>, String> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Number(n)) => n.as_u64().map(Some).ok_or_else(|| format!("`{key}` must be a non-negative integer")),
        Some(Value::String(s)) => s.parse().map(Some).map_err(|_| format!("`{key}` must be a non-negative integer")),
        Some(_) => Err(format!("`{key}` must be a non-negative integer")),
    }
}

pub(crate) fn opt_bool(args: &Value, key: &str) -> bool {
    match args.get(key) {
        Some(Value::Bool(b)) => *b,
        Some(Value::String(s)) => s == "true",
        _ => false,
    }
}

/// One-line human summary of a call, used by the TUI and headless output.
pub fn summarize(name: &str, arguments: &str) -> String {
    let args: Value = serde_json::from_str(arguments).unwrap_or(Value::Null);
    let get = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
    let s = match name {
        "bash" => get("command"),
        "read_file" | "write_file" | "edit_file" => get("path"),
        "glob" => get("pattern"),
        "grep" => {
            let path = get("path");
            if path.is_empty() { get("pattern") } else { format!("{}  in {path}", get("pattern")) }
        }
        "task" => get("description"),
        _ => arguments.to_owned(),
    };
    let first = s.lines().next().unwrap_or("");
    let mut out: String = first.chars().take(120).collect();
    if first.chars().count() > 120 || s.lines().count() > 1 {
        out.push('…');
    }
    out
}

/// Tool-JSON schema helper: an object with the given properties and required keys.
pub(crate) fn schema(props: Value, required: &[&str]) -> Value {
    serde_json::json!({"type": "object", "properties": props, "required": required})
}
