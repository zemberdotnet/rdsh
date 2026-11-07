//! Review comments queued by the user and their rendering into the next user message.

/// What a comment is attached to.
#[derive(Clone, Debug, PartialEq)]
pub enum Anchor {
    /// Text selected in the conversation. `source` names where it came from
    /// (for example "assistant response" or "bash output").
    Chat { source: String, quote: String },
    /// Lines of a file as shown in its diff view. `lines` is the inclusive line range
    /// (new-side numbers where available); `snippet` keeps the `+`/`-`/` ` prefixes.
    File { path: String, lines: Option<(usize, usize)>, snippet: String },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Comment {
    pub anchor: Anchor,
    pub body: String,
}

impl Comment {
    /// Short label for lists, e.g. `src/main.rs:10-12` or `assistant response`.
    pub fn label(&self) -> String {
        match &self.anchor {
            Anchor::Chat { source, .. } => source.clone(),
            Anchor::File { path, lines: Some((a, b)), .. } if a == b => format!("{path}:{a}"),
            Anchor::File { path, lines: Some((a, b)), .. } => format!("{path}:{a}-{b}"),
            Anchor::File { path, lines: None, .. } => path.clone(),
        }
    }

    /// The quoted text or snippet.
    pub fn excerpt(&self) -> &str {
        match &self.anchor {
            Anchor::Chat { quote, .. } => quote,
            Anchor::File { snippet, .. } => snippet,
        }
    }
}

/// Builds the user message for a turn: the review block (if any) followed by `text`.
///
/// @param comments queued comments, in the order the user made them.
/// @param text what the user typed in the prompt (may be empty).
/// @returns the exact content sent to the model.
pub fn compose_message(comments: &[Comment], text: &str) -> String {
    let text = text.trim();
    if comments.is_empty() {
        return text.to_owned();
    }
    let mut out = String::from(
        "<review_comments>\nI left comments on specific parts of the conversation and the working tree. \
         Each quotes what it refers to. Address every comment.\n",
    );
    for (i, c) in comments.iter().enumerate() {
        out.push('\n');
        match &c.anchor {
            Anchor::Chat { source, quote } => {
                out.push_str(&format!("[{}] On {source}:\n", i + 1));
                for line in quote.lines() {
                    out.push_str("> ");
                    out.push_str(line);
                    out.push('\n');
                }
            }
            Anchor::File { snippet, .. } => {
                out.push_str(&format!("[{}] On {} (current diff):\n", i + 1, c.label()));
                let fence = fence_for(snippet);
                out.push_str(&format!("{fence}diff\n{snippet}\n{fence}\n"));
            }
        }
        out.push_str("Comment: ");
        out.push_str(c.body.trim());
        out.push('\n');
    }
    out.push_str("</review_comments>");
    if !text.is_empty() {
        out.push_str("\n\n");
        out.push_str(text);
    }
    out
}

/// A backtick fence longer than any backtick run inside `s`.
fn fence_for(s: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in s.chars() {
        if c == '`' {
            run += 1;
            longest = longest.max(run);
        } else {
            run = 0;
        }
    }
    "`".repeat((longest + 1).max(3))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_passes_through() {
        assert_eq!(compose_message(&[], "  hi \n"), "hi");
    }

    #[test]
    fn composes_chat_and_file_comments() {
        let comments = vec![
            Comment {
                anchor: Anchor::Chat { source: "assistant response".into(), quote: "line one\nline two".into() },
                body: "why?".into(),
            },
            Comment {
                anchor: Anchor::File {
                    path: "src/a.rs".into(),
                    lines: Some((3, 4)),
                    snippet: "+let x = 1;\n+```".into(),
                },
                body: "rename x".into(),
            },
        ];
        let m = compose_message(&comments, "then run tests");
        assert!(m.starts_with("<review_comments>"));
        assert!(m.contains("[1] On assistant response:\n> line one\n> line two\nComment: why?"));
        assert!(m.contains("[2] On src/a.rs:3-4 (current diff):\n````diff\n+let x = 1;\n+```\n````\nComment: rename x"));
        assert!(m.ends_with("</review_comments>\n\nthen run tests"));
    }
}
