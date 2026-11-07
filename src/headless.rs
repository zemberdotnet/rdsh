//! `rdsh run`: one turn without the TUI. Assistant text goes to stdout, progress to stderr.

use std::{io::Write, sync::Arc};

use tokio_util::sync::CancellationToken;

use crate::{
    agent::{Agent, AgentEvent, EventSink},
    tools::summarize,
};

/// Runs `prompt` as a single turn; Ctrl-C interrupts it.
///
/// @returns process exit code: 0 on success, 1 on error, 130 when interrupted.
pub async fn run(mut agent: Agent, prompt: String) -> i32 {
    if let Some(id) = agent.session_id() {
        eprintln!("session {id}");
    }
    let sink: EventSink = Arc::new(|ev| print_event(&ev, 0));
    let cancel = CancellationToken::new();
    let watcher = {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            if tokio::signal::ctrl_c().await.is_ok() {
                cancel.cancel();
            }
        })
    };
    let outcome = agent.run_turn(prompt, sink, cancel).await;
    watcher.abort();
    let u = agent.usage;
    eprintln!("\n[tokens: {} in ({} cached), {} out]", u.prompt_tokens, u.cached_tokens, u.completion_tokens);
    match (outcome.cancelled, outcome.error) {
        (true, _) => 130,
        (_, Some(_)) => 1,
        _ => 0,
    }
}

fn print_event(ev: &AgentEvent, depth: usize) {
    let indent = "  ".repeat(depth);
    match ev {
        AgentEvent::TextDelta(t) if depth == 0 => {
            print!("{t}");
            let _ = std::io::stdout().flush();
        }
        AgentEvent::Assistant { message: crate::llm::Message::Assistant { tool_calls, .. }, .. }
            if depth == 0 && !tool_calls.is_empty() =>
        {
            println!();
        }
        AgentEvent::ToolStart { call } => {
            eprintln!("{indent}▸ {} {}", call.name, summarize(&call.name, &call.arguments));
        }
        AgentEvent::ToolEnd { output, duration_ms, .. } if output.is_error => {
            let first = output.content.lines().next().unwrap_or("");
            eprintln!("{indent}  ✗ {first} ({duration_ms} ms)");
        }
        AgentEvent::Subagent { event, .. } => print_event(event, depth + 1),
        AgentEvent::Warning(w) => eprintln!("{indent}warning: {w}"),
        AgentEvent::TurnEnd { error: Some(e), .. } if depth == 0 => eprintln!("\nerror: {e}"),
        AgentEvent::TurnEnd { cancelled: true, .. } if depth == 0 => eprintln!("\ninterrupted"),
        _ => {}
    }
}
