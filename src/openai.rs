//! OpenAI streaming Responses API provider. With an API key it calls api.openai.com; with a
//! ChatGPT login it calls the Codex backend at chatgpt.com, which bills the subscription and
//! takes the same request shape plus account headers. Requests are `store: false`, so the
//! encrypted reasoning of the current turn is sent back with each follow-up.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    auth::{self, OpenAiAuth},
    chatgpt,
    config::Config,
    llm::{BoxFuture, Completion, Delta, DeltaSink, Effort, Message, Provider, Request, ToolCall, ToolSpec, Usage},
};

pub struct OpenAi {
    client: reqwest::Client,
    api_base: String,
    chatgpt_base: String,
    /// Loaded from the credentials file on first use when the config had none, so a login made
    /// while the hub runs takes effect without a restart.
    auth: tokio::sync::Mutex<Option<OpenAiAuth>>,
    /// The login came from `OPENAI_API_KEY`: never reloaded from or saved to disk.
    from_env: bool,
    model: String,
    max_retries: u32,
    idle_timeout: Duration,
}

/// Where one request goes and how it is authorized.
struct Endpoint {
    url: String,
    bearer: String,
    /// ChatGPT logins only.
    account_id: Option<String>,
}

impl OpenAi {
    pub fn new(cfg: &Config, model: String) -> Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            api_base: cfg.openai_base_url.trim_end_matches('/').to_owned(),
            chatgpt_base: cfg.chatgpt_base_url.trim_end_matches('/').to_owned(),
            auth: tokio::sync::Mutex::new(cfg.openai.clone()),
            from_env: cfg.openai_from_env,
            model,
            max_retries: cfg.max_retries,
            idle_timeout: Duration::from_secs(cfg.stream_idle_timeout_secs),
        })
    }

    /// The endpoint for the current login, refreshing a ChatGPT token that is stale (or, with
    /// `rejected`, that the server just refused).
    async fn endpoint(&self, rejected: bool) -> Result<Endpoint> {
        let mut auth = self.auth.lock().await;
        if auth.is_none() && !self.from_env {
            *auth = auth::load()?.openai;
        }
        let token = match auth.as_ref() {
            None => bail!(
                "not logged in to OpenAI: run `rdsh login chatgpt` (subscription) or `rdsh login openai` (API key), or set OPENAI_API_KEY"
            ),
            Some(OpenAiAuth::ApiKey { key }) => {
                return Ok(Endpoint { url: format!("{}/responses", self.api_base), bearer: key.clone(), account_id: None });
            }
            Some(OpenAiAuth::Chatgpt(t)) => t.clone(),
        };
        let token = if rejected || token.stale() {
            // Another rdsh process may have refreshed already; refresh tokens are single-use.
            let on_disk = match auth::load()?.openai {
                Some(OpenAiAuth::Chatgpt(d)) if d.access != token.access && !d.stale() => Some(d),
                _ => None,
            };
            let fresh = match on_disk {
                Some(d) => d,
                None => {
                    let fresh = chatgpt::refresh(&token).await?;
                    auth::update(|c| c.openai = Some(OpenAiAuth::Chatgpt(fresh.clone())))?;
                    fresh
                }
            };
            *auth = Some(OpenAiAuth::Chatgpt(fresh.clone()));
            fresh
        } else {
            token
        };
        Ok(Endpoint {
            url: format!("{}/codex/responses", self.chatgpt_base),
            bearer: token.access,
            account_id: Some(token.account_id),
        })
    }

    async fn send(&self, body: &Value, cache_key: Option<&str>) -> Result<reqwest::Response> {
        let mut attempt = 0;
        let mut refreshed = false;
        loop {
            let ep = self.endpoint(false).await?;
            let mut rb = self.client.post(&ep.url).bearer_auth(&ep.bearer).header("accept", "text/event-stream").json(body);
            if let Some(account) = &ep.account_id {
                rb = rb.header("chatgpt-account-id", account).header("OpenAI-Beta", "responses=experimental").header("originator", chatgpt::ORIGINATOR);
                if let Some(k) = cache_key {
                    rb = rb.header("session-id", k);
                }
            }
            match rb.send().await {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if status.as_u16() == 401 && ep.account_id.is_some() && !refreshed {
                        refreshed = true;
                        self.endpoint(true).await?;
                        continue;
                    }
                    // A spent subscription allowance will not come back within the retries.
                    let limit = text.contains("usage_limit") || text.contains("usage limit");
                    if !(status.as_u16() == 429 || status.is_server_error()) || limit || attempt >= self.max_retries {
                        bail!("OpenAI error {status}: {}", text.trim());
                    }
                }
                Err(e) if (e.is_connect() || e.is_timeout()) && attempt < self.max_retries => {}
                Err(e) => return Err(anyhow!(e).context("request failed")),
            }
            attempt += 1;
            tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
        }
    }
}

impl Provider for OpenAi {
    fn model(&self) -> &str {
        &self.model
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_delta: DeltaSink<'a>) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let body = request_body(&req, req.model.unwrap_or(&self.model));
            let resp = self.send(&body, req.cache_key).await?;
            let mut stream = resp.bytes_stream();
            let mut acc = Accumulator::default();
            while !acc.done {
                let next = tokio::time::timeout(self.idle_timeout, stream.next())
                    .await
                    .map_err(|_| anyhow!("model stream idle for {}s", self.idle_timeout.as_secs()))?;
                match next {
                    Some(chunk) => acc.feed(&chunk.context("reading model stream")?, on_delta)?,
                    None => break,
                }
            }
            acc.finish()
        })
    }
}

/// OpenAI's reasoning effort for `effort` on `model`. Some models cannot turn reasoning off and
/// take their lowest level instead; the newest ones have `max` above `xhigh`.
pub fn effort_level(model: &str, effort: Effort) -> &'static str {
    const NO_REASONING: [&str; 12] = [
        "gpt-5.1", "gpt-5.2", "gpt-5.3-codex", "gpt-5.4", "gpt-5.4-mini", "gpt-5.4-nano", "gpt-5.5",
        "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna", "gpt-6-sol", "gpt-6-luna",
    ];
    match effort {
        Effort::Off if NO_REASONING.contains(&model) => "none",
        Effort::Off | Effort::Low => "low",
        Effort::High => "high",
        Effort::Max if model.contains("gpt-5.6") || model.contains("gpt-6") => "max",
        Effort::Max if ["gpt-5.2", "gpt-5.3", "gpt-5.4", "gpt-5.5"].iter().any(|v| model.contains(v)) => "xhigh",
        Effort::Max => "high",
    }
}

/// The Responses request for `req`.
pub fn request_body(req: &Request<'_>, model: &str) -> Value {
    let instructions = req
        .messages
        .iter()
        .find_map(|m| match m {
            Message::System { content } => Some(content.as_str()),
            _ => None,
        })
        .unwrap_or("You are a helpful assistant.");
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": wire_input(req.messages),
        "store": false,
        "stream": true,
        "tool_choice": "auto",
        "parallel_tool_calls": true,
        "reasoning": {"effort": effort_level(model, req.effort), "summary": "auto"},
        "include": ["reasoning.encrypted_content"],
    });
    if !req.tools.is_empty() {
        body["tools"] = Value::Array(wire_tools(req.tools));
    }
    if let Some(k) = req.cache_key {
        body["prompt_cache_key"] = json!(k);
    }
    body
}

fn wire_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| json!({"type": "function", "name": t.name, "description": t.description, "parameters": t.parameters, "strict": false}))
        .collect()
}

/// Converts the conversation to Responses input items. The first system message goes in
/// `instructions`; later ones become developer messages. Replayed reasoning is sent only for the
/// current turn (after the latest user message), the way DeepSeek's reasoning is.
pub fn wire_input(messages: &[Message]) -> Vec<Value> {
    let last_user = messages.iter().rposition(|m| matches!(m, Message::User { .. }));
    let text = |role: &str, kind: &str, t: &str| json!({"type": "message", "role": role, "content": [{"type": kind, "text": t}]});
    let mut out = Vec::new();
    let mut seen_system = false;
    for (i, m) in messages.iter().enumerate() {
        match m {
            Message::System { content } => {
                if seen_system {
                    out.push(text("developer", "input_text", content));
                }
                seen_system = true;
            }
            Message::User { content } | Message::Context { content } | Message::Directive { content } => {
                out.push(text("user", "input_text", content));
            }
            Message::Assistant { content, tool_calls, replay, .. } => {
                if last_user.is_some_and(|u| i > u) {
                    out.extend(replay.iter().cloned());
                }
                if !content.is_empty() {
                    out.push(text("assistant", "output_text", content));
                }
                for c in tool_calls {
                    out.push(json!({"type": "function_call", "call_id": c.id, "name": c.name, "arguments": c.arguments}));
                }
            }
            Message::Tool { call_id, content, .. } => {
                out.push(json!({"type": "function_call_output", "call_id": call_id, "output": content}));
            }
        }
    }
    out
}

/// Incremental parser for Responses server-sent events.
#[derive(Default)]
pub struct Accumulator {
    buf: Vec<u8>,
    content: String,
    reasoning: String,
    calls: Vec<ToolCall>,
    replay: Vec<Value>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    received: bool,
    pub done: bool,
}

impl Accumulator {
    pub fn feed(&mut self, chunk: &[u8], on_delta: DeltaSink<'_>) -> Result<()> {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            if let Some(data) = line.trim_end_matches(['\r', '\n']).strip_prefix("data:") {
                self.event(data.trim(), on_delta)?;
            }
        }
        Ok(())
    }

    fn event(&mut self, data: &str, on_delta: DeltaSink<'_>) -> Result<()> {
        if data.is_empty() || data == "[DONE]" {
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).with_context(|| format!("bad stream event: {data}"))?;
        self.received = true;
        let delta = || v["delta"].as_str().filter(|s| !s.is_empty()).map(str::to_owned);
        match v["type"].as_str().unwrap_or_default() {
            "response.output_text.delta" => {
                if let Some(t) = delta() {
                    self.content.push_str(&t);
                    on_delta(Delta::Text(t));
                }
            }
            "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
                if let Some(t) = delta() {
                    self.reasoning.push_str(&t);
                    on_delta(Delta::Reasoning(t));
                }
            }
            "response.reasoning_summary_part.done" => {
                self.reasoning.push_str("\n\n");
                on_delta(Delta::Reasoning("\n\n".into()));
            }
            "response.output_item.added" if v["item"]["type"] == "function_call" => {
                if let Some(name) = v["item"]["name"].as_str() {
                    on_delta(Delta::ToolCall { name: name.to_owned() });
                }
            }
            "response.output_item.done" => {
                let item = &v["item"];
                match item["type"].as_str() {
                    Some("function_call") => self.calls.push(ToolCall {
                        id: item["call_id"].as_str().unwrap_or_default().to_owned(),
                        name: item["name"].as_str().unwrap_or_default().to_owned(),
                        arguments: item["arguments"].as_str().unwrap_or("{}").to_owned(),
                    }),
                    Some("reasoning") if item.get("encrypted_content").is_some_and(Value::is_string) => {
                        self.replay.push(item.clone());
                    }
                    // Text normally arrives as deltas; take the item's when none did.
                    Some("message") if self.content.is_empty() => {
                        if let Some(parts) = item["content"].as_array() {
                            self.content = parts.iter().filter_map(|p| p["text"].as_str()).collect();
                        }
                    }
                    _ => {}
                }
            }
            "response.completed" | "response.done" | "response.incomplete" => {
                let r = &v["response"];
                let u = &r["usage"];
                if u.is_object() {
                    let n = |x: &Value| x.as_u64().unwrap_or(0);
                    self.usage = Some(Usage {
                        prompt_tokens: n(&u["input_tokens"]),
                        completion_tokens: n(&u["output_tokens"]),
                        cached_tokens: n(&u["input_tokens_details"]["cached_tokens"]),
                    });
                }
                self.finish_reason = match r["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => Some("length".into()),
                    Some(other) => Some(other.to_owned()),
                    None if !self.calls.is_empty() => Some("tool_calls".into()),
                    None => Some("stop".into()),
                };
                self.done = true;
            }
            "response.failed" => {
                let e = &v["response"]["error"];
                bail!("OpenAI response failed: {}: {}", e["code"].as_str().unwrap_or("error"), e["message"].as_str().unwrap_or("no details"));
            }
            "error" => {
                let e = if v["error"].is_object() { &v["error"] } else { &v };
                bail!("OpenAI stream error: {}: {}", e["code"].as_str().unwrap_or("error"), e["message"].as_str().unwrap_or("no details"));
            }
            _ => {}
        }
        Ok(())
    }

    pub fn finish(self) -> Result<Completion> {
        if !self.received {
            bail!("model stream ended without any data");
        }
        if !self.done {
            bail!("model stream ended before the response completed");
        }
        let reasoning = self.reasoning.trim_end().to_owned();
        Ok(Completion {
            message: Message::Assistant {
                content: self.content,
                reasoning: (!reasoning.is_empty()).then_some(reasoning),
                tool_calls: self.calls,
                replay: self.replay,
            },
            usage: self.usage,
            finish_reason: self.finish_reason,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    #[test]
    fn effort_levels_follow_each_model() {
        assert_eq!(effort_level("gpt-6-sol", Effort::Off), "none");
        assert_eq!(effort_level("gpt-6.2-sol", Effort::Off), "low");
        assert_eq!(effort_level("gpt-6.2-sol", Effort::Max), "max");
        assert_eq!(effort_level("gpt-5.4", Effort::Max), "xhigh");
        assert_eq!(effort_level("o3", Effort::Max), "high");
    }

    #[test]
    fn input_items_carry_tools_and_current_turn_reasoning() {
        let item = json!({"type": "reasoning", "id": "rs_1", "encrypted_content": "enc", "summary": []});
        let old = json!({"type": "reasoning", "id": "rs_0", "encrypted_content": "old", "summary": []});
        let msgs = vec![
            Message::System { content: "sys".into() },
            Message::Assistant { content: "earlier".into(), reasoning: None, tool_calls: vec![], replay: vec![old] },
            Message::user("hi"),
            Message::Assistant {
                content: String::new(),
                reasoning: Some("r".into()),
                tool_calls: vec![ToolCall { id: "call_1".into(), name: "bash".into(), arguments: "{}".into() }],
                replay: vec![item.clone()],
            },
            Message::Tool { call_id: "call_1".into(), name: "bash".into(), content: "ok".into() },
            Message::System { content: "later".into() },
        ];
        let w = wire_input(&msgs);
        assert_eq!(w[0]["content"][0], json!({"type": "output_text", "text": "earlier"}));
        assert_eq!(w[1]["content"][0], json!({"type": "input_text", "text": "hi"}));
        assert_eq!(w[2], item, "current-turn reasoning is replayed, the older one is not");
        assert_eq!(w[3], json!({"type": "function_call", "call_id": "call_1", "name": "bash", "arguments": "{}"}));
        assert_eq!(w[4], json!({"type": "function_call_output", "call_id": "call_1", "output": "ok"}));
        assert_eq!(w[5]["role"], "developer");
        assert_eq!(w.len(), 6);

        let tools = [ToolSpec { name: "bash".into(), description: "d".into(), parameters: json!({"type": "object"}) }];
        let req = Request { messages: &msgs, tools: &tools, effort: Effort::High, model: None, cache_key: Some("s1") };
        let body = request_body(&req, "gpt-6.2-sol");
        assert_eq!(body["instructions"], "sys");
        assert_eq!(body["store"], false);
        assert_eq!(body["reasoning"]["effort"], "high");
        assert_eq!(body["tools"][0]["name"], "bash");
        assert_eq!(body["prompt_cache_key"], "s1");
    }

    #[test]
    fn accumulates_text_reasoning_calls_and_usage() {
        let deltas = Mutex::new(Vec::new());
        let sink = |d: Delta| deltas.lock().unwrap().push(d);
        let mut acc = Accumulator::default();
        let stream = concat!(
            "event: response.created\ndata: {\"type\":\"response.created\",\"response\":{}}\n\n",
            "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"plan\"}\n",
            "data: {\"type\":\"response.reasoning_summary_part.done\"}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"reasoning\",\"id\":\"rs_1\",\"encrypted_content\":\"enc\",\"summary\":[]}}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Hel\"}\n",
            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"lo\"}\r\n",
            "data: {\"type\":\"response.output_item.added\",\"item\":{\"type\":\"function_call\",\"name\":\"bash\"}}\n",
            "data: {\"type\":\"response.output_item.done\",\"item\":{\"type\":\"function_call\",\"call_id\":\"call_9\",\"name\":\"bash\",\"arguments\":\"{\\\"command\\\":1}\"}}\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"usage\":{\"input_tokens\":7,\"output_tokens\":3,\"input_tokens_details\":{\"cached_tokens\":4}}}}\n",
        );
        let (a, b) = stream.as_bytes().split_at(61);
        acc.feed(a, &sink).unwrap();
        acc.feed(b, &sink).unwrap();
        assert!(acc.done);
        let c = acc.finish().unwrap();
        let Message::Assistant { content, reasoning, tool_calls, replay } = c.message else { panic!() };
        assert_eq!(content, "Hello");
        assert_eq!(reasoning.as_deref(), Some("plan"));
        assert_eq!(tool_calls, vec![ToolCall { id: "call_9".into(), name: "bash".into(), arguments: "{\"command\":1}".into() }]);
        assert_eq!(replay[0]["encrypted_content"], "enc");
        assert_eq!(c.usage, Some(Usage { prompt_tokens: 7, completion_tokens: 3, cached_tokens: 4 }));
        assert_eq!(c.finish_reason.as_deref(), Some("tool_calls"));
        let d = deltas.into_inner().unwrap();
        assert_eq!(d[0], Delta::Reasoning("plan".into()));
        assert!(d.contains(&Delta::ToolCall { name: "bash".into() }));
    }

    #[test]
    fn failures_and_cut_off_streams_are_errors() {
        let sink = |_d: Delta| {};
        let mut acc = Accumulator::default();
        let failed = "data: {\"type\":\"response.failed\",\"response\":{\"error\":{\"code\":\"rate_limit\",\"message\":\"slow\"}}}\n";
        assert!(acc.feed(failed.as_bytes(), &sink).unwrap_err().to_string().contains("rate_limit"));
        let mut acc = Accumulator::default();
        acc.feed(b"data: {\"type\":\"response.output_text.delta\",\"delta\":\"x\"}\n", &sink).unwrap();
        assert!(acc.finish().is_err());
    }
}
