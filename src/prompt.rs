//! System prompts and the runtime-context message.
//!
//! The system prompt holds only stable text (identity, general rules, per-tool guidance), so it
//! stays a cacheable prefix. Process-specific facts (working directory, platform, date) go in a
//! separate runtime-context message that the agent appends when they change.

use std::path::Path;

use crate::{tools::Registry, util::today};

const WORKTREE_GUIDANCE: &str = "When making changes in a Git repository, prefer a dedicated Git worktree \
    unless instructed otherwise. Reuse a worktree already set aside for the task.";

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
         {WORKTREE_GUIDANCE}\n\n\
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
         {WORKTREE_GUIDANCE}\n\n\
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

const DIAGRAMS_ON: &str = "<diagrams>\n\
The user is reading this session in rdsh's browser view, which draws fenced ```d3 code blocks as \
interactive diagrams. Draw one when a picture explains better than text: architecture and data flow, \
dependency or call graphs, state machines, timelines, sizes and comparisons. Don't draw what a short \
list says as well, and keep explaining in prose around it.\n\n\
The block is the body of a function run in a sandboxed frame with no network access, so use only data \
you already have. In scope: `d3` (v7), `svg` (sized to the frame), `g` (a group with pan and zoom: draw \
into it), `W` and `H` (frame size in pixels), `C` (colors for a dark background: bg fg fgdim gray panel \
sel red green yellow blue purple aqua orange), `CH` (pixel width of one character of the 12px monospace \
font, for sizing labels), and `pick(label)`, which puts `label` into the user's prompt as context: call \
it from click handlers on the meaningful items. Begin with `// title: <caption>`; `// rows: <n>` sets \
the height in text rows (default 18, at most 60).\n\
</diagrams>";

const DIAGRAMS_OFF: &str = "<diagrams>The user is now in a terminal, which shows ```d3 blocks as plain \
code: don't draw diagrams.</diagrams>";

/// The note telling the model whether its ```d3 blocks are drawn, when that differs from what the
/// history last said (so it is sent once per change, not every turn).
///
/// @param drawn whether this turn comes from a client that draws diagrams.
pub fn diagrams_note(history: &[crate::llm::Message], drawn: bool) -> Option<String> {
    let last = history.iter().rev().find_map(|m| match m {
        crate::llm::Message::Directive { content } if content.starts_with("<diagrams>") => Some(content.as_str()),
        _ => None,
    });
    match (last, drawn) {
        (None, false) => None,
        (Some(l), true) if l == DIAGRAMS_ON => None,
        (Some(l), false) if l == DIAGRAMS_OFF => None,
        (_, true) => Some(DIAGRAMS_ON.to_owned()),
        (_, false) => Some(DIAGRAMS_OFF.to_owned()),
    }
}

#[cfg(test)]
mod diagram_tests {
    use super::*;
    use crate::llm::Message;

    #[test]
    fn the_diagram_note_is_sent_once_per_change() {
        let mut h = vec![Message::user("hi")];
        assert_eq!(diagrams_note(&h, false), None, "terminal sessions never hear about diagrams");
        let on = diagrams_note(&h, true).unwrap();
        h.push(Message::Directive { content: on });
        assert_eq!(diagrams_note(&h, true), None);
        let off = diagrams_note(&h, false).unwrap();
        assert!(off.contains("don't draw"));
        h.push(Message::Directive { content: off });
        assert_eq!(diagrams_note(&h, false), None);
        assert!(diagrams_note(&h, true).is_some());
    }
}
