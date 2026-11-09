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
    hub::{Factory, Hub, Notice, OpenKind, Request},
    llm::{Effort, Message, Provider, ScriptedProvider},
    tools, trace,
};
use tokio::sync::mpsc::UnboundedReceiver;

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
fn hub(provider: Arc<ScriptedProvider>, dir: PathBuf, builds: Arc<AtomicUsize>) -> Hub {
    let n = Arc::new(AtomicUsize::new(0));
    let d = dir.clone();
    let factory: Factory = Arc::new(move |kind| {
        builds.fetch_add(1, Ordering::SeqCst);
        let p: Arc<dyn Provider> = provider.clone();
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

/// Skips `Sessions` notices.
async fn next_non_list(rx: &mut UnboundedReceiver<Notice>) -> Notice {
    loop {
        match next(rx).await {
            Notice::Sessions { .. } => continue,
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
    a_tx.send(Request::Send { session_id: id.clone(), text: "hello".into(), effort: Effort::Low, model: None }).unwrap();
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
    b_tx.send(Request::Send { session_id: id.clone(), text: "again".into(), effort: Effort::Off, model: None }).unwrap();
    let a2 = turn_events(&mut a_rx).await;
    let b2 = turn_events(&mut b_rx).await;
    assert_eq!(kinds(&a2), kinds(&b2));
    assert!(matches!(&a2[0], AgentEvent::TurnStarted { text } if text == "again"));
    assert_eq!(builds.load(Ordering::SeqCst), 1, "joining does not build a second agent");
    assert_eq!(provider.efforts(), [Effort::Low, Effort::Off], "each turn thinks as its sender asked");
    assert_eq!(provider.models(), [None, None]);
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
    tx.send(Request::Send { session_id: id.clone(), text: "work".into(), effort: Effort::High, model: None }).unwrap();
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
    tx2.send(Request::Send { session_id: id.clone(), text: "continue".into(), effort: Effort::High, model: None }).unwrap();
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
        tx.send(Request::Send { session_id: id.clone(), text: text.into(), effort: Effort::High, model }).unwrap();
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
    tx.send(Request::Send { session_id: id.clone(), text: "x".into(), effort: Effort::High, model: None }).unwrap();
    let Notice::Event { event: AgentEvent::TurnStarted { .. }, .. } = next_non_list(&mut rx).await else { panic!() };
    tx.send(Request::Cancel { session_id: id.clone() }).unwrap();
    let evs = turn_events(&mut rx).await;
    assert!(matches!(evs.last(), Some(AgentEvent::TurnEnd { cancelled: true, .. })));

    tx.send(Request::ReadTrace { session_id: id.clone() }).unwrap();
    let Notice::Trace { history, .. } = next_non_list(&mut rx).await else { panic!() };
    assert!(history.iter().any(|m| matches!(m, Message::User { content } if content == "x")));

    tx.send(Request::Open { session_id: "nope".into() }).unwrap();
    assert!(matches!(next_non_list(&mut rx).await, Notice::Error { .. }));
    tx.send(Request::Send { session_id: "nope".into(), text: "x".into(), effort: Effort::High, model: None }).unwrap();
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
    let r = Request::Send { session_id: "s1".into(), text: "hi".into(), effort: Effort::High, model: None };
    assert_eq!(serde_json::from_str::<Request>(&serde_json::to_string(&r).unwrap()).unwrap(), r);
    // Clients from before thinking effort existed get the default.
    let old: Request = serde_json::from_str(r#"{"type":"send","session_id":"s1","text":"hi"}"#).unwrap();
    assert_eq!(old, Request::Send { session_id: "s1".into(), text: "hi".into(), effort: Effort::High, model: None });
    assert!(text.contains(r#""type":"event""#));
}
