use std::{
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rdsh::{
    agent::{Agent, AgentEvent, AgentOptions},
    hub::{Factory, Hub, Notice, OpenKind, QueuedTurn, Request, Snapshot},
    llm::{self, Effort, Message, Provider, ScriptedProvider},
    tools, trace,
};
use tokio::sync::{Semaphore, mpsc::UnboundedReceiver};

fn options(provider: Arc<dyn Provider>, cwd: &Path, t: trace::Trace) -> AgentOptions {
    AgentOptions {
        provider,
        tools: Arc::new(tools::builtin()),
        system_prompt: "sys".into(),
        max_steps: 5,
        cwd: cwd.to_path_buf(),
        bash_timeout: Duration::from_secs(5),
        max_tool_output: 30_000,
        trace: Some(t),
        length: None,
        titler: None,
    }
}

/// A hub whose agents answer from `provider`, with traces in `dir`. Counts factory calls.
fn hub(provider: Arc<dyn Provider>, dir: PathBuf, builds: Arc<AtomicUsize>) -> Hub {
    let n = Arc::new(AtomicUsize::new(0));
    let d = dir.clone();
    let factory: Factory = Arc::new(move |kind| {
        builds.fetch_add(1, Ordering::SeqCst);
        let p = provider.clone();
        Ok(match kind {
            OpenKind::New(_) => {
                let id = format!("s{}", n.fetch_add(1, Ordering::SeqCst) + 1);
                Agent::new(options(p, &d, trace::Trace::create(&d, &id, "scripted", &d, None)?))
            }
            OpenKind::Resume(id) => {
                let path = trace::find_session(&d, &id)?;
                let loaded = trace::load(&path)?;
                Agent::resume(options(p, &d, trace::Trace::open_append(&path, &id)?), loaded.messages)
            }
        })
    });
    Hub::new(factory, Some(dir))
}

async fn next(rx: &mut UnboundedReceiver<Notice>) -> Notice {
    tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.expect("notice in time").expect("hub alive")
}

/// Skips `Sessions` and `Hello` notices.
async fn next_non_list(rx: &mut UnboundedReceiver<Notice>) -> Notice {
    loop {
        match next(rx).await {
            Notice::Sessions { .. } | Notice::Hello { .. } => continue,
            n => return n,
        }
    }
}

/// Collects one session's events until its `TurnEnd`.
async fn turn_events(rx: &mut UnboundedReceiver<Notice>) -> Vec<AgentEvent> {
    let mut out = Vec::new();
    loop {
        if let Notice::Event { event, .. } = next_non_list(rx).await {
            let end = matches!(event, AgentEvent::TurnEnd { .. });
            out.push(event);
            if end {
                return out;
            }
        }
    }
}

fn kinds(evs: &[AgentEvent]) -> Vec<String> {
    evs.iter().map(|e| format!("{e:?}").split([' ', '(', '{']).next().unwrap().to_owned()).collect()
}

#[tokio::test]
async fn two_clients_see_the_same_session_and_a_late_joiner_misses_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(
        ScriptedProvider::new([Message::assistant("first answer", vec![]), Message::assistant("second answer", vec![])])
            .with_delay(Duration::from_millis(200)),
    );
    let builds = Arc::new(AtomicUsize::new(0));
    let hub = hub(provider.clone(), dir.path().to_path_buf(), builds.clone());
    let (a_tx, mut a_rx) = hub.connect();
    let (b_tx, mut b_rx) = hub.connect();

    a_tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut a_rx).await else { panic!() };
    let id = snapshot.session_id.clone();
    assert_eq!(id, "s1");

    // A starts a turn; B joins while the model is still thinking.
    a_tx.send(Request::Send { session_id: id.clone(), text: "hello".into(), effort: Effort::Low, model: None, diagrams: false }).unwrap();
    let Notice::Event { event: AgentEvent::TurnStarted { .. }, .. } = next_non_list(&mut a_rx).await else { panic!() };
    b_tx.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot: late } = next_non_list(&mut b_rx).await else { panic!() };
    assert!(late.running);
    assert!(matches!(late.turn.first(), Some(AgentEvent::TurnStarted { text }) if text == "hello"));

    let a_rest = turn_events(&mut a_rx).await;
    let b_rest = turn_events(&mut b_rx).await;
    // Snapshot + live events for B cover exactly what A saw after TurnStarted.
    let mut b_all = late.turn.clone();
    b_all.extend(b_rest);
    let mut a_all = vec![AgentEvent::TurnStarted { text: "hello".into() }];
    a_all.extend(a_rest);
    assert_eq!(kinds(&a_all), kinds(&b_all));
    assert!(b_all.iter().any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "first answer")));

    // Either client can drive the session; both see the turn.
    b_tx.send(Request::Send { session_id: id.clone(), text: "again".into(), effort: Effort::Off, model: None, diagrams: false }).unwrap();
    let a2 = turn_events(&mut a_rx).await;
    let b2 = turn_events(&mut b_rx).await;
    assert_eq!(kinds(&a2), kinds(&b2));
    assert!(matches!(&a2[0], AgentEvent::TurnStarted { text } if text == "again"));
    assert_eq!(builds.load(Ordering::SeqCst), 1, "joining does not build a second agent");
    assert_eq!(provider.efforts(), [Effort::Low, Effort::Off], "each turn thinks as its sender asked");
    assert_eq!(provider.models(), [None, None]);
}

#[tokio::test]
async fn untouched_sessions_stay_out_of_the_rail_until_the_first_message() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("ok", vec![])]));
    let hub = hub(provider, dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();

    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    let id = snapshot.session_id;
    // The `Sessions` notice that follows `New` leaves the untouched session out of the rail.
    let Notice::Sessions { recorded, open, .. } = next(&mut rx).await else { panic!() };
    assert!(open.contains(&id));
    assert!(!recorded.iter().any(|s| s.id == id), "an empty session is not listed");
    assert!(dir.path().join(format!("{id}.jsonl")).exists(), "its trace is kept until the hub exits");

    // Sending the first message makes it a rail row immediately, titled from the text while
    // the trace still only has its `session_start` record.
    let send = Request::Send { session_id: id.clone(), text: "hello".into(), effort: Effort::High, model: None, diagrams: false };
    tx.send(send).unwrap();
    let mut row = None;
    for _ in 0..10 {
        if let Notice::Sessions { recorded, .. } = next(&mut rx).await
            && let Some(s) = recorded.into_iter().find(|s| s.id == id)
        {
            row = Some(s);
            break;
        }
    }
    let row = row.expect("the session appears once the user types");
    assert_eq!(row.title, "hello");
}

#[test]
fn a_hub_shutdown_prunes_sessions_that_were_never_typed_into() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([]));
    let builds = Arc::new(AtomicUsize::new(0));
    let d = dir.path().to_path_buf();
    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).enable_all().build().unwrap();
    let path = rt.block_on(async {
        let (tx, mut rx) = hub(provider, d.clone(), builds).connect();
        tx.send(Request::New { cwd: None }).unwrap();
        let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
        let path = d.join(format!("{}.jsonl", snapshot.session_id));
        assert!(path.exists());
        drop(tx);
        path
    });
    // Dropping the runtime drops the hub's agent tasks; the last `Hub` clone then sweeps empty traces.
    drop(rt);
    assert!(!path.exists(), "an untouched session's trace is deleted when the hub shuts down");
}

#[tokio::test]
async fn sessions_keep_running_without_viewers_and_resume_on_open() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("done unattended", vec![]),
        Message::assistant("resumed answer", vec![]),
    ]));
    let builds = Arc::new(AtomicUsize::new(0));
    let hub1 = hub(provider.clone(), dir.path().to_path_buf(), builds.clone());
    let (tx, mut rx) = hub1.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    let id = snapshot.session_id;
    // Stop watching, then send: the turn still runs.
    tx.send(Request::Close { session_id: id.clone() }).unwrap();
    tx.send(Request::Send { session_id: id.clone(), text: "work".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();
    // Wait until the hub reports the session idle again.
    let mut saw_running = false;
    loop {
        if let Notice::Sessions { running, recorded, .. } = next(&mut rx).await {
            saw_running |= running.contains(&id);
            if saw_running && !running.contains(&id) {
                assert!(recorded.iter().any(|s| s.id == id && s.title == "work"));
                break;
            }
        }
    }
    tx.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    assert!(snapshot.history.iter().any(|m| matches!(m, Message::Assistant { content, .. } if content == "done unattended")));

    // A fresh hub (a restart) resumes the recorded session from its trace when opened.
    drop(tx);
    let hub2 = hub(provider, dir.path().to_path_buf(), builds.clone());
    let (tx2, mut rx2) = hub2.connect();
    tx2.send(Request::Open { session_id: "last".into() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx2).await else { panic!() };
    assert_eq!(snapshot.session_id, id);
    assert!(!snapshot.history.is_empty());
    tx2.send(Request::Send { session_id: id.clone(), text: "continue".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();
    let evs = turn_events(&mut rx2).await;
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "resumed answer")));
    assert_eq!(builds.load(Ordering::SeqCst), 2, "one New, one Resume");
}

#[tokio::test]
async fn a_model_picked_for_a_turn_sticks_and_is_traced() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("one", vec![]),
        Message::assistant("two", vec![]),
        Message::assistant("three", vec![]),
    ]));
    let hub = hub(provider.clone(), dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    let id = snapshot.session_id;
    assert_eq!(snapshot.model, "scripted");
    let send = |text: &str, model: Option<&str>| {
        let model = model.map(str::to_owned);
        tx.send(Request::Send { session_id: id.clone(), text: text.into(), effort: Effort::High, model, diagrams: false }).unwrap();
    };
    send("a", None);
    turn_events(&mut rx).await;
    send("b", Some("pro"));
    turn_events(&mut rx).await;
    // A turn that names no model keeps the one picked last.
    send("c", None);
    turn_events(&mut rx).await;
    assert_eq!(provider.models(), [None, Some("pro".into()), Some("pro".into())]);
    tx.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    assert_eq!(snapshot.model, "pro");
    let loaded = trace::load(&dir.path().join(format!("{id}.jsonl"))).unwrap();
    let turns: Vec<String> = loaded
        .records
        .iter()
        .filter_map(|r| match &r.event {
            trace::TraceEvent::Turn { model, .. } => Some(model.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(turns, ["scripted", "pro", "pro"]);
    assert_eq!(loaded.turn_model.as_deref(), Some("pro"));
}

#[tokio::test]
async fn cancel_read_trace_and_errors() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("slow", vec![])]).with_delay(Duration::from_secs(30)));
    let hub = hub(provider, dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    let id = snapshot.session_id;
    tx.send(Request::Send { session_id: id.clone(), text: "x".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();
    let Notice::Event { event: AgentEvent::TurnStarted { .. }, .. } = next_non_list(&mut rx).await else { panic!() };
    tx.send(Request::Cancel { session_id: id.clone() }).unwrap();
    let evs = turn_events(&mut rx).await;
    assert!(matches!(evs.last(), Some(AgentEvent::TurnEnd { cancelled: true, .. })));

    tx.send(Request::ReadTrace { session_id: id.clone() }).unwrap();
    let Notice::Trace { history, .. } = next_non_list(&mut rx).await else { panic!() };
    assert!(history.iter().any(|m| matches!(m, Message::User { content } if content == "x")));

    tx.send(Request::Open { session_id: "nope".into() }).unwrap();
    assert!(matches!(next_non_list(&mut rx).await, Notice::Error { .. }));
    tx.send(Request::Send { session_id: "nope".into(), text: "x".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();
    assert!(matches!(next_non_list(&mut rx).await, Notice::Error { .. }));
}

#[test]
fn protocol_messages_roundtrip_as_json() {
    let n = Notice::Event {
        session_id: "s1".into(),
        event: AgentEvent::Subagent {
            call_id: "c".into(),
            event: Box::new(AgentEvent::ToolEnd {
                call_id: "d".into(),
                output: tools::ToolOutput::ok("ok"),
                duration_ms: 3,
            }),
        },
    };
    let text = serde_json::to_string(&n).unwrap();
    let back: Notice = serde_json::from_str(&text).unwrap();
    assert_eq!(serde_json::to_string(&back).unwrap(), text);
    let r = Request::Send { session_id: "s1".into(), text: "hi".into(), effort: Effort::High, model: None, diagrams: false };
    assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
    // Clients from before thinking effort existed get the default.
    let old: Request = serde_json::from_str(r#"{"type":"send","session_id":"s1","text":"hi"}"#).unwrap();
    assert_eq!(old, Request::Send { session_id: "s1".into(), text: "hi".into(), effort: Effort::High, model: None, diagrams: false });
    assert!(text.contains(r#""type":"event""#));
}

#[test]
fn queue_protocol_defaults_to_empty_for_old_snapshots() {
    let old = serde_json::json!({
        "session_id": "s1", "model": "scripted", "cwd": "/tmp", "history": [], "turn": [],
        "running": false, "usage": llm::Usage::default(),
    });
    let mut snapshot: Snapshot = serde_json::from_value(old.clone()).unwrap();
    assert!(snapshot.queued.is_empty());
    assert_eq!(serde_json::to_value(&snapshot).unwrap(), old);

    snapshot.queued.push(QueuedTurn { id: 7, text: "hi".into() });
    let opened = serde_json::to_value(Notice::Opened { snapshot }).unwrap();
    assert_eq!(opened["snapshot"]["queued"], serde_json::json!([{"id": 7, "text": "hi"}]));
    assert_eq!(serde_json::to_value(serde_json::from_value::<Notice>(opened.clone()).unwrap()).unwrap(), opened);

    for (request, json) in [
        (Request::WatchQueue, serde_json::json!({"type": "watch_queue"})),
        (
            Request::EditQueued { session_id: "s1".into(), turn_id: 7, text: "hello".into() },
            serde_json::json!({"type": "edit_queued", "session_id": "s1", "turn_id": 7, "text": "hello"}),
        ),
    ] {
        assert_eq!(serde_json::to_value(&request).unwrap(), json);
        assert_eq!(serde_json::from_value::<Request>(json).unwrap(), request);
    }
}

#[tokio::test]
async fn a_job_finishing_while_idle_starts_a_turn_and_keeps_the_hub_busy() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant(
            "",
            vec![rdsh::llm::ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "sleep 0.5; echo built", "run_in_background": true}).to_string(),
            }],
        ),
        Message::assistant("started the build", vec![]),
        Message::assistant("the build is done", vec![]),
    ]));
    let d = dir.path().to_path_buf();
    let p: Arc<dyn Provider> = provider.clone();
    let factory: Factory = Arc::new(move |_| {
        let mut opts = options(p.clone(), &d, trace::Trace::create(&d, "s1", "scripted", &d, None)?);
        let mut reg = tools::builtin();
        reg.add(Arc::new(tools::bash::Bash { background: true }));
        opts.tools = Arc::new(reg);
        let mut agent = Agent::new(opts);
        agent.enable_jobs(tools::jobs::Jobs::new(d.join("jobs")));
        Ok(agent)
    });
    let hub = Hub::new(factory, Some(dir.path().to_path_buf()));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    tx.send(Request::Send { session_id: snapshot.session_id, text: "build".into(), effort: Effort::Low, model: None, diagrams: false }).unwrap();
    turn_events(&mut rx).await;
    // The turn is over but the job runs: the hub must not shut down as idle.
    assert!(hub.busy());
    let evs = turn_events(&mut rx).await;
    let Some(AgentEvent::TurnStarted { text }) = evs.first() else { panic!("{evs:?}") };
    assert!(text.starts_with("<background_jobs>") && text.contains("finished: exit 0") && text.contains("built"), "{text}");
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Assistant { message: Message::Assistant { content, .. }, .. } if content == "the build is done")));
    assert!(!hub.busy());
}

/// Waits for a session event matching `f`, skipping the rest.
async fn event_where(rx: &mut UnboundedReceiver<Notice>, f: impl Fn(&AgentEvent) -> bool) -> AgentEvent {
    loop {
        if let Notice::Event { event, .. } = next_non_list(rx).await
            && f(&event)
        {
            return event;
        }
    }
}

#[tokio::test]
async fn ask_waits_for_a_client_and_the_first_answer_wins() {
    use rdsh::{
        llm::ToolCall,
        tools::ask::{Answer, AskTool},
    };
    let dir = tempfile::tempdir().unwrap();
    let form = serde_json::json!({"title": "Open a PR", "forms": [{"fields": [
        {"id": "title", "label": "PR title", "type": "text", "default": "Fix rail"},
        {"id": "draft", "label": "Draft?", "type": "confirm"}
    ]}]});
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![ToolCall { id: "q1".into(), name: "ask".into(), arguments: form.to_string() }]),
        Message::assistant("opening it", vec![]),
    ]));
    let d = dir.path().to_path_buf();
    let p = provider.clone();
    let factory: Factory = Arc::new(move |_| {
        let mut opts = options(p.clone(), &d, trace::Trace::create(&d, "s1", "scripted", &d, None)?);
        let mut tools = tools::builtin();
        tools.add(Arc::new(AskTool));
        opts.tools = Arc::new(tools);
        Ok(Agent::new(opts))
    });
    let hub = Hub::new(factory, Some(dir.path().to_path_buf()));
    let (a, mut a_rx) = hub.connect();
    let (b, mut b_rx) = hub.connect();
    a.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut a_rx).await else { panic!("opened") };
    let id = snapshot.session_id;
    b.send(Request::Open { session_id: id.clone() }).unwrap();
    assert!(matches!(next_non_list(&mut b_rx).await, Notice::Opened { .. }));
    a.send(Request::Send { session_id: id.clone(), text: "open a PR".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();

    // Both clients see the question; a client joining now sees it in the snapshot.
    let q = event_where(&mut a_rx, |e| matches!(e, AgentEvent::Question { .. })).await;
    let AgentEvent::Question { call_id, ask } = q else { unreachable!() };
    assert_eq!((call_id.as_str(), ask.forms[0].fields.len()), ("q1", 2));
    event_where(&mut b_rx, |e| matches!(e, AgentEvent::Question { .. })).await;
    let (c, mut c_rx) = hub.connect();
    c.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut c_rx).await else { panic!("opened") };
    assert!(snapshot.turn.iter().any(|e| matches!(e, AgentEvent::Question { .. })));

    // The model was offered `ask` because hub sessions can answer.
    assert!(provider.requests().len() == 1);

    let mut values = serde_json::Map::new();
    values.insert("title".into(), serde_json::json!("Fix rail counts"));
    values.insert("draft".into(), serde_json::json!(false));
    b.send(Request::Answer { session_id: id.clone(), question_id: "q1".into(), answer: Answer::Form { values } }).unwrap();
    let ans = event_where(&mut a_rx, |e| matches!(e, AgentEvent::Answered { .. })).await;
    assert!(matches!(ans, AgentEvent::Answered { call_id, .. } if call_id == "q1"));
    event_where(&mut a_rx, |e| matches!(e, AgentEvent::TurnEnd { .. })).await;

    // A late answer from another client is refused.
    a.send(Request::Answer { session_id: id.clone(), question_id: "q1".into(), answer: Answer::Dismissed }).unwrap();
    assert!(matches!(next_non_list(&mut a_rx).await, Notice::Error { .. }));

    let reqs = provider.requests();
    let result = reqs[1].iter().find_map(|m| match m {
        Message::Tool { name, content, .. } if name == "ask" => Some(content.clone()),
        _ => None,
    });
    let result = result.expect("the answer reached the model");
    assert!(result.contains("title: \"Fix rail counts\"") && result.contains("draft: false"), "{result}");
}

/// Holds each model response until the test explicitly releases the active turn.
struct GatedProvider {
    scripted: Arc<ScriptedProvider>,
    permits: Semaphore,
}

impl Provider for GatedProvider {
    fn model(&self) -> &str {
        self.scripted.model()
    }

    fn complete<'a>(&'a self, req: llm::Request<'a>, on_delta: llm::DeltaSink<'a>) -> llm::BoxFuture<'a, anyhow::Result<llm::Completion>> {
        Box::pin(async move {
            self.permits.acquire().await.unwrap().forget();
            self.scripted.complete(req, on_delta).await
        })
    }
}

async fn queue_notice(rx: &mut UnboundedReceiver<Notice>, id: &str) -> Vec<QueuedTurn> {
    let notice = next_non_list(rx).await;
    let Notice::Queue { session_id, queued } = notice else { panic!("expected Queue, got {notice:?}") };
    assert_eq!(session_id, id);
    queued
}

async fn assert_edit_reply(rx: &mut UnboundedReceiver<Notice>, id: &str, turn: u64, accepted: bool) {
    let notice = next_non_list(rx).await;
    let Notice::QueueEdited { session_id, turn_id, updated } = notice else { panic!("expected QueueEdited, got {notice:?}") };
    assert_eq!((session_id.as_str(), turn_id, updated), (id, turn, accepted));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn clients_share_queue_edits_and_pending_turns_drain_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let scripted = Arc::new(ScriptedProvider::new((0..3).map(|_| Message::assistant("ok", vec![]))));
    let provider = Arc::new(GatedProvider { scripted: scripted.clone(), permits: Semaphore::new(0) });
    let hub = hub(provider.clone(), dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (a, mut a_rx) = hub.connect();
    assert!(matches!(next(&mut a_rx).await, Notice::Hello { .. }));
    a.send(Request::WatchQueue).unwrap();
    a.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut a_rx).await else { panic!("opened") };
    assert!(snapshot.queued.is_empty());
    let id = snapshot.session_id;
    let (legacy, mut legacy_rx) = hub.connect();
    legacy.send(Request::Open { session_id: id.clone() }).unwrap();
    assert!(matches!(next_non_list(&mut legacy_rx).await, Notice::Opened { .. }));

    let send = |text: &str, effort: Effort, model: Option<&str>, diagrams: bool| Request::Send {
        session_id: id.clone(),
        text: text.into(),
        effort,
        model: model.map(str::to_owned),
        diagrams,
    };
    a.send(send("active", Effort::High, None, false)).unwrap();
    let first = queue_notice(&mut a_rx, &id).await;
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].text, "active");
    assert!(queue_notice(&mut a_rx, &id).await.is_empty());
    assert!(matches!(next_non_list(&mut a_rx).await, Notice::Event { event: AgentEvent::TurnStarted { .. }, .. }));

    a.send(send("draft", Effort::Low, Some("pro"), true)).unwrap();
    let second = queue_notice(&mut a_rx, &id).await;
    assert_eq!(second.len(), 1);
    assert_eq!(second[0].text, "draft");
    a.send(send("last", Effort::Off, None, false)).unwrap();
    let mut pending = queue_notice(&mut a_rx, &id).await;
    assert_eq!(pending.len(), 2);
    assert_eq!(pending[0], second[0]);
    assert_eq!(pending[1].text, "last");
    assert!(first[0].id < pending[0].id && pending[0].id < pending[1].id);

    let (b, mut b_rx) = hub.connect();
    assert!(matches!(next(&mut b_rx).await, Notice::Hello { .. }));
    b.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut b_rx).await else { panic!("opened") };
    assert!(snapshot.running);
    assert_eq!(snapshot.queued, pending);
    b.send(Request::WatchQueue).unwrap();
    assert_eq!(queue_notice(&mut b_rx, &id).await, pending, "late opt-in refreshes the queue");

    let edit = |turn_id, text: &str| Request::EditQueued { session_id: id.clone(), turn_id, text: text.into() };
    let edited_id = pending[0].id;
    b.send(edit(edited_id, " \n\t")).unwrap();
    assert_edit_reply(&mut b_rx, &id, edited_id, false).await;
    b.send(Request::Close { session_id: id.clone() }).unwrap();
    b.send(edit(edited_id, "not subscribed")).unwrap();
    assert_edit_reply(&mut b_rx, &id, edited_id, false).await;
    b.send(Request::Open { session_id: id.clone() }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut b_rx).await else { panic!("reopened") };
    assert_eq!(snapshot.queued, pending);

    pending[0].text = "edited".into();
    for _ in 0..2 {
        // An identical replacement is accepted too. Only B gets the reply, both get the queue.
        b.send(edit(edited_id, "edited")).unwrap();
        assert_edit_reply(&mut b_rx, &id, edited_id, true).await;
        assert_eq!(queue_notice(&mut a_rx, &id).await, pending);
        assert_eq!(queue_notice(&mut b_rx, &id).await, pending);
    }

    a.send(Request::Restart).unwrap();
    assert!(matches!(next_non_list(&mut a_rx).await, Notice::Restarting));
    assert!(matches!(next_non_list(&mut b_rx).await, Notice::Restarting));
    assert!(hub.busy());
    provider.permits.add_permits(1);
    turn_events(&mut a_rx).await;
    turn_events(&mut b_rx).await;
    assert_eq!(queue_notice(&mut a_rx, &id).await, &pending[1..]);
    assert_eq!(queue_notice(&mut b_rx, &id).await, &pending[1..]);
    for rx in [&mut a_rx, &mut b_rx] {
        assert!(matches!(next_non_list(rx).await, Notice::Event { event: AgentEvent::TurnStarted { text }, .. } if text == "edited"));
    }
    b.send(edit(edited_id, "too late")).unwrap();
    assert_edit_reply(&mut b_rx, &id, edited_id, false).await;
    assert!(hub.busy());

    provider.permits.add_permits(1);
    turn_events(&mut a_rx).await;
    turn_events(&mut b_rx).await;
    assert!(queue_notice(&mut a_rx, &id).await.is_empty());
    assert!(queue_notice(&mut b_rx, &id).await.is_empty());
    provider.permits.add_permits(1);
    let last = turn_events(&mut a_rx).await;
    assert!(matches!(last.first(), Some(AgentEvent::TurnStarted { text }) if text == "last"));
    // `TurnEnd` is published just before the slot is marked idle. Wait for that report.
    loop {
        if let Notice::Sessions { running, .. } = next(&mut a_rx).await
            && !running.contains(&id)
        {
            break;
        }
    }
    assert!(!hub.busy());

    let mut starts = Vec::new();
    loop {
        match next_non_list(&mut legacy_rx).await {
            Notice::Event { event: AgentEvent::TurnStarted { text }, .. } => starts.push(text),
            Notice::Event { event: AgentEvent::TurnEnd { .. }, .. } if starts.len() == 3 => break,
            Notice::Event { .. } | Notice::Restarting => {}
            notice => panic!("legacy client received {notice:?}"),
        }
    }
    assert_eq!(starts, ["active", "edited", "last"]);
    assert_eq!(scripted.efforts(), [Effort::High, Effort::Low, Effort::Off]);
    assert_eq!(scripted.models(), [None, Some("pro".into()), Some("pro".into())]);
    let diagrams = rdsh::prompt::diagrams_note(&[], true).unwrap();
    assert!(scripted.requests()[1].contains(&Message::Directive { content: diagrams }));
}

#[tokio::test]
async fn queued_turn_ids_increase_across_sessions() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("ok", vec![]), Message::assistant("ok", vec![])]));
    let hub = hub(provider, dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::WatchQueue).unwrap();

    let mut ids = Vec::new();
    for session_id in ["s1", "s2"] {
        tx.send(Request::New { cwd: None }).unwrap();
        let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!("opened") };
        assert_eq!(snapshot.session_id, session_id);
        tx.send(Request::Send { session_id: session_id.into(), text: "hi".into(), effort: Effort::High, model: None, diagrams: false }).unwrap();
        let queued = queue_notice(&mut rx, session_id).await;
        assert_eq!(queued.len(), 1);
        ids.push(queued[0].id);
        turn_events(&mut rx).await;
    }
    assert!(ids[0] < ids[1]);
}

#[tokio::test]
async fn a_draining_hub_refuses_new_turns() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("never", vec![])]));
    let hub = hub(provider.clone(), dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    tx.send(Request::Restart).unwrap();
    assert!(matches!(next_non_list(&mut rx).await, Notice::Restarting));
    let send = Request::Send { session_id: snapshot.session_id, text: "more".into(), effort: Effort::High, model: None, diagrams: false };
    tx.send(send).unwrap();
    let Notice::Error { message, .. } = next_non_list(&mut rx).await else { panic!() };
    assert!(message.contains("restarting"), "{message}");
    assert!(!hub.busy(), "nothing started, so the hub can exit");
    assert!(provider.requests().is_empty());
}

#[tokio::test]
async fn running_the_queue_now_merges_it_and_interrupts_the_turn() {
    let dir = tempfile::tempdir().unwrap();
    let slow = Message::assistant("slow", vec![]);
    let provider = Arc::new(ScriptedProvider::new([slow, Message::assistant("merged", vec![])]).with_delay(Duration::from_millis(300)));
    let hub = hub(provider.clone(), dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    tx.send(Request::New { cwd: None }).unwrap();
    let Notice::Opened { snapshot } = next_non_list(&mut rx).await else { panic!() };
    let id = snapshot.session_id;
    let send = |text: &str| Request::Send { session_id: id.clone(), text: text.into(), effort: Effort::High, model: None, diagrams: false };
    tx.send(send("first")).unwrap();
    let Notice::Event { event: AgentEvent::TurnStarted { .. }, .. } = next_non_list(&mut rx).await else { panic!() };
    tx.send(send("second")).unwrap();
    tx.send(send("third")).unwrap();
    tx.send(Request::RunQueuedNow { session_id: id.clone() }).unwrap();
    // The first turn ends interrupted, then one turn carries both waiting messages.
    let evs = turn_events(&mut rx).await;
    assert!(matches!(evs.last(), Some(AgentEvent::TurnEnd { cancelled: true, .. })), "{evs:?}");
    let evs = turn_events(&mut rx).await;
    assert!(matches!(&evs[0], AgentEvent::TurnStarted { text } if text == "second\n\nthird"), "{evs:?}");
    assert!(matches!(evs.last(), Some(AgentEvent::TurnEnd { cancelled: false, .. })), "{evs:?}");
}

#[tokio::test]
async fn only_paths_that_are_files_on_the_host_resolve() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("src")).unwrap();
    std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
    let hub = hub(Arc::new(ScriptedProvider::new([])), dir.path().to_path_buf(), Arc::new(AtomicUsize::new(0)));
    let (tx, mut rx) = hub.connect();
    let cwd = dir.path().display().to_string();
    let paths = vec!["src/main.rs".into(), "something/else".into(), "src".into()];
    tx.send(Request::ResolvePaths { cwd: cwd.clone(), request_id: 7, paths }).unwrap();
    let Notice::ResolvedPaths { request_id, cwd: back, found } = next_non_list(&mut rx).await else { panic!() };
    assert_eq!((request_id, back), (7, cwd));
    let expected = dir.path().join("src/main.rs").display().to_string();
    assert_eq!(found, vec![("src/main.rs".to_owned(), expected)], "directories and missing paths are not links");
}
