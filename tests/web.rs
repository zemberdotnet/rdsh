//! `web_search` and `web_fetch` against a local stand-in for the Kagi API.

use std::{sync::Arc, time::Duration};

use rdsh::tools::{
    Tool, ToolCtx,
    web::{Kagi, WebFetch, WebSearch},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::mpsc,
};
use tokio_util::sync::CancellationToken;

/// A request the stand-in received.
struct Seen {
    head: String,
    body: Value,
}

/// Serves one canned `(status, body)` per connection, in order, and reports each request.
async fn serve(replies: Vec<(u16, Value)>) -> (String, mpsc::UnboundedReceiver<Seen>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/api/v1", listener.local_addr().unwrap());
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        for (status, reply) in replies {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
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
            let reply = reply.to_string();
            let resp = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{reply}",
                reply.len()
            );
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });
    (url, rx)
}

fn ctx() -> ToolCtx {
    ToolCtx {
        cwd: std::env::temp_dir(),
        cancel: CancellationToken::new(),
        sink: Arc::new(|_| {}),
        call_id: "c1".into(),
        bash_timeout: Duration::from_secs(5),
        effort: Default::default(),
        model: None,
        jobs: None,
        questions: None,
    }
}

#[tokio::test]
async fn search_sends_the_query_with_a_lens_and_the_key() {
    let (url, mut seen) = serve(vec![(
        200,
        json!({"meta": {"trace": "t"}, "data": {"search": [{"url": "https://docs.rs/tokio", "title": "tokio - Rust", "snippet": "An async runtime"}]}}),
    )])
    .await;
    let tool = WebSearch(Kagi::new("secret".into(), url).unwrap());
    let out = tool.run(json!({"query": "tokio select", "limit": 3, "recency": "week", "sites": ["docs.rs"]}), &ctx()).await;
    assert!(!out.is_error, "{}", out.content);
    assert_eq!(out.content, "1. tokio - Rust\n   https://docs.rs/tokio\n   An async runtime");
    let req = seen.recv().await.unwrap();
    assert!(req.head.starts_with("POST /api/v1/search "), "{}", req.head);
    assert!(req.head.to_ascii_lowercase().contains("authorization: bearer secret"), "{}", req.head);
    assert_eq!(
        req.body,
        json!({"query": "tokio select", "limit": 3, "lens": {"time_relative": "week", "sites_included": ["docs.rs"]}})
    );
}

#[tokio::test]
async fn api_errors_reach_the_model_and_bad_arguments_never_leave() {
    let (url, mut seen) = serve(vec![(
        401,
        json!({"meta": {}, "data": null, "error": [{"code": "2", "url": "u", "message": "Unauthorized", "location": null}]}),
    )])
    .await;
    let tool = WebSearch(Kagi::new("wrong".into(), url).unwrap());
    let out = tool.run(json!({"query": "x", "recency": "year"}), &ctx()).await;
    assert!(out.is_error && out.content.contains("`recency` must be day, week or month"), "{}", out.content);
    let out = tool.run(json!({"query": "x"}), &ctx()).await;
    assert!(out.is_error);
    assert_eq!(out.content, "Error: Kagi API error 401: Unauthorized (code 2) — check KAGI_API_KEY");
    assert!(seen.recv().await.is_some());
    assert!(seen.try_recv().is_err(), "the bad recency was rejected locally");
}

#[tokio::test]
async fn fetch_upgrades_http_and_returns_markdown() {
    let (url, mut seen) = serve(vec![(
        200,
        json!({"meta": {}, "data": [{"url": "https://example.com/a", "markdown": "# A\n\ntext"}]}),
    )])
    .await;
    let tool = WebFetch(Kagi::new("k".into(), url).unwrap());
    let out = tool.run(json!({"url": "http://example.com/a"}), &ctx()).await;
    assert_eq!(out.content, "# https://example.com/a\n\n# A\n\ntext");
    let req = seen.recv().await.unwrap();
    assert!(req.head.starts_with("POST /api/v1/extract "));
    assert_eq!(req.body, json!({"pages": [{"url": "https://example.com/a"}]}));
    let many: Vec<String> = (0..11).map(|i| format!("https://e.com/{i}")).collect();
    assert!(tool.run(json!({"urls": many}), &ctx()).await.content.contains("at most 10"));
}
