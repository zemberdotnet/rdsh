//! Messages between the hub and its clients.
//!
//! Every type here is serializable so the same messages can cross a socket unchanged when the
//! hub runs in its own process (`rdsh serve`) or on another machine.

use serde::{Deserialize, Serialize};

use crate::{
    agent::AgentEvent,
    diff::FileDiff,
    llm::{Effort, Message, Usage},
    trace::SessionInfo,
};

/// Client → hub.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Request {
    /// Ask for a `Sessions` notice.
    List,
    /// Start a new session in `cwd` (the host's default directory when absent); answered with
    /// `Opened`.
    New {
        #[serde(default)]
        cwd: Option<String>,
    },
    /// Attach to a session (id, unique prefix, or `last`), resuming it from its trace when no
    /// agent runs it yet; answered with `Opened`.
    Open { session_id: String },
    /// Queue a user turn. Turns run one at a time in arrival order, each with its own thinking
    /// effort (`high` from clients that do not send one) and model (`None` keeps the session's).
    Send {
        session_id: String,
        text: String,
        #[serde(default)]
        effort: Effort,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
    },
    /// Interrupt the running turn.
    Cancel { session_id: String },
    /// Stop receiving events for a session. Its agent keeps running.
    Close { session_id: String },
    /// Load `path` (relative to `cwd`) with its git diff on the host; answered with `File`.
    ReadFile { cwd: String, path: String },
    /// Files with uncommitted changes under `cwd` on the host; answered with `ChangedFiles`.
    ChangedFiles { cwd: String },
    /// Read a recorded session without running it (subagents, sessions owned elsewhere);
    /// answered with `Trace`.
    ReadTrace { session_id: String },
}

/// Hub → client.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Notice {
    /// Recorded sessions plus which of them the hub currently runs. Sent on `List` and whenever
    /// that set changes.
    Sessions { recorded: Vec<SessionInfo>, open: Vec<String>, running: Vec<String> },
    /// The client is now subscribed to `snapshot.session_id`; events follow the snapshot.
    Opened { snapshot: Snapshot },
    /// An event of a subscribed session, in order.
    Event { session_id: String, event: AgentEvent },
    /// Reply to `ReadTrace`; `cwd` is where the session ran.
    Trace { session_id: String, cwd: String, history: Vec<Message> },
    /// Reply to `ReadFile`.
    File { file: Box<FileDiff> },
    /// Reply to `ChangedFiles`: `(git status, path relative to cwd)` pairs.
    ChangedFiles { cwd: String, files: Vec<(String, String)> },
    Error { session_id: Option<String>, message: String },
}

/// Everything needed to show a session joined mid-flight: the history as of the start of the
/// current turn, plus that turn's events so far (beginning with `TurnStarted`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub session_id: String,
    pub model: String,
    /// Working directory of the session on its host.
    pub cwd: String,
    pub history: Vec<Message>,
    pub turn: Vec<AgentEvent>,
    pub running: bool,
    pub usage: Usage,
}
