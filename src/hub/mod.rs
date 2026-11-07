//! The session hub: owns every running agent and fans its events out to subscribed clients.
//!
//! Clients talk to the hub only through [`Request`] and [`Notice`] values over a pair of
//! channels ([`Hub::connect`]), so the TUI does not know whether the hub lives in its own
//! process or behind a socket. Each session's agent runs in its own task and keeps running
//! while no client is looking at it.

pub mod net;
pub mod proto;

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

pub use proto::{Notice, Request, Snapshot};

use crate::{
    agent::{Agent, AgentEvent, EventSink},
    llm::{Message, Usage},
    trace,
};

/// How the hub asks for an agent.
pub enum OpenKind {
    /// In this directory, or the factory's default when `None`.
    New(Option<PathBuf>),
    /// An exact session id.
    Resume(String),
}

/// Builds agents for the hub (`session::build` in the binary, scripted agents in tests).
pub type Factory = Arc<dyn Fn(OpenKind) -> Result<Agent> + Send + Sync>;

#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    factory: Factory,
    sessions_dir: Option<PathBuf>,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Slot>,
    clients: Vec<Client>,
    next_client: u64,
}

struct Client {
    id: u64,
    tx: UnboundedSender<Notice>,
    subscribed: HashSet<String>,
}

/// Hub-side record of one running agent.
struct Slot {
    cmd_tx: UnboundedSender<String>,
    cancel: Option<CancellationToken>,
    model: String,
    cwd: String,
    /// History as of the start of the current turn (or the end of the last one).
    history: Vec<Message>,
    /// Events of the current turn, with consecutive text deltas merged.
    turn: Vec<AgentEvent>,
    running: bool,
    usage: Usage,
}

impl Slot {
    fn snapshot(&self, session_id: &str) -> Snapshot {
        Snapshot {
            session_id: session_id.to_owned(),
            model: self.model.clone(),
            cwd: self.cwd.clone(),
            history: self.history.clone(),
            turn: self.turn.clone(),
            running: self.running,
            usage: self.usage,
        }
    }

    fn record(&mut self, ev: &AgentEvent) {
        match (self.turn.last_mut(), ev) {
            (Some(AgentEvent::TextDelta(a)), AgentEvent::TextDelta(b)) => a.push_str(b),
            (Some(AgentEvent::ReasoningDelta(a)), AgentEvent::ReasoningDelta(b)) => a.push_str(b),
            _ => self.turn.push(ev.clone()),
        }
    }
}

impl Hub {
    /// @param factory builds agents for `New` and `Open`.
    /// @param sessions_dir where traces live; `None` disables listing and `ReadTrace`.
    pub fn new(factory: Factory, sessions_dir: Option<PathBuf>) -> Self {
        Self { inner: Arc::new(Inner { factory, sessions_dir, state: Mutex::new(State::default()) }) }
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        // A panic while holding the lock leaves plain data behind; keep serving it.
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Attaches a client. Must be called inside a Tokio runtime.
    ///
    /// @returns the client's request sender and notice receiver. Dropping the sender detaches.
    pub fn connect(&self) -> (UnboundedSender<Request>, UnboundedReceiver<Notice>) {
        let (req_tx, mut req_rx) = mpsc::unbounded_channel();
        let (ntx, nrx) = mpsc::unbounded_channel();
        let id = {
            let mut st = self.state();
            st.next_client += 1;
            let id = st.next_client;
            st.clients.push(Client { id, tx: ntx, subscribed: HashSet::new() });
            id
        };
        let hub = self.clone();
        tokio::spawn(async move {
            while let Some(req) = req_rx.recv().await {
                hub.handle(id, req);
            }
            hub.state().clients.retain(|c| c.id != id);
        });
        (req_tx, nrx)
    }

    /// Connected clients.
    pub fn clients(&self) -> usize {
        self.state().clients.len()
    }

    /// Whether any session is mid-turn.
    pub fn busy(&self) -> bool {
        self.state().sessions.values().any(|s| s.running)
    }

    fn notify(&self, client: u64, notice: Notice) {
        if let Some(c) = self.state().clients.iter().find(|c| c.id == client) {
            // A closed receiver means the client is going away; its task cleans up.
            let _ = c.tx.send(notice);
        }
    }

    fn error(&self, client: u64, session_id: Option<String>, message: String) {
        self.notify(client, Notice::Error { session_id, message });
    }

    fn handle(&self, client: u64, req: Request) {
        match req {
            Request::List => {
                let n = self.sessions_notice();
                self.notify(client, n);
            }
            Request::New { cwd } => match (self.inner.factory)(OpenKind::New(cwd.map(PathBuf::from))) {
                Ok(agent) => {
                    let id = self.spawn(agent);
                    self.subscribe(client, &id);
                    self.broadcast_sessions();
                }
                Err(e) => self.error(client, None, format!("could not start a session: {e:#}")),
            },
            Request::Open { session_id } => {
                let id = match self.resolve(&session_id) {
                    Ok(id) => id,
                    Err(e) => return self.error(client, Some(session_id), format!("{e:#}")),
                };
                if !self.state().sessions.contains_key(&id) {
                    match (self.inner.factory)(OpenKind::Resume(id.clone())) {
                        Ok(agent) => {
                            self.spawn(agent);
                            self.broadcast_sessions();
                        }
                        Err(e) => return self.error(client, Some(id), format!("could not open session: {e:#}")),
                    }
                }
                self.subscribe(client, &id);
            }
            Request::Send { session_id, text } => {
                let sent = self.state().sessions.get(&session_id).map(|s| s.cmd_tx.send(text).is_ok());
                match sent {
                    Some(true) => {}
                    Some(false) => self.error(client, Some(session_id), "the session's agent has stopped".into()),
                    None => self.error(client, Some(session_id), "session is not open".into()),
                }
            }
            Request::Cancel { session_id } => {
                if let Some(c) = self.state().sessions.get(&session_id).and_then(|s| s.cancel.as_ref()) {
                    c.cancel();
                }
            }
            Request::Close { session_id } => {
                if let Some(c) = self.state().clients.iter_mut().find(|c| c.id == client) {
                    c.subscribed.remove(&session_id);
                }
            }
            Request::ReadFile { cwd, path } => {
                let cwd = PathBuf::from(cwd);
                let full = crate::tools::resolve_path(&cwd, &path);
                if full.is_file() {
                    self.notify(client, Notice::File { file: Box::new(crate::diff::load(&cwd, &full)) });
                } else {
                    self.error(client, None, format!("no such file on this host: {}", full.display()));
                }
            }
            Request::ChangedFiles { cwd } => {
                let files = crate::diff::changed_files(std::path::Path::new(&cwd));
                self.notify(client, Notice::ChangedFiles { cwd, files });
            }
            Request::ReadTrace { session_id } => {
                let loaded = self
                    .resolve(&session_id)
                    .and_then(|id| {
                        let dir = self.inner.sessions_dir.as_deref().ok_or_else(|| anyhow::anyhow!("no trace directory"))?;
                        Ok((id.clone(), trace::load(&trace::find_session(dir, &id)?)?))
                    });
                match loaded {
                    Ok((id, l)) => self.notify(client, Notice::Trace { session_id: id, cwd: l.cwd, history: l.messages }),
                    Err(e) => self.error(client, Some(session_id), format!("{e:#}")),
                }
            }
        }
    }

    /// Maps a query (id, prefix, `last`) to an exact session id.
    fn resolve(&self, query: &str) -> Result<String> {
        if self.state().sessions.contains_key(query) {
            return Ok(query.to_owned());
        }
        let Some(dir) = &self.inner.sessions_dir else { return Ok(query.to_owned()) };
        let path = trace::find_session(dir, query)?;
        Ok(path.file_stem().map_or_else(|| query.to_owned(), |s| s.to_string_lossy().into_owned()))
    }

    /// Subscribes `client` and sends it a snapshot, atomically with respect to `publish`, so
    /// the client sees every later event exactly once.
    fn subscribe(&self, client: u64, id: &str) {
        let mut st = self.state();
        let Some(snapshot) = st.sessions.get(id).map(|s| s.snapshot(id)) else { return };
        if let Some(c) = st.clients.iter_mut().find(|c| c.id == client) {
            c.subscribed.insert(id.to_owned());
            let _ = c.tx.send(Notice::Opened { snapshot });
        }
    }

    fn publish(&self, id: &str, ev: AgentEvent) {
        let titled = matches!(ev, AgentEvent::Titled { .. });
        {
            let mut st = self.state();
            if let Some(slot) = st.sessions.get_mut(id) {
                slot.record(&ev);
            }
            for c in st.clients.iter().filter(|c| c.subscribed.contains(id)) {
                let _ = c.tx.send(Notice::Event { session_id: id.to_owned(), event: ev.clone() });
            }
        }
        // Every client's rail shows titles, not only subscribers'.
        if titled {
            self.broadcast_sessions();
        }
    }

    fn sessions_notice(&self) -> Notice {
        let recorded = self.inner.sessions_dir.as_deref().map(trace::list_sessions).unwrap_or_default();
        let st = self.state();
        let mut open: Vec<String> = st.sessions.keys().cloned().collect();
        open.sort();
        let running = open.iter().filter(|id| st.sessions[*id].running).cloned().collect();
        Notice::Sessions { recorded, open, running }
    }

    fn broadcast_sessions(&self) {
        let n = self.sessions_notice();
        for c in self.state().clients.iter() {
            let _ = c.tx.send(n.clone());
        }
    }

    /// Hands `agent` to its own task and registers it.
    ///
    /// @returns the session id.
    fn spawn(&self, mut agent: Agent) -> String {
        let id = agent.session_id().unwrap_or("unsaved").to_owned();
        let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<String>();
        self.state().sessions.insert(
            id.clone(),
            Slot {
                cmd_tx,
                cancel: None,
                model: agent.model().to_owned(),
                cwd: agent.cwd().display().to_string(),
                history: agent.history().to_vec(),
                turn: Vec::new(),
                running: false,
                usage: agent.usage,
            },
        );
        let hub = self.clone();
        let sid = id.clone();
        tokio::spawn(async move {
            while let Some(text) = cmd_rx.recv().await {
                let cancel = CancellationToken::new();
                hub.turn_boundary(&sid, &agent, Some(cancel.clone()));
                let (h, s) = (hub.clone(), sid.clone());
                let sink: EventSink = Arc::new(move |ev| h.publish(&s, ev));
                agent.run_turn(text, sink, cancel).await;
                hub.turn_boundary(&sid, &agent, None);
            }
        });
        id
    }

    /// Records the agent's state at the start (`cancel` set) or end of a turn.
    fn turn_boundary(&self, id: &str, agent: &Agent, cancel: Option<CancellationToken>) {
        {
            let mut st = self.state();
            if let Some(slot) = st.sessions.get_mut(id) {
                slot.history = agent.history().to_vec();
                slot.turn.clear();
                slot.running = cancel.is_some();
                slot.cancel = cancel;
                slot.usage = agent.usage;
            }
        }
        self.broadcast_sessions();
    }
}
