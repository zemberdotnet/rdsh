//! The hub protocol over sockets and over a command's stdin/stdout (how SSH reaches it).

use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use rdsh::{
    agent::{Agent, AgentEvent, AgentOptions},
    hub::{Factory, Hub, Notice, OpenKind, Request, net},
    llm::{Effort, Message, Provider, ScriptedProvider},
    tools, trace,
};
use tokio::sync::mpsc::UnboundedReceiver;

fn scripted_hub(dir: PathBuf, answers: Vec<&'static str>) -> Hub {
    let provider = Arc::new(ScriptedProvider::new(answers.into_iter().map(|a| Message::assistant(a, vec![]))));
    let n = Arc::new(AtomicUsize::new(0));
    let d = dir.clone();
    let factory: Factory = Arc::new(move |kind| {
        let p: Arc<dyn Provider> = provider.clone();
        let cwd = match &kind {
            OpenKind::New(Some(c)) => c.clone(),
            _ => d.clone(),
        };
        let id = format!("n{}", n.fetch_add(1, Ordering::SeqCst) + 1);
        let t = trace::Trace::create(&d, &id, "scripted", &cwd, None)?;
        Ok(Agent::new(AgentOptions {
            provider: p,
            tools: Arc::new(tools::builtin()),
            system_prompt: "sys".into(),
            max_steps: 3,
            cwd,
            bash_timeout: Duration::from_secs(5),
            max_tool_output: 30_000,
            trace: Some(t),
            length: None,
            titler: None,
        }))
    });
    Hub::new(factory, Some(dir))
}

async fn next(rx: &mut UnboundedReceiver<Notice>) -> Notice {
    loop {
        let n = tokio::time::timeout(Duration::from_secs(10), rx.recv()).await.expect("notice in time").expect("connected");
        if !matches!(n, Notice::Sessions { .. }) {
            return n;
        }
    }
}

async fn wait_for_socket(socket: &Path) {
    for _ in 0..200 {
        if tokio::net::UnixStream::connect(socket).await.is_ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("socket never came up");
}

async fn run_turn(tx: &tokio::sync::mpsc::UnboundedSender<Request>, rx: &mut UnboundedReceiver<Notice>, id: &str, text: &str) -> Vec<AgentEvent> {
    tx.send(Request::Send { session_id: id.into(), text: text.into(), effort: Effort::High, model: None }).unwrap();
    let mut out = Vec::new();
    loop {
        if let Notice::Event { event, .. } = next(rx).await {
            let end = matches!(event, AgentEvent::TurnEnd { .. });
            out.push(event);
            if end {
                return out;
            }
        }
    }
}

#[tokio::test]
async fn socket_clients_share_sessions_and_a_second_server_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("run/hub.sock");
    // A stale socket file from a crashed hub is replaced.
    std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
    drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
    let hub = scripted_hub(dir.path().join("sessions"), vec!["over the socket"]);
    let server = tokio::spawn({
        let (hub, socket) = (hub.clone(), socket.clone());
        async move { net::serve(hub, &socket, None).await }
    });
    wait_for_socket(&socket).await;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(socket.parent().unwrap()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700, "only the user can reach the hub");
    }
    let again = net::serve(hub.clone(), &socket, None).await;
    assert!(again.unwrap_err().to_string().contains("already running"));

    let (a_tx, mut a_rx) = net::connect_socket(&socket).await.unwrap();
    let work = dir.path().join("work");
    std::fs::create_dir_all(&work).unwrap();
    a_tx.send(Request::New { cwd: Some(work.display().to_string()) }).unwrap();
    let Notice::Opened { snapshot } = next(&mut a_rx).await else { panic!() };
    assert_eq!(snapshot.cwd, work.display().to_string());
    let evs = run_turn(&a_tx, &mut a_rx, &snapshot.session_id, "hi").await;
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "over the socket")));

    // A second client attaches to the same session and sees its history.
    let (b_tx, mut b_rx) = net::connect_socket(&socket).await.unwrap();
    b_tx.send(Request::Open { session_id: snapshot.session_id.clone() }).unwrap();
    let Notice::Opened { snapshot: b } = next(&mut b_rx).await else { panic!() };
    assert!(b.history.iter().any(|m| matches!(m, Message::Assistant { content, .. } if content == "over the socket")));

    // Disconnecting detaches; the session stays in the hub.
    drop(a_tx);
    drop(a_rx);
    for _ in 0..100 {
        if hub.clients() == 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(hub.clients(), 1);
    server.abort();
}

#[tokio::test]
async fn the_protocol_runs_over_a_commands_stdio_like_ssh() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("hub.sock");
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    let git = |args: &[&str]| assert!(Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap().status.success());
    git(&["init", "-q"]);
    git(&["config", "user.email", "t@t"]);
    git(&["config", "user.name", "t"]);
    std::fs::write(repo.join("a.rs"), "fn a() {}\n").unwrap();
    git(&["add", "."]);
    git(&["commit", "-qm", "init"]);
    std::fs::write(repo.join("a.rs"), "fn a2() {}\n").unwrap();

    let hub = scripted_hub(dir.path().join("sessions"), vec!["from far away"]);
    let server = tokio::spawn({
        let socket = socket.clone();
        async move { net::serve(hub, &socket, None).await }
    });
    wait_for_socket(&socket).await;
    // `ssh box rdsh connect` runs this same bridge on the remote machine.
    let exe = env!("CARGO_BIN_EXE_rdsh");
    let args = vec!["connect".to_owned(), "--socket".to_owned(), socket.display().to_string()];
    let (tx, mut rx) = net::connect_command(exe, &args).unwrap();
    tx.send(Request::New { cwd: Some(repo.display().to_string()) }).unwrap();
    let Notice::Opened { snapshot } = next(&mut rx).await else { panic!() };
    let evs = run_turn(&tx, &mut rx, &snapshot.session_id, "hello").await;
    assert!(evs.iter().any(|e| matches!(e, AgentEvent::TextDelta(t) if t == "from far away")));

    // Files and changes of a remote session are read on its host.
    tx.send(Request::ReadFile { cwd: repo.display().to_string(), path: "a.rs".into() }).unwrap();
    let Notice::File { file } = next(&mut rx).await else { panic!() };
    assert_eq!(file.rel, "a.rs");
    assert_eq!((file.added, file.removed), (1, 1));
    tx.send(Request::ChangedFiles { cwd: repo.display().to_string() }).unwrap();
    let Notice::ChangedFiles { files, .. } = next(&mut rx).await else { panic!() };
    assert_eq!(files, vec![("M".to_owned(), "a.rs".to_owned())]);
    tx.send(Request::ReadFile { cwd: repo.display().to_string(), path: "missing.rs".into() }).unwrap();
    assert!(matches!(next(&mut rx).await, Notice::Error { message, .. } if message.contains("no such file")));
    server.abort();
}

#[tokio::test]
async fn a_failing_connection_command_reports_its_error() {
    let args = vec!["-c".to_owned(), "echo 'Permission denied (publickey)' >&2; exit 255".to_owned()];
    let (_tx, mut rx) = net::connect_command("sh", &args).unwrap();
    let n = tokio::time::timeout(Duration::from_secs(5), rx.recv()).await.unwrap();
    match n {
        Some(Notice::Error { message, .. }) => assert!(message.contains("Permission denied"), "{message}"),
        other => panic!("{other:?}"),
    }
    assert!(rx.recv().await.is_none(), "then the connection is closed");
}

#[tokio::test]
async fn rdsh_starts_its_hub_on_demand_and_it_stops_when_idle() {
    let dir = tempfile::tempdir().unwrap();
    let socket = dir.path().join("rt/hub.sock");
    let config = dir.path().join("config/rdsh");
    std::fs::create_dir_all(&config).unwrap();
    std::fs::write(config.join("config.json"), r#"{"serve_idle_secs": 1}"#).unwrap();
    let exe = PathBuf::from(env!("CARGO_BIN_EXE_rdsh"));
    // The daemon inherits this environment: an isolated config, data dir, and a dummy key.
    // SAFETY: test-only environment changes before the child is spawned.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", dir.path().join("config"));
        std::env::set_var("XDG_DATA_HOME", dir.path().join("data"));
        std::env::set_var("DEEPSEEK_API_KEY", "dummy");
    }
    net::ensure_server(&socket, &exe).await.unwrap();
    let (tx, mut rx) = net::connect_socket(&socket).await.unwrap();
    tx.send(Request::New { cwd: Some(dir.path().display().to_string()) }).unwrap();
    let Notice::Opened { snapshot } = next(&mut rx).await else { panic!() };
    assert_eq!(snapshot.model, "deepseek-flash");
    // Already running: a second call does not start another.
    net::ensure_server(&socket, &exe).await.unwrap();
    drop(tx);
    drop(rx);
    // Idle (1 s, checked every 5 s): the hub exits and removes its socket.
    for _ in 0..150 {
        if !socket.exists() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("the idle hub did not exit");
}
