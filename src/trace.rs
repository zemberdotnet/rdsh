//! Append-only JSONL session traces.
//!
//! Every message that reaches the model is recorded as a `message` event, so a session's
//! model-visible history can be rebuilt exactly from its trace (`load`). Other events
//! (usage, tool timing, subagent links, errors) are metadata for inspection.

use std::{
    fs::File,
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    time::SystemTime,
};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

use crate::{
    llm::{Message, Usage},
    util::now_ms,
};

/// Trace format version written in `session_start`.
pub const TRACE_FORMAT: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TraceEvent {
    SessionStart {
        session_id: String,
        model: String,
        cwd: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent: Option<String>,
        format: u32,
    },
    Message {
        message: Message,
    },
    ToolMeta {
        call_id: String,
        is_error: bool,
        duration_ms: u64,
    },
    Usage {
        usage: Usage,
    },
    Error {
        message: String,
    },
    Cancelled,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub ts: u64,
    #[serde(flatten)]
    pub event: TraceEvent,
}

/// Writer for one session's trace file.
pub struct Trace {
    file: File,
    pub path: PathBuf,
    pub session_id: String,
}

impl Trace {
    /// Creates `<dir>/<session_id>.jsonl` and writes its `session_start` record.
    pub fn create(dir: &Path, session_id: &str, model: &str, cwd: &Path, parent: Option<&str>) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join(format!("{session_id}.jsonl"));
        let file = File::options()
            .create_new(true)
            .append(true)
            .open(&path)
            .with_context(|| format!("creating trace {}", path.display()))?;
        let mut t = Self { file, path, session_id: session_id.to_owned() };
        t.record(TraceEvent::SessionStart {
            session_id: session_id.to_owned(),
            model: model.to_owned(),
            cwd: cwd.display().to_string(),
            parent: parent.map(str::to_owned),
            format: TRACE_FORMAT,
        })?;
        Ok(t)
    }

    /// Reopens an existing trace for appending (session resume).
    pub fn open_append(path: &Path, session_id: &str) -> Result<Self> {
        let file = File::options()
            .append(true)
            .open(path)
            .with_context(|| format!("opening trace {}", path.display()))?;
        Ok(Self { file, path: path.to_owned(), session_id: session_id.to_owned() })
    }

    /// Appends one event as a single JSON line.
    pub fn record(&mut self, event: TraceEvent) -> Result<()> {
        let mut line = serde_json::to_string(&Record { ts: now_ms(), event })?;
        line.push('\n');
        self.file.write_all(line.as_bytes()).with_context(|| format!("writing {}", self.path.display()))
    }
}

/// A trace read back from disk.
pub struct Loaded {
    pub session_id: String,
    pub model: String,
    pub cwd: String,
    pub parent: Option<String>,
    pub messages: Vec<Message>,
    pub records: Vec<Record>,
}

/// Reads a trace. A malformed *final* line (torn write after a crash) is ignored;
/// malformed lines elsewhere are an error.
pub fn load(path: &Path) -> Result<Loaded> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let lines: Vec<String> = BufReader::new(file).lines().collect::<std::io::Result<_>>()?;
    let mut records = Vec::new();
    let last = lines.iter().rposition(|l| !l.trim().is_empty());
    for (i, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Record>(line) {
            Ok(r) => records.push(r),
            Err(_) if Some(i) == last => break,
            Err(e) => bail!("{}:{}: invalid trace record: {e}", path.display(), i + 1),
        }
    }
    let Some(Record { event: TraceEvent::SessionStart { session_id, model, cwd, parent, format }, .. }) =
        records.first().cloned()
    else {
        bail!("{}: trace does not start with session_start", path.display());
    };
    if format > TRACE_FORMAT {
        bail!("{}: trace format {format} is newer than supported ({TRACE_FORMAT})", path.display());
    }
    let messages = records
        .iter()
        .filter_map(|r| match &r.event {
            TraceEvent::Message { message } => Some(message.clone()),
            _ => None,
        })
        .collect();
    Ok(Loaded { session_id, model, cwd, parent, messages, records })
}

/// Summary row for `rdsh sessions`.
pub struct SessionInfo {
    pub id: String,
    pub path: PathBuf,
    pub modified: SystemTime,
    pub parent: Option<String>,
    /// Working directory the session ran in.
    pub cwd: String,
    pub title: String,
}

/// Lists sessions newest first. Reads only the first few lines of each file.
pub fn list_sessions(dir: &Path) -> Vec<SessionInfo> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<SessionInfo> = rd
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| {
            let path = e.path();
            let modified = e.metadata().ok()?.modified().ok()?;
            let file = File::open(&path).ok()?;
            let mut parent = None;
            let mut cwd = String::new();
            let mut title = String::new();
            for line in BufReader::new(file).lines().take(8).map_while(Result::ok) {
                match serde_json::from_str::<Record>(&line).map(|r| r.event) {
                    Ok(TraceEvent::SessionStart { parent: p, cwd: c, .. }) => {
                        parent = p;
                        cwd = c;
                    }
                    Ok(TraceEvent::Message { message: Message::User { content } }) => {
                        title = content.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(80).collect();
                        break;
                    }
                    _ => {}
                }
            }
            let id = path.file_stem()?.to_string_lossy().into_owned();
            Some(SessionInfo { id, path, modified, parent, cwd, title })
        })
        .collect();
    out.sort_by_key(|s| std::cmp::Reverse(s.modified));
    out
}

/// Finds a session by id, unique id prefix, or `last` (newest top-level session).
pub fn find_session(dir: &Path, query: &str) -> Result<PathBuf> {
    let sessions = list_sessions(dir);
    if query == "last" {
        return sessions
            .into_iter()
            .find(|s| s.parent.is_none())
            .map(|s| s.path)
            .context("no sessions found");
    }
    let matches: Vec<_> = sessions.into_iter().filter(|s| s.id.starts_with(query)).collect();
    match matches.len() {
        0 => bail!("no session matches {query}"),
        1 => Ok(matches.into_iter().next().unwrap().path),
        n => {
            if let Some(exact) = matches.iter().find(|s| s.id == query) {
                return Ok(exact.path.clone());
            }
            bail!("{n} sessions match {query}; use a longer prefix")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_torn_tail() {
        let dir = tempfile::tempdir().unwrap();
        let mut t = Trace::create(dir.path(), "s1", "m", Path::new("/w"), None).unwrap();
        t.record(TraceEvent::Message { message: Message::user("hello") }).unwrap();
        t.record(TraceEvent::Usage { usage: Usage { prompt_tokens: 1, completion_tokens: 2, cached_tokens: 0 } })
            .unwrap();
        drop(t);
        let mut f = File::options().append(true).open(dir.path().join("s1.jsonl")).unwrap();
        f.write_all(b"{\"ts\":1,\"type\":\"mess").unwrap();
        let l = load(&dir.path().join("s1.jsonl")).unwrap();
        assert_eq!(l.session_id, "s1");
        assert_eq!(l.messages, vec![Message::user("hello")]);
        assert_eq!(l.records.len(), 3);
        let s = list_sessions(dir.path());
        assert_eq!(s[0].title, "hello");
        assert_eq!(find_session(dir.path(), "last").unwrap(), dir.path().join("s1.jsonl"));
    }
}
