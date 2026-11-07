//! `task`: delegate work to a subagent with its own context window and trace.

use std::{
    path::PathBuf,
    sync::{Arc, atomic::AtomicUsize, atomic::Ordering},
    time::Duration,
};

use serde_json::{Value, json};

use super::{Registry, Tool, ToolCtx, ToolOutput, schema, str_arg};
use crate::{
    agent::{Agent, AgentEvent, AgentOptions, EventSink},
    llm::{BoxFuture, Provider, ToolSpec},
    prompt,
    trace::Trace,
};

/// Everything a subagent needs besides its prompt.
pub struct Task {
    pub provider: Arc<dyn Provider>,
    /// Subagent tools; excludes `task`, so delegation is one level deep.
    pub tools: Arc<Registry>,
    pub max_steps: usize,
    pub bash_timeout: Duration,
    pub max_tool_output: usize,
    /// When set, each subagent writes `<parent>-sub<N>.jsonl` here.
    pub trace_dir: Option<PathBuf>,
    pub parent_session: String,
    counter: AtomicUsize,
}

impl Task {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: Arc<Registry>,
        max_steps: usize,
        bash_timeout: Duration,
        max_tool_output: usize,
        trace_dir: Option<PathBuf>,
        parent_session: String,
    ) -> Self {
        Self {
            provider,
            tools,
            max_steps,
            bash_timeout,
            max_tool_output,
            trace_dir,
            parent_session,
            counter: AtomicUsize::new(0),
        }
    }
}

impl Tool for Task {
    fn guidance(&self) -> Option<&'static str> {
        Some(
            "Start independent task calls together in one response; they run in parallel. Give each subagent complete instructions, and split parallel write work into disjoint files or directories. Collect every result you need before giving the final answer, then review the combined changes and run the relevant tests.",
        )
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "task".into(),
            description: "Delegate a self-contained task to a subagent that has the file, shell and search tools \
                          but not your conversation. Give it complete instructions and say what to report back. \
                          Several task calls in one response run in parallel. Returns the subagent's final answer."
                .into(),
            parameters: schema(
                json!({
                    "description": {"type": "string", "description": "3-6 word label shown to the user"},
                    "prompt": {"type": "string", "description": "Full instructions for the subagent"}
                }),
                &["description", "prompt"],
            ),
        }
    }

    fn parallel_safe(&self) -> bool {
        true
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let prompt_text = match str_arg(&args, "prompt") {
                Ok(p) => p,
                Err(e) => return ToolOutput::err(e),
            };
            let n = self.counter.fetch_add(1, Ordering::Relaxed) + 1;
            let child_id = format!("{}-sub{n}", self.parent_session);
            let trace = match &self.trace_dir {
                Some(dir) => {
                    match Trace::create(dir, &child_id, self.provider.model(), &ctx.cwd, Some(&self.parent_session)) {
                        Ok(t) => Some(t),
                        Err(e) => return ToolOutput::err(format!("could not create subagent trace: {e:#}")),
                    }
                }
                None => None,
            };
            let mut agent = Agent::new(AgentOptions {
                provider: self.provider.clone(),
                tools: self.tools.clone(),
                system_prompt: prompt::subagent(self.provider.model(), &self.tools),
                max_steps: self.max_steps,
                cwd: ctx.cwd.clone(),
                bash_timeout: self.bash_timeout,
                max_tool_output: self.max_tool_output,
                trace,
            });
            let parent = ctx.sink.clone();
            let call_id = ctx.call_id.clone();
            let sink: EventSink = Arc::new(move |event| {
                parent(AgentEvent::Subagent { call_id: call_id.clone(), event: Box::new(event) })
            });
            let outcome = agent.run_turn(prompt_text, sink, ctx.cancel.child_token()).await;
            let header = format!("[subagent session {child_id}]\n");
            match (outcome.cancelled, outcome.error) {
                (true, _) => ToolOutput::err(format!("subagent interrupted\n{header}{}", outcome.text)),
                (_, Some(e)) => ToolOutput::err(format!("subagent failed: {e}\n{header}{}", outcome.text)),
                _ if outcome.text.trim().is_empty() => ToolOutput::err(format!("subagent returned no answer\n{header}")),
                _ => ToolOutput::ok(format!("{header}{}", outcome.text)),
            }
        })
    }
}

