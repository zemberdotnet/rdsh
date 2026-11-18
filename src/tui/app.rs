//! TUI state: modes, views, key and mouse handling, and folding hub events into transcripts.
//!
//! The TUI never runs agents. It sends [`Request`]s to the hub and renders the [`Notice`]s it
//! gets back; every open session has a [`SessionView`] that keeps updating in the background.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use super::{
    fileview::FileView,
    input::{Input, InputAction},
    motion::{self, Cursor, Lines, Motion, Pos},
    render::FILE_GUTTER,
    sidebar::{Live, LiveSub, STALE_SUBAGENT, Sidebar},
    transcript::{GUTTER, Kind, ToolEntry, ToolStatus, Transcript},
};
use crate::{
    agent::AgentEvent,
    comments::{Anchor, Comment, compose_message},
    config::HostConfig,
    diff,
    hub::{net, Notice, QueuedTurn, Request, Snapshot},
    length::Routing,
    llm::{Effort, Message, Usage},
    tools::{jobs, resolve_path, summarize},
    trace::SessionInfo,
    util::base64,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// `line`: linewise (`V`) vs charwise (`v`). `anchor` is where the selection started.
    Visual { line: bool, anchor: Pos },
    Comment,
    Search { forward: bool },
    Command,
}

pub enum View {
    Chat,
    File(Box<FileView>),
    Queue { sel: usize },
    /// Changed files under `cwd` on `host`.
    Files { items: Vec<(String, String)>, sel: usize, host: usize, cwd: PathBuf },
    /// `:model`: the configured models; Enter picks one for the session on screen.
    Models { sel: usize },
    Help,
}

/// The buffer that motions act on in the current view.
struct Active<'a> {
    lines: &'a dyn Lines,
    cursor: &'a mut Cursor,
    scroll: &'a mut usize,
    blocks: &'a [usize],
}

/// What the TUI shows for one session. The agent itself runs in the hub; this is only the
/// view, kept up to date from the hub's events whether or not it is on screen.
pub struct SessionView {
    /// Index of the host the session lives on.
    pub(crate) host: usize,
    /// Working directory of the session on its host.
    pub(crate) cwd: PathBuf,
    pub(crate) session_id: String,
    /// Whether the user has typed into this session (a fresh session is not resumable yet).
    pub(crate) has_input: bool,
    pub(crate) model: String,
    pub(crate) transcript: Transcript,
    pub(crate) chat_cursor: Cursor,
    pub(crate) chat_scroll: usize,
    /// Keep the cursor pinned to the newest row while output streams.
    pub(crate) follow: bool,
    /// Draft in the prompt for this session.
    pub(crate) prompt: Input,
    hist_pos: Option<usize>,
    /// Review comments queued for this session's next message.
    pub(crate) queue: Vec<Comment>,
    pub(crate) running: bool,
    /// The hub's queue, oldest first, not including the running turn.
    pub(crate) queued: Vec<QueuedTurn>,
    pub(crate) queue_edit: Option<QueueEdit>,
    pub(crate) usage: Usage,
    /// The last turn's response-length pick, shown in the status line.
    pub(crate) length: Option<String>,
    pub(crate) activity: Option<String>,
    pub(crate) turn_started: Instant,
    streaming_text: Option<usize>,
    streaming_reasoning: Option<usize>,
    tool_entries: HashMap<String, usize>,
    /// Tool call id → subagent session id, for `task` calls in flight (from `SubagentStarted`).
    pub(crate) subagent_calls: HashMap<String, String>,
    /// A recorded trace shown without an agent (subagents, sessions owned elsewhere).
    pub(crate) read_only: bool,
    /// Tool call id of an `ask` waiting for the user; the next message answers it.
    pub(crate) question: Option<String>,
}

pub(crate) struct QueueEdit {
    pub(crate) turn_id: u64,
    original: String,
    pub(crate) submitted: Option<String>,
    /// The turn Ctrl-Enter meant to interrupt, if still current when the edit is acknowledged.
    interrupt: Option<Instant>,
}

impl SessionView {
    /// @param local whether the session's files are on this machine (decides link checking).
    pub fn new(host: usize, cwd: PathBuf, local: bool, session_id: String, model: String) -> Self {
        let transcript = if local { Transcript::new(cwd.clone()) } else { Transcript::remote(cwd.clone()) };
        Self {
            host,
            cwd,
            session_id,
            has_input: false,
            model,
            transcript,
            chat_cursor: Cursor::default(),
            chat_scroll: 0,
            follow: true,
            prompt: Input::default(),
            hist_pos: None,
            queue: Vec::new(),
            running: false,
            queued: Vec::new(),
            queue_edit: None,
            usage: Usage::default(),
            length: None,
            activity: None,
            turn_started: Instant::now(),
            streaming_text: None,
            streaming_reasoning: None,
            tool_entries: HashMap::new(),
            subagent_calls: HashMap::new(),
            read_only: false,
            question: None,
        }
    }
}

/// A session the user asked to see; the next matching `Opened` or `Trace` becomes current.
#[derive(Clone, Debug, PartialEq)]
enum PendingOpen {
    /// Whatever opens next on this host (startup, `:new`).
    Any(usize),
    Id(usize, String),
}

/// Tab completion of the path before the prompt cursor.
#[derive(Debug)]
pub(crate) struct Completion {
    /// Awaiting the hub's answer to this request id.
    request: Option<u64>,
    host: usize,
    session_id: String,
    /// Byte offset where the path starts in the prompt.
    start: usize,
    /// The text there now: what was typed, then the completion put in.
    current: String,
    /// Matches Tab cycles through once the answer has several; `next` is the one Tab puts in.
    choices: Vec<String>,
    next: usize,
}

/// What a host's connection task reports to the event loop.
#[derive(Debug)]
pub enum HostEvent {
    Notice(Notice),
    /// The connection ended.
    Closed,
    /// A reconnect succeeded: requests go to this sender from now on.
    Linked(UnboundedSender<Request>),
    /// Reconnecting failed for long enough to stop trying.
    GaveUp(String),
}

/// Feeds `notices` from `host` into the event loop, then reports the connection closed.
pub fn forward(host: usize, mut notices: UnboundedReceiver<Notice>, tx: UnboundedSender<(usize, HostEvent)>) {
    tokio::spawn(async move {
        while let Some(n) = notices.recv().await {
            if tx.send((host, HostEvent::Notice(n))).is_err() {
                return;
            }
        }
        let _ = tx.send((host, HostEvent::Closed));
    });
}

/// Delays between reconnect attempts: quick at first, for a `:restart`, then every two
/// seconds for about a minute.
const RECONNECT_DELAYS_MS: [u64; 4] = [200, 500, 1000, 2000];
const RECONNECT_ATTEMPTS: usize = 32;

/// Retries `relink` until it answers, then hands the new connection to the event loop.
fn spawn_reconnect(host: usize, relink: net::Relink, tx: UnboundedSender<(usize, HostEvent)>) {
    tokio::spawn(async move {
        let mut last = String::new();
        for attempt in 0..RECONNECT_ATTEMPTS {
            let delay = RECONNECT_DELAYS_MS[attempt.min(RECONNECT_DELAYS_MS.len() - 1)];
            tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
            match relink.connect().await {
                Ok((req_tx, first, rest)) => {
                    if tx.send((host, HostEvent::Linked(req_tx))).is_err() {
                        return;
                    }
                    let _ = tx.send((host, HostEvent::Notice(first)));
                    return forward(host, rest, tx);
                }
                Err(e) => last = format!("{e:#}"),
            }
        }
        let _ = tx.send((host, HostEvent::GaveUp(last)));
    });
}

/// One attached hub: this machine's or a remote one.
pub struct Host {
    pub name: String,
    /// Files of this host's sessions are on this machine (links are checked, diffs read directly).
    pub local: bool,
    /// Directory for new sessions; also limits the rail to sessions from it.
    pub dir: Option<PathBuf>,
    req_tx: UnboundedSender<Request>,
    pub connected: bool,
    /// How to reach the hub again when the connection ends; `None` for an in-process hub.
    pub relink: Option<net::Relink>,
    /// A reconnect task is running.
    reconnecting: bool,
    /// Reopen subscribed sessions once this new connection sends `Hello`.
    reopen_on_hello: bool,
    /// Hub version from its `Hello` notice, once seen.
    pub version: Option<String>,
    /// The hub version we already asked to restart; avoids a restart loop on reconnect.
    restart_requested: Option<String>,
    /// From the host's last `Sessions` notice.
    pub recorded: Vec<SessionInfo>,
    pub open: HashSet<String>,
    pub running: HashSet<String>,
    /// Subagents seen starting in this host's sessions, keyed by their session id.
    pub live: HashMap<String, LiveSub>,
}

impl Host {
    pub fn new(name: impl Into<String>, local: bool, dir: Option<PathBuf>, req_tx: UnboundedSender<Request>) -> Self {
        Self {
            name: name.into(),
            local,
            dir,
            req_tx,
            connected: true,
            relink: None,
            reconnecting: false,
            reopen_on_hello: false,
            version: None,
            restart_requested: None,
            recorded: Vec::new(),
            open: HashSet::new(),
            running: HashSet::new(),
            live: HashMap::new(),
        }
    }
}

pub struct App {
    /// The session on screen.
    pub(crate) cur: SessionView,
    /// Other sessions this TUI has open; they keep receiving events.
    pub(crate) others: Vec<SessionView>,
    /// This TUI's working directory (local default for new sessions and placeholders).
    cwd: PathBuf,
    pub(crate) hosts: Vec<Host>,
    pending_open: Option<PendingOpen>,
    /// Line to place the cursor on when a requested remote file arrives.
    pending_file: Option<Option<usize>>,
    /// Set while applying a background session's event, so view-level side effects stay off.
    background: bool,
    pub(crate) view: View,
    pub(crate) mode: Mode,
    mode_before_search: Mode,
    pub(crate) comment: Input,
    pub(crate) minibuf: Input,
    pub(crate) comment_target: Option<Anchor>,
    pub(crate) editing: Option<usize>,
    count: Option<usize>,
    pending: Option<char>,
    last_search: Option<(String, bool)>,
    pub(crate) flash: Option<String>,
    pub quit: bool,
    ctrl_c_armed: bool,
    pub(crate) main_area: Rect,
    pub(crate) prompt_area: Rect,
    /// Where the session rail is drawn; `None` when it is hidden.
    pub(crate) sidebar_area: Option<Rect>,
    /// Sessions with subagents nested beneath them.
    pub(crate) sidebar: Sidebar,
    /// Keys go to the rail instead of the main view.
    pub(crate) sidebar_focus: bool,
    /// Selected rail row.
    pub(crate) sidebar_sel: usize,
    /// First visible rail row.
    pub(crate) sidebar_scroll: usize,
    /// Whether the session rail is shown.
    pub(crate) sidebar_open: bool,
    /// Show subagents that finished long ago (`zh` toggles, as in lf).
    sidebar_show_stale: bool,
    /// How many old subagent rows the rail is hiding.
    pub(crate) sidebar_hidden: usize,
    /// Read-only views of rail rows loaded for previewing, with their trace's age when loaded.
    previews: HashMap<(usize, String), (Option<std::time::SystemTime>, SessionView)>,
    preview_requested: HashSet<(usize, String)>,
    /// Building a preview: its messages stay out of the prompt history.
    preview_loading: bool,
    /// Drawing a preview: no cursor.
    pub(crate) previewing: bool,
    /// Prompt history across sessions.
    history: Vec<String>,
    pub spinner: usize,
    drag_anchor: Option<Pos>,
    /// Text written to the terminal outside ratatui (OSC 52 clipboard) on the next draw.
    pub(crate) osc_out: Option<String>,
    /// Thinking effort sent with each turn; Shift-Tab cycles it.
    pub effort: Effort,
    /// Model ids `:model` lists.
    pub models: Vec<String>,
    /// SSH client used when `:host` adds a remote machine.
    pub ssh_program: String,
    /// Command run on a remote host to reach its hub (usually `rdsh connect`).
    pub remote_command: Vec<String>,
    /// Where this TUI's merged notice stream is fed; set by the event loop.
    pub merged_tx: Option<UnboundedSender<(usize, HostEvent)>>,
    /// Opens a URL or local file outside the TUI (`gx`); tests replace it.
    pub(crate) open_url: Box<dyn Fn(&str)>,
    /// Tab completion: the request in flight, then the matches Tab cycles through.
    completion: Option<Completion>,
    completion_ids: u64,
}

impl App {
    /// Creates a TUI attached to `hosts` (index 0 first). No session is shown until a hub
    /// answers an `Open`/`New` request with `Opened` (see `expect_open`).
    pub fn new(cwd: PathBuf, hosts: Vec<Host>) -> Self {
        Self {
            cur: SessionView::new(0, cwd.clone(), true, String::new(), String::new()),
            others: Vec::new(),
            cwd,
            hosts,
            pending_open: None,
            pending_file: None,
            background: false,
            view: View::Chat,
            mode: Mode::Insert,
            mode_before_search: Mode::Normal,
            comment: Input::default(),
            minibuf: Input::default(),
            comment_target: None,
            editing: None,
            count: None,
            pending: None,
            last_search: None,
            flash: None,
            quit: false,
            ctrl_c_armed: false,
            main_area: Rect::default(),
            prompt_area: Rect::default(),
            sidebar_area: None,
            sidebar: Default::default(),
            sidebar_focus: false,
            sidebar_sel: 0,
            sidebar_scroll: 0,
            sidebar_open: false,
            sidebar_show_stale: false,
            sidebar_hidden: 0,
            previews: HashMap::new(),
            preview_requested: HashSet::new(),
            preview_loading: false,
            previewing: false,
            history: Vec::new(),
            spinner: 0,
            drag_anchor: None,
            osc_out: None,
            effort: Effort::default(),
            models: crate::providers::DEEPSEEK_MODELS.map(String::from).to_vec(),
            ssh_program: "ssh".into(),
            remote_command: vec!["rdsh".into(), "connect".into()],
            merged_tx: None,
            completion: None,
            completion_ids: 0,
            open_url: Box::new(crate::util::open_browser),
        }
    }

    /// The next `Opened` (or `Trace`) notice from `host` becomes the session on screen.
    pub fn expect_open(&mut self, host: usize) {
        self.pending_open = Some(PendingOpen::Any(host));
    }

    /// Sends a request to `host`; a closed connection is reported in the status line.
    fn request(&mut self, host: usize, req: Request) -> bool {
        let Some(h) = self.hosts.get(host) else { return false };
        if h.connected && h.req_tx.send(req).is_ok() {
            return true;
        }
        let name = h.name.clone();
        self.set_flash(format!("not connected to {name}"));
        false
    }

    /// The default directory for new sessions on `host`.
    fn new_session_dir(&self, host: usize) -> Option<String> {
        let h = self.hosts.get(host)?;
        match (&h.dir, h.local) {
            (Some(d), _) => Some(d.display().to_string()),
            (None, true) => Some(self.cwd.display().to_string()),
            (None, false) => None,
        }
    }

    /// Applies one event from `host`'s connection.
    pub fn on_host_event(&mut self, host: usize, ev: HostEvent) {
        match ev {
            HostEvent::Notice(n) => self.on_notice(host, n),
            HostEvent::Closed => self.on_disconnect(host),
            HostEvent::Linked(tx) => self.on_relinked(host, tx),
            HostEvent::GaveUp(e) => {
                let Some(h) = self.hosts.get_mut(host) else { return };
                h.reconnecting = false;
                let name = h.name.clone();
                self.set_flash(format!("could not reconnect to {name}: {e}"));
            }
        }
    }

    /// The hub on `host` went away: its views stay readable and the connection is retried in
    /// the background (a `:restart` ends it this way).
    pub fn on_disconnect(&mut self, host: usize) {
        let Some(h) = self.hosts.get_mut(host) else { return };
        h.connected = false;
        h.reopen_on_hello = false;
        h.running.clear();
        let name = h.name.clone();
        for v in std::iter::once(&mut self.cur).chain(self.others.iter_mut()) {
            if v.host == host {
                v.running = false;
                v.queued.clear();
                // Queue IDs may be reused by a restarted hub; keep only the draft text.
                v.queue_edit = None;
            }
        }
        let h = &mut self.hosts[host];
        match (&h.relink, &self.merged_tx) {
            (Some(relink), Some(tx)) if !h.reconnecting => {
                h.reconnecting = true;
                spawn_reconnect(host, relink.clone(), tx.clone());
                self.set_flash(format!("lost the connection to {name} — reconnecting…"));
            }
            _ => self.set_flash(format!("lost the connection to {name}")),
        }
        self.refresh_sidebar();
    }

    /// A reconnect to `host` succeeded. Wait for its Hello before re-subscribing to sessions.
    fn on_relinked(&mut self, host: usize, tx: UnboundedSender<Request>) {
        let Some(h) = self.hosts.get_mut(host) else { return };
        h.req_tx = tx;
        h.connected = true;
        h.reconnecting = false;
        h.reopen_on_hello = true;
    }

    fn reopen_sessions(&mut self, host: usize) {
        let name = self.hosts[host].name.clone();
        self.request(host, Request::List);
        let ids: Vec<String> = std::iter::once(&self.cur)
            .chain(self.others.iter())
            .filter(|v| v.host == host && !v.read_only && !v.session_id.is_empty())
            .map(|v| v.session_id.clone())
            .collect();
        for session_id in ids {
            self.request(host, Request::Open { session_id });
        }
        self.set_flash(format!("reconnected to {name}"));
        self.refresh_sidebar();
    }

    /// Applies one notice from `host`.
    pub fn on_notice(&mut self, host: usize, n: Notice) {
        if host >= self.hosts.len() {
            return;
        }
        match n {
            Notice::Hello { version } => {
                self.request(host, Request::WatchQueue);
                if std::mem::take(&mut self.hosts[host].reopen_on_hello) {
                    self.reopen_sessions(host);
                }
                // The connecting TUI is assumed to be the latest binary. A hub whose version is
                // strictly older is asked to drain and restart; an equal or newer hub is left alone.
                let client_version = env!("CARGO_PKG_VERSION");
                let older = crate::util::compare_versions(&version, client_version).is_lt();
                let already = self.hosts[host].restart_requested.as_deref() == Some(version.as_str());
                self.hosts[host].version = Some(version.clone());
                if older && !already {
                    self.hosts[host].restart_requested = Some(version.clone());
                    self.request(host, Request::Restart);
                    self.set_flash(format!("{} is older (hub {version}, client {client_version}) — restarting…", self.hosts[host].name));
                }
            }
            Notice::Restarting => {
                self.set_flash(format!("{} is draining and will restart", self.hosts[host].name));
            }
            Notice::Sessions { recorded, open, running } => {
                // Drop previews of sessions whose trace changed since they were loaded.
                let modified: HashMap<&str, std::time::SystemTime> = recorded.iter().map(|s| (s.id.as_str(), s.modified)).collect();
                self.previews
                    .retain(|(h, id), (m, _)| *h != host || modified.get(id.as_str()).copied() == *m);
                let h = &mut self.hosts[host];
                h.recorded = recorded;
                h.open = open.into_iter().collect();
                h.running = running.into_iter().collect();
                self.refresh_sidebar();
            }
            Notice::Opened { snapshot } => {
                let current = self.claims(host, &snapshot.session_id);
                self.install(host, snapshot, current);
            }
            Notice::Trace { session_id, cwd, history } => {
                self.preview_requested.remove(&(host, session_id.clone()));
                let current = self.claims(host, &session_id);
                let snapshot = Snapshot {
                    session_id: session_id.clone(),
                    model: "trace".into(),
                    cwd,
                    history,
                    turn: Vec::new(),
                    running: false,
                    queued: Vec::new(),
                    usage: Usage::default(),
                };
                if current {
                    self.previews.remove(&(host, session_id));
                    self.install(host, snapshot, true);
                    self.cur.read_only = true;
                    self.cur.queue_edit = None;
                } else {
                    // Answer to a preview request: cache it, stamped with the trace's age.
                    self.preview_loading = true;
                    let mut view = self.build_view(host, snapshot);
                    self.preview_loading = false;
                    view.read_only = true;
                    let modified = self.recorded_modified(host, &session_id);
                    self.previews.insert((host, session_id), (modified, view));
                }
            }
            Notice::File { file } => {
                if let Some(line) = self.pending_file.take() {
                    let cwd = self.cur.cwd.display().to_string();
                    let mut fv = FileView::new(*file, line);
                    fv.remote = Some((host, cwd));
                    self.view = View::File(Box::new(fv));
                    self.mode = Mode::Normal;
                } else if let View::File(fv) = &mut self.view
                    && fv.remote.as_ref().is_some_and(|(h, _)| *h == host)
                {
                    // A reload.
                    fv.replace(*file);
                    self.check_file_base();
                }
            }
            Notice::ChangedFiles { cwd, files } => {
                if files.is_empty() {
                    self.set_flash("no uncommitted changes");
                } else {
                    self.view = View::Files { items: files, sel: 0, host, cwd: PathBuf::from(cwd) };
                    self.mode = Mode::Normal;
                }
            }
            Notice::Event { session_id, event } => self.on_session_event(host, &session_id, event),
            Notice::Queue { session_id, queued } => {
                if let Some(v) = self.session_view_mut(host, &session_id) {
                    v.queued = queued;
                }
            }
            Notice::QueueEdited { session_id, turn_id, updated } => {
                self.on_queue_edited(host, session_id, turn_id, updated);
            }
            Notice::PathCompletions { request_id, paths } => self.on_path_completions(host, request_id, paths),
            Notice::Error { message, .. } => {
                self.pending_open = None;
                self.pending_file = None;
                let name = &self.hosts[host].name;
                let msg = if self.hosts.len() > 1 { format!("{name}: {message}") } else { message };
                self.set_flash(msg);
            }
        }
    }

    /// Whether an arriving session answers the user's pending open (and so goes on screen).
    fn claims(&mut self, host: usize, id: &str) -> bool {
        let hit = match &self.pending_open {
            Some(PendingOpen::Any(h)) => *h == host,
            Some(PendingOpen::Id(h, want)) => *h == host && (id.starts_with(want.as_str()) || want == "last"),
            None => false,
        };
        if hit {
            self.pending_open = None;
        }
        hit
    }

    fn placeholder(&self) -> SessionView {
        SessionView::new(0, self.cwd.clone(), true, String::new(), String::new())
    }

    /// Builds a view from a snapshot without touching what is on screen.
    fn build_view(&mut self, host: usize, snapshot: Snapshot) -> SessionView {
        let cwd = if snapshot.cwd.is_empty() { self.cwd.clone() } else { PathBuf::from(&snapshot.cwd) };
        let local = self.hosts.get(host).is_none_or(|h| h.local);
        let view = SessionView::new(host, cwd, local, snapshot.session_id.clone(), snapshot.model.clone());
        let previous = std::mem::replace(&mut self.cur, view);
        let was_background = std::mem::replace(&mut self.background, true);
        self.load_history(&snapshot.history);
        for ev in snapshot.turn {
            self.on_agent(ev);
        }
        self.cur.running = snapshot.running;
        self.cur.queued = snapshot.queued;
        self.cur.usage = snapshot.usage;
        self.background = was_background;
        std::mem::replace(&mut self.cur, previous)
    }

    fn recorded_modified(&self, host: usize, id: &str) -> Option<std::time::SystemTime> {
        self.hosts.get(host)?.recorded.iter().find(|s| s.id == id).map(|s| s.modified)
    }

    fn other_index(&self, host: usize, id: &str) -> Option<usize> {
        self.others.iter().position(|v| v.host == host && v.session_id == id)
    }

    /// The rail row being previewed: the selection, while the rail has keyboard focus, when it
    /// is a session that is not the one on screen. Hovering never previews.
    pub(crate) fn preview_id(&self) -> Option<(usize, String)> {
        if !self.sidebar_open || !self.sidebar_focus {
            return None;
        }
        let row = self.sidebar.rows().get(self.sidebar_sel)?;
        (!row.header && !(row.host == self.cur.host && row.id == self.cur.session_id)).then(|| (row.host, row.id.clone()))
    }

    /// Asks the row's host for the previewed session's trace unless a view of it is already here.
    fn request_preview(&mut self) {
        let Some((host, id)) = self.preview_id() else { return };
        let key = (host, id.clone());
        let have = self.other_index(host, &id).is_some() || self.previews.contains_key(&key);
        if !have && self.preview_requested.insert(key) {
            self.request(host, Request::ReadTrace { session_id: id });
        }
    }

    /// Runs `f` with the view of `(host, id)` swapped in as `cur`, for drawing a preview.
    pub(crate) fn with_view<R>(&mut self, host: usize, id: &str, f: impl FnOnce(&mut Self) -> R) -> Option<R> {
        let key = (host, id.to_owned());
        let from_others = self.other_index(host, id);
        let mut slot = match from_others {
            Some(i) => {
                let empty = self.placeholder();
                std::mem::replace(&mut self.others[i], empty)
            }
            None => self.previews.remove(&key)?.1,
        };
        std::mem::swap(&mut self.cur, &mut slot);
        let r = f(self);
        std::mem::swap(&mut self.cur, &mut slot);
        match from_others {
            Some(i) => self.others[i] = slot,
            None => {
                let m = self.recorded_modified(host, id);
                self.previews.insert(key, (m, slot));
            }
        }
        Some(r)
    }

    fn install(&mut self, host: usize, snapshot: Snapshot, make_current: bool) {
        let id = snapshot.session_id.clone();
        let same = self.cur.host == host && self.cur.session_id == id;
        let mut fresh = self.build_view(host, snapshot);
        let mut previous_other = self.other_index(host, &id).map(|i| self.others.remove(i));
        // Refresh server state, but retain the local draft, edit and comments even in the background.
        let previous_view = if same { Some(&mut self.cur) } else { previous_other.as_mut() };
        if let Some(v) = previous_view {
            fresh.prompt = std::mem::take(&mut v.prompt);
            fresh.hist_pos = v.hist_pos;
            fresh.queue = std::mem::take(&mut v.queue);
            fresh.queue_edit = v.queue_edit.take();
            fresh.has_input |= v.has_input;
        }
        let previous = std::mem::replace(&mut self.cur, fresh);
        let keep_previous = !previous.session_id.is_empty() && !same;
        if make_current || same {
            if keep_previous {
                self.others.push(previous);
            }
            self.view = View::Chat;
        } else {
            let fresh = std::mem::replace(&mut self.cur, previous);
            self.others.push(fresh);
        }
        self.refresh_sidebar();
        if make_current {
            self.sidebar_sel = self.sidebar.index_of(host, &id).unwrap_or(self.sidebar_sel);
        }
    }

    fn session_view_mut(&mut self, host: usize, id: &str) -> Option<&mut SessionView> {
        if self.cur.host == host && self.cur.session_id == id {
            Some(&mut self.cur)
        } else {
            self.others.iter_mut().find(|v| v.host == host && v.session_id == id)
        }
    }

    fn on_queue_edited(&mut self, host: usize, session_id: String, turn_id: u64, updated: bool) {
        let current = self.cur.host == host && self.cur.session_id == session_id;
        let Some(v) = self.session_view_mut(host, &session_id) else { return };
        if !v.queue_edit.as_ref().is_some_and(|e| e.turn_id == turn_id && e.submitted.is_some()) {
            return;
        }
        let edit = v.queue_edit.take().unwrap();
        if updated {
            if edit.submitted.as_deref() == Some(v.prompt.text.as_str()) {
                v.prompt.take();
            }
            // Do not interrupt another turn that started while waiting for the edit ack.
            if v.running && edit.interrupt == Some(v.turn_started) && v.queued.iter().any(|t| t.id == turn_id) {
                self.interrupt_for_queue(host, session_id);
            }
        } else {
            let message = "queued turn already started — edit kept as an unsent draft";
            if current {
                self.set_flash(message);
            } else {
                self.set_flash(format!("{session_id}: {message}"));
            }
        }
    }

    /// Routes a session event to its view, on screen or not.
    fn on_session_event(&mut self, host: usize, id: &str, ev: AgentEvent) {
        if self.cur.host == host && self.cur.session_id == id {
            self.on_agent(ev);
            return;
        }
        let Some(i) = self.other_index(host, id) else { return };
        std::mem::swap(&mut self.cur, &mut self.others[i]);
        let was = std::mem::replace(&mut self.background, true);
        self.on_agent(ev);
        self.background = was;
        std::mem::swap(&mut self.cur, &mut self.others[i]);
    }

    /// Puts an already-open view on screen.
    fn switch_to(&mut self, i: usize) {
        std::mem::swap(&mut self.cur, &mut self.others[i]);
        self.view = View::Chat;
        self.mode = Mode::Normal;
        self.sidebar_focus = false;
        self.refresh_sidebar();
        self.sidebar_sel = self.sidebar.index_of(self.cur.host, &self.cur.session_id).unwrap_or(self.sidebar_sel);
    }

    /// Starts a new session on `host` and shows it when it opens.
    fn new_session(&mut self, host: usize) {
        self.pending_open = Some(PendingOpen::Any(host));
        let cwd = self.new_session_dir(host);
        self.request(host, Request::New { cwd });
    }

    /// Adds a remote host to the rail from `:host DEST[:DIR]` and starts a session there.
    ///
    /// The hub is started on the remote machine by the same `rdsh connect` handshake the CLI
    /// `--host` path uses: `ssh -T DEST rdsh connect` makes sure a hub is listening, then relays
    /// the hub protocol. The host is added for this TUI process only (config persistence is a
    /// follow-up), and failures surface through the notice stream as a disconnect or error.
    fn add_remote_host(&mut self, spec: &str) {
        let h = match HostConfig::parse(spec) {
            Ok(h) => h,
            Err(e) => return self.set_flash(format!(":host — {e:#}")),
        };
        if self.hosts.iter().any(|x| x.name == h.name) {
            return self.set_flash(format!("host {} is already in the rail", h.name));
        }
        let mut ssh_args = vec!["-T".to_owned(), h.ssh.clone()];
        ssh_args.extend(self.remote_command.iter().cloned());
        let (req_tx, notices) = match net::connect_command(&self.ssh_program, &ssh_args) {
            Ok(pair) => pair,
            Err(e) => return self.set_flash(format!(":host {} — {e:#}", h.name)),
        };
        let host = self.hosts.len();
        if let Some(tx) = &self.merged_tx {
            forward(host, notices, tx.clone());
        }
        let _ = req_tx.send(Request::List);
        let mut added = Host::new(h.name.clone(), false, h.dir.clone().map(PathBuf::from), req_tx);
        added.relink = Some(net::Relink::Command { program: self.ssh_program.clone(), args: ssh_args });
        self.hosts.push(added);
        self.refresh_sidebar();
        self.set_flash(format!("added {} — starting a session…", h.name));
        self.new_session(host);
    }

    /// Enter on a rail row: show that session, resuming it on its host if needed. Subagent
    /// rows open their trace read-only; a host heading starts a new session there.
    fn open_selected(&mut self) {
        let Some(row) = self.sidebar.rows().get(self.sidebar_sel).cloned() else { return };
        self.sidebar_focus = false;
        self.mode = Mode::Normal;
        if row.header {
            let name = self.hosts.get(row.host).map(|h| h.name.clone()).unwrap_or_default();
            self.set_flash(format!("starting a session on {name}…"));
            return self.new_session(row.host);
        }
        if row.host == self.cur.host && row.id == self.cur.session_id {
            return;
        }
        if let Some(i) = self.other_index(row.host, &row.id) {
            return self.switch_to(i);
        }
        // A subagent trace already loaded for the preview opens as is.
        if row.depth > 0
            && let Some((_, view)) = self.previews.remove(&(row.host, row.id.clone()))
        {
            self.others.push(view);
            let i = self.others.len() - 1;
            return self.switch_to(i);
        }
        self.pending_open = Some(PendingOpen::Id(row.host, row.id.clone()));
        if row.depth == 0 {
            self.set_flash(format!("opening {}…", row.id));
            self.request(row.host, Request::Open { session_id: row.id });
        } else {
            self.set_flash(format!("loading subagent trace {}…", row.id));
            // A preview load already in flight answers this open too.
            if self.preview_requested.insert((row.host, row.id.clone())) {
                self.request(row.host, Request::ReadTrace { session_id: row.id });
            }
        }
    }

    /// Sessions running on any host other than the one on screen.
    fn others_running(&self) -> usize {
        self.hosts
            .iter()
            .enumerate()
            .map(|(i, h)| h.running.iter().filter(|id| !(i == self.cur.host && **id == self.cur.session_id)).count())
            .sum()
    }

    #[cfg(test)]
    pub(crate) fn history_contains(&self, text: &str) -> bool {
        self.history.iter().any(|h| h == text)
    }

    /// `(host name, session id)` of the sessions this TUI shows (current first), for the exit
    /// message.
    pub fn open_sessions(&self) -> Vec<(String, String)> {
        std::iter::once(&self.cur)
            .chain(&self.others)
            .filter(|v| v.has_input && !v.session_id.is_empty() && !v.read_only)
            .map(|v| (self.hosts.get(v.host).map(|h| h.name.clone()).unwrap_or_default(), v.session_id.clone()))
            .collect()
    }

    /// Rebuilds the transcript from a resumed session's history.
    pub fn load_history(&mut self, history: &[Message]) {
        for m in history {
            match m {
                Message::System { .. } | Message::Context { .. } | Message::Directive { .. } => {}
                Message::User { content } if jobs::is_note(content) => {
                    self.cur.transcript.push(Kind::Info, jobs::note_body(content).to_owned());
                }
                Message::User { content } => {
                    self.cur.has_input = true;
                    self.cur.transcript.push(Kind::User, content.clone());
                    if !self.preview_loading {
                        self.history.push(content.clone());
                    }
                }
                Message::Assistant { content, reasoning, tool_calls, .. } => {
                    if let Some(r) = reasoning {
                        self.cur.transcript.push(Kind::Reasoning, r.clone());
                    }
                    if !content.is_empty() {
                        self.cur.transcript.push(Kind::Assistant, content.clone());
                    }
                    for c in tool_calls {
                        let i = self.cur.transcript.push(Kind::Tool(Box::new(tool_entry(&c.id, &c.name, &c.arguments))), "");
                        self.cur.tool_entries.insert(c.id.clone(), i);
                    }
                }
                Message::Tool { call_id, content, .. } => {
                    if let Some(&i) = self.cur.tool_entries.get(call_id) {
                        let failed = content.starts_with("Error:");
                        self.finish_tool(i, content.clone(), failed, None);
                    }
                }
            }
        }
        if history.iter().any(|m| matches!(m, Message::User { .. })) {
            self.cur.transcript.push(Kind::Info, format!("session {}", self.cur.session_id));
        }
    }

    /// Rebuilds the session rail from the hosts' last session lists and the live subagents
    /// seen. Cheap: no filesystem work happens here.
    pub fn refresh_sidebar(&mut self) {
        let selected = self.sidebar.rows().get(self.sidebar_sel).map(|r| (r.host, r.header, r.id.clone()));
        let cutoff = std::time::SystemTime::now() - STALE_SUBAGENT;
        let mut hidden = 0;
        let parts: Vec<(String, Sidebar)> = self
            .hosts
            .iter()
            .enumerate()
            .map(|(i, h)| {
                let filter = match (&h.dir, h.local) {
                    (Some(d), _) => Some(d.display().to_string()),
                    (None, true) => Some(self.cwd.display().to_string()),
                    (None, false) => None,
                };
                let title = if h.connected { h.name.clone() } else { format!("{} (disconnected)", h.name) };
                let mut part = Sidebar::build(i, &h.recorded, filter.as_deref(), &h.live, &h.running);
                if !self.sidebar_show_stale {
                    // The session on screen and the row under the cursor stay put.
                    hidden += part.hide_finished_before(cutoff, |r| {
                        (i == self.cur.host && r.id == self.cur.session_id)
                            || selected.as_ref().is_some_and(|(h, _, id)| *h == i && *id == r.id)
                    });
                }
                (title, part)
            })
            .collect();
        self.sidebar_hidden = hidden;
        // A single host needs no heading.
        self.sidebar = if parts.len() == 1 {
            parts.into_iter().next().map(|(_, s)| s).unwrap_or_default()
        } else {
            Sidebar::grouped(parts)
        };
        // Keep the selection on the same row when rows shift.
        let fallback = self.sidebar.index_of(self.cur.host, &self.cur.session_id).unwrap_or(0);
        self.sidebar_sel = selected
            .and_then(|(h, header, id)| {
                if header {
                    self.sidebar.rows().iter().position(|r| r.header && r.host == h)
                } else {
                    self.sidebar.index_of(h, &id)
                }
            })
            .unwrap_or(fallback);
    }

    fn set_sidebar_focus(&mut self, focus: bool) {
        if focus && !self.sidebar_open {
            self.sidebar_open = true;
            self.refresh_sidebar();
        }
        if focus && !self.sidebar_focus {
            // Start from the session on screen.
            self.sidebar_sel = self.sidebar.index_of(self.cur.host, &self.cur.session_id).unwrap_or(self.sidebar_sel);
        }
        self.sidebar_focus = focus && self.sidebar_open;
        self.mode = Mode::Normal;
        self.request_preview();
    }

    /// Keys while the rail has focus. Esc, `l`, Tab or `Ctrl-w l` return to the main view.
    fn sidebar_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.sidebar.rows().len();
        let last = n.saturating_sub(1);
        let half = (self.sidebar_area.map_or(10, |a| a.height as usize) / 2).max(1);
        match self.pending.take() {
            Some('W') => {
                if matches!(k.code, KeyCode::Char('l' | 'w') | KeyCode::Right) {
                    self.sidebar_focus = false;
                }
                return;
            }
            Some('z') => {
                if k.code == KeyCode::Char('h') {
                    self.sidebar_show_stale = !self.sidebar_show_stale;
                    let before = self.sidebar.rows().len();
                    self.refresh_sidebar();
                    let changed = self.sidebar.rows().len().abs_diff(before);
                    self.set_flash(match (self.sidebar_show_stale, changed) {
                        (true, 0) => "no finished subagents to show".to_owned(),
                        (true, n) => format!("showing {n} finished subagents"),
                        (false, _) => "hiding subagents finished over 5 minutes ago".to_owned(),
                    });
                    self.request_preview();
                }
                self.count = None;
                return;
            }
            _ => {}
        }
        if let KeyCode::Char(c) = k.code
            && !ctrl
            && c.is_ascii_digit()
            && (c != '0' || self.count.is_some())
        {
            let d = c.to_digit(10).unwrap_or(0) as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d).min(100_000));
            return;
        }
        let count = self.count.take();
        let step = count.unwrap_or(1);
        match (k.code, ctrl) {
            (KeyCode::Char('j') | KeyCode::Down, false) => self.sidebar_sel = (self.sidebar_sel + step).min(last),
            (KeyCode::Char('k') | KeyCode::Up, false) => self.sidebar_sel = self.sidebar_sel.saturating_sub(step),
            (KeyCode::Char('g') | KeyCode::Home, false) => self.sidebar_sel = 0,
            (KeyCode::Char('G') | KeyCode::End, false) => self.sidebar_sel = count.map_or(last, |n| n.saturating_sub(1).min(last)),
            (KeyCode::Char('z'), false) => self.pending = Some('z'),
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => self.sidebar_sel = (self.sidebar_sel + half).min(last),
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => self.sidebar_sel = self.sidebar_sel.saturating_sub(half),
            (KeyCode::Char('w'), true) => self.pending = Some('W'),
            (KeyCode::Enter, _) => self.open_selected(),
            (KeyCode::Char('y'), false) => {
                if let Some(r) = self.sidebar.rows().get(self.sidebar_sel) {
                    let id = r.id.clone();
                    self.osc_out = Some(format!("\x1b]52;c;{}\x07", base64(id.as_bytes())));
                    self.set_flash(format!("copied {id}"));
                }
            }
            (KeyCode::Char('r'), false) => {
                self.refresh_sidebar();
                self.set_flash("sessions reloaded");
            }
            (KeyCode::Char('S'), false) => {
                self.sidebar_open = false;
                self.sidebar_focus = false;
            }
            (KeyCode::Char('i' | 'a'), false) => {
                self.sidebar_focus = false;
                self.mode = Mode::Insert;
            }
            (KeyCode::Char(':'), false) => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            (KeyCode::Esc | KeyCode::Tab | KeyCode::Char('l' | 'q') | KeyCode::Right, _) => self.sidebar_focus = false,
            _ => {}
        }
        self.request_preview();
    }

    pub(crate) fn set_flash(&mut self, s: impl Into<String>) {
        self.flash = Some(s.into());
    }

    // ---------------------------------------------------------------- events

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => match self.mode {
                Mode::Comment => self.comment.insert_str(&s),
                Mode::Search { .. } | Mode::Command => self.minibuf.insert_str(&s.replace('\n', " ")),
                _ => {
                    self.mode = Mode::Insert;
                    self.cur.prompt.insert_str(&s);
                    self.cur.hist_pos = None;
                }
            },
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.ctrl_c();
            return;
        }
        if ctrl && k.code == KeyCode::Enter {
            self.send_now();
            return;
        }
        if k.code == KeyCode::BackTab || (k.code == KeyCode::Tab && k.modifiers.contains(KeyModifiers::SHIFT)) {
            self.ctrl_c_armed = false;
            // An open file switches its diff base instead.
            if matches!(self.view, View::File(_)) && matches!(self.mode, Mode::Normal | Mode::Visual { .. }) {
                return self.toggle_file_base();
            }
            self.effort = self.effort.next();
            self.set_flash(format!("thinking {}", self.effort.name()));
            return;
        }
        self.ctrl_c_armed = false;
        self.flash = None;
        match self.mode {
            Mode::Insert => self.insert_key(k),
            Mode::Comment => self.comment_key(k),
            Mode::Search { forward } => self.search_key(k, forward),
            Mode::Command => self.command_key(k),
            Mode::Normal if self.sidebar_focus => self.sidebar_key(k),
            Mode::Normal | Mode::Visual { .. } => match self.view {
                View::Chat | View::File(_) => self.buffer_key(k),
                View::Queue { .. } => self.queue_key(k),
                View::Files { .. } => self.files_key(k),
                View::Models { .. } => self.models_key(k),
                View::Help => self.view = View::Chat,
            },
        }
    }

    fn ctrl_c(&mut self) {
        if self.cur.running {
            let session_id = self.cur.session_id.clone();
            self.request(self.cur.host, Request::Cancel { session_id });
            self.set_flash("interrupting…");
            return;
        }
        let cleared = match self.mode {
            Mode::Insert if self.cur.queue_edit.is_some() || !self.cur.prompt.text.is_empty() => {
                self.discard_prompt();
                true
            }
            Mode::Comment | Mode::Search { .. } | Mode::Command | Mode::Visual { .. } => {
                self.mode = Mode::Normal;
                true
            }
            _ => false,
        };
        if cleared {
            return;
        }
        if self.ctrl_c_armed {
            self.quit = true;
        } else {
            self.ctrl_c_armed = true;
            let n = self.others_running();
            self.set_flash(if n == 0 {
                "press Ctrl-C again to quit".to_owned()
            } else {
                format!("press Ctrl-C again to quit — stops {n} running session{}", if n == 1 { "" } else { "s" })
            });
        }
    }

    fn discard_prompt(&mut self) {
        if self.cur.queue_edit.as_ref().is_some_and(|e| e.submitted.is_some()) {
            self.set_flash("edit already sent — waiting for the hub");
            return;
        }
        self.cur.queue_edit = None;
        self.cur.hist_pos = None;
        self.cur.prompt.take();
    }

    /// Tab in the prompt: cycle the matches of the last completion, or ask the session's host
    /// to complete the path before the cursor (relative to the session's directory).
    fn complete_path(&mut self) {
        let p = &self.cur.prompt;
        if let Some(c) = &mut self.completion
            && !c.choices.is_empty()
            && p.text.get(c.start..p.cursor) == Some(c.current.as_str())
        {
            let pick = c.choices[c.next].clone();
            c.next = (c.next + 1) % c.choices.len();
            let (start, old) = (c.start, std::mem::replace(&mut c.current, pick.clone()));
            self.cur.prompt.replace_range(start..start + old.len(), &pick);
            return;
        }
        if self.cur.read_only || self.cur.session_id.is_empty() {
            return self.set_flash("no session to complete paths in");
        }
        let before = &p.text[..p.cursor];
        let start = before
            .rfind(|c: char| c.is_whitespace() || "'\"`()<>[]{},;".contains(c))
            .map_or(0, |i| i + before[i..].chars().next().map_or(1, char::len_utf8));
        let token = before[start..].to_owned();
        self.completion_ids += 1;
        let id = self.completion_ids;
        let (host, session_id) = (self.cur.host, self.cur.session_id.clone());
        let req = Request::CompletePath { session_id: session_id.clone(), request_id: id, prefix: token.clone() };
        self.completion = self.request(host, req).then(|| Completion {
            request: Some(id),
            host,
            session_id,
            start,
            current: token,
            choices: Vec::new(),
            next: 0,
        });
    }

    fn on_path_completions(&mut self, host: usize, request_id: u64, paths: Vec<String>) {
        let Some(c) = &mut self.completion else { return };
        if c.request != Some(request_id) || c.host != host {
            return;
        }
        let p = &self.cur.prompt;
        let same = self.cur.host == host && self.cur.session_id == c.session_id;
        // Typing since the request moved or changed the path: leave the prompt alone.
        if !same || p.text.get(c.start..p.cursor) != Some(c.current.as_str()) {
            self.completion = None;
            return;
        }
        c.request = None;
        let (start, typed) = (c.start, c.current.clone());
        let put = match paths.len() {
            0 => {
                self.completion = None;
                return self.set_flash(if typed.is_empty() { "no files here".to_owned() } else { format!("no paths start with {typed}") });
            }
            1 => paths[0].clone(),
            _ => paths.iter().skip(1).fold(paths[0].clone(), |acc, s| {
                let n = acc.chars().zip(s.chars()).take_while(|(a, b)| a == b).map(|(a, _)| a.len_utf8()).sum();
                acc[..n].to_owned()
            }),
        };
        if paths.len() > 1 {
            let shown: Vec<&str> = paths.iter().take(6).map(String::as_str).collect();
            let more = if paths.len() > 6 { format!(" +{}", paths.len() - 6) } else { String::new() };
            let flash = format!("{} matches: {}{more} — Tab cycles", paths.len(), shown.join("  "));
            c.choices = paths;
            self.set_flash(flash);
        } else {
            self.completion = None;
        }
        if put.len() > typed.len() {
            self.cur.prompt.replace_range(start..start + typed.len(), &put);
            if let Some(c) = &mut self.completion {
                c.current = put;
            }
        }
    }

    fn insert_key(&mut self, k: KeyEvent) {
        if k.code == KeyCode::Tab && k.modifiers.is_empty() {
            return self.complete_path();
        }
        if self.completion.as_ref().is_some_and(|c| c.request.is_none()) {
            self.completion = None;
        }
        match self.cur.prompt.handle(k) {
            InputAction::Submit => self.submit(),
            InputAction::Cancel => {
                if self.cur.queue_edit.is_some() {
                    self.discard_prompt();
                }
                self.mode = Mode::Normal;
            }
            // Input only returns these actions at the first/last logical line.
            InputAction::HistoryPrev => {
                if self.recall_queued(true) {
                    return;
                }
                if !self.history.is_empty() && (self.cur.prompt.text.is_empty() || self.cur.hist_pos.is_some()) {
                    let i = self.cur.hist_pos.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                    self.cur.hist_pos = Some(i);
                    self.cur.prompt.set(self.history[i].clone());
                }
            }
            InputAction::HistoryNext => {
                if self.recall_queued(false) {
                    return;
                }
                if let Some(i) = self.cur.hist_pos {
                    if i + 1 < self.history.len() {
                        self.cur.hist_pos = Some(i + 1);
                        self.cur.prompt.set(self.history[i + 1].clone());
                    } else {
                        self.cur.hist_pos = None;
                        self.cur.prompt.take();
                    }
                }
            }
            InputAction::Edited => self.cur.hist_pos = None,
            _ => {}
        }
    }

    /// Navigate the server's queue by ID, never replacing a separate draft or a changed recall.
    /// Returns true if this arrow belongs to queue navigation rather than history.
    fn recall_queued(&mut self, older: bool) -> bool {
        let i = if let Some(edit) = &self.cur.queue_edit {
            if edit.submitted.is_some() || self.cur.prompt.text != edit.original {
                return true;
            }
            let pos = self.cur.queued.iter().position(|t| t.id == edit.turn_id);
            if older {
                let Some(i) = pos else { return true };
                i.saturating_sub(1)
            } else {
                // If the recalled turn started already, anything still pending is newer.
                let i = pos.map_or(0, |i| i + 1);
                if i == self.cur.queued.len() {
                    self.discard_prompt();
                    return true;
                }
                i
            }
        } else if older && self.cur.prompt.text.is_empty() && !self.cur.queued.is_empty() {
            self.cur.queued.len() - 1
        } else {
            return false;
        };
        let turn = &self.cur.queued[i];
        if self.cur.queue_edit.as_ref().is_some_and(|e| e.turn_id == turn.id) {
            return true;
        }
        self.cur.hist_pos = None;
        self.cur.prompt.set(turn.text.clone());
        self.cur.queue_edit = Some(QueueEdit { turn_id: turn.id, original: turn.text.clone(), submitted: None, interrupt: None });
        true
    }

    /// Ctrl-Enter from any mode (and `:now`): send the prompt or save its queued edit, and
    /// interrupt the running turn so the next queued message can start.
    fn send_now(&mut self) {
        self.ctrl_c_armed = false;
        self.flash = None;
        match self.mode {
            Mode::Comment => self.save_comment(),
            Mode::Visual { .. } | Mode::Search { .. } | Mode::Command => self.mode = Mode::Normal,
            Mode::Normal | Mode::Insert => {}
        }
        if self.cur.queue_edit.is_some() {
            self.submit_queue_edit(true);
            return;
        }
        let typed = !self.cur.queue.is_empty() || !self.cur.prompt.is_blank();
        if !typed && self.cur.queued.is_empty() {
            self.set_flash("nothing to send — comment with c or type with i");
            return;
        }
        // Text typed for an `ask` form answers it; interrupting would end the turn it serves.
        let answering = typed && self.cur.question.is_some();
        if typed && !self.submit_new() {
            return;
        }
        if self.cur.running && !answering {
            self.interrupt_for_queue(self.cur.host, self.cur.session_id.clone());
        }
    }

    /// Merges the queue into one message and interrupts the running turn, so every waiting
    /// message starts now.
    fn interrupt_for_queue(&mut self, host: usize, session_id: String) {
        let current = self.cur.host == host && self.cur.session_id == session_id;
        let n = if current { self.cur.queued.len() } else { 0 };
        if self.request(host, Request::RunQueuedNow { session_id }) && current {
            self.set_flash(match n {
                0 | 1 => "interrupting — the waiting message starts now".to_owned(),
                n => format!("interrupting — the {n} waiting messages start now, as one"),
            });
        }
    }

    pub(crate) fn submit(&mut self) {
        if self.cur.queue_edit.is_some() {
            self.submit_queue_edit(false);
        } else {
            self.submit_new();
        }
    }

    fn submit_queue_edit(&mut self, now: bool) {
        let Some(edit) = &self.cur.queue_edit else { return };
        if edit.submitted.is_some() {
            self.set_flash("edit already sent — waiting for the hub");
            return;
        }
        if self.cur.prompt.is_blank() {
            self.set_flash("queued turn cannot be empty — Esc discards the edit");
            return;
        }
        let turn_id = edit.turn_id;
        let text = self.cur.prompt.text.clone();
        let session_id = self.cur.session_id.clone();
        if self.request(self.cur.host, Request::EditQueued { session_id, turn_id, text: text.clone() }) {
            let edit = self.cur.queue_edit.as_mut().unwrap();
            edit.submitted = Some(text);
            edit.interrupt = (now && self.cur.running).then_some(self.cur.turn_started);
        }
    }

    /// Sends the prompt plus all queued comments as the next user message (or ask answer).
    fn submit_new(&mut self) -> bool {
        let text = self.cur.prompt.text.trim().to_owned();
        if text.is_empty() && self.cur.queue.is_empty() {
            return false;
        }
        if self.cur.read_only || self.cur.session_id.is_empty() {
            self.set_flash(if self.cur.read_only { "read-only trace — open a session to send" } else { "no session yet" });
            return false;
        }
        let msg = compose_message(&self.cur.queue, &text);
        let session_id = self.cur.session_id.clone();
        let req = if let Some(question_id) = self.cur.question.clone() {
            let answer = crate::tools::ask::Answer::Text { text: msg };
            Request::Answer { session_id, question_id, answer }
        } else {
            // The user entry appears when the hub starts the turn, not when it is queued.
            Request::Send { session_id, text: msg, effort: self.effort, model: Some(self.cur.model.clone()).filter(|m| !m.is_empty()), diagrams: false }
        };
        if !self.request(self.cur.host, req) {
            return false;
        }
        self.cur.prompt.take();
        self.cur.has_input = true;
        if !text.is_empty() {
            self.history.push(text);
        }
        self.cur.hist_pos = None;
        self.cur.queue.clear();
        self.cur.question = None;
        self.view = View::Chat;
        true
    }

    // --------------------------------------------------------- normal/visual

    fn active(&mut self) -> Option<Active<'_>> {
        match &mut self.view {
            View::Chat => Some(Active {
                lines: &self.cur.transcript,
                cursor: &mut self.cur.chat_cursor,
                scroll: &mut self.cur.chat_scroll,
                blocks: self.cur.transcript.entry_starts(),
            }),
            View::File(fv) => {
                let fv = &mut **fv;
                Some(Active { lines: &fv.texts, cursor: &mut fv.cursor, scroll: &mut fv.scroll, blocks: &fv.blocks })
            }
            _ => None,
        }
    }

    fn page(&self) -> usize {
        let h = self.main_area.height as usize;
        if matches!(self.view, View::File(_)) { h.saturating_sub(2).max(1) } else { h.max(1) }
    }

    fn cursor_pos(&mut self) -> Pos {
        self.active().map_or_else(Pos::default, |a| a.cursor.pos)
    }

    pub(crate) fn motion(&mut self, m: Motion) {
        let count = self.count.take().unwrap_or(1);
        if let Some(a) = self.active() {
            motion::apply(a.cursor, m, a.lines, a.blocks, count);
        }
        self.update_follow();
    }

    fn update_follow(&mut self) {
        if matches!(self.view, View::Chat) {
            self.cur.follow = self.cur.chat_cursor.pos.row + 1 >= self.cur.transcript.total();
        }
    }

    /// Scrolls the viewport by `d` rows, dragging the cursor along when it leaves the view.
    fn scroll_by(&mut self, d: isize) {
        let page = self.page();
        if let Some(a) = self.active() {
            let n = a.lines.count();
            let max = n.saturating_sub(1);
            *a.scroll = ((*a.scroll as isize + d).max(0) as usize).min(max);
            let row = a.cursor.pos.row.clamp(*a.scroll, (*a.scroll + page).saturating_sub(1).min(max));
            if row != a.cursor.pos.row {
                motion::apply(a.cursor, Motion::Row(row), a.lines, &[], 1);
            }
        }
        self.update_follow();
    }

    fn buffer_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let visual = matches!(self.mode, Mode::Visual { .. });
        let in_file = matches!(self.view, View::File(_));
        if let Some(p) = self.pending.take() {
            self.pending_key(p, k);
            return;
        }
        if let KeyCode::Char(c) = k.code
            && !ctrl
            && c.is_ascii_digit()
            && (c != '0' || self.count.is_some())
        {
            let d = c.to_digit(10).unwrap_or(0) as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d).min(100_000));
            return;
        }
        let page = self.page() as isize;
        match (k.code, ctrl) {
            (KeyCode::Char('j') | KeyCode::Down, false) => self.motion(Motion::Down),
            (KeyCode::Char('k') | KeyCode::Up, false) => self.motion(Motion::Up),
            (KeyCode::Char('h') | KeyCode::Left, false) => self.motion(Motion::Left),
            (KeyCode::Char('l') | KeyCode::Right, false) => self.motion(Motion::Right),
            (KeyCode::Char('w'), false) => self.motion(Motion::WordForward),
            (KeyCode::Char('b'), false) => self.motion(Motion::WordBack),
            (KeyCode::Char('e'), false) => self.motion(Motion::WordEnd),
            (KeyCode::Char('0') | KeyCode::Home, false) => self.motion(Motion::LineStart),
            (KeyCode::Char('^'), false) => self.motion(Motion::FirstNonBlank),
            (KeyCode::Char('$') | KeyCode::End, false) => self.motion(Motion::LineEnd),
            (KeyCode::Char('G'), false) => match self.count.take() {
                Some(n) => self.motion(Motion::Row(n.saturating_sub(1))),
                None => self.motion(Motion::Bottom),
            },
            (KeyCode::Char('g'), false) => self.pending = Some('g'),
            (KeyCode::Char('z'), false) => self.pending = Some('z'),
            (KeyCode::Char(']'), false) => self.pending = Some(']'),
            (KeyCode::Char('['), false) => self.pending = Some('['),
            (KeyCode::Char('}'), false) => self.motion(Motion::NextBlock),
            (KeyCode::Char('{'), false) => self.motion(Motion::PrevBlock),
            (KeyCode::Char('d'), true) => {
                self.motion(Motion::Rows(page / 2));
                self.scroll_by(page / 2);
            }
            (KeyCode::Char('u'), true) => {
                self.motion(Motion::Rows(-page / 2));
                self.scroll_by(-page / 2);
            }
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => {
                self.motion(Motion::Rows(page));
                self.scroll_by(page);
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                self.motion(Motion::Rows(-page));
                self.scroll_by(-page);
            }
            (KeyCode::Char('e'), true) => self.scroll_by(1),
            (KeyCode::Char('y'), true) => self.scroll_by(-1),
            (KeyCode::Char('H'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s));
            }
            (KeyCode::Char('M'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s + self.page() / 2));
            }
            (KeyCode::Char('L'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s + self.page().saturating_sub(1)));
            }
            (KeyCode::Char('/'), false) => self.start_search(true),
            (KeyCode::Char('?'), false) => self.start_search(false),
            (KeyCode::Char('n'), false) => self.repeat_search(false),
            (KeyCode::Char('N'), false) => self.repeat_search(true),
            (KeyCode::Char('v'), false) => self.toggle_visual(false),
            (KeyCode::Char('V'), false) => self.toggle_visual(true),
            (KeyCode::Char('o'), false) if visual => {
                if let Mode::Visual { line, anchor } = self.mode {
                    let cur = self.cursor_pos();
                    self.mode = Mode::Visual { line, anchor: cur };
                    if let Some(a) = self.active() {
                        a.cursor.jump(anchor, a.lines);
                    }
                }
            }
            (KeyCode::Char('y'), false) if visual => self.yank_selection(),
            (KeyCode::Char('c'), false) => self.begin_comment(),
            (KeyCode::Enter, _) if visual || in_file => self.begin_comment(),
            (KeyCode::Enter, _) => {
                if !self.open_link_at_cursor(true) && !self.toggle_collapse() {
                    self.open_link_at_cursor(false);
                }
            }
            (KeyCode::Tab, _) if in_file => {
                if let View::File(fv) = &mut self.view {
                    fv.toggle_full();
                }
            }
            (KeyCode::Tab, _) => {
                self.toggle_collapse();
            }
            (KeyCode::Esc, _) if visual => self.mode = Mode::Normal,
            (KeyCode::Esc | KeyCode::Char('q'), _) if in_file => self.close_view(),
            (KeyCode::Char('r'), false) if in_file => {
                self.reload_file();
                self.set_flash("reloaded");
            }
            (KeyCode::Char('i' | 'a' | 'A' | 'I'), false) => self.mode = Mode::Insert,
            (KeyCode::Char(':'), false) => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            (KeyCode::Char('Q'), false) => self.open_queue(),
            (KeyCode::Char('F'), false) => self.open_files(),
            (KeyCode::Char('S'), false) => {
                self.sidebar_open = !self.sidebar_open;
                self.sidebar_focus = false;
                if self.sidebar_open {
                    self.refresh_sidebar();
                }
            }
            // Ctrl-w h / Ctrl-w w: focus the session rail (vim window motions).
            (KeyCode::Char('w'), true) => self.pending = Some('W'),
            (KeyCode::F(1), _) => self.view = View::Help,
            _ => {}
        }
        self.count = None;
    }

    fn pending_key(&mut self, p: char, k: KeyEvent) {
        match (p, k.code) {
            ('g', KeyCode::Char('g')) => match self.count.take() {
                Some(n) => self.motion(Motion::Row(n.saturating_sub(1))),
                None => self.motion(Motion::Top),
            },
            ('W', KeyCode::Char('h' | 'w') | KeyCode::Left) => self.set_sidebar_focus(true),
            ('g', KeyCode::Char('f')) => {
                if !self.open_link_at_cursor(false) {
                    self.set_flash("no file link on this line");
                }
            }
            ('g', KeyCode::Char('x')) => self.open_external(),
            (']', KeyCode::Char(']' | 'c')) => self.motion(Motion::NextBlock),
            ('[', KeyCode::Char('[' | 'c')) => self.motion(Motion::PrevBlock),
            ('z', KeyCode::Char('a')) => {
                self.toggle_collapse();
            }
            ('z', KeyCode::Char(c @ ('z' | 't' | 'b'))) => {
                let page = self.page();
                if let Some(a) = self.active() {
                    let row = a.cursor.pos.row;
                    *a.scroll = match c {
                        'z' => row.saturating_sub(page / 2),
                        't' => row,
                        _ => row.saturating_sub(page.saturating_sub(1)),
                    };
                }
                self.cur.follow = false;
            }
            _ => {}
        }
        self.count = None;
    }

    fn toggle_visual(&mut self, line: bool) {
        self.mode = match self.mode {
            Mode::Visual { line: l, .. } if l == line => Mode::Normal,
            Mode::Visual { anchor, .. } => Mode::Visual { line, anchor },
            _ => Mode::Visual { line, anchor: self.cursor_pos() },
        };
    }

    fn selection(&mut self) -> (Pos, Pos, bool) {
        let cur = self.cursor_pos();
        match self.mode {
            Mode::Visual { line, anchor } => (anchor, cur, line),
            _ => (cur, cur, true),
        }
    }

    /// Opens the comment editor for the visual selection (or the current line).
    pub(crate) fn begin_comment(&mut self) {
        let (a, b, line) = self.selection();
        let anchor = match &self.view {
            View::Chat => match self.cur.transcript.quote(a, b, line) {
                Some((source, quote)) => Anchor::Chat { source, quote },
                None => {
                    self.set_flash("nothing to comment on here — select text with v or V");
                    return;
                }
            },
            View::File(fv) if !fv.lines.is_empty() => fv.anchor(a.row, b.row),
            _ => return,
        };
        self.comment_target = Some(anchor);
        self.editing = None;
        self.comment = Input::default();
        self.mode = Mode::Comment;
    }

    /// Saves the comment being edited into the queue (or updates the edited one).
    fn save_comment(&mut self) {
        let body = self.comment.take();
        if body.trim().is_empty() {
            self.set_flash("empty comment discarded");
        } else if let Some(i) = self.editing.take() {
            if let Some(c) = self.cur.queue.get_mut(i) {
                c.body = body;
            }
        } else if let Some(anchor) = self.comment_target.take() {
            self.cur.queue.push(Comment { anchor, body });
            let n = self.cur.queue.len();
            self.set_flash(format!("comment queued ({n}) — sent with your next message"));
        }
        self.comment_target = None;
        self.mode = Mode::Normal;
    }

    fn comment_key(&mut self, k: KeyEvent) {
        match self.comment.handle(k) {
            InputAction::Submit => self.save_comment(),
            InputAction::Cancel => {
                self.comment_target = None;
                self.editing = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn yank_selection(&mut self) {
        let (a, b, line) = self.selection();
        let text = match &self.view {
            View::Chat => self.cur.transcript.quote(a, b, line).map(|(_, q)| q),
            View::File(fv) if !fv.lines.is_empty() => match fv.anchor(a.row, b.row) {
                Anchor::File { snippet, .. } => Some(snippet),
                Anchor::Chat { quote, .. } => Some(quote),
            },
            _ => None,
        };
        if let Some(t) = text {
            self.osc_out = Some(format!("\x1b]52;c;{}\x07", base64(t.as_bytes())));
            self.set_flash(format!("yanked {} lines to clipboard", t.lines().count()));
        }
        self.mode = Mode::Normal;
    }

    fn start_search(&mut self, forward: bool) {
        self.mode_before_search = match self.mode {
            Mode::Visual { .. } => self.mode,
            _ => Mode::Normal,
        };
        self.minibuf = Input::default();
        self.mode = Mode::Search { forward };
    }

    fn search_key(&mut self, k: KeyEvent, forward: bool) {
        match self.minibuf.handle(k) {
            InputAction::Submit => {
                let needle = self.minibuf.take();
                self.mode = self.mode_before_search;
                let needle = if needle.is_empty() {
                    match &self.last_search {
                        Some((n, _)) => n.clone(),
                        None => return,
                    }
                } else {
                    needle
                };
                self.last_search = Some((needle.clone(), forward));
                self.find(&needle, forward);
            }
            InputAction::Cancel => self.mode = self.mode_before_search,
            _ => {}
        }
    }

    fn repeat_search(&mut self, reverse: bool) {
        if let Some((needle, forward)) = self.last_search.clone() {
            self.find(&needle, forward != reverse);
        }
    }

    fn find(&mut self, needle: &str, forward: bool) {
        let found = match self.active() {
            Some(a) => match motion::search(a.lines, a.cursor.pos, needle, forward) {
                Some(p) => {
                    a.cursor.jump(p, a.lines);
                    true
                }
                None => false,
            },
            None => true,
        };
        if !found {
            self.set_flash(format!("pattern not found: {needle}"));
        }
        self.update_follow();
    }

    fn command_key(&mut self, k: KeyEvent) {
        match self.minibuf.handle(k) {
            InputAction::Submit => {
                let cmd = self.minibuf.take();
                self.mode = Mode::Normal;
                self.run_command(cmd.trim());
            }
            InputAction::Cancel => self.mode = Mode::Normal,
            _ => {}
        }
    }

    pub(crate) fn run_command(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
        match name {
            "" => {}
            "q!" | "qa!" => self.quit = true,
            "q" | "quit" | "qa" => match self.others_running() {
                0 => self.quit = true,
                n => self.set_flash(format!("{n} other session(s) still running — :q! quits and stops them")),
            },
            "new" => {
                self.new_session(self.cur.host);
            }
            "restart" => {
                self.request(self.cur.host, Request::Restart);
                self.set_flash(format!("restarting {}…", self.hosts[self.cur.host].name));
            }
            "host" if !arg.is_empty() => self.add_remote_host(arg),
            "host" => self.set_flash(":host needs an SSH destination, like user@box or user@box:/dir"),
            "help" | "h" => self.view = View::Help,
            "files" => self.open_files(),
            "model" | "models" if arg.is_empty() => self.open_models(),
            "model" => self.set_model(arg.to_owned()),
            "comments" | "queue" => self.open_queue(),
            "dropcomments" => {
                let n = self.cur.queue.len();
                self.cur.queue.clear();
                self.set_flash(format!("dropped {n} comments"));
            }
            "send" => self.submit(),
            "now" => self.send_now(),
            "e" | "edit" | "open" if !arg.is_empty() => {
                let (p, line) = match arg.rsplit_once(':') {
                    Some((p, l)) if l.parse::<usize>().is_ok() => (p, l.parse().ok()),
                    _ => (arg, None),
                };
                let path = resolve_path(&self.cur.cwd, p);
                if !self.host_is_local(self.cur.host) || path.is_file() {
                    self.open_file(path, line);
                } else {
                    self.set_flash(format!("not a file: {arg}"));
                }
            }
            n if n.parse::<usize>().is_ok() => {
                let line: usize = n.parse().unwrap_or(1);
                match &mut self.view {
                    View::File(fv) => fv.jump_to_line(line),
                    _ => self.motion(Motion::Row(line.saturating_sub(1))),
                }
            }
            _ => self.set_flash(format!("unknown command :{cmd} (see :help)")),
        }
    }

    // ------------------------------------------------------------ views

    fn close_view(&mut self) {
        self.view = View::Chat;
        self.mode = Mode::Normal;
    }

    fn open_queue(&mut self) {
        self.view = View::Queue { sel: 0 };
        self.mode = Mode::Normal;
    }

    /// Lists changed files of the session on screen, asking its host when it is remote.
    fn open_files(&mut self) {
        let (host, cwd) = (self.cur.host, self.cur.cwd.clone());
        if !self.host_is_local(host) {
            self.request(host, Request::ChangedFiles { cwd: cwd.display().to_string() });
            return;
        }
        let items = diff::changed_files(&cwd);
        if items.is_empty() {
            self.set_flash("no uncommitted changes");
            return;
        }
        self.view = View::Files { items, sel: 0, host, cwd };
        self.mode = Mode::Normal;
    }

    fn open_models(&mut self) {
        if self.models.is_empty() {
            return self.set_flash("no models configured — :model <id> picks one");
        }
        let sel = self.models.iter().position(|m| *m == self.cur.model).unwrap_or(0);
        self.view = View::Models { sel };
        self.mode = Mode::Normal;
    }

    /// The session on screen sends its next turns with `model`.
    fn set_model(&mut self, model: String) {
        self.set_flash(format!("{model} from the next turn"));
        self.cur.model = model;
    }

    fn host_is_local(&self, host: usize) -> bool {
        self.hosts.get(host).is_none_or(|h| h.local)
    }

    /// Opens `path` of the session on screen. Remote files are read on their host; the view
    /// opens when the `File` notice arrives.
    pub(crate) fn open_file(&mut self, path: PathBuf, line: Option<usize>) {
        self.open_file_on(self.cur.host, self.cur.cwd.clone(), path, line);
    }

    /// Re-reads the open file against its view's base, on its host when remote.
    fn reload_file(&mut self) {
        let cwd = self.cur.cwd.clone();
        let View::File(fv) = &mut self.view else { return };
        match &fv.remote {
            Some((h, c)) => {
                let (h, req) = (*h, Request::ReadFile { cwd: c.clone(), path: fv.file.path.display().to_string(), base: fv.base });
                self.request(h, req);
            }
            None => {
                fv.reload(&cwd);
                self.check_file_base();
            }
        }
    }

    /// Switches the open file between its uncommitted changes and the branch's changes against
    /// the default branch.
    fn toggle_file_base(&mut self) {
        let View::File(fv) = &mut self.view else { return };
        fv.base = fv.base.toggle();
        if fv.base.is_head() {
            self.set_flash("diff against HEAD");
        }
        self.reload_file();
    }

    /// After a load against the default branch: names the branch, or falls back to HEAD when
    /// the repository has none.
    fn check_file_base(&mut self) {
        let View::File(fv) = &mut self.view else { return };
        if fv.base.is_head() {
            return;
        }
        match fv.file.base.clone() {
            Some(b) => self.set_flash(format!("diff against {b}")),
            None => {
                fv.base = diff::Base::Head;
                self.set_flash("no default branch (origin/HEAD, origin/main, origin/master); showing HEAD");
            }
        }
    }

    fn open_file_on(&mut self, host: usize, cwd: PathBuf, path: PathBuf, line: Option<usize>) {
        if !self.host_is_local(host) {
            self.pending_file = Some(line);
            let req = Request::ReadFile { cwd: cwd.display().to_string(), path: path.display().to_string(), base: diff::Base::Head };
            self.request(host, req);
            return;
        }
        let file = diff::load(&cwd, &path);
        self.view = View::File(Box::new(FileView::new(file, line)));
        self.mode = Mode::Normal;
        self.count = None;
    }

    /// Opens the file link under the cursor, or (when `exact` is false) the first link on the row.
    fn open_link_at_cursor(&mut self, exact: bool) -> bool {
        if !matches!(self.view, View::Chat) || self.cur.transcript.total() == 0 {
            return false;
        }
        let pos = self.cur.chat_cursor.pos;
        let row = self.cur.transcript.row(pos.row);
        let link = row
            .links
            .iter()
            .find(|l| l.start <= pos.col && pos.col < l.end)
            .or(if exact { None } else { row.links.first() })
            .cloned();
        match link {
            Some(l) => {
                self.open_file((*l.path).clone(), l.line);
                true
            }
            None => false,
        }
    }

    /// `gx`: opens the URL under the cursor in the browser, or a file link with the system's
    /// default app. Off any link, the line's first URL (then file link) is used, like `gf`.
    fn open_external(&mut self) {
        if !matches!(self.view, View::Chat) || self.cur.transcript.total() == 0 {
            return self.set_flash("gx opens links in the chat");
        }
        let pos = self.cur.chat_cursor.pos;
        let (line, col) = self.logical_line_at(pos.row, pos.col);
        let urls = crate::util::find_urls(&line);
        let row = self.cur.transcript.row(pos.row);
        let file = row.links.iter().find(|l| l.start <= pos.col && pos.col < l.end).map(|l| l.path.clone());
        let url = urls.iter().find(|(s, e, _)| *s <= col && col < *e).map(|(_, _, u)| u.clone());
        let target = match (url, file) {
            (Some(u), _) => Ok(u),
            (None, Some(p)) => Err(p),
            (None, None) => match (urls.first(), row.links.first()) {
                (Some((_, _, u)), _) => Ok(u.clone()),
                (None, Some(l)) => Err(l.path.clone()),
                (None, None) => return self.set_flash("no link on this line"),
            },
        };
        match target {
            Ok(url) => {
                (self.open_url)(&url);
                self.set_flash(format!("opened {url}"));
            }
            Err(_) if !self.host_is_local(self.cur.host) => {
                let name = self.hosts[self.cur.host].name.clone();
                self.set_flash(format!("that file is on {name} — gf shows it here"));
            }
            Err(path) => {
                (self.open_url)(&path.display().to_string());
                self.set_flash(format!("opened {}", path.display()));
            }
        }
    }

    /// The whole logical line under chat row `row` (a wrapped line's rows joined), and `col`
    /// translated to a char offset in it.
    fn logical_line_at(&self, row: usize, col: usize) -> (String, usize) {
        let t = &self.cur.transcript;
        let here = t.row(row);
        let same = |r: usize| {
            let x = t.row(r);
            x.entry == here.entry && x.logical == here.logical
        };
        let mut first = row;
        while first > 0 && t.row(first).offset > 0 && same(first - 1) {
            first -= 1;
        }
        let mut line = String::new();
        let mut r = first;
        while r < t.total() && (r == first || (same(r) && t.row(r).offset > 0)) {
            let x = t.row(r);
            // Wrapping may drop the spaces it breaks at; put them back so offsets line up.
            let have = line.chars().count();
            line.extend(std::iter::repeat_n(' ', x.offset.saturating_sub(have)));
            line.push_str(&x.text);
            r += 1;
        }
        (line, here.offset + col)
    }

    fn toggle_collapse(&mut self) -> bool {
        if !matches!(self.view, View::Chat) || self.cur.transcript.total() == 0 {
            return false;
        }
        let e = self.cur.transcript.entry_at(self.cur.chat_cursor.pos.row);
        if !self.cur.transcript.entries[e].collapsible() {
            return false;
        }
        let entry = self.cur.transcript.entry_mut(e);
        entry.collapsed = !entry.collapsed;
        self.cur.transcript.layout(self.main_area.width as usize);
        let start = self.cur.transcript.entry_starts()[e];
        motion::apply(&mut self.cur.chat_cursor, Motion::Row(start), &self.cur.transcript, &[], 1);
        self.update_follow();
        true
    }

    fn queue_key(&mut self, k: KeyEvent) {
        let View::Queue { sel } = &mut self.view else { return };
        let n = self.cur.queue.len();
        if let Some(p) = self.pending.take() {
            if p == 'd' && k.code == KeyCode::Char('d') && *sel < n {
                self.cur.queue.remove(*sel);
                *sel = (*sel).min(self.cur.queue.len().saturating_sub(1));
            }
            return;
        }
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') => *sel = 0,
            KeyCode::Char('G') => *sel = n.saturating_sub(1),
            KeyCode::Char('d') => self.pending = Some('d'),
            KeyCode::Char('x') | KeyCode::Delete if *sel < n => {
                self.cur.queue.remove(*sel);
                *sel = (*sel).min(self.cur.queue.len().saturating_sub(1));
            }
            KeyCode::Char('D') => {
                self.cur.queue.clear();
                *sel = 0;
            }
            KeyCode::Char('e') | KeyCode::Enter if *sel < n => {
                let i = *sel;
                self.editing = Some(i);
                self.comment = Input::with_text(self.cur.queue[i].body.clone());
                self.mode = Mode::Comment;
            }
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => self.close_view(),
            KeyCode::Char(':') => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            _ => {}
        }
    }

    fn files_key(&mut self, k: KeyEvent) {
        let View::Files { items, sel, host, cwd } = &mut self.view else { return };
        let n = items.len();
        let (host, cwd) = (*host, cwd.clone());
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') => *sel = 0,
            KeyCode::Char('G') => *sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l' | 'o') if *sel < n => {
                let path = resolve_path(&cwd, &items[*sel].1);
                self.open_file_on(host, cwd, path, None);
            }
            KeyCode::Char('r') => self.open_files(),
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Esc | KeyCode::Char('q') => self.close_view(),
            _ => {}
        }
    }

    fn models_key(&mut self, k: KeyEvent) {
        let View::Models { sel } = &mut self.view else { return };
        let n = self.models.len();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') => *sel = 0,
            KeyCode::Char('G') => *sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l' | 'o') if *sel < n => {
                let model = self.models[*sel].clone();
                self.close_view();
                self.set_model(model);
            }
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Esc | KeyCode::Char('q') => self.close_view(),
            _ => {}
        }
    }

    // ------------------------------------------------------------- mouse

    fn mouse_pos(&mut self, col: u16, row: u16) -> Option<Pos> {
        let area = self.main_area;
        if self.sidebar_area.is_some_and(|r| r.contains((col, row).into())) || !area.contains((col, row).into()) {
            return None;
        }
        let y = (row - area.y) as usize;
        let x = (col - area.x) as usize;
        match &self.view {
            View::Chat => {
                let r = self.cur.chat_scroll + y;
                (r < self.cur.transcript.total()).then(|| Pos { row: r, col: x.saturating_sub(GUTTER) })
            }
            View::File(fv) => {
                // Row 0 of the main area is the file title.
                let r = fv.scroll + y.checked_sub(1)?;
                (r < fv.lines.len()).then(|| Pos { row: r, col: x.saturating_sub(FILE_GUTTER) + fv.hscroll })
            }
            _ => None,
        }
    }

    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
        if let Some(rail) = self.sidebar_area
            && rail.contains((m.column, m.row).into())
        {
            let last = self.sidebar.rows().len().saturating_sub(1);
            match m.kind {
                // Row 0 of the rail is its header.
                MouseEventKind::Down(MouseButton::Left) if m.row > rail.y => {
                    let i = self.sidebar_scroll + (m.row - rail.y - 1) as usize;
                    if i <= last && !self.sidebar.rows().is_empty() {
                        self.set_sidebar_focus(true);
                        self.sidebar_sel = i;
                    }
                }
                MouseEventKind::ScrollDown => self.sidebar_sel = (self.sidebar_sel + 1).min(last),
                MouseEventKind::ScrollUp => self.sidebar_sel = self.sidebar_sel.saturating_sub(1),
                // Moving the mouse over the rail changes nothing: selection and previews follow
                // clicks and keys only.
                _ => {}
            }
            self.request_preview();
            return;
        }
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.sidebar_focus = false;
        }
        match m.kind {
            MouseEventKind::ScrollDown => match &mut self.view {
                View::Queue { sel } => *sel = (*sel + 1).min(self.cur.queue.len().saturating_sub(1)),
                _ => self.scroll_by(3),
            },
            MouseEventKind::ScrollUp => match &mut self.view {
                View::Queue { sel } => *sel = sel.saturating_sub(1),
                _ => self.scroll_by(-3),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if self.prompt_area.contains((m.column, m.row).into()) {
                    self.mode = Mode::Insert;
                    return;
                }
                let Some(pos) = self.mouse_pos(m.column, m.row) else { return };
                if matches!(self.view, View::Chat) {
                    let row = self.cur.transcript.row(pos.row);
                    if let Some(l) = row.links.iter().find(|l| l.start <= pos.col && pos.col < l.end).cloned() {
                        self.open_file((*l.path).clone(), l.line);
                        return;
                    }
                }
                if let Some(a) = self.active() {
                    a.cursor.jump(pos, a.lines);
                }
                self.drag_anchor = Some(pos);
                if !matches!(self.mode, Mode::Normal) {
                    self.mode = Mode::Normal;
                }
                self.update_follow();
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.drag_anchor
                    && let Some(pos) = self.mouse_pos(m.column, m.row)
                {
                    self.mode = Mode::Visual { line: false, anchor };
                    if let Some(a) = self.active() {
                        a.cursor.jump(pos, a.lines);
                    }
                    self.update_follow();
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag_anchor = None,
            _ => {}
        }
    }

    // ------------------------------------------------------- agent events

    fn set_sub_state(&mut self, id: &str, state: Live) {
        if let Some(s) = self.hosts.get_mut(self.cur.host).and_then(|h| h.live.get_mut(id))
            && s.state != state
        {
            s.state = state;
            s.finished = (state != Live::Running).then(std::time::SystemTime::now);
            self.refresh_sidebar();
        }
    }

    fn finish_tool(&mut self, i: usize, output: String, failed: bool, duration_ms: Option<u64>) {
        let e = self.cur.transcript.entry_mut(i);
        if let Kind::Tool(t) = &mut e.kind {
            t.status = if failed { ToolStatus::Failed } else { ToolStatus::Ok };
            t.output = output;
            t.duration_ms = duration_ms;
        }
        e.collapsed = e.collapsible();
    }

    fn close_streams(&mut self) {
        for i in [self.cur.streaming_text.take(), self.cur.streaming_reasoning.take()].into_iter().flatten() {
            self.cur.transcript.entry_mut(i).streaming = false;
        }
    }

    fn stream_into(&mut self, reasoning: bool, delta: &str) {
        let slot = if reasoning { self.cur.streaming_reasoning } else { self.cur.streaming_text };
        let i = match slot {
            Some(i) => i,
            None => {
                if !reasoning {
                    // Reasoning ends when visible text starts.
                    if let Some(r) = self.cur.streaming_reasoning.take() {
                        self.cur.transcript.entry_mut(r).streaming = false;
                    }
                }
                let i = self.cur.transcript.push(if reasoning { Kind::Reasoning } else { Kind::Assistant }, "");
                self.cur.transcript.entry_mut(i).streaming = true;
                if reasoning {
                    self.cur.streaming_reasoning = Some(i);
                } else {
                    self.cur.streaming_text = Some(i);
                }
                i
            }
        };
        self.cur.transcript.entry_mut(i).text.push_str(delta);
    }

    pub fn on_agent(&mut self, ev: AgentEvent) {
        match ev {
            // The rail picks titles up from the hub's next `Sessions` notice.
            AgentEvent::Titled { .. } => {}
            AgentEvent::TurnStarted { text } => {
                self.close_streams();
                // A job note starts a turn of its own; it is not one of the user's messages.
                if jobs::is_note(&text) {
                    self.cur.transcript.push(Kind::Info, jobs::note_body(&text).to_owned());
                } else {
                    self.cur.has_input = true;
                    self.cur.transcript.push(Kind::User, text);
                }
                self.cur.running = true;
                self.cur.follow = true;
                self.cur.turn_started = Instant::now();
                self.cur.activity = Some("waiting for model".into());
            }
            AgentEvent::TextDelta(t) => {
                self.cur.activity = Some("writing".into());
                self.stream_into(false, &t);
            }
            AgentEvent::ReasoningDelta(t) => {
                self.cur.activity = Some("thinking".into());
                self.stream_into(true, &t);
            }
            AgentEvent::ToolCallStreaming { name } => {
                self.cur.activity = Some(format!("preparing {name}"));
            }
            AgentEvent::Assistant { message, usage } => {
                if let Some(u) = usage {
                    self.cur.usage += u;
                }
                if let Message::Assistant { content, .. } = &message {
                    match self.cur.streaming_text {
                        Some(i) => self.cur.transcript.entry_mut(i).text = content.clone(),
                        None if !content.is_empty() => {
                            self.cur.transcript.push(Kind::Assistant, content.clone());
                        }
                        None => {}
                    }
                }
                self.close_streams();
                self.cur.activity = Some("waiting for model".into());
            }
            AgentEvent::ToolStart { call } => {
                self.close_streams();
                let i = self.cur.transcript.push(Kind::Tool(Box::new(tool_entry(&call.id, &call.name, &call.arguments))), "");
                self.cur.tool_entries.insert(call.id, i);
                self.cur.activity = Some(format!("running {}", call.name));
            }
            AgentEvent::SubagentStarted { call_id, session_id, title } => {
                self.cur.subagent_calls.insert(call_id, session_id.clone());
                let parent = self.cur.session_id.clone();
                if let Some(h) = self.hosts.get_mut(self.cur.host) {
                    h.live.insert(session_id, LiveSub { parent, title, state: Live::Running, finished: None });
                }
                self.refresh_sidebar();
            }
            AgentEvent::ToolEnd { call_id, output, duration_ms } => {
                if let Some(&i) = self.cur.tool_entries.get(&call_id) {
                    self.finish_tool(i, output.content, output.is_error, Some(duration_ms));
                }
                if let Some(id) = self.cur.subagent_calls.remove(&call_id) {
                    self.set_sub_state(&id, if output.is_error { Live::Failed } else { Live::Done });
                }
            }
            AgentEvent::Subagent { call_id, event } => {
                if let AgentEvent::TurnEnd { cancelled, error } = &*event
                    && let Some(id) = self.cur.subagent_calls.get(&call_id).cloned()
                {
                    self.set_sub_state(&id, if *cancelled || error.is_some() { Live::Failed } else { Live::Done });
                }
                let Some(&i) = self.cur.tool_entries.get(&call_id) else { return };
                let line = match *event {
                    AgentEvent::ToolStart { call } => Some(format!("↳ {} {}", call.name, summarize(&call.name, &call.arguments))),
                    AgentEvent::ToolEnd { output, .. } if output.is_error => {
                        Some(format!("  ✗ {}", output.content.lines().next().unwrap_or("")))
                    }
                    AgentEvent::Assistant { usage: Some(u), .. } => {
                        self.cur.usage += u;
                        None
                    }
                    _ => None,
                };
                if let Some(l) = line
                    && let Kind::Tool(t) = &mut self.cur.transcript.entry_mut(i).kind
                {
                    t.children.push(l);
                }
            }
            AgentEvent::ResponseLength(r) => {
                self.cur.length = Some(match r {
                    Routing::Picked { level, confidence } => format!("{level} {confidence:.2}"),
                    Routing::Unsure { level, confidence } => format!("unsure ({level} {confidence:.2})"),
                    Routing::Unavailable { .. } => "unrouted".into(),
                });
            }
            AgentEvent::JobsNote { text } => {
                self.cur.transcript.push(Kind::Info, jobs::note_body(&text).to_owned());
            }
            AgentEvent::Question { call_id, ask } => {
                // Forms are drawn by `rdsh web`; here the questions are text and the next
                // message is the answer.
                self.cur.question = Some(call_id);
                let text = format!("{}\n\nYour next message is the answer (or fill in the form in rdsh web).", ask.as_text());
                self.cur.transcript.push(Kind::Info, text);
                self.cur.activity = Some("waiting for your answer".into());
            }
            AgentEvent::Answered { call_id, .. } => {
                if self.cur.question.as_deref() == Some(call_id.as_str()) {
                    self.cur.question = None;
                }
                self.cur.activity = Some("waiting for model".into());
            }
            AgentEvent::Warning(w) => {
                self.cur.transcript.push(Kind::Info, w);
            }
            AgentEvent::TurnEnd { cancelled, error } => {
                self.close_streams();
                self.cur.question = None;
                self.cur.running = false;
                self.cur.activity = None;
                let running: Vec<usize> = self
                    .cur
                    .tool_entries
                    .values()
                    .copied()
                    .filter(|&i| matches!(&self.cur.transcript.entries[i].kind, Kind::Tool(t) if t.status == ToolStatus::Running))
                    .collect();
                for i in running {
                    self.finish_tool(i, "interrupted".into(), true, None);
                }
                if let Some(e) = error {
                    self.cur.transcript.push(Kind::Error, e);
                }
                if cancelled {
                    self.cur.transcript.push(Kind::Info, "interrupted");
                }
                // A turn that ended with subagents still in flight was interrupted.
                for (_, id) in std::mem::take(&mut self.cur.subagent_calls) {
                    self.set_sub_state(&id, Live::Failed);
                }
                self.refresh_sidebar();
            }
        }
    }
}

fn tool_entry(id: &str, name: &str, arguments: &str) -> ToolEntry {
    ToolEntry {
        call_id: id.to_owned(),
        name: name.to_owned(),
        summary: summarize(name, arguments),
        status: ToolStatus::Running,
        output: String::new(),
        children: Vec::new(),
        duration_ms: None,
    }
}
