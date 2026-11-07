//! System prompts and the runtime-context message.
//!
//! The system prompt holds only stable text (identity, general rules, per-tool guidance), so it
//! stays a cacheable prefix. Process-specific facts (working directory, platform, date) go in a
//! separate runtime-context message that the agent appends when they change.

use std::path::Path;

use crate::{tools::Registry, util::today};

/// Guidance for the tools in `tools`, one paragraph per tool that provides any, in registry order.
fn tool_guidance(tools: &Registry) -> String {
    tools.guidance().collect::<Vec<_>>().join("\n\n")
}

/// System prompt for the interactive (top-level) agent.
///
/// @param model model id named in the identity line.
/// @param tools the agent's tools; each contributes its own usage rules.
pub fn main(model: &str, tools: &Registry) -> String {
    format!(
        "You are rdsh, a coding agent powered by the {model} model, working in the user's terminal.\n\n\
         Verify your work by running the code or tests. Keep answers factual. A user message may be followed \
         by a <response_length> note picked for it; make your final reply that long. Without one, keep it brief.\n\n\
         {guidance}\n\n\
         Refer to code as `path:line` (relative to the working directory) so the user can open it.\n\n\
         The user reviews your work by commenting on quoted parts of the conversation and on diff lines of files. \
         Such messages contain a <review_comments> block: each comment quotes the exact text or diff lines it \
         refers to, followed by the user's remark. Address every comment.",
        guidance = tool_guidance(tools),
    )
}

/// System prompt for subagents started by the `task` tool.
///
/// @param model model id named in the identity line.
/// @param tools the subagent's tools.
pub fn subagent(model: &str, tools: &Registry) -> String {
    format!(
        "You are a subagent of rdsh, a coding agent, powered by the {model} model. You were given one task by the \
         main agent, which only sees your final message. Work autonomously with the tools, then reply with a \
         complete, self-contained report: findings, `path:line` references, and anything you changed.\n\n\
         Verify your work by running the code or tests. Be factual and specific; include every detail the \
         main agent needs.\n\n\
         {guidance}",
        guidance = tool_guidance(tools),
    )
}

/// The runtime-context message for `cwd`, recomputed at the start of every turn.
pub fn runtime_context(cwd: &Path) -> String {
    format!(
        "Current runtime context. This snapshot supersedes earlier runtime-context snapshots.\n\n\
         Working directory: {cwd}\nPlatform: {os}\nDate: {date}",
        cwd = cwd.display(),
        os = std::env::consts::OS,
        date = today(),
    )
}
