//! DeepSeek (OpenAI-compatible) streaming chat-completions provider.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::StreamExt;
use serde_json::{Value, json};

use crate::{
    config::Config,
    llm::{BoxFuture, Completion, Delta, DeltaSink, Message, Provider, Request, ToolCall, Usage, wire_messages, wire_tools},
};

pub struct DeepSeek {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    model: String,
    max_retries: u32,
    idle_timeout: Duration,
}

impl DeepSeek {
    /// Builds a provider from configuration; fails when no API key is configured.
    pub fn new(cfg: &Config) -> Result<Self> {
        let api_key = cfg
            .api_key
            .clone()
            .context("no API key: run `rdsh login` (or set DEEPSEEK_API_KEY)")?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(30))
            .build()
            .context("building HTTP client")?;
        Ok(Self {
            client,
            base_url: cfg.base_url.trim_end_matches('/').to_owned(),
            api_key,
            model: cfg.model.clone(),
            max_retries: cfg.max_retries,
            idle_timeout: Duration::from_secs(cfg.stream_idle_timeout_secs),
        })
    }

    async fn send(&self, body: &Value) -> Result<reqwest::Response> {
        let url = format!("{}/chat/completions", self.base_url);
        let mut attempt = 0;
        loop {
            let res = self.client.post(&url).bearer_auth(&self.api_key).json(body).send().await;
            match res {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();
                    if !(status.as_u16() == 429 || status.is_server_error()) || attempt >= self.max_retries {
                        bail!("API error {status}: {}", text.trim());
                    }
                }
                Err(e) if (e.is_connect() || e.is_timeout()) && attempt < self.max_retries => {}
                Err(e) => return Err(anyhow!(e).context("request failed")),
            }
            attempt += 1;
            let wait = Duration::from_millis(500 * 2u64.pow(attempt));
            tokio::time::sleep(wait).await;
        }
    }
}

impl Provider for DeepSeek {
    fn model(&self) -> &str {
        &self.model
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_delta: DeltaSink<'a>) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            let mut body = json!({
                "model": self.model,
                "messages": wire_messages(req.messages),
                "stream": true,
                "stream_options": {"include_usage": true},
            });
            if !req.tools.is_empty() {
                body["tools"] = Value::Array(wire_tools(req.tools));
            }
            let resp = self.send(&body).await?;
            let mut stream = resp.bytes_stream();
            let mut acc = SseAccumulator::default();
            loop {
                let next = tokio::time::timeout(self.idle_timeout, stream.next())
                    .await
                    .map_err(|_| anyhow!("model stream idle for {}s", self.idle_timeout.as_secs()))?;
                match next {
                    Some(chunk) => acc.feed(&chunk.context("reading model stream")?, on_delta)?,
                    None => break,
                }
                if acc.done {
                    break;
                }
            }
            acc.finish()
        })
    }
}

#[derive(Default)]
struct PartialCall {
    id: String,
    name: String,
    arguments: String,
}

/// Incremental parser for OpenAI-style server-sent events.
#[derive(Default)]
pub struct SseAccumulator {
    buf: Vec<u8>,
    content: String,
    reasoning: String,
    calls: Vec<PartialCall>,
    usage: Option<Usage>,
    finish_reason: Option<String>,
    received: bool,
    pub done: bool,
}

impl SseAccumulator {
    /// Consumes a chunk of bytes, emitting deltas for every complete event line.
    pub fn feed(&mut self, chunk: &[u8], on_delta: DeltaSink<'_>) -> Result<()> {
        self.buf.extend_from_slice(chunk);
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            let line = String::from_utf8_lossy(&line);
            self.line(line.trim_end_matches(['\r', '\n']), on_delta)?;
        }
        Ok(())
    }

    fn line(&mut self, line: &str, on_delta: DeltaSink<'_>) -> Result<()> {
        let Some(data) = line.strip_prefix("data:") else {
            return Ok(());
        };
        let data = data.trim();
        if data == "[DONE]" {
            self.done = true;
            return Ok(());
        }
        let v: Value = serde_json::from_str(data).with_context(|| format!("bad stream event: {data}"))?;
        if let Some(err) = v.get("error") {
            bail!("API stream error: {err}");
        }
        self.received = true;
        if let Some(u) = v.get("usage").filter(|u| u.is_object()) {
            let n = |k: &str| u.get(k).and_then(Value::as_u64).unwrap_or(0);
            self.usage = Some(Usage {
                prompt_tokens: n("prompt_tokens"),
                completion_tokens: n("completion_tokens"),
                cached_tokens: n("prompt_cache_hit_tokens"),
            });
        }
        let Some(choice) = v.get("choices").and_then(|c| c.get(0)) else {
            return Ok(());
        };
        if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
            self.finish_reason = Some(reason.to_owned());
        }
        let Some(delta) = choice.get("delta") else {
            return Ok(());
        };
        if let Some(r) = delta.get("reasoning_content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.reasoning.push_str(r);
            on_delta(Delta::Reasoning(r.to_owned()));
        }
        if let Some(t) = delta.get("content").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            self.content.push_str(t);
            on_delta(Delta::Text(t.to_owned()));
        }
        if let Some(calls) = delta.get("tool_calls").and_then(Value::as_array) {
            for c in calls {
                let index = c.get("index").and_then(Value::as_u64).map_or(self.calls.len(), |i| i as usize);
                while self.calls.len() <= index {
                    self.calls.push(PartialCall::default());
                }
                let slot = &mut self.calls[index];
                if let Some(id) = c.get("id").and_then(Value::as_str) {
                    slot.id = id.to_owned();
                }
                if let Some(f) = c.get("function") {
                    if let Some(name) = f.get("name").and_then(Value::as_str).filter(|s| !s.is_empty()) {
                        if slot.name.is_empty() {
                            on_delta(Delta::ToolCall { name: name.to_owned() });
                        }
                        slot.name.push_str(name);
                    }
                    if let Some(a) = f.get("arguments").and_then(Value::as_str) {
                        slot.arguments.push_str(a);
                    }
                }
            }
        }
        Ok(())
    }

    /// Assembles the final assistant message.
    pub fn finish(self) -> Result<Completion> {
        if !self.received {
            bail!("model stream ended without any data");
        }
        let tool_calls = self
            .calls
            .into_iter()
            .enumerate()
            .filter(|(_, c)| !c.name.is_empty())
            .map(|(i, c)| ToolCall {
                id: if c.id.is_empty() { format!("call_{i}") } else { c.id },
                name: c.name,
                arguments: c.arguments,
            })
            .collect();
        Ok(Completion {
            message: Message::Assistant {
                content: self.content,
                reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
                tool_calls,
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
    fn accumulates_text_reasoning_tool_calls_and_usage() {
        let deltas = Mutex::new(Vec::new());
        let sink = |d: Delta| deltas.lock().unwrap().push(d);
        let mut acc = SseAccumulator::default();
        let stream = concat!(
            "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"think\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\r\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"bash\",\"arguments\":\"{\\\"comm\"}}]}}]}\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"and\\\":1}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n",
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":7,\"completion_tokens\":3,\"prompt_cache_hit_tokens\":4}}\n",
            "data: [DONE]\n",
        );
        // Split mid-line to exercise buffering.
        let (a, b) = stream.as_bytes().split_at(37);
        acc.feed(a, &sink).unwrap();
        acc.feed(b, &sink).unwrap();
        assert!(acc.done);
        let c = acc.finish().unwrap();
        let Message::Assistant { content, reasoning, tool_calls } = c.message else { panic!() };
        assert_eq!(content, "Hello");
        assert_eq!(reasoning.as_deref(), Some("think"));
        assert_eq!(tool_calls[0].arguments, "{\"command\":1}");
        assert_eq!(c.usage.unwrap().cached_tokens, 4);
        assert_eq!(c.finish_reason.as_deref(), Some("tool_calls"));
        let d = deltas.into_inner().unwrap();
        assert_eq!(d[0], Delta::Reasoning("think".into()));
        assert!(d.contains(&Delta::ToolCall { name: "bash".into() }));
    }

    #[test]
    fn stream_errors_surface() {
        let sink = |_d: Delta| {};
        let mut acc = SseAccumulator::default();
        assert!(acc.feed(b"data: {\"error\":{\"message\":\"boom\"}}\n", &sink).is_err());
        assert!(SseAccumulator::default().finish().is_err());
    }
}
