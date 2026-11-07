//! Response-length routing: before each user turn, the decision model picks how long the reply
//! should be, and the agent adds a note saying so after the user's message.
//!
//! Only the top-level agent routes; subagents report to the main agent at whatever length the
//! task needs. With no answer (no key, timeout, error, low confidence) no note is added.

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use crate::{
    decision::{Decider, Pick},
    llm::Message,
};

/// Below this probability for the picked level, no note is added.
pub const MIN_CONFIDENCE: f64 = 0.5;

const QUESTION: &str = "How long should the assistant's reply to the latest user message be? Judge from what \
     the user asks and how they have been asking in this conversation. When unsure, choose the shorter level.";

/// `(label, description for the decision model, note for the agent)`, shortest first.
const LEVELS: [(&str, &str, &str); 4] = [
    (
        "yesno",
        "A yes/no question or a request for one fact: does X exist, is Y enabled, what is the value of Z.",
        "Answer in one line: yes or no, or the single fact asked for, with at most one short sentence of reason.",
    ),
    (
        "brief",
        "A simple question, or a small clear task whose outcome needs only a short report.",
        "Reply in a few sentences. No headings, no preamble, no recap.",
    ),
    (
        "standard",
        "An explanation, or a change touching several places, whose answer needs some structure.",
        "Say what is needed to understand or act on it; short lists or a small code block are fine, but no padding.",
    ),
    (
        "detailed",
        "Planning, design, comparing approaches, or an open-ended question the user wants worked through at length.",
        "Be thorough: lay out the options, trade-offs, steps and risks, with headings where they help.",
    ),
];

/// Earlier user messages shown to the decision model, besides the latest.
const PRIOR_PROMPTS: usize = 3;
const PRIOR_PROMPT_CHARS: usize = 400;
const REPLY_CHARS: usize = 500;
const PROMPT_CHARS: usize = 4000;

/// What the router decided for one turn.
#[derive(Clone, Debug, PartialEq)]
pub enum Routing {
    /// A note was added for `level`.
    Picked { level: &'static str, confidence: f64 },
    /// The model answered below `MIN_CONFIDENCE`; no note.
    Unsure { level: String, confidence: f64 },
    /// No answer; no note.
    Unavailable { reason: String },
}

pub struct LengthRouter {
    decider: Arc<dyn Decider>,
    warned: AtomicBool,
}

impl LengthRouter {
    pub fn new(decider: Arc<dyn Decider>) -> Self {
        Self { decider, warned: AtomicBool::new(false) }
    }

    /// Decides the reply length for `prompt`, given the conversation before it.
    ///
    /// @param history messages before the new user message.
    /// @param prompt the new user message.
    /// @returns the decision and, when a level was picked, the note to send after the prompt.
    pub async fn route(&self, history: &[Message], prompt: &str) -> (Routing, Option<String>) {
        let state = state(history, prompt);
        let options: Vec<(&str, &str)> = LEVELS.iter().map(|(l, d, _)| (*l, *d)).collect();
        match self.decider.choose(&state, QUESTION, &options).await {
            Err(e) => (Routing::Unavailable { reason: format!("{e:#}") }, None),
            Ok(Pick { label, confidence }) => match LEVELS.iter().find(|(l, ..)| *l == label) {
                Some((level, _, note)) if confidence >= MIN_CONFIDENCE => {
                    (Routing::Picked { level, confidence }, Some(directive(level, note)))
                }
                _ => (Routing::Unsure { level: label, confidence }, None),
            },
        }
    }

    /// True the first time it is called: failures are reported once per session, not every turn.
    pub fn first_failure(&self) -> bool {
        !self.warned.swap(true, Ordering::Relaxed)
    }
}

fn directive(level: &str, note: &str) -> String {
    format!("<response_length level=\"{level}\">{note} This applies to your final reply to the message above.</response_length>")
}

/// The text the decision model judges: the last few user prompts, the start of the last
/// assistant reply, and the new prompt.
fn state(history: &[Message], prompt: &str) -> String {
    let mut prior: Vec<&str> = history
        .iter()
        .rev()
        .filter_map(|m| match m {
            Message::User { content } => Some(content.as_str()),
            _ => None,
        })
        .take(PRIOR_PROMPTS)
        .collect();
    prior.reverse();
    let reply = history.iter().rev().find_map(|m| match m {
        Message::Assistant { content, .. } if !content.trim().is_empty() => Some(content.as_str()),
        _ => None,
    });
    let mut out = String::new();
    if !prior.is_empty() {
        out.push_str("Earlier user messages:\n");
        for p in prior {
            out.push_str(&format!("- {}\n", clip(p, PRIOR_PROMPT_CHARS)));
        }
        out.push('\n');
    }
    if let Some(r) = reply {
        out.push_str(&format!("Start of the assistant's last reply:\n{}\n\n", clip(r, REPLY_CHARS)));
    }
    out.push_str(&format!("Latest user message:\n{}", clip(prompt, PROMPT_CHARS)));
    out
}

fn clip(s: &str, max: usize) -> String {
    let s = s.trim();
    match s.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &s[..i]),
        None => s.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decision::ScriptedDecider;

    fn pick(label: &str, confidence: f64) -> Result<Pick, String> {
        Ok(Pick { label: label.into(), confidence })
    }

    #[tokio::test]
    async fn confident_pick_yields_a_note() {
        let r = LengthRouter::new(Arc::new(ScriptedDecider::new([pick("yesno", 0.9)])));
        let (routing, note) = r.route(&[], "is the cache on?").await;
        assert_eq!(routing, Routing::Picked { level: "yesno", confidence: 0.9 });
        assert!(note.unwrap().starts_with("<response_length level=\"yesno\">Answer in one line"));
    }

    #[tokio::test]
    async fn low_confidence_and_failures_add_nothing() {
        let r = LengthRouter::new(Arc::new(ScriptedDecider::new([pick("detailed", 0.3), Err("HTTP 503".into())])));
        assert!(matches!(r.route(&[], "x").await, (Routing::Unsure { .. }, None)));
        assert!(matches!(r.route(&[], "x").await, (Routing::Unavailable { reason }, None) if reason.contains("503")));
        assert!(r.first_failure());
        assert!(!r.first_failure());
    }

    #[test]
    fn state_carries_recent_prompts_and_last_reply() {
        let mut history = vec![Message::System { content: "sys".into() }];
        for i in 0..5 {
            history.push(Message::user(format!("prompt {i}")));
            history.push(Message::assistant(format!("reply {i}"), vec![]));
        }
        let s = state(&history, "and now?");
        assert!(!s.contains("prompt 1") && s.contains("prompt 2") && s.contains("prompt 4"));
        assert!(s.contains("last reply:\nreply 4") && !s.contains("reply 3"));
        assert!(s.ends_with("Latest user message:\nand now?"));
        assert_eq!(clip("abcdef", 3), "abc…");
    }
}
