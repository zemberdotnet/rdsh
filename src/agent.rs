//! The agent loop: model request → tool calls → tool results → repeat until a final answer.

use std::{path::PathBuf, sync::Arc, time::Duration, time::Instant};

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{
    length::{LengthRouter, Routing},
    title::{self, Titler},
    llm::{Delta, Message, Provider, Request, ToolCall, ToolSpec, Usage},
    prompt,
    tools::{Registry, ToolCtx, ToolOutput},
    trace::{Trace, TraceEvent},
    util::truncate_middle,
};

/// Observable progress of a turn. Delivered in order through an `EventSink`.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
#[serde(tag = "type", content = "data", rename_all = "snake_case")]
pub enum AgentEvent {
    /// A turn began with this user message (always the first event of a turn).
    TurnStarted { text: String },
    /// The session got its short title (written next to its trace).
    Titled { title: String },
    TextDelta(String),
    ReasoningDelta(String),
    /// The model started emitting a call to `name` (arguments still streaming).
    ToolCallStreaming { name: String },
    /// A completed model response (already appended to history).
    Assistant { message: Message, usage: Option<Usage> },
    ToolStart { call: ToolCall },
    ToolEnd { call_id: String, output: ToolOutput, duration_ms: u64 },
    /// The `task` call `call_id` started a subagent recorded as session `session_id`.
    SubagentStarted { call_id: String, session_id: String, title: String },
    /// An event from the subagent running inside tool call `call_id`.
    Subagent { call_id: String, event: Box<AgentEvent> },
    /// What the response-length router decided for this turn (top-level agent only).
    ResponseLength(Routing),
    /// Non-fatal problem (for example, the trace could not be written).
    Warning(String),
    TurnEnd { cancelled: bool, error: Option<String> },
}

pub type EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>;

/// Final state of one user turn.
#[derive(Clone, Debug, PartialEq)]
pub struct TurnOutcome {
    /// Text of the last assistant message.
    pub text: String,
    pub cancelled: bool,
    pub error: Option<String>,
}

pub struct AgentOptions {
    pub provider: Arc<dyn Provider>,
    pub tools: Arc<Registry>,
    pub system_prompt: String,
    pub max_steps: usize,
    pub cwd: PathBuf,
    pub bash_timeout: Duration,
    pub max_tool_output: usize,
    pub trace: Option<Trace>,
    /// Picks the reply length each turn; `None` for subagents and headless runs.
    pub length: Option<Arc<LengthRouter>>,
    /// Names the session after its first message; `None` for subagents and headless runs.
    pub titler: Option<Arc<Titler>>,
}

pub struct Agent {
    opts: AgentOptions,
    specs: Vec<ToolSpec>,
    history: Vec<Message>,
    pub usage: Usage,
    trace_failed: Option<String>,
}

impl Agent {
    /// Starts a fresh conversation seeded with the system prompt.
    pub fn new(opts: AgentOptions) -> Self {
        let mut agent = Self::bare(opts, Vec::new());
        let system = Message::System { content: agent.opts.system_prompt.clone() };
        agent.push(system);
        agent
    }

    /// Continues a conversation loaded from a trace. Tool calls left without results
    /// (a crash mid-turn) receive synthetic "interrupted" results so the history stays valid.
    pub fn resume(opts: AgentOptions, history: Vec<Message>) -> Self {
        let mut agent = Self::bare(opts, history);
        if let Some(i) = agent.history.iter().rposition(|m| matches!(m, Message::Assistant { .. }))
            && let Message::Assistant { tool_calls, .. } = agent.history[i].clone()
        {
            for call in tool_calls {
                let answered = agent.history[i + 1..]
                    .iter()
                    .any(|m| matches!(m, Message::Tool { call_id, .. } if *call_id == call.id));
                if !answered {
                    agent.push(Message::Tool {
                        call_id: call.id,
                        name: call.name,
                        content: "Error: interrupted before this tool call finished".into(),
                    });
                }
            }
        }
        agent
    }

    /// Starts naming the session in the background, once, if it has a trace and no title yet.
    /// Uses the session's first user message (from history when resumed). Failures leave the
    /// session unnamed; the rail then shows its first message.
    fn name_session(&mut self, input: &str, sink: &EventSink) {
        let (Some(titler), Some(trace)) = (self.opts.titler.take(), self.opts.trace.as_ref()) else { return };
        let path = title::path_for(&trace.path);
        if path.exists() {
            return;
        }
        let first = self
            .history
            .iter()
            .find_map(|m| match m {
                Message::User { content } => Some(content.clone()),
                _ => None,
            })
            .unwrap_or_else(|| input.to_owned());
        let sink = sink.clone();
        tokio::spawn(async move {
            // A failed title leaves the session unnamed; the rail falls back to its first message.
            if let Ok(t) = titler.title(&first).await {
                match std::fs::write(&path, format!("{t}\n")) {
                    Ok(()) => sink(AgentEvent::Titled { title: t }),
                    Err(e) => sink(AgentEvent::Warning(format!("could not save the session title: {e}"))),
                }
            }
        });
    }

    fn bare(opts: AgentOptions, history: Vec<Message>) -> Self {
        let specs = opts.tools.specs();
        Self { opts, specs, history, usage: Usage::default(), trace_failed: None }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn session_id(&self) -> Option<&str> {
        self.opts.trace.as_ref().map(|t| t.session_id.as_str())
    }

    pub fn model(&self) -> &str {
        self.opts.provider.model()
    }

    fn log(&mut self, event: TraceEvent) {
        if self.trace_failed.is_some() {
            return;
        }
        if let Some(t) = &mut self.opts.trace
            && let Err(e) = t.record(event)
        {
            self.trace_failed = Some(format!("trace disabled: {e:#}"));
        }
    }

    fn push(&mut self, m: Message) {
        self.log(TraceEvent::Message { message: m.clone() });
        self.history.push(m);
    }

    fn warn_trace(&mut self, sink: &EventSink) {
        if let Some(w) = self.trace_failed.as_ref().filter(|_| self.opts.trace.is_some()) {
            sink(AgentEvent::Warning(w.clone()));
            // Report once; keep `trace_failed` set so logging stays off.
            self.opts.trace = None;
        }
    }

    fn finish(&mut self, sink: &EventSink, text: String, cancelled: bool, error: Option<String>) -> TurnOutcome {
        if cancelled {
            self.log(TraceEvent::Cancelled);
        }
        if let Some(e) = &error {
            self.log(TraceEvent::Error { message: e.clone() });
        }
        self.warn_trace(sink);
        sink(AgentEvent::TurnEnd { cancelled, error: error.clone() });
        TurnOutcome { text, cancelled, error }
    }

    /// Runs one user turn to completion, cancellation, or error.
    ///
    /// @param input the user message (already including any review comments).
    /// @param sink receives progress events; always ends with `TurnEnd`.
    /// @param cancel interrupts the model request or running tools.
    /// @returns the final assistant text and how the turn ended.
    pub async fn run_turn(&mut self, input: String, sink: EventSink, cancel: CancellationToken) -> TurnOutcome {
        sink(AgentEvent::TurnStarted { text: input.clone() });
        self.name_session(&input, &sink);
        let context = prompt::runtime_context(&self.opts.cwd);
        let latest = self.history.iter().rev().find_map(|m| match m {
            Message::Context { content } => Some(content),
            _ => None,
        });
        if latest != Some(&context) {
            self.push(Message::Context { content: context });
        }
        let directive = match self.opts.length.clone() {
            Some(router) => {
                let routed = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => None,
                    r = router.route(&self.history, &input) => Some(r),
                };
                let Some((routing, directive)) = routed else {
                    self.push(Message::User { content: input });
                    return self.finish(&sink, String::new(), true, None);
                };
                if let Routing::Unavailable { reason } = &routing
                    && router.first_failure()
                {
                    sink(AgentEvent::Warning(format!("response length not routed: {reason}")));
                }
                sink(AgentEvent::ResponseLength(routing));
                directive
            }
            None => None,
        };
        self.push(Message::User { content: input });
        if let Some(content) = directive {
            self.push(Message::Directive { content });
        }
        let mut last_text = String::new();
        for _ in 0..self.opts.max_steps {
            let delta_sink = |d: Delta| {
                sink(match d {
                    Delta::Text(t) => AgentEvent::TextDelta(t),
                    Delta::Reasoning(t) => AgentEvent::ReasoningDelta(t),
                    Delta::ToolCall { name } => AgentEvent::ToolCallStreaming { name },
                })
            };
            let req = Request { messages: &self.history, tools: &self.specs };
            let result = tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                r = self.opts.provider.complete(req, &delta_sink) => Some(r),
            };
            let completion = match result {
                None => return self.finish(&sink, last_text, true, None),
                Some(Err(e)) => return self.finish(&sink, last_text, false, Some(format!("{e:#}"))),
                Some(Ok(c)) => c,
            };
            if let Some(u) = completion.usage {
                self.usage += u;
                self.log(TraceEvent::Usage { usage: u });
            }
            let calls = match &completion.message {
                Message::Assistant { content, tool_calls, .. } => {
                    last_text = content.clone();
                    tool_calls.clone()
                }
                _ => Vec::new(),
            };
            self.push(completion.message.clone());
            self.warn_trace(&sink);
            sink(AgentEvent::Assistant { message: completion.message, usage: completion.usage });
            if calls.is_empty() {
                return self.finish(&sink, last_text, false, None);
            }
            let results = self.run_tools(&calls, &sink, &cancel).await;
            for (call, (output, duration_ms)) in calls.into_iter().zip(results) {
                self.log(TraceEvent::ToolMeta { call_id: call.id.clone(), is_error: output.is_error, duration_ms });
                self.push(Message::Tool { call_id: call.id, name: call.name, content: output.content });
            }
            if cancel.is_cancelled() {
                return self.finish(&sink, last_text, true, None);
            }
        }
        let max = self.opts.max_steps;
        self.finish(&sink, last_text, false, Some(format!("stopped after {max} model requests without a final answer")))
    }

    async fn run_tools(&self, calls: &[ToolCall], sink: &EventSink, cancel: &CancellationToken) -> Vec<(ToolOutput, u64)> {
        let parallel = calls.len() > 1
            && calls.iter().all(|c| self.opts.tools.get(&c.name).is_some_and(|t| t.parallel_safe()));
        if parallel {
            futures_util::future::join_all(calls.iter().map(|c| self.run_tool(c, sink, cancel))).await
        } else {
            let mut out = Vec::with_capacity(calls.len());
            for c in calls {
                out.push(self.run_tool(c, sink, cancel).await);
            }
            out
        }
    }

    async fn run_tool(&self, call: &ToolCall, sink: &EventSink, cancel: &CancellationToken) -> (ToolOutput, u64) {
        sink(AgentEvent::ToolStart { call: call.clone() });
        let start = Instant::now();
        let output = if cancel.is_cancelled() {
            ToolOutput::err("interrupted by user before this call ran")
        } else {
            match (self.opts.tools.get(&call.name), parse_args(&call.arguments)) {
                (None, _) => ToolOutput::err(format!(
                    "unknown tool `{}`; available: {}",
                    call.name,
                    self.opts.tools.names().collect::<Vec<_>>().join(", ")
                )),
                (_, Err(e)) => ToolOutput::err(format!("invalid JSON arguments for {}: {e}", call.name)),
                (Some(tool), Ok(args)) => {
                    let ctx = ToolCtx {
                        cwd: self.opts.cwd.clone(),
                        cancel: cancel.clone(),
                        sink: sink.clone(),
                        call_id: call.id.clone(),
                        bash_timeout: self.opts.bash_timeout,
                    };
                    tokio::select! {
                        biased;
                        _ = cancel.cancelled() => ToolOutput::err("interrupted by user"),
                        out = tool.run(args, &ctx) => out,
                    }
                }
            }
        };
        let output = ToolOutput {
            content: truncate_middle(output.content, self.opts.max_tool_output),
            is_error: output.is_error,
        };
        let duration_ms = start.elapsed().as_millis() as u64;
        sink(AgentEvent::ToolEnd { call_id: call.id.clone(), output: output.clone(), duration_ms });
        (output, duration_ms)
    }
}

fn parse_args(raw: &str) -> Result<Value, String> {
    if raw.trim().is_empty() {
        return Ok(Value::Object(Default::default()));
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(v @ Value::Object(_)) => Ok(v),
        Ok(_) => Err("arguments must be a JSON object".into()),
        Err(e) => Err(e.to_string()),
    }
}
