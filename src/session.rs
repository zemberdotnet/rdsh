//! Assembles an agent (provider, tools, subagents, trace) for a new or resumed session.

use std::{path::Path, sync::Arc, time::Duration};

use anyhow::{Context, Result};

use crate::{
    agent::{Agent, AgentOptions},
    config::Config,
    decision::SystemOne,
    length::LengthRouter,
    llm::Provider,
    prompt,
    tools::{self, task::Task},
    trace::{self, Trace},
    util::new_session_id,
};

/// How to start a session.
pub enum Start<'a> {
    New,
    /// Session id, unique prefix, or `last`.
    Resume(&'a str),
}

/// Builds the top-level agent.
///
/// @param cfg validated configuration.
/// @param cwd working directory for tools and prompts.
/// @param provider model backend shared with subagents.
/// @param start new session or one to resume from its trace.
/// @returns the agent; its history already holds any resumed messages.
pub fn build(cfg: &Config, cwd: &Path, provider: Arc<dyn Provider>, start: Start<'_>) -> Result<Agent> {
    let dir = cfg.sessions_dir();
    let (session_id, trace, history) = match start {
        Start::New => {
            let id = new_session_id();
            let t = Trace::create(&dir, &id, provider.model(), cwd, None)?;
            (id, t, None)
        }
        Start::Resume(query) => {
            let path = trace::find_session(&dir, query)?;
            let loaded = trace::load(&path).with_context(|| format!("loading session {}", path.display()))?;
            let t = Trace::open_append(&path, &loaded.session_id)?;
            (loaded.session_id, t, Some(loaded.messages))
        }
    };
    let bash_timeout = Duration::from_secs(cfg.bash_timeout_secs);
    let base = Arc::new(tools::builtin());
    let mut registry = (*base).clone();
    registry.add(Arc::new(Task::new(
        provider.clone(),
        base,
        cfg.subagent_max_steps,
        bash_timeout,
        cfg.max_tool_output_chars,
        Some(dir),
        session_id,
    )));
    let system_prompt = prompt::main(provider.model(), &registry);
    let length = match cfg.length_routing {
        true => SystemOne::from_config(cfg)?.map(|d| Arc::new(LengthRouter::new(Arc::new(d)))),
        false => None,
    };
    let opts = AgentOptions {
        provider,
        tools: Arc::new(registry),
        system_prompt,
        max_steps: cfg.max_steps,
        cwd: cwd.to_owned(),
        bash_timeout,
        max_tool_output: cfg.max_tool_output_chars,
        trace: Some(trace),
        length,
    };
    Ok(match history {
        Some(h) => Agent::resume(opts, h),
        None => Agent::new(opts),
    })
}
