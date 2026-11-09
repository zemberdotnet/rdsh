//! Short session titles: after a session's first message, the model writes a 2-5 word title
//! that the session rail shows instead of the whole first sentence.
//!
//! Titles live in a `<id>.title` file next to the trace so listing stays a cheap read. They are
//! never sent to the agent's model, so they are not part of the model-visible trace.

use std::{path::Path, sync::Arc};

use anyhow::{Result, bail};

use crate::llm::{Delta, Effort, Message, Provider, Request};

const MAX_WORDS: usize = 6;
const MAX_CHARS: usize = 48;
const MAX_INPUT_CHARS: usize = 2000;

const INSTRUCTIONS: &str = "You name coding sessions. Reply with a 2 to 5 word title for the task in the user's \
     message: lowercase, no quotes, no trailing punctuation, nothing else. Examples: fix markdown rendering; \
     add session sidebar; explain config loading.";

/// Writes titles with the main model.
pub struct Titler {
    provider: Arc<dyn Provider>,
}

impl Titler {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }

    /// Asks the model for a title for a session that starts with `first_message`.
    ///
    /// @returns the cleaned title; errors when the model fails or answers with nothing usable.
    pub async fn title(&self, first_message: &str) -> Result<String> {
        let input: String = first_message.chars().take(MAX_INPUT_CHARS).collect();
        let messages = [Message::System { content: INSTRUCTIONS.into() }, Message::user(input)];
        let ignore = |_: Delta| {};
        let done = self.provider.complete(Request { messages: &messages, tools: &[], effort: Effort::Off, model: None }, &ignore).await?;
        let Message::Assistant { content, .. } = done.message else { bail!("no title in the reply") };
        let t = clean(&content);
        if t.is_empty() {
            bail!("empty title");
        }
        Ok(t)
    }
}

/// First line, without quotes, markup or trailing punctuation, capped in words and characters.
pub fn clean(raw: &str) -> String {
    let line = raw.lines().map(str::trim).find(|l| !l.is_empty()).unwrap_or("");
    let line = line.strip_prefix("Title:").or_else(|| line.strip_prefix("title:")).unwrap_or(line);
    let line: String = line.chars().filter(|c| !matches!(c, '"' | '`' | '*' | '#')).collect();
    let words: Vec<&str> = line.split_whitespace().take(MAX_WORDS).collect();
    let mut out = String::new();
    for w in words {
        if out.chars().count() + w.chars().count() + 1 > MAX_CHARS {
            break;
        }
        if !out.is_empty() {
            out.push(' ');
        }
        out.push_str(w);
    }
    out.trim_end_matches(['.', '!', '?', ',', ':', ';', '\'']).to_owned()
}

/// The title file for the trace at `trace_path`.
pub fn path_for(trace_path: &Path) -> std::path::PathBuf {
    trace_path.with_extension("title")
}

/// Reads a session's title, if one was written.
pub fn read(trace_path: &Path) -> Option<String> {
    let t = std::fs::read_to_string(path_for(trace_path)).ok()?;
    let t = t.trim();
    (!t.is_empty()).then(|| t.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::ScriptedProvider;

    #[test]
    fn cleaning() {
        assert_eq!(clean("\n\"Fix the Markdown renderer!\"\nmore"), "Fix the Markdown renderer");
        assert_eq!(clean("Title: **add session sidebar**."), "add session sidebar");
        assert_eq!(clean("one two three four five six seven eight"), "one two three four five six");
        assert!(clean("a ".repeat(40).as_str()).chars().count() <= MAX_CHARS);
        assert_eq!(clean("   "), "");
    }

    #[tokio::test]
    async fn asks_the_model_without_tools() {
        let provider = Arc::new(ScriptedProvider::new([Message::assistant("explain config loading.", vec![])]));
        let t = Titler::new(provider.clone()).title("how does config get loaded?").await.unwrap();
        assert_eq!(t, "explain config loading");
        let req = &provider.requests()[0];
        assert!(matches!(&req[0], Message::System { content } if content.contains("2 to 5 word title")));
        assert!(matches!(&req[1], Message::User { content } if content == "how does config get loaded?"));
    }
}
