use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use rdsh::{
    agent::{Agent, AgentEvent, AgentOptions, EventSink},
    llm::{Message, Provider, ScriptedProvider, ToolCall},
    tools::{self, task::Task},
    trace,
};
use tokio_util::sync::CancellationToken;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }
}

fn agent_with(provider: Arc<dyn Provider>, cwd: &std::path::Path, trace_dir: Option<&std::path::Path>) -> Agent {
    let base = Arc::new(tools::builtin());
    let mut reg = (*base).clone();
    reg.add(Arc::new(Task::new(
        provider.clone(),
        base,
        10,
        Duration::from_secs(10),
        30_000,
        trace_dir.map(|d| d.to_path_buf()),
        "parent".into(),
    )));
    let trace = trace_dir.map(|d| trace::Trace::create(d, "parent", "scripted", cwd, None).unwrap());
    Agent::new(AgentOptions {
        provider,
        tools: Arc::new(reg),
        system_prompt: "sys".into(),
        max_steps: 10,
        cwd: cwd.to_path_buf(),
        bash_timeout: Duration::from_secs(10),
        max_tool_output: 30_000,
        trace,
        length: None,
        titler: None,
    })
}

fn collecting_sink() -> (EventSink, Arc<Mutex<Vec<AgentEvent>>>) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let e = events.clone();
    (Arc::new(move |ev| e.lock().unwrap().push(ev)), events)
}

#[tokio::test]
async fn tool_loop_edits_file_and_finishes() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "hello world\n").unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "read_file", serde_json::json!({"path": "a.txt"}))]),
        Message::assistant(
            "",
            vec![call("c2", "edit_file", serde_json::json!({"path": "a.txt", "old_string": "world", "new_string": "rust"}))],
        ),
        Message::assistant("done", vec![]),
    ]));
    let mut agent = agent_with(provider.clone(), dir.path(), None);
    let (sink, events) = collecting_sink();
    let out = agent.run_turn("change it".into(), sink, CancellationToken::new()).await;
    assert_eq!(out.text, "done");
    assert!(!out.cancelled && out.error.is_none());
    assert_eq!(std::fs::read_to_string(dir.path().join("a.txt")).unwrap(), "hello rust\n");
    let reqs = provider.requests();
    assert_eq!(reqs.len(), 3);
    // Second request carries the read_file result with line numbers.
    let Message::Tool { content, .. } = reqs[1].last().unwrap() else { panic!() };
    assert!(content.contains("     1\thello world"));
    let evs = events.lock().unwrap();
    assert!(matches!(evs.last(), Some(AgentEvent::TurnEnd { cancelled: false, error: None })));
    assert_eq!(evs.iter().filter(|e| matches!(e, AgentEvent::ToolEnd { .. })).count(), 2);
}

#[tokio::test]
async fn bad_tool_calls_are_reported_to_the_model() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant(
            "",
            vec![
                ToolCall { id: "1".into(), name: "nope".into(), arguments: "{}".into() },
                ToolCall { id: "2".into(), name: "bash".into(), arguments: "{not json".into() },
            ],
        ),
        Message::assistant("ok", vec![]),
    ]));
    let mut agent = agent_with(provider.clone(), dir.path(), None);
    let (sink, _) = collecting_sink();
    agent.run_turn("x".into(), sink, CancellationToken::new()).await;
    let req = &provider.requests()[1];
    let tool_msgs: Vec<_> = req.iter().filter_map(|m| match m { Message::Tool { content, .. } => Some(content.clone()), _ => None }).collect();
    assert!(tool_msgs[0].starts_with("Error: unknown tool `nope`"));
    assert!(tool_msgs[1].starts_with("Error: invalid JSON arguments for bash"));
}

#[tokio::test]
async fn parallel_subagents_run_concurrently_and_write_traces() {
    let dir = tempfile::tempdir().unwrap();
    let traces = tempfile::tempdir().unwrap();
    // Each request waits 300 ms. Parent asks for two tasks; each subagent answers immediately.
    let provider = Arc::new(
        ScriptedProvider::new([
            Message::assistant(
                "",
                vec![
                    call("t1", "task", serde_json::json!({"description": "a", "prompt": "do a"})),
                    call("t2", "task", serde_json::json!({"description": "b", "prompt": "do b"})),
                ],
            ),
            Message::assistant("sub answer 1", vec![]),
            Message::assistant("sub answer 2", vec![]),
            Message::assistant("combined", vec![]),
        ])
        .with_delay(Duration::from_millis(300)),
    );
    let mut agent = agent_with(provider.clone(), dir.path(), Some(traces.path()));
    let (sink, events) = collecting_sink();
    let start = std::time::Instant::now();
    let out = agent.run_turn("go".into(), sink, CancellationToken::new()).await;
    let elapsed = start.elapsed();
    assert_eq!(out.text, "combined");
    // Sequential would take 4 × 300 ms; parallel subagents overlap.
    assert!(elapsed < Duration::from_millis(1100), "took {elapsed:?}");
    let evs = events.lock().unwrap();
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Subagent { call_id, .. } if call_id == "t1")));
    let sessions = trace::list_sessions(traces.path());
    assert_eq!(sessions.len(), 3);
    let child = trace::load(&traces.path().join("parent-sub1.jsonl")).unwrap();
    assert_eq!(child.parent.as_deref(), Some("parent"));
    // The parent's history records both task results.
    let results: Vec<_> = agent
        .history()
        .iter()
        .filter_map(|m| match m { Message::Tool { content, .. } => Some(content.clone()), _ => None })
        .collect();
    assert_eq!(results.len(), 2);
    assert!(results.iter().all(|r| r.contains("sub answer")));
}

#[tokio::test]
async fn cancellation_interrupts_running_bash() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant(
        "",
        vec![call("b", "bash", serde_json::json!({"command": "sleep 30"}))],
    )]));
    let mut agent = agent_with(provider, dir.path(), None);
    let (sink, _) = collecting_sink();
    let cancel = CancellationToken::new();
    let c2 = cancel.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(200)).await;
        c2.cancel();
    });
    let start = std::time::Instant::now();
    let out = agent.run_turn("x".into(), sink, cancel).await;
    assert!(out.cancelled);
    assert!(start.elapsed() < Duration::from_secs(5));
    // History stays valid: the call has a result.
    assert!(matches!(agent.history().last(), Some(Message::Tool { content, .. }) if content.contains("interrupted")));
}

#[tokio::test]
async fn trace_rebuilds_model_visible_history() {
    let dir = tempfile::tempdir().unwrap();
    let traces = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "echo hi"}))]),
        Message::assistant("said hi", vec![]),
    ]));
    let mut agent = agent_with(provider, dir.path(), Some(traces.path()));
    let (sink, _) = collecting_sink();
    agent.run_turn("say hi".into(), sink, CancellationToken::new()).await;
    let loaded = trace::load(&traces.path().join("parent.jsonl")).unwrap();
    assert_eq!(loaded.messages, agent.history());
    assert!(matches!(&loaded.messages[1], Message::Context { content } if content.contains("Working directory:")));
    let Message::Tool { content, .. } = &loaded.messages[4] else { panic!("{:?}", loaded.messages) };
    assert_eq!(content.trim(), "hi");
}

#[tokio::test]
async fn resume_answers_dangling_tool_calls() {
    let dir = tempfile::tempdir().unwrap();
    let provider: Arc<dyn Provider> = Arc::new(ScriptedProvider::new([]));
    let history = vec![
        Message::System { content: "sys".into() },
        Message::user("x"),
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "true"}))]),
    ];
    let agent = Agent::resume(
        AgentOptions {
            provider,
            tools: Arc::new(tools::builtin()),
            system_prompt: "sys".into(),
            max_steps: 3,
            cwd: dir.path().to_path_buf(),
            bash_timeout: Duration::from_secs(5),
            max_tool_output: 30_000,
            trace: None,
            length: None,
            titler: None,
        },
        history,
    );
    assert!(matches!(agent.history().last(), Some(Message::Tool { call_id, .. }) if call_id == "c1"));
}

#[tokio::test]
async fn runtime_context_is_sent_once_until_it_changes() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("a", vec![]), Message::assistant("b", vec![])]));
    let mut agent = agent_with(provider.clone(), dir.path(), None);
    let (sink, _) = collecting_sink();
    agent.run_turn("one".into(), sink.clone(), CancellationToken::new()).await;
    agent.run_turn("two".into(), sink, CancellationToken::new()).await;
    let contexts: Vec<_> = agent.history().iter().filter(|m| matches!(m, Message::Context { .. })).collect();
    assert_eq!(contexts.len(), 1);
    // The system prompt carries no per-process facts, so it stays a stable cached prefix.
    let Message::System { content } = &agent.history()[0] else { panic!() };
    assert!(!content.contains(&dir.path().display().to_string()));
    // The context precedes the first user message on the wire, as a user-role message.
    let wire = rdsh::llm::wire_messages(&provider.requests()[0]);
    assert_eq!(wire[1]["role"], "user");
    assert!(wire[1]["content"].as_str().unwrap().starts_with("Current runtime context."));
}

#[test]
fn system_prompt_carries_guidance_only_for_registered_tools() {
    let base = tools::builtin();
    let main = rdsh::prompt::main("deepseek-chat", &base);
    assert!(main.starts_with("You are rdsh, a coding agent powered by the deepseek-chat model"));
    assert!(main.contains("Use read_file — not shell commands like cat, sed, head, or tail"));
    assert!(main.contains("never with sed -i"));
    assert!(main.contains("Check the [exit code N] marker"));
    assert!(!main.contains("task calls together"), "task is not registered in the base set");
    let mut with_task = base.clone();
    with_task.add(Arc::new(Task::new(
        Arc::new(ScriptedProvider::default()),
        Arc::new(base),
        1,
        Duration::from_secs(1),
        30_000,
        None,
        "p".into(),
    )));
    assert!(rdsh::prompt::main("m", &with_task).contains("task calls together"));
}

#[tokio::test]
async fn subagent_ids_continue_after_existing_traces_and_are_reported() {
    let dir = tempfile::tempdir().unwrap();
    let traces = tempfile::tempdir().unwrap();
    // A resumed session already has two subagent traces on disk.
    for n in [1, 2] {
        trace::Trace::create(traces.path(), &format!("parent-sub{n}"), "m", dir.path(), Some("parent")).unwrap();
    }
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("t1", "task", serde_json::json!({"description": "scan lexer", "prompt": "go"}))]),
        Message::assistant("sub done", vec![]),
        Message::assistant("all done", vec![]),
    ]));
    let mut agent = agent_with(provider, dir.path(), Some(traces.path()));
    let (sink, events) = collecting_sink();
    let out = agent.run_turn("x".into(), sink, CancellationToken::new()).await;
    assert_eq!(out.text, "all done");
    assert!(traces.path().join("parent-sub3.jsonl").exists());
    let evs = events.lock().unwrap();
    let started = evs.iter().find_map(|e| match e {
        AgentEvent::SubagentStarted { call_id, session_id, title } => Some((call_id.clone(), session_id.clone(), title.clone())),
        _ => None,
    });
    assert_eq!(started, Some(("t1".into(), "parent-sub3".into(), "scan lexer".into())));
}

#[tokio::test]
async fn length_note_follows_the_user_message_but_not_subagent_tasks() {
    use rdsh::{
        decision::{Pick, ScriptedDecider},
        length::{LengthRouter, Routing},
    };
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("t1", "task", serde_json::json!({"description": "look", "prompt": "find it"}))]),
        Message::assistant("sub report", vec![]),
        Message::assistant("yes", vec![]),
        Message::assistant("second", vec![]),
    ]));
    let decider = Arc::new(ScriptedDecider::new([
        Ok(Pick { label: "yesno".into(), confidence: 0.91 }),
        Err("HTTP 503".into()),
    ]));
    let mut agent = {
        // Same tools as `agent_with`, plus the router.
        let base = Arc::new(tools::builtin());
        let mut reg = (*base).clone();
        reg.add(Arc::new(Task::new(provider.clone(), base, 10, Duration::from_secs(10), 30_000, None, "p".into())));
        Agent::new(AgentOptions {
            provider: provider.clone(),
            tools: Arc::new(reg),
            system_prompt: "sys".into(),
            max_steps: 10,
            cwd: dir.path().to_path_buf(),
            bash_timeout: Duration::from_secs(10),
            max_tool_output: 30_000,
            trace: None,
            length: Some(Arc::new(LengthRouter::new(decider.clone()))),
            titler: None,
        })
    };
    let (sink, events) = collecting_sink();
    let out = agent.run_turn("is it there?".into(), sink.clone(), CancellationToken::new()).await;
    assert_eq!(out.text, "yes");
    let reqs = provider.requests();
    let is_note = |m: &Message| matches!(m, Message::Directive { content } if content.contains("level=\"yesno\""));
    // Main agent: the note comes right after the user's message, which keeps its typed text.
    assert!(is_note(reqs[0].last().unwrap()));
    assert!(matches!(&reqs[0][reqs[0].len() - 2], Message::User { content } if content == "is it there?"));
    // Subagent: no note, and the decider was asked only once.
    assert!(!reqs[1].iter().any(|m| matches!(m, Message::Directive { .. })));
    assert_eq!(decider.states().len(), 1);
    assert!(decider.states()[0].ends_with("is it there?"));
    assert!(events.lock().unwrap().iter().any(|e| matches!(
        e,
        AgentEvent::ResponseLength(Routing::Picked { level, .. }) if level == "yesno"
    )));

    // An unavailable decision adds no note and warns once.
    events.lock().unwrap().clear();
    agent.run_turn("and then?".into(), sink, CancellationToken::new()).await;
    let last = provider.requests().last().unwrap().clone();
    assert!(matches!(last.last().unwrap(), Message::User { content } if content == "and then?"));
    let evs = events.lock().unwrap();
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::Warning(w) if w.contains("503"))));
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::ResponseLength(Routing::Unavailable { .. }))));
}

#[tokio::test]
async fn sessions_are_titled_once_in_the_background() {
    let dir = tempfile::tempdir().unwrap();
    let traces = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([Message::assistant("a", vec![]), Message::assistant("b", vec![])]));
    let namer = Arc::new(ScriptedProvider::new([Message::assistant("Fix Markdown Rendering.", vec![])]));
    let t = trace::Trace::create(traces.path(), "s1", "scripted", dir.path(), None).unwrap();
    let path = t.path.clone();
    let mut agent = Agent::new(AgentOptions {
        provider: provider.clone(),
        tools: Arc::new(tools::builtin()),
        system_prompt: "sys".into(),
        max_steps: 3,
        cwd: dir.path().to_path_buf(),
        bash_timeout: Duration::from_secs(5),
        max_tool_output: 30_000,
        trace: Some(t),
        length: None,
        titler: Some(Arc::new(rdsh::title::Titler::new(namer.clone()))),
    });
    let (sink, events) = collecting_sink();
    agent.run_turn("the markdown renderer eats my asterisks".into(), sink.clone(), CancellationToken::new()).await;
    for _ in 0..50 {
        if events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::Titled { .. })) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(events.lock().unwrap().iter().any(|e| matches!(e, AgentEvent::Titled { title } if title == "Fix Markdown Rendering")));
    assert_eq!(rdsh::title::read(&path).as_deref(), Some("Fix Markdown Rendering"));
    assert_eq!(trace::list_sessions(traces.path())[0].slug.as_deref(), Some("Fix Markdown Rendering"));
    // The title request never reaches the session's own history or model.
    assert!(provider.requests().iter().all(|r| r.iter().all(|m| !matches!(m, Message::System { content } if content.contains("2 to 5 word")))));
    agent.run_turn("again".into(), sink, CancellationToken::new()).await;
    assert_eq!(namer.requests().len(), 1, "named once");
}
