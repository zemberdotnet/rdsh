//! `glob` and `grep`, gitignore-aware via the `ignore` walker.

use std::{
    path::Path,
    time::SystemTime,
};

use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, opt_bool, opt_str, resolve_path, schema, str_arg};
use crate::llm::{BoxFuture, ToolSpec};

const MAX_GLOB_RESULTS: usize = 500;
const MAX_GREP_MATCHES: usize = 300;
const MAX_MATCH_LINE_CHARS: usize = 300;

fn walker(root: &Path) -> ignore::Walk {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().into_owned()
}

pub struct Glob;

impl Tool for Glob {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "glob".into(),
            description: "Find files by glob pattern (e.g. `**/*.rs`, `src/**/mod.rs`), respecting .gitignore. \
                          Results are sorted by modification time, newest first."
                .into(),
            parameters: schema(
                json!({
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Directory to search (default: working directory)"}
                }),
                &["pattern"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let pattern = match str_arg(&args, "pattern") {
                Ok(p) => p,
                Err(e) => return ToolOutput::err(e),
            };
            let root = opt_str(&args, "path").map_or_else(|| ctx.cwd.clone(), |p| resolve_path(&ctx.cwd, &p));
            let res = tokio::task::spawn_blocking(move || glob(&root, &pattern)).await;
            ToolOutput::from_result(res.unwrap_or_else(|e| Err(format!("glob task failed: {e}"))))
        })
    }
}

fn glob(root: &Path, pattern: &str) -> Result<String, String> {
    let matcher = globset::Glob::new(pattern).map_err(|e| e.to_string())?.compile_matcher();
    let mut hits: Vec<(SystemTime, String)> = walker(root)
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|e| {
            let r = rel(root, e.path());
            matcher.is_match(&r).then(|| {
                let mtime = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(SystemTime::UNIX_EPOCH);
                (mtime, r)
            })
        })
        .collect();
    if hits.is_empty() {
        return Ok(format!("No files match {pattern}"));
    }
    hits.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    let total = hits.len();
    let mut out: Vec<String> = hits.into_iter().take(MAX_GLOB_RESULTS).map(|(_, p)| p).collect();
    if total > MAX_GLOB_RESULTS {
        out.push(format!("… {} more files", total - MAX_GLOB_RESULTS));
    }
    Ok(out.join("\n"))
}

pub struct Grep;

impl Tool for Grep {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "grep".into(),
            description: "Search file contents with a regular expression (Rust regex syntax), respecting \
                          .gitignore. Output lines are `path:line: text`."
                .into(),
            parameters: schema(
                json!({
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "File or directory to search (default: working directory)"},
                    "glob": {"type": "string", "description": "Only search files whose relative path matches this glob"},
                    "case_insensitive": {"type": "boolean"}
                }),
                &["pattern"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let pattern = match str_arg(&args, "pattern") {
                Ok(p) => p,
                Err(e) => return ToolOutput::err(e),
            };
            let root = opt_str(&args, "path").map_or_else(|| ctx.cwd.clone(), |p| resolve_path(&ctx.cwd, &p));
            let filter = opt_str(&args, "glob");
            let ci = opt_bool(&args, "case_insensitive");
            let cwd = ctx.cwd.clone();
            let res = tokio::task::spawn_blocking(move || grep(&cwd, &root, &pattern, filter.as_deref(), ci)).await;
            ToolOutput::from_result(res.unwrap_or_else(|e| Err(format!("grep task failed: {e}"))))
        })
    }
}

fn grep(cwd: &Path, root: &Path, pattern: &str, filter: Option<&str>, ci: bool) -> Result<String, String> {
    let re = regex::RegexBuilder::new(pattern).case_insensitive(ci).build().map_err(|e| e.to_string())?;
    let filter = filter
        .map(|g| globset::Glob::new(g).map(|g| g.compile_matcher()))
        .transpose()
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut total = 0usize;
    for entry in walker(root).filter_map(Result::ok) {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if let Some(f) = &filter
            && !f.is_match(rel(root, path))
            && !f.is_match(path.file_name().unwrap_or_default())
        {
            continue;
        }
        let Ok(bytes) = std::fs::read(path) else { continue };
        if bytes[..bytes.len().min(8000)].contains(&0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        // Paths are shown relative to the working directory so they are clickable in the TUI.
        let shown = rel(cwd, path);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                total += 1;
                if out.len() < MAX_GREP_MATCHES {
                    let mut l: String = line.chars().take(MAX_MATCH_LINE_CHARS).collect();
                    if line.chars().count() > MAX_MATCH_LINE_CHARS {
                        l.push('…');
                    }
                    out.push(format!("{shown}:{}: {}", i + 1, l.trim_end()));
                }
            }
        }
    }
    if out.is_empty() {
        return Ok(format!("No matches for {pattern}"));
    }
    if total > out.len() {
        out.push(format!("… {} more matches; narrow the pattern or path", total - out.len()));
    }
    Ok(out.join("\n"))
}
