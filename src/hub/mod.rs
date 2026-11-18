//! The session hub: owns every running agent and fans its events out to subscribed clients.
//!
//! Clients talk to the hub only through [`Request`] and [`Notice`] values over a pair of
//! channels ([`Hub::connect`]), so a client does not know whether the hub lives in its own
//! process or behind a socket. Each session's agent runs in its own task and keeps running
//! while no client is looking at it.

pub mod net;
mod path_completion;
pub mod proto;

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::Entry},
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use anyhow::Result;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use tokio_util::sync::CancellationToken;

pub use proto::{Notice, QueuedTurn, Request, Snapshot};

use crate::{
    agent::{Agent, AgentEvent, EventSink},
    llm::{Effort, Message, Usage},
    tools::jobs::Jobs,
    trace,
};

/// Paths checked per `ResolvePaths` request; the rest are left unlinked.
const MAX_RESOLVE: usize = 500;

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
    /// Set by `Request::Restart`; the listener stops accepting clients and exits.
    restart: AtomicBool,
}

#[derive(Default)]
struct State {
    sessions: HashMap<String, Slot>,
    clients: Vec<Client>,
    next_client: u64,
    next_turn: u64,
}

impl State {
    /// Called under the state lock so updates cannot overtake each other or `Opened`.
    fn broadcast_queue(&self, id: &str) {
        let mut watchers = self.clients.iter().filter(|c| c.watch_queue && c.subscribed.contains(id)).peekable();
        if watchers.peek().is_some() {
            let notice = Notice::Queue { session_id: id.to_owned(), queued: self.sessions[id].queued() };
            for c in watchers {
                let _ = c.tx.send(notice.clone());
            }
        }
    }
}

struct Client {
    id: u64,
    tx: UnboundedSender<Notice>,
    subscribed: HashSet<String>,
    watch_queue: bool,
    completing_path: bool,
}

/// A queued user turn and its original settings.
struct Turn {
    queued: QueuedTurn,
    effort: Effort,
    model: Option<String>,
    diagrams: bool,
}

/// Hub-side record of one running agent.
struct Slot {
    pending: VecDeque<Turn>,
    // Carries one wake per enqueued turn; the editable payload stays under the state lock.
    wake_tx: UnboundedSender<()>,
    cancel: Option<CancellationToken>,
    model: String,
    cwd: PathBuf,
    /// History as of the start of the current turn (or the end of the last one).
    history: Vec<Message>,
    /// Events of the current turn, with consecutive text deltas merged.
    turn: Vec<AgentEvent>,
    running: bool,
    usage: Usage,
    /// The agent's background jobs, which keep the hub alive while they run.
    jobs: Option<Arc<Jobs>>,
    /// Its `ask` calls waiting for an answer.
    questions: Arc<crate::tools::ask::Questions>,
    /// Whether the user has sent anything to this session. A fresh session is not listed in
    /// the rail until they do, so empty sessions do not take up space.
    has_input: bool,
    /// The first text sent, shown as the title until the trace has a user message.
    title_hint: Option<String>,
}

impl Slot {
    fn snapshot(&self, session_id: &str) -> Snapshot {
        Snapshot {
            session_id: session_id.to_owned(),
            model: self.model.clone(),
            cwd: self.cwd.display().to_string(),
            history: self.history.clone(),
            turn: self.turn.clone(),
            running: self.running,
            usage: self.usage,
            queued: self.queued(),
        }
    }

    fn queued(&self) -> Vec<QueuedTurn> {
        self.pending.iter().map(|t| t.queued.clone()).collect()
    }

    /// Records the agent's state at the start (`cancel` set) or end of a turn.
    fn turn_boundary(&mut self, agent: &Agent, cancel: Option<CancellationToken>) {
        self.history = agent.history().to_vec();
        self.turn.clear();
        self.running = cancel.is_some();
        self.cancel = cancel;
        self.usage = agent.usage;
        self.model = agent.model().to_owned();
    }

    fn record(&mut self, ev: &AgentEvent) {
        match (self.turn.last_mut(), ev) {
            (Some(AgentEvent::TextDelta(a)), AgentEvent::TextDelta(b)) => a.push_str(b),
            (Some(AgentEvent::ReasoningDelta(a)), AgentEvent::ReasoningDelta(b)) => a.push_str(b),
            _ => self.turn.push(ev.clone()),
        }
    }
}

/// Whether a session's history has a message the user sent (job notes are harness messages).
fn has_user_input(history: &[Message]) -> bool {
    history.iter().any(|m| matches!(m, Message::User { content } if !crate::tools::jobs::is_note(content)))
}

/// The first line of `text`, short enough for a rail row.
fn title_hint(text: &str) -> String {
    text.lines().find(|l| !l.trim().is_empty()).unwrap_or("").chars().take(80).collect()
}

impl Drop for Inner {
    fn drop(&mut self) {
        // The last clone is gone, so no agent task can still be writing: sweep empty traces.
        if let Some(dir) = &self.sessions_dir {
            trace::prune_empty_sessions(dir);
        }
    }
}

impl Hub {
    /// @param factory builds agents for `New` and `Open`.
    /// @param sessions_dir where traces live; `None` disables listing and `ReadTrace`.
    pub fn new(factory: Factory, sessions_dir: Option<PathBuf>) -> Self {
        // Sweep traces left by sessions the user never typed into (crashed or killed hubs).
        if let Some(dir) = &sessions_dir {
            trace::prune_empty_sessions(dir);
        }
        Self { inner: Arc::new(Inner { factory, sessions_dir, state: Mutex::new(State::default()), restart: AtomicBool::new(false) }) }
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
        // Sent first on every connection so the client can compare versions before any
        // session traffic (upgrades key off this).
        let _ = ntx.send(Notice::Hello { version: env!("CARGO_PKG_VERSION").to_owned() });
        let id = {
            let mut st = self.state();
            st.next_client += 1;
            let id = st.next_client;
            st.clients.push(Client { id, tx: ntx, subscribed: HashSet::new(), watch_queue: false, completing_path: false });
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

    /// Whether any session has active or pending turns or has background jobs running.
    pub fn busy(&self) -> bool {
        self.state().sessions.values().any(|s| s.running || !s.pending.is_empty() || s.jobs.as_ref().is_some_and(|j| j.any_running()))
    }

    /// Whether `Request::Restart` has been received.
    pub fn restart_requested(&self) -> bool {
        self.inner.restart.load(Ordering::SeqCst)
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
            Request::Send { session_id, text, effort, model, diagrams } => {
                match self.enqueue(&session_id, text, effort, model, diagrams) {
                    // The first message turns the session into a rail row right away.
                    Ok(true) => self.broadcast_sessions(),
                    Ok(false) => {}
                    Err(message) => self.error(client, Some(session_id), message.into()),
                }
            }
            Request::WatchQueue => {
                let mut st = self.state();
                let State { clients, sessions, .. } = &mut *st;
                if let Some(c) = clients.iter_mut().find(|c| c.id == client && !c.watch_queue) {
                    c.watch_queue = true;
                    // An older startup path may have subscribed before opting in.
                    for id in &c.subscribed {
                        if let Some(slot) = sessions.get(id) {
                            let _ = c.tx.send(Notice::Queue { session_id: id.clone(), queued: slot.queued() });
                        }
                    }
                }
            }
            Request::EditQueued { session_id, turn_id, text } => {
                self.edit_queued(client, session_id, turn_id, text);
            }
            Request::Answer { session_id, question_id, answer } => {
                let questions = self.state().sessions.get(&session_id).map(|s| s.questions.clone());
                if !questions.is_some_and(|q| q.answer(&question_id, answer)) {
                    self.error(client, Some(session_id), "that question is no longer waiting (answered elsewhere, or the turn ended)".into());
                }
            }
            Request::Cancel { session_id } => {
                if let Some(c) = self.state().sessions.get(&session_id).and_then(|s| s.cancel.as_ref()) {
                    c.cancel();
                }
            }
            Request::RunQueuedNow { session_id } => self.run_queued_now(&session_id),
            Request::Close { session_id } => {
                if let Some(c) = self.state().clients.iter_mut().find(|c| c.id == client) {
                    c.subscribed.remove(&session_id);
                }
            }
            Request::ReadFile { cwd, path, base } => {
                let cwd = PathBuf::from(cwd);
                let full = crate::tools::resolve_path(&cwd, &path);
                if full.is_file() {
                    self.notify(client, Notice::File { file: Box::new(crate::diff::load_against(&cwd, &full, base)) });
                } else {
                    self.error(client, None, format!("no such file on this host: {}", full.display()));
                }
            }
            Request::CompletePath { session_id, request_id, prefix } => {
                self.complete_path(client, session_id, request_id, prefix);
            }
            Request::ResolvePaths { cwd, request_id, paths } => {
                let hub = self.clone();
                tokio::task::spawn_blocking(move || {
                    let base = PathBuf::from(&cwd);
                    let found = paths
                        .into_iter()
                        .take(MAX_RESOLVE)
                        .filter_map(|p| {
                            let full = crate::tools::resolve_path(&base, &p);
                            full.is_file().then(|| (p, full.to_string_lossy().into_owned()))
                        })
                        .collect();
                    hub.notify(client, Notice::ResolvedPaths { request_id, cwd, found });
                });
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
            Request::Restart => {
                // Draining happens in the serve loop: it stops accepting new clients and exits
                // once no session is busy. Serialize the restart flag with enqueueing so
                // every accepted turn is visible to the drain check.
                let st = self.state();
                self.inner.restart.store(true, Ordering::SeqCst);
                let n = Notice::Restarting;
                for c in &st.clients {
                    let _ = c.tx.send(n.clone());
                }
            }
        }
    }

    fn complete_path(&self, client: u64, session_id: String, request_id: u64, prefix: String) {
        let cwd = {
            let mut st = self.state();
            let State { clients, sessions, .. } = &mut *st;
            let Some(c) = clients.iter_mut().find(|c| c.id == client) else { return };
            // At most one filesystem job per client; don't queue stale Tab presses.
            let Some(slot) = sessions.get(&session_id).filter(|_| !c.completing_path && c.subscribed.contains(&session_id)) else {
                let _ = c.tx.send(Notice::PathCompletions { request_id, paths: Vec::new() });
                return;
            };
            c.completing_path = true;
            slot.cwd.clone()
        };
        let task = tokio::task::spawn_blocking(move || path_completion::complete(&cwd, &prefix));
        let hub = self.clone();
        tokio::spawn(async move {
            let mut paths = task.await.unwrap_or_default();
            if let Some(c) = hub.state().clients.iter_mut().find(|c| c.id == client) {
                c.completing_path = false;
                if !c.subscribed.contains(&session_id) {
                    paths.clear();
                }
                let _ = c.tx.send(Notice::PathCompletions { request_id, paths });
            }
        });
    }

    fn enqueue(&self, id: &str, text: String, effort: Effort, model: Option<String>, diagrams: bool) -> Result<bool, &'static str> {
        let mut st = self.state();
        // New turns would keep a draining hub alive; clients reconnect to its successor.
        if self.restart_requested() {
            return Err("the hub is restarting; send again once it is back");
        }
        let State { sessions, next_turn, .. } = &mut *st;
        let slot = sessions.get_mut(id).ok_or("session is not open")?;
        let turn_id = next_turn.checked_add(1).expect("turn ids exhausted");
        // The task takes the same lock before popping, so it sees the payload after this wake.
        slot.wake_tx.send(()).map_err(|_| "the session's agent has stopped")?;
        *next_turn = turn_id;
        let newly_input = !slot.has_input;
        slot.has_input = true;
        slot.title_hint.get_or_insert_with(|| title_hint(&text));
        slot.pending.push_back(Turn { queued: QueuedTurn { id: turn_id, text }, effort, model, diagrams });
        st.broadcast_queue(id);
        Ok(newly_input)
    }

    /// Folds the queue into its first turn (texts joined in order; the newest turn's settings
    /// win) and interrupts the running turn, so the merged message starts next.
    fn run_queued_now(&self, id: &str) {
        let mut st = self.state();
        let Some(slot) = st.sessions.get_mut(id) else { return };
        if slot.pending.len() > 1 {
            let mut rest = slot.pending.split_off(1);
            let first = slot.pending.front_mut().expect("two or more pending");
            for t in rest.drain(..) {
                first.queued.text.push_str("\n\n");
                first.queued.text.push_str(&t.queued.text);
                first.effort = t.effort;
                first.model = t.model.or(first.model.take());
                first.diagrams |= t.diagrams;
            }
        }
        if let Some(c) = &slot.cancel {
            c.cancel();
        }
        st.broadcast_queue(id);
    }

    fn edit_queued(&self, client: u64, session_id: String, turn_id: u64, text: String) {
        let mut st = self.state();
        let State { clients, sessions, .. } = &mut *st;
        let Some(c) = clients.iter().find(|c| c.id == client) else { return };
        let updated = if c.subscribed.contains(&session_id)
            && !text.trim().is_empty()
            && let Some(turn) = sessions.get_mut(&session_id).and_then(|s| s.pending.iter_mut().find(|t| t.queued.id == turn_id))
        {
            turn.queued.text = text;
            true
        } else {
            false
        };
        let _ = c.tx.send(Notice::QueueEdited { session_id: session_id.clone(), turn_id, updated });
        if updated {
            st.broadcast_queue(&session_id);
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
        let mut recorded = self.inner.sessions_dir.as_deref().map(trace::list_sessions).unwrap_or_default();
        let st = self.state();
        // A session the user has not typed into yet is not a session: keep it out of the rail
        // (the hub also deletes its empty trace when it shuts down).
        recorded.retain(|s| s.has_input || st.sessions.get(&s.id).is_some_and(|slot| slot.has_input));
        // The trace gets its first user message just after `Send`; show the sent text until then.
        for s in &mut recorded {
            if s.title.is_empty()
                && let Some(slot) = st.sessions.get(&s.id)
            {
                s.title = slot.title_hint.clone().unwrap_or_default();
            }
        }
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
        let has_input = has_user_input(agent.history());
        // Clients of the hub can answer forms, so the agent may ask.
        let questions = Arc::new(crate::tools::ask::Questions::default());
        agent.enable_questions(questions.clone());
        let (wake_tx, mut wake_rx) = mpsc::unbounded_channel::<()>();
        {
            let mut st = self.state();
            // Two clients can build the same resumed session concurrently. Keep the first
            // slot and its task together; replacing it would lose pending turns and let a
            // wake from the old task pop a turn belonging to the new one.
            let Entry::Vacant(entry) = st.sessions.entry(id.clone()) else { return id };
            entry.insert(Slot {
                pending: VecDeque::new(),
                wake_tx,
                cancel: None,
                model: agent.model().to_owned(),
                cwd: agent.cwd().to_owned(),
                history: agent.history().to_vec(),
                turn: Vec::new(),
                running: false,
                usage: agent.usage,
                jobs: agent.jobs().cloned(),
                questions,
                has_input,
                title_hint: None,
            });
        }
        let hub = self.clone();
        let sid = id.clone();
        let jobs = agent.jobs().cloned();
        tokio::spawn(async move {
            loop {
                // A job that finishes while the session is idle gets a turn of its own.
                let from_queue = tokio::select! {
                    wake = wake_rx.recv() => match wake {
                        Some(()) => true,
                        None => break,
                    },
                    _ = job_finished(jobs.as_deref()) => false,
                };
                if !from_queue && !jobs.as_ref().is_some_and(|j| j.has_news()) {
                    // Already reported mid-turn.
                    continue;
                }
                let cancel = CancellationToken::new();
                let turn = {
                    let mut st = hub.state();
                    let slot = st.sessions.get_mut(&sid).expect("agent has a slot");
                    let turn = if from_queue {
                        let Some(t) = slot.pending.pop_front() else { continue };
                        agent.effort = t.effort;
                        agent.diagrams = t.diagrams;
                        if t.model.is_some() {
                            agent.model = t.model;
                        }
                        Some(t.queued.text)
                    } else {
                        None
                    };
                    // Popping and marking the turn active are atomic with edits and `busy`.
                    slot.turn_boundary(&agent, Some(cancel.clone()));
                    if from_queue {
                        st.broadcast_queue(&sid);
                    }
                    turn
                };
                hub.broadcast_sessions();
                let (h, s) = (hub.clone(), sid.clone());
                let sink: EventSink = Arc::new(move |ev| h.publish(&s, ev));
                match turn {
                    Some(text) => drop(agent.run_turn(text, sink, cancel).await),
                    None => drop(agent.run_job_turn(sink, cancel).await),
                }
                hub.state().sessions.get_mut(&sid).expect("agent has a slot").turn_boundary(&agent, None);
                hub.broadcast_sessions();
            }
        });
        id
    }
}

/// Resolves when one of `jobs` finishes; never without jobs.
async fn job_finished(jobs: Option<&Jobs>) {
    match jobs {
        Some(j) => j.finished().await,
        None => std::future::pending().await,
    }
}
