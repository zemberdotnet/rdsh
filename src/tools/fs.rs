//! `read_file`, `write_file` and `edit_file`.

use serde_json::{Value, json};

use super::{Tool, ToolCtx, ToolOutput, opt_bool, opt_u64, resolve_path, schema, str_arg};
use crate::llm::{BoxFuture, ToolSpec};

const DEFAULT_READ_LIMIT: u64 = 2000;
const MAX_LINE_CHARS: usize = 2000;

pub struct ReadFile;

impl Tool for ReadFile {
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Use read_file — not shell commands like cat, sed, head, or tail — to inspect text files. Use offset and limit to read a line range or continue a large file.",
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file with line numbers (`   12\\tline`). Reading a directory lists it. \
                          Use offset/limit for large files."
                .into(),
            parameters: schema(
                json!({
                    "path": {"type": "string", "description": "File path, absolute or relative to the working directory"},
                    "offset": {"type": "integer", "description": "1-based first line to read (default 1)"},
                    "limit": {"type": "integer", "description": "Maximum lines to read (default 2000)"}
                }),
                &["path"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move { ToolOutput::from_result(read(&args, ctx).await) })
    }
}

async fn read(args: &Value, ctx: &ToolCtx) -> Result<String, String> {
    let raw = str_arg(args, "path")?;
    let path = resolve_path(&ctx.cwd, &raw);
    let meta = tokio::fs::metadata(&path).await.map_err(|e| format!("{raw}: {e}"))?;
    if meta.is_dir() {
        let mut rd = tokio::fs::read_dir(&path).await.map_err(|e| format!("{raw}: {e}"))?;
        let mut names = Vec::new();
        while let Some(entry) = rd.next_entry().await.map_err(|e| e.to_string())? {
            let dir = entry.file_type().await.is_ok_and(|t| t.is_dir());
            names.push(format!("{}{}", entry.file_name().to_string_lossy(), if dir { "/" } else { "" }));
        }
        names.sort();
        return Ok(format!("{raw} is a directory:\n{}", names.join("\n")));
    }
    let bytes = tokio::fs::read(&path).await.map_err(|e| format!("{raw}: {e}"))?;
    if bytes[..bytes.len().min(8000)].contains(&0) {
        return Ok(format!("{raw} is a binary file ({} bytes)", bytes.len()));
    }
    let text = String::from_utf8_lossy(&bytes);
    if text.is_empty() {
        return Ok(format!("{raw} is empty"));
    }
    let offset = opt_u64(args, "offset")?.unwrap_or(1).max(1) as usize;
    let limit = opt_u64(args, "limit")?.unwrap_or(DEFAULT_READ_LIMIT).max(1) as usize;
    let total = text.lines().count();
    if offset > total {
        return Err(format!("offset {offset} is past the end of {raw} ({total} lines)"));
    }
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(offset - 1).take(limit) {
        let line: String = if line.chars().count() > MAX_LINE_CHARS {
            line.chars().take(MAX_LINE_CHARS).chain("…".chars()).collect()
        } else {
            line.to_owned()
        };
        out.push_str(&format!("{:>6}\t{line}\n", i + 1));
    }
    let shown_end = (offset - 1 + limit).min(total);
    if shown_end < total {
        out.push_str(&format!("… {} more lines (continue with offset {})\n", total - shown_end, shown_end + 1));
    }
    Ok(out)
}

pub struct WriteFile;

impl Tool for WriteFile {
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Read an existing file before overwriting it with write_file, and prefer edit_file for targeted changes.",
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "write_file".into(),
            description: "Create or overwrite a file with the given content. Parent directories are created.".into(),
            parameters: schema(
                json!({
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "Complete new file content"}
                }),
                &["path", "content"],
            ),
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            ToolOutput::from_result(async {
                let raw = str_arg(&args, "path")?;
                let content = str_arg(&args, "content")?;
                let path = resolve_path(&ctx.cwd, &raw);
                let existed = tokio::fs::try_exists(&path).await.unwrap_or(false);
                if let Some(parent) = path.parent() {
                    tokio::fs::create_dir_all(parent).await.map_err(|e| format!("{raw}: {e}"))?;
                }
                tokio::fs::write(&path, &content).await.map_err(|e| format!("{raw}: {e}"))?;
                Ok(format!(
                    "{} {raw} ({} lines)",
                    if existed { "Overwrote" } else { "Created" },
                    content.lines().count()
                ))
            }
            .await)
        })
    }
}

pub struct EditFile;

impl Tool for EditFile {
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Change files only with edit_file or write_file — never with sed -i, perl -pi, awk, or scripts that edit by line number. Read a file before editing it unless you just created or edited it in this session, and include enough surrounding lines in old_string to match one place.",
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "edit_file".into(),
            description: "Replace an exact string in a file. `old_string` must match exactly once unless \
                          `replace_all` is true; include surrounding lines to make it unique. Read the file first."
                .into(),
            parameters: schema(
                json!({
                    "path": {"type": "string"},
                    "old_string": {"type": "string", "description": "Exact text to replace (whitespace-sensitive)"},
                    "new_string": {"type": "string", "description": "Replacement text"},
                    "replace_all": {"type": "boolean", "description": "Replace every occurrence (default false)"}
                }),
                &["path", "old_string", "new_string"],
            ),
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            ToolOutput::from_result(async {
                let raw = str_arg(&args, "path")?;
                let old = str_arg(&args, "old_string")?;
                let new = str_arg(&args, "new_string")?;
                let all = opt_bool(&args, "replace_all");
                if old.is_empty() {
                    return Err("old_string must not be empty; use write_file to create files".into());
                }
                if old == new {
                    return Err("old_string and new_string are identical".into());
                }
                let path = resolve_path(&ctx.cwd, &raw);
                let text = tokio::fs::read_to_string(&path).await.map_err(|e| format!("{raw}: {e}"))?;
                let count = text.matches(old.as_str()).count();
                match count {
                    0 => return Err(format!("old_string not found in {raw}")),
                    n if n > 1 && !all => {
                        return Err(format!(
                            "old_string matches {n} times in {raw}; add surrounding context or set replace_all"
                        ));
                    }
                    _ => {}
                }
                let first = text.find(old.as_str()).unwrap_or(0);
                let line = text[..first].matches('\n').count() + 1;
                let updated = if all { text.replace(old.as_str(), &new) } else { text.replacen(old.as_str(), &new, 1) };
                tokio::fs::write(&path, updated).await.map_err(|e| format!("{raw}: {e}"))?;
                Ok(format!(
                    "Edited {raw}:{line} ({} replacement{})",
                    if all { count } else { 1 },
                    if all && count != 1 { "s" } else { "" }
                ))
            }
            .await)
        })
    }
}
