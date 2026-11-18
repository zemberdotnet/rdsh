//! The OpenAI provider and model routing against a local stand-in for the APIs.

use rdsh::{
    auth::OpenAiAuth,
    chatgpt::Token,
    config::Config,
    llm::{Delta, Effort, Message, Request},
    providers,
};
use serde_json::Value;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};

struct Seen {
    head: String,
    body: Value,
}

/// Serves one canned `(content-type, body)` per connection and reports each request.
async fn serve(replies: Vec<(&'static str, String)>) -> (String, mpsc::UnboundedReceiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (kind, reply) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 8192];
            let (head, body) = loop {
                let n = sock.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf).into_owned();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let len = head
                        .lines()
                        .find_map(|l| l.to_ascii_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                        .unwrap_or(0);
                    if body.len() >= len {
                        break (head.to_owned(), body.to_owned());
                    }
                }
            };
            tx.send(Seen { head, body: serde_json::from_str(&body).unwrap() }).unwrap();
            let resp = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: {kind}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });
    (url, rx)
}

fn responses_stream(text: &str) -> String {
    format!(
        "data: {{\"type\":\"response.output_text.delta\",\"delta\":\"{text}\"}}\n\n\
         data: {{\"type\":\"response.completed\",\"response\":{{\"usage\":{{\"input_tokens\":5,\"output_tokens\":1}}}}}}\n\n"
    )
}

async fn ask(cfg: &Config, model: Option<&str>) -> (String, Vec<Delta>) {
    let provider = providers::build(cfg).unwrap();
    let messages = [Message::System { content: "be brief".into() }, Message::user("hi")];
    let req = Request { messages: &messages, tools: &[], effort: Effort::Low, model, cache_key: Some("sess-1") };
    let deltas = std::sync::Mutex::new(Vec::new());
    let sink = |d: Delta| deltas.lock().unwrap().push(d);
    let c = provider.complete(req, &sink).await.unwrap();
    let Message::Assistant { content, .. } = c.message else { panic!() };
    (content, deltas.into_inner().unwrap())
}

#[tokio::test]
async fn an_api_key_calls_the_responses_api() {
    let (url, mut seen) = serve(vec![("text/event-stream", responses_stream("hello"))]).await;
    let cfg = Config {
        model: "gpt-6.2-sol".into(),
        openai_base_url: format!("{url}/v1"),
        openai: Some(OpenAiAuth::ApiKey { key: "sk-test".into() }),
        openai_from_env: true,
        ..Config::default()
    };
    let (text, deltas) = ask(&cfg, None).await;
    assert_eq!((text.as_str(), deltas), ("hello", vec![Delta::Text("hello".into())]));
    let s = seen.recv().await.unwrap();
    assert!(s.head.starts_with("POST /v1/responses "), "{}", s.head);
    assert!(s.head.to_lowercase().contains("authorization: bearer sk-test"));
    assert!(!s.head.to_lowercase().contains("chatgpt-account-id"));
    assert_eq!(s.body["model"], "gpt-6.2-sol");
    assert_eq!(s.body["instructions"], "be brief");
    assert_eq!(s.body["input"][0]["content"][0]["text"], "hi");
    assert_eq!(s.body["reasoning"]["effort"], "low");
    assert_eq!(s.body["prompt_cache_key"], "sess-1");
}

#[tokio::test]
async fn a_chatgpt_login_calls_codex_with_its_account() {
    let (url, mut seen) = serve(vec![("text/event-stream", responses_stream("sub"))]).await;
    let token = Token { access: "acc".into(), refresh: "ref".into(), expires_ms: u64::MAX, account_id: "acct-7".into() };
    let cfg = Config {
        model: "gpt-6.2-sol".into(),
        chatgpt_base_url: format!("{url}/backend-api"),
        openai: Some(OpenAiAuth::Chatgpt(token)),
        openai_from_env: true,
        ..Config::default()
    };
    assert_eq!(ask(&cfg, None).await.0, "sub");
    let s = seen.recv().await.unwrap();
    let head = s.head.to_lowercase();
    assert!(head.starts_with("post /backend-api/codex/responses "), "{head}");
    for h in ["authorization: bearer acc", "chatgpt-account-id: acct-7", "originator: rdsh", "session-id: sess-1", "openai-beta: responses=experimental"] {
        assert!(head.contains(h), "missing {h}:\n{head}");
    }
    assert_eq!(s.body["store"], false);
}

#[tokio::test]
async fn each_turn_goes_to_the_provider_of_its_model() {
    let chat = "data: {\"choices\":[{\"delta\":{\"content\":\"ds\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let (url, mut seen) = serve(vec![("text/event-stream", chat.into()), ("text/event-stream", responses_stream("oa"))]).await;
    let cfg = Config {
        base_url: url.clone(),
        api_key: Some("ds-key".into()),
        openai_base_url: format!("{url}/v1"),
        openai: Some(OpenAiAuth::ApiKey { key: "sk".into() }),
        openai_from_env: true,
        ..Config::default()
    };
    // The default (DeepSeek), then a turn switched to an OpenAI model.
    assert_eq!(ask(&cfg, None).await.0, "ds");
    assert!(seen.recv().await.unwrap().head.starts_with("POST /chat/completions "));
    assert_eq!(ask(&cfg, Some("gpt-6-sol")).await.0, "oa");
    let s = seen.recv().await.unwrap();
    assert!(s.head.starts_with("POST /v1/responses "));
    assert_eq!(s.body["model"], "gpt-6-sol");
}
