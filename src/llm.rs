//! Provider-neutral chat types, the `Provider` trait, and OpenAI-style wire conversion.

use std::{collections::VecDeque, future::Future, pin::Pin, sync::Mutex, time::Duration};

use anyhow::{Result, anyhow};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// One function call requested by the model. `arguments` is the raw JSON text.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

/// A conversation message. This is also the persisted form inside traces.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "snake_case")]
pub enum Message {
    System {
        content: String,
    },
    User {
        content: String,
    },
    Assistant {
        #[serde(default)]
        content: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCall>,
    },
    Tool {
        call_id: String,
        name: String,
        content: String,
    },
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Message::User { content: content.into() }
    }

    pub fn assistant(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Message::Assistant { content: content.into(), reasoning: None, tool_calls }
    }
}

/// A tool definition offered to the model.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Token accounting for one or more requests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    #[serde(default)]
    pub cached_tokens: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, o: Self) {
        self.prompt_tokens += o.prompt_tokens;
        self.completion_tokens += o.completion_tokens;
        self.cached_tokens += o.cached_tokens;
    }
}

/// Incremental output while a completion streams.
#[derive(Clone, Debug, PartialEq)]
pub enum Delta {
    Text(String),
    Reasoning(String),
    ToolCall { name: String },
}

/// A finished model response; `message` is always `Message::Assistant`.
#[derive(Clone, Debug)]
pub struct Completion {
    pub message: Message,
    pub usage: Option<Usage>,
    pub finish_reason: Option<String>,
}

#[derive(Clone, Copy)]
pub struct Request<'a> {
    pub messages: &'a [Message],
    pub tools: &'a [ToolSpec],
}

pub type DeltaSink<'a> = &'a (dyn Fn(Delta) + Send + Sync);

/// A chat-completion backend. Dropping the returned future cancels the request.
pub trait Provider: Send + Sync {
    fn model(&self) -> &str;

    /// Runs one completion, reporting streamed output through `on_delta`.
    ///
    /// @param req conversation and tool definitions.
    /// @param on_delta receives text, reasoning and tool-call deltas as they arrive.
    /// @returns the assembled assistant message.
    fn complete<'a>(&'a self, req: Request<'a>, on_delta: DeltaSink<'a>) -> BoxFuture<'a, Result<Completion>>;
}

/// Converts messages to OpenAI chat-completions JSON.
///
/// Reasoning is echoed back only for assistant messages after the latest user message:
/// DeepSeek thinking mode requires it during a tool loop and ignores older reasoning.
pub fn wire_messages(messages: &[Message]) -> Vec<Value> {
    let last_user = messages.iter().rposition(|m| matches!(m, Message::User { .. }));
    messages
        .iter()
        .enumerate()
        .map(|(i, m)| match m {
            Message::System { content } => json!({"role": "system", "content": content}),
            Message::User { content } => json!({"role": "user", "content": content}),
            Message::Assistant { content, reasoning, tool_calls } => {
                let mut v = json!({"role": "assistant", "content": content});
                if !tool_calls.is_empty() {
                    v["tool_calls"] = tool_calls
                        .iter()
                        .map(|c| {
                            json!({"id": c.id, "type": "function",
                                   "function": {"name": c.name, "arguments": c.arguments}})
                        })
                        .collect();
                }
                if let Some(r) = reasoning
                    && last_user.is_some_and(|u| i > u)
                {
                    v["reasoning_content"] = json!(r);
                }
                v
            }
            Message::Tool { call_id, content, .. } => {
                json!({"role": "tool", "tool_call_id": call_id, "content": content})
            }
        })
        .collect()
}

pub fn wire_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({"type": "function",
                   "function": {"name": t.name, "description": t.description, "parameters": t.parameters}})
        })
        .collect()
}

/// A deterministic provider for tests and demos: replays queued assistant messages and
/// records every request it receives.
#[derive(Default)]
pub struct ScriptedProvider {
    responses: Mutex<VecDeque<Message>>,
    requests: Mutex<Vec<Vec<Message>>>,
    delay: Option<Duration>,
}

impl ScriptedProvider {
    pub fn new(responses: impl IntoIterator<Item = Message>) -> Self {
        Self { responses: Mutex::new(responses.into_iter().collect()), ..Default::default() }
    }

    /// Waits `delay` before answering each request (to exercise cancellation and concurrency).
    pub fn with_delay(mut self, delay: Duration) -> Self {
        self.delay = Some(delay);
        self
    }

    pub fn push(&self, m: Message) {
        self.responses.lock().unwrap().push_back(m);
    }

    /// Message lists of every request so far.
    pub fn requests(&self) -> Vec<Vec<Message>> {
        self.requests.lock().unwrap().clone()
    }
}

impl Provider for ScriptedProvider {
    fn model(&self) -> &str {
        "scripted"
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_delta: DeltaSink<'a>) -> BoxFuture<'a, Result<Completion>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(req.messages.to_vec());
            if let Some(d) = self.delay {
                tokio::time::sleep(d).await;
            }
            let message = self
                .responses
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| anyhow!("scripted provider has no response left"))?;
            if let Message::Assistant { content, tool_calls, .. } = &message {
                if !content.is_empty() {
                    on_delta(Delta::Text(content.clone()));
                }
                for c in tool_calls {
                    on_delta(Delta::ToolCall { name: c.name.clone() });
                }
            }
            Ok(Completion {
                message,
                usage: Some(Usage { prompt_tokens: 10, completion_tokens: 5, cached_tokens: 0 }),
                finish_reason: Some("stop".into()),
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_format_includes_tool_calls_and_recent_reasoning() {
        let msgs = vec![
            Message::System { content: "s".into() },
            Message::Assistant { content: "old".into(), reasoning: Some("r0".into()), tool_calls: vec![] },
            Message::user("u"),
            Message::Assistant {
                content: String::new(),
                reasoning: Some("r1".into()),
                tool_calls: vec![ToolCall { id: "c1".into(), name: "bash".into(), arguments: "{}".into() }],
            },
            Message::Tool { call_id: "c1".into(), name: "bash".into(), content: "ok".into() },
        ];
        let w = wire_messages(&msgs);
        assert!(w[1].get("reasoning_content").is_none());
        assert_eq!(w[3]["reasoning_content"], "r1");
        assert_eq!(w[3]["tool_calls"][0]["function"]["name"], "bash");
        assert_eq!(w[4]["tool_call_id"], "c1");
    }

    #[test]
    fn messages_roundtrip_through_json() {
        let m = Message::assistant("hi", vec![ToolCall { id: "1".into(), name: "x".into(), arguments: "{}".into() }]);
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("\"role\":\"assistant\""));
        assert_eq!(serde_json::from_str::<Message>(&s).unwrap(), m);
    }
}
