//! Background bash jobs: moving long commands to the background, notes to the agent, and the
//! job tools.

use std::{sync::Arc, time::Duration};

use rdsh::{
    agent::{Agent, AgentEvent, AgentOptions, EventSink},
    llm::{Message, Provider, ScriptedProvider, ToolCall},
    tools::{
        self,
        bash::Bash,
        jobs::{JobKill, JobOutput, Jobs},
    },
};
use tokio_util::sync::CancellationToken;

fn call(id: &str, name: &str, args: serde_json::Value) -> ToolCall {
    ToolCall { id: id.into(), name: name.into(), arguments: args.to_string() }
}

/// An agent with jobs whose foreground limit is `limit`.
fn agent(provider: Arc<dyn Provider>, dir: &std::path::Path, limit: Duration) -> Agent {
    let mut reg = tools::builtin();
    reg.add(Arc::new(Bash { background: true }));
    reg.add(Arc::new(JobOutput));
    reg.add(Arc::new(JobKill));
    let mut agent = Agent::new(AgentOptions {
        provider,
        tools: Arc::new(reg),
        system_prompt: "sys".into(),
        max_steps: 10,
        cwd: dir.to_path_buf(),
        bash_timeout: limit,
        max_tool_output: 30_000,
        trace: None,
        length: None,
        titler: None,
    });
    agent.enable_jobs(Jobs::new(dir.join("jobs")));
    agent
}

fn sink() -> EventSink {
    Arc::new(|_| {})
}

fn tool_result(reqs: &[Vec<Message>], i: usize) -> String {
    match reqs[i].last() {
        Some(Message::Tool { content, .. }) => content.clone(),
        other => panic!("request {i} does not end with a tool result: {other:?}"),
    }
}

fn notes(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Directive { content } | Message::User { content } if content.starts_with("<background_jobs>") => {
                Some(content.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn a_command_outliving_the_limit_moves_to_the_background_and_reports_its_end() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "echo early; sleep 1; echo late"}))]),
        Message::assistant("it is building", vec![]),
        Message::assistant("build finished", vec![]),
    ]));
    let mut agent = agent(provider.clone(), dir.path(), Duration::from_millis(300));
    let out = agent.run_turn("build".into(), sink(), CancellationToken::new()).await;
    assert_eq!(out.text, "it is building");
    let result = tool_result(&provider.requests(), 1);
    assert!(result.contains("background job 1") && result.contains("early"), "{result}");
    let jobs = agent.jobs().unwrap().clone();
    assert!(jobs.any_running());
    // Idle: the end of the job gets a turn of its own.
    tokio::time::timeout(Duration::from_secs(5), jobs.finished()).await.unwrap();
    let out = agent.run_job_turn(sink(), CancellationToken::new()).await.expect("a job turn");
    assert_eq!(out.text, "build finished");
    let reqs = provider.requests();
    let note = notes(&reqs[2]).pop().unwrap();
    assert!(note.contains("job 1 `echo early; sleep 1; echo late` finished: exit 0"), "{note}");
    assert!(note.contains("late"), "the note quotes the output's tail: {note}");
    // Reported once.
    assert!(agent.run_job_turn(sink(), CancellationToken::new()).await.is_none());
}

#[tokio::test]
async fn turns_start_with_running_jobs_and_the_tools_read_and_kill_them() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant(
            "",
            vec![call("c1", "bash", serde_json::json!({"command": "echo serving; sleep 30", "run_in_background": true}))],
        ),
        Message::assistant("started", vec![]),
        Message::assistant("", vec![call("c2", "job_output", serde_json::json!({"id": 1}))]),
        Message::assistant("", vec![call("c3", "job_kill", serde_json::json!({"id": 1}))]),
        Message::assistant("stopped", vec![]),
    ]));
    let mut agent = agent(provider.clone(), dir.path(), Duration::from_secs(10));
    agent.run_turn("serve".into(), sink(), CancellationToken::new()).await;
    assert!(tool_result(&provider.requests(), 1).contains("Started background job 1"));
    tokio::time::sleep(Duration::from_millis(200)).await;
    agent.run_turn("stop it".into(), sink(), CancellationToken::new()).await;
    let reqs = provider.requests();
    let note = notes(&reqs[2]).pop().unwrap();
    assert!(note.contains("Still running: job 1 `echo serving; sleep 30` (0s)"), "{note}");
    assert!(!note.contains("finished"));
    let output = tool_result(&reqs, 3);
    assert!(output.contains("job 1 running") && output.contains("serving"), "{output}");
    assert_eq!(tool_result(&reqs, 4), "killed job 1");
    let jobs = agent.jobs().unwrap().clone();
    tokio::time::timeout(Duration::from_secs(5), jobs.finished()).await.unwrap();
    assert!(!jobs.any_running());
    // A job the agent killed itself needs no note.
    assert!(!jobs.has_news());
}

#[tokio::test]
async fn a_job_finishing_mid_turn_is_noted_before_the_next_request() {
    let dir = tempfile::tempdir().unwrap();
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "echo hi", "run_in_background": true}))]),
        Message::assistant("", vec![call("c2", "bash", serde_json::json!({"command": "sleep 0.5"}))]),
        Message::assistant("done", vec![]),
    ]));
    let mut agent = agent(provider.clone(), dir.path(), Duration::from_secs(10));
    let (tx, rx) = std::sync::mpsc::channel();
    let sink: EventSink = Arc::new(move |ev| {
        if let AgentEvent::JobsNote { text } = ev {
            tx.send(text).unwrap();
        }
    });
    agent.run_turn("go".into(), sink, CancellationToken::new()).await;
    let reqs = provider.requests();
    assert!(notes(&reqs[1]).is_empty());
    let note = notes(&reqs[2]).pop().expect("a note after the second tool round");
    assert!(note.contains("job 1 `echo hi` finished: exit 0") && note.contains("hi"), "{note}");
    assert!(matches!(reqs[2].last(), Some(Message::Directive { .. })));
    assert_eq!(rx.try_recv().unwrap(), note, "clients see the same note");
    assert!(!agent.jobs().unwrap().has_news());
}

#[tokio::test]
async fn the_models_timeout_still_kills_and_dropping_the_agent_kills_jobs() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("survived");
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "sleep 5", "timeout_secs": 0}))]),
        Message::assistant(
            "",
            vec![call(
                "c2",
                "bash",
                serde_json::json!({"command": format!("sleep 1 && touch {}", marker.display()), "run_in_background": true}),
            )],
        ),
        Message::assistant("ok", vec![]),
    ]));
    let mut agent = agent(provider.clone(), dir.path(), Duration::from_secs(10));
    agent.run_turn("go".into(), sink(), CancellationToken::new()).await;
    assert!(tool_result(&provider.requests(), 1).contains("timed out after 0s"));
    drop(agent);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!marker.exists(), "the job died with its agent");
}

#[tokio::test]
async fn subagent_bash_still_kills_at_the_limit() {
    let dir = tempfile::tempdir().unwrap();
    // `builtin()` is what subagents get: no background, no job tools.
    let reg = tools::builtin();
    let spec = reg.get("bash").unwrap().spec();
    assert!(!spec.parameters.to_string().contains("run_in_background"));
    assert!(reg.get("job_output").is_none());
    let provider = Arc::new(ScriptedProvider::new([
        Message::assistant("", vec![call("c1", "bash", serde_json::json!({"command": "sleep 5", "run_in_background": true}))]),
        Message::assistant("ok", vec![]),
    ]));
    let mut agent = Agent::new(AgentOptions {
        provider: provider.clone(),
        tools: Arc::new(reg),
        system_prompt: "sys".into(),
        max_steps: 10,
        cwd: dir.path().to_path_buf(),
        bash_timeout: Duration::from_millis(300),
        max_tool_output: 30_000,
        trace: None,
        length: None,
        titler: None,
    });
    agent.run_turn("go".into(), sink(), CancellationToken::new()).await;
    assert!(tool_result(&provider.requests(), 1).contains("timed out"));
}
