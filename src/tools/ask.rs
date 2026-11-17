//! `ask`: the agent puts one or more forms in front of the user and waits for the answers.
//!
//! The tool emits `AgentEvent::Question` with the forms; every client watching the session shows
//! them. The first `Request::Answer` for the call wins: the hub hands it to [`Questions`], the
//! tool wakes, emits `AgentEvent::Answered` (so the other clients close the form) and returns the
//! values to the model. A client that cannot draw forms (the TUI) shows the questions as text and
//! sends the user's next message as a plain-text answer.

use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tokio::sync::oneshot;

use super::{Tool, ToolCtx, ToolOutput};
use crate::{
    agent::AgentEvent,
    llm::{BoxFuture, ToolSpec},
};

/// What the agent asks: one or more forms, shown one after another.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Ask {
    /// Short heading for the whole questionnaire.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub forms: Vec<Form>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Form {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Context shown above the fields.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub fields: Vec<Field>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Field {
    /// Key of this field's value in the answer.
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub help: Option<String>,
    #[serde(default = "yes")]
    pub required: bool,
    #[serde(flatten)]
    pub kind: Kind,
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Kind {
    /// Free text; `multiline` for a paragraph (a PR description) rather than a line (a title).
    Text {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        placeholder: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<String>,
        #[serde(default)]
        multiline: bool,
    },
    /// Pick one option (or several with `multiple`); `other` adds a free-text option.
    Choice {
        options: Vec<Choice>,
        #[serde(default)]
        multiple: bool,
        #[serde(default)]
        other: bool,
        /// Labels selected at first.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        default: Vec<String>,
    },
    /// Yes or no.
    Confirm {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<bool>,
    },
    Number {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        min: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        max: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        default: Option<f64>,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    /// Shown, and returned as the value.
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// The user's response to an `ask` call.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Answer {
    /// Field values by id: strings, numbers, booleans, or string lists for multiple choice.
    /// Optional fields left empty are absent.
    Form { values: Map<String, Value> },
    /// The user replied in their own words instead of filling in the form.
    Text { text: String },
    /// The user closed the form without answering.
    Dismissed,
}

const MAX_FORMS: usize = 10;
const MAX_FIELDS: usize = 30;

impl Ask {
    /// Checks what a model wrote; the error is for the model to fix its call.
    pub fn parse(args: Value) -> Result<Self, String> {
        let ask: Ask = serde_json::from_value(args).map_err(|e| format!("invalid ask arguments: {e}"))?;
        if ask.forms.is_empty() || ask.forms.len() > MAX_FORMS {
            return Err(format!("give 1 to {MAX_FORMS} forms"));
        }
        let mut ids = HashSet::new();
        let mut count = 0;
        for f in &ask.forms {
            if f.fields.is_empty() {
                return Err("every form needs at least one field".into());
            }
            for field in &f.fields {
                count += 1;
                if field.id.trim().is_empty() || field.label.trim().is_empty() {
                    return Err("every field needs an id and a label".into());
                }
                if !ids.insert(field.id.as_str()) {
                    return Err(format!("field id `{}` is used twice; ids must be unique across all forms", field.id));
                }
                if let Kind::Choice { options, .. } = &field.kind
                    && options.is_empty()
                {
                    return Err(format!("choice field `{}` has no options", field.id));
                }
            }
        }
        if count > MAX_FIELDS {
            return Err(format!("at most {MAX_FIELDS} fields in all; ask about the rest later"));
        }
        Ok(ask)
    }

    /// The questions as plain text, for clients that cannot draw forms and for traces.
    pub fn as_text(&self) -> String {
        let mut out = String::new();
        if let Some(t) = &self.title {
            out.push_str(t);
            out.push('\n');
        }
        for f in &self.forms {
            if let Some(t) = &f.title {
                out.push_str(&format!("\n{t}\n"));
            }
            if let Some(d) = &f.description {
                out.push_str(&format!("{d}\n"));
            }
            for field in &f.fields {
                let opt = if field.required { "" } else { " (optional)" };
                let hint = match &field.kind {
                    Kind::Choice { options, multiple, other, .. } => {
                        let mut labels: Vec<&str> = options.iter().map(|o| o.label.as_str()).collect();
                        if *other {
                            labels.push("other…");
                        }
                        format!(" [{}{}]", if *multiple { "any of: " } else { "" }, labels.join(" / "))
                    }
                    Kind::Confirm { .. } => " [yes / no]".into(),
                    _ => String::new(),
                };
                out.push_str(&format!("- {}{opt}{hint}\n", field.label));
            }
        }
        out.trim().to_owned()
    }
}

impl Answer {
    /// What the model reads as the tool result.
    pub fn for_model(&self, ask: &Ask) -> String {
        match self {
            Answer::Form { values } => {
                let mut out = String::from("The user filled in the form (values by field id):\n");
                for field in ask.forms.iter().flat_map(|f| &f.fields) {
                    match values.get(&field.id) {
                        Some(v) => out.push_str(&format!("{}: {v}\n", field.id)),
                        None => out.push_str(&format!("{}: (left empty)\n", field.id)),
                    }
                }
                out
            }
            Answer::Text { text } => format!("The user answered in their own words instead of the form:\n\n{text}"),
            Answer::Dismissed => "The user closed the form without answering. Don't ask the same thing again right away: \
                                  go on with your best judgment, or ask in plain text if you truly cannot."
                .into(),
        }
    }
}

/// Questions waiting for an answer in one session, by tool call id.
#[derive(Default)]
pub struct Questions {
    pending: Mutex<HashMap<String, oneshot::Sender<Answer>>>,
}

impl Questions {
    fn wait(&self, id: &str) -> oneshot::Receiver<Answer> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).insert(id.to_owned(), tx);
        rx
    }

    fn forget(&self, id: &str) {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
    }

    /// Delivers the answer to question `id`.
    ///
    /// @returns false when no such question is waiting (answered already, or interrupted).
    pub fn answer(&self, id: &str, answer: Answer) -> bool {
        let tx = self.pending.lock().unwrap_or_else(|e| e.into_inner()).remove(id);
        tx.is_some_and(|tx| tx.send(answer).is_ok())
    }

    pub fn waiting(&self) -> bool {
        !self.pending.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }
}

/// Removes the pending question when the call ends any way (answered, interrupted, dropped).
struct Forget<'a>(&'a Questions, &'a str);

impl Drop for Forget<'_> {
    fn drop(&mut self) {
        self.0.forget(self.1);
    }
}

pub struct AskTool;

const DESCRIPTION: &str = "Ask the user one or more questions in a form and wait for the answers. Use it when you \
need something only the user can decide or provide (a PR title, which of several approaches, names, values), \
especially several related things at once, instead of asking in prose and ending your turn. Group related questions \
in one form; use several forms (shown one after another) for distinct topics. Prefer `choice` when the plausible \
answers are known (put your recommended option first and say so in its description; set `other` to allow \
something else), `confirm` for yes/no, and `text` with a `default` you propose when the user may just accept it. \
Don't ask what you can find out yourself. The result has the values by field id, or the user's reply in their \
own words, or that they closed the form.";

impl Tool for AskTool {
    fn spec(&self) -> ToolSpec {
        let field = json!({
            "type": "object",
            "properties": {
                "id": {"type": "string", "description": "key of this value in the answer, unique across all forms"},
                "label": {"type": "string", "description": "the question, short"},
                "help": {"type": "string", "description": "one line of context under the question"},
                "required": {"type": "boolean", "description": "default true"},
                "type": {"type": "string", "enum": ["text", "choice", "confirm", "number"]},
                "multiline": {"type": "boolean", "description": "text: a paragraph rather than one line"},
                "placeholder": {"type": "string", "description": "text: grey hint in the empty box"},
                "options": {
                    "type": "array",
                    "description": "choice: the options; the label is also the value returned",
                    "items": {
                        "type": "object",
                        "properties": {"label": {"type": "string"}, "description": {"type": "string"}},
                        "required": ["label"]
                    }
                },
                "multiple": {"type": "boolean", "description": "choice: allow several (the answer is a list)"},
                "other": {"type": "boolean", "description": "choice: add a free-text 'other' option"},
                "min": {"type": "number"},
                "max": {"type": "number"},
                "default": {"description": "proposed answer: a string (text), a list of labels (choice), a boolean (confirm) or a number"}
            },
            "required": ["id", "label", "type"]
        });
        ToolSpec {
            name: "ask".into(),
            description: DESCRIPTION.into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "title": {"type": "string", "description": "short heading for the whole questionnaire"},
                    "forms": {
                        "type": "array",
                        "description": "one or more forms, shown one after another",
                        "items": {
                            "type": "object",
                            "properties": {
                                "title": {"type": "string"},
                                "description": {"type": "string", "description": "context shown above the fields"},
                                "fields": {"type": "array", "items": field}
                            },
                            "required": ["fields"]
                        }
                    }
                },
                "required": ["forms"]
            }),
        }
    }

    fn run<'a>(&'a self, args: Value, ctx: &'a ToolCtx) -> BoxFuture<'a, ToolOutput> {
        Box::pin(async move {
            let Some(questions) = ctx.questions.clone() else {
                return ToolOutput::err("no one can answer forms in this session; decide yourself or ask in your reply");
            };
            let ask = match Ask::parse(args) {
                Ok(a) => a,
                Err(e) => return ToolOutput::err(e),
            };
            let rx = questions.wait(&ctx.call_id);
            let _forget = Forget(&questions, &ctx.call_id);
            (ctx.sink)(AgentEvent::Question { call_id: ctx.call_id.clone(), ask: ask.clone() });
            // The agent races this against the user's interrupt.
            let answer = match rx.await {
                Ok(a) => a,
                Err(_) => return ToolOutput::err("the question was withdrawn"),
            };
            (ctx.sink)(AgentEvent::Answered { call_id: ctx.call_id.clone(), answer: answer.clone() });
            ToolOutput::ok(answer.for_model(&ask))
        })
    }
}

/// Shared handle the agent passes to its tools.
pub type SharedQuestions = Arc<Questions>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_forms_and_rejects_bad_ones() {
        let ok = json!({"title": "PR", "forms": [
            {"title": "Basics", "fields": [
                {"id": "title", "label": "PR title", "type": "text", "default": "Fix rail counts"},
                {"id": "body", "label": "Description", "type": "text", "multiline": true, "required": false}
            ]},
            {"fields": [
                {"id": "reviewers", "label": "Reviewers", "type": "choice", "multiple": true, "other": true,
                 "options": [{"label": "alice"}, {"label": "bob", "description": "owns the TUI"}]},
                {"id": "draft", "label": "Open as draft?", "type": "confirm", "default": false}
            ]}
        ]});
        let ask = Ask::parse(ok).unwrap();
        assert_eq!(ask.forms.len(), 2);
        assert!(!ask.forms[0].fields[1].required);
        assert!(ask.as_text().contains("- Reviewers [any of: alice / bob / other…]"));
        let dup = json!({"forms": [{"fields": [{"id": "a", "label": "A", "type": "confirm"}, {"id": "a", "label": "B", "type": "confirm"}]}]});
        assert!(Ask::parse(dup).unwrap_err().contains("twice"));
        assert!(Ask::parse(json!({"forms": []})).is_err());
        let no_opts = json!({"forms": [{"fields": [{"id": "c", "label": "C", "type": "choice", "options": []}]}]});
        assert!(Ask::parse(no_opts).unwrap_err().contains("no options"));
        assert!(Ask::parse(json!({"forms": [{"fields": [{"id": "x", "label": "X", "type": "slider"}]}]})).is_err());
    }

    #[test]
    fn answers_read_well_for_the_model() {
        let ask = Ask::parse(json!({"forms": [{"fields": [
            {"id": "title", "label": "T", "type": "text"},
            {"id": "body", "label": "B", "type": "text", "required": false},
            {"id": "draft", "label": "D", "type": "confirm"}
        ]}]}))
        .unwrap();
        let mut values = Map::new();
        values.insert("title".into(), json!("Fix rail"));
        values.insert("draft".into(), json!(true));
        let text = Answer::Form { values }.for_model(&ask);
        assert!(text.contains("title: \"Fix rail\"\nbody: (left empty)\ndraft: true"), "{text}");
        assert!(Answer::Dismissed.for_model(&ask).contains("closed the form"));
    }

    #[test]
    fn the_first_answer_wins_and_late_ones_are_refused() {
        let q = Questions::default();
        let mut rx = q.wait("c1");
        assert!(q.waiting());
        assert!(q.answer("c1", Answer::Dismissed));
        assert!(!q.answer("c1", Answer::Dismissed), "already answered");
        assert_eq!(rx.try_recv().unwrap(), Answer::Dismissed);
        assert!(!q.answer("nope", Answer::Dismissed));
    }
}
