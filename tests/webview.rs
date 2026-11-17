//! `rdsh web`: login, the access checks, and the page's WebSocket relayed to a hub.

use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use futures_util::{SinkExt, StreamExt};
use rdsh::{
    agent::{Agent, AgentOptions},
    hub::{Factory, Hub, OpenKind},
    llm::{Effort, Message, Provider, ScriptedProvider},
    tools, trace,
    webview::{self, Connector, HostSpec, Settings},
};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue, protocol::Message as Ws};

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
        let id = format!("w{}", n.fetch_add(1, Ordering::SeqCst) + 1);
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

struct Server {
    port: u16,
    token: String,
    _dir: tempfile::TempDir,
}

async fn start(answers: Vec<&'static str>) -> Server {
    let dir = tempfile::tempdir().unwrap();
    let hub = scripted_hub(dir.path().to_path_buf(), answers);
    let connector: Connector = Arc::new(move |_spec: HostSpec| {
        let hub = hub.clone();
        Box::pin(async move { Ok(hub.connect()) })
    });
    let settings = Settings {
        cwd: dir.path().display().to_string(),
        effort: Effort::Low,
        models: vec!["scripted".into()],
        hosts: vec![HostSpec { name: "local".into(), local: true, dir: None, ssh: None }],
        assets_dir: None,
    };
    let token = webview::load_token(dir.path()).unwrap();
    assert_eq!(webview::load_token(dir.path()).unwrap(), token, "the token is reused");
    let listener = webview::bind(0, false).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(webview::serve(listener, settings, connector, token.clone()));
    Server { port, token, _dir: dir }
}

/// A raw HTTP/1.1 GET; returns the status line and headers, and the body.
async fn get(port: u16, path: &str, host: &str, cookie: Option<&str>) -> (String, String) {
    let mut s = tokio::net::TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    let cookie = cookie.map_or(String::new(), |c| format!("Cookie: {c}\r\n"));
    let req = format!("GET {path} HTTP/1.1\r\nHost: {host}\r\n{cookie}Connection: close\r\n\r\n");
    s.write_all(req.as_bytes()).await.unwrap();
    let mut raw = Vec::new();
    s.read_to_end(&mut raw).await.unwrap();
    let out = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = out.split_once("\r\n\r\n").unwrap_or((&out, ""));
    (head.to_owned(), body.to_owned())
}

fn status(head: &str) -> u16 {
    head.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0)
}

#[tokio::test]
async fn login_trades_the_token_for_a_cookie_and_everything_else_is_refused() {
    let s = start(vec![]).await;
    let host = format!("127.0.0.1:{}", s.port);
    assert_eq!(status(&get(s.port, "/", &host, None).await.0), 403, "no cookie");
    assert_eq!(status(&get(s.port, "/?t=nope", &host, None).await.0), 403, "wrong token");
    let (head, _) = get(s.port, &format!("/?t={}", s.token), &host, None).await;
    assert_eq!(status(&head), 303);
    let set = head.lines().find(|l| l.to_ascii_lowercase().starts_with("set-cookie:")).expect("a cookie");
    assert!(set.contains("HttpOnly") && set.contains("SameSite=Strict"), "{set}");
    let cookie = set.split_once(':').unwrap().1.trim().split(';').next().unwrap().to_owned();
    let (head, body) = get(s.port, "/", &host, Some(&cookie)).await;
    assert_eq!(status(&head), 200);
    assert!(body.contains("app.js"));
    // Another name for this server (DNS rebinding) is refused even with the cookie.
    assert_eq!(status(&get(s.port, "/", &format!("evil.example:{}", s.port), Some(&cookie)).await.0), 403);
    assert_eq!(status(&get(s.port, "/app.js", &host, None).await.0), 200, "the client code itself is public");
}

async fn ws(s: &Server, origin: Option<&str>, cookie: bool) -> Result<tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>, String> {
    let mut req = format!("ws://127.0.0.1:{}/ws", s.port).into_client_request().unwrap();
    if let Some(o) = origin {
        req.headers_mut().insert("Origin", HeaderValue::from_str(o).unwrap());
    }
    if cookie {
        req.headers_mut().insert("Cookie", HeaderValue::from_str(&format!("rdsh_{}={}", s.port, s.token)).unwrap());
    }
    tokio_tungstenite::connect_async(req).await.map(|(w, _)| w).map_err(|e| e.to_string())
}

async fn next_json<S>(w: &mut S) -> Value
where
    S: StreamExt<Item = Result<Ws, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), w.next()).await.expect("a message in time").expect("open").unwrap();
        if let Ws::Text(t) = msg {
            return serde_json::from_str(&t).unwrap();
        }
    }
}

/// Skips messages until one matches.
async fn until<S>(w: &mut S, f: impl Fn(&Value) -> bool) -> Value
where
    S: StreamExt<Item = Result<Ws, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let v = next_json(w).await;
        if f(&v) {
            return v;
        }
    }
}

#[tokio::test]
async fn the_socket_needs_this_origin_and_the_cookie() {
    let s = start(vec![]).await;
    let origin = format!("http://127.0.0.1:{}", s.port);
    assert!(ws(&s, None, true).await.is_err(), "no origin");
    assert!(ws(&s, Some("https://evil.example"), true).await.is_err(), "another site");
    assert!(ws(&s, Some(&origin), false).await.is_err(), "no cookie");
    assert!(ws(&s, Some(&format!("http://localhost:{}", s.port)), true).await.is_ok());
}

#[tokio::test]
async fn the_page_runs_a_turn_through_the_relay() {
    let s = start(vec!["hello from the hub"]).await;
    let mut w = ws(&s, Some(&format!("http://127.0.0.1:{}", s.port)), true).await.unwrap();
    let welcome = next_json(&mut w).await;
    assert_eq!(welcome["type"], "welcome");
    assert_eq!(welcome["effort"], "low");
    assert_eq!(welcome["hosts"][0]["name"], "local");
    assert!(welcome["hosts"][0].get("ssh").is_none(), "SSH destinations stay on the server");
    let hello = until(&mut w, |v| v["type"] == "notice" && v["notice"]["type"] == "hello").await;
    assert_eq!(hello["host"], 0);

    let send = |v: Value| Ws::Text(v.to_string().into());
    w.send(send(json!({"type": "request", "host": 0, "request": {"type": "new"}}))).await.unwrap();
    let opened = until(&mut w, |v| v["notice"]["type"] == "opened").await;
    let id = opened["notice"]["snapshot"]["session_id"].as_str().unwrap().to_owned();
    w.send(send(json!({"type": "request", "host": 0, "request": {"type": "send", "session_id": id, "text": "hi", "effort": "low"}})))
        .await
        .unwrap();
    let started = until(&mut w, |v| v["notice"]["type"] == "event" && v["notice"]["event"]["type"] == "turn_started").await;
    assert_eq!(started["notice"]["event"]["data"]["text"], "hi");
    let reply = until(&mut w, |v| v["notice"]["event"]["type"] == "assistant").await;
    assert_eq!(reply["notice"]["event"]["data"]["message"]["content"], "hello from the hub");
    until(&mut w, |v| v["notice"]["event"]["type"] == "turn_end").await;

    // Requests to a host that does not exist come back as an error, not silence.
    w.send(send(json!({"type": "request", "host": 7, "request": {"type": "list"}}))).await.unwrap();
    let err = until(&mut w, |v| v["type"] == "error").await;
    assert!(err["message"].as_str().unwrap().contains("not connected"), "{err}");
}

#[tokio::test]
async fn frame_names_reach_only_the_frame_files_which_allow_no_network() {
    let s = start(vec![]).await;
    let frame_host = format!("d1-x9.localhost:{}", s.port);
    let (head, body) = get(s.port, "/frame", &frame_host, None).await;
    assert_eq!(status(&head), 200);
    assert!(body.contains("/frame.js"));
    let csp = head.lines().find(|l| l.to_ascii_lowercase().starts_with("content-security-policy:")).expect("a CSP");
    assert!(csp.contains("default-src 'none'") && !csp.contains("connect-src"), "{csp}");
    assert!(csp.contains(&format!("script-src http://{frame_host}/d3.js http://{frame_host}/frame.js 'unsafe-eval'")), "{csp}");
    for path in ["/frame.js", "/d3.js"] {
        assert_eq!(status(&get(s.port, path, &frame_host, None).await.0), 200, "{path}");
    }
    let cookie = format!("rdsh_{}={}", s.port, s.token);
    for path in ["/", "/ws", "/app.js"] {
        assert_eq!(status(&get(s.port, path, &frame_host, Some(&cookie)).await.0), 403, "{path} from a frame name");
    }
    assert_eq!(status(&get(s.port, "/frame", &format!("bad_name.localhost:{}", s.port), None).await.0), 403);
    assert_eq!(status(&get(s.port, "/frame", &format!("evil.example:{}", s.port), None).await.0), 403);
    // Fonts are served to frames too, with CORS (frames have an opaque origin).
    let (head, _) = get(s.port, "/fonts/DejaVuSansMono.woff2", &frame_host, None).await;
    assert_eq!(status(&head), 200);
    assert!(head.to_ascii_lowercase().contains("access-control-allow-origin: *"), "{head}");
    assert!(csp.contains(&format!("font-src http://{frame_host}/fonts/")), "{csp}");
    // The page itself may embed only those frames, and loads nothing inline.
    let (head, _) = get(s.port, "/", &format!("127.0.0.1:{}", s.port), Some(&cookie)).await;
    let csp = head.lines().find(|l| l.to_ascii_lowercase().starts_with("content-security-policy:")).expect("a CSP");
    assert!(csp.contains(&format!("frame-src http://*.localhost:{}/frame", s.port)) && csp.contains("frame-ancestors 'none'"), "{csp}");
    assert!(csp.contains("font-src 'self'") && !csp.contains("https:"), "nothing from other sites: {csp}");
}
