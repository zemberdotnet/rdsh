//! Typed decisions from a System One endpoint (TypeSafe's Jev, hosted on OpenRouter or TypeSafe).
//!
//! A decision is a choice question about a piece of state, answered with a probability per option
//! and no text to parse. Consumers treat every failure as "no answer" and carry on undecided.

use std::{sync::Mutex, time::Duration};

use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;
use serde_json::json;

use crate::{config::Config, llm::BoxFuture};

pub const OPENROUTER_URL: &str = "https://openrouter.ai/api/alpha/decisions";
pub const OPENROUTER_MODEL: &str = "~typesafe/jev-latest";
pub const TYPESAFE_URL: &str = "https://api.typesafe.ai/v1/systemone";
pub const TYPESAFE_MODEL: &str = "jev-latest";

/// The picked option of a choice question and the probability the model gave it.
#[derive(Clone, Debug, PartialEq)]
pub struct Pick {
    pub label: String,
    pub confidence: f64,
}

/// Answers choice questions. Dropping the returned future cancels the request.
pub trait Decider: Send + Sync {
    /// @param state the text the question is about.
    /// @param instructions the question.
    /// @param options `(label, description)` pairs; the answer is one of the labels.
    fn choose<'a>(
        &'a self,
        state: &'a str,
        instructions: &'a str,
        options: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Pick>>;
}

/// Client for the System One wire format (`POST {state, questions}` → `{answers}`).
pub struct SystemOne {
    client: reqwest::Client,
    endpoint: String,
    api_key: Option<String>,
    model: String,
}

impl SystemOne {
    /// Builds the decision client from configuration, or `None` when no decision key is set
    /// (decisions are then off). A localhost `decision_url` needs no key.
    pub fn from_config(cfg: &Config) -> Result<Option<Self>> {
        let (default_url, default_model) = match cfg.decision_key_source {
            Some(KeySource::TypeSafe) => (TYPESAFE_URL, TYPESAFE_MODEL),
            _ => (OPENROUTER_URL, OPENROUTER_MODEL),
        };
        let endpoint = cfg.decision_url.clone().unwrap_or_else(|| default_url.into());
        let local = reqwest::Url::parse(&endpoint)
            .with_context(|| format!("config: invalid decision_url {endpoint:?}"))?
            .host_str()
            .is_some_and(|h| matches!(h, "localhost" | "127.0.0.1" | "[::1]"));
        if cfg.decision_api_key.is_none() && !local {
            return Ok(None);
        }
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_millis(cfg.decision_timeout_ms))
            .build()
            .context("building decision HTTP client")?;
        Ok(Some(Self {
            client,
            endpoint,
            api_key: cfg.decision_api_key.clone(),
            model: cfg.decision_model.clone().unwrap_or_else(|| default_model.into()),
        }))
    }
}

/// Which environment variable supplied the decision key; it picks the default endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeySource {
    OpenRouter,
    TypeSafe,
}

#[derive(Deserialize)]
struct WireResponse {
    #[serde(default)]
    answers: std::collections::HashMap<String, WireAnswer>,
}

#[derive(Deserialize)]
struct WireAnswer {
    #[serde(default)]
    choice: String,
    #[serde(default)]
    probabilities: std::collections::HashMap<String, f64>,
    #[serde(default)]
    confidence: Option<f64>,
}

impl Decider for SystemOne {
    fn choose<'a>(
        &'a self,
        state: &'a str,
        instructions: &'a str,
        options: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Pick>> {
        Box::pin(async move {
            let criteria: serde_json::Map<_, _> = options.iter().map(|(l, d)| ((*l).to_owned(), json!(d))).collect();
            let body = json!({
                "model": self.model,
                "state": state,
                "questions": {"q": {"type": "choice", "instructions": instructions, "criteria": criteria}},
            });
            let mut req = self.client.post(&self.endpoint).json(&body);
            if let Some(k) = &self.api_key {
                req = req.bearer_auth(k);
            }
            let resp = req.send().await.map_err(|e| anyhow!("decision request failed: {e}"))?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if !status.is_success() {
                let mut detail = text.trim().to_owned();
                if detail.len() > 200 {
                    detail.truncate(detail.floor_char_boundary(200));
                    detail.push('…');
                }
                bail!("decision API error {status}: {detail}");
            }
            let wire: WireResponse =
                serde_json::from_str(&text).map_err(|e| anyhow!("bad decision response ({e}): {text:.200}"))?;
            let a = wire.answers.get("q").context("decision response has no answer")?;
            if !options.iter().any(|(l, _)| *l == a.choice) {
                bail!("decision picked unknown option {:?}", a.choice);
            }
            let confidence = a.confidence.or_else(|| a.probabilities.get(&a.choice).copied()).unwrap_or(0.0);
            Ok(Pick { label: a.choice.clone(), confidence })
        })
    }
}

/// A deterministic decider for tests: replays queued answers (`Err` text for a failure) and
/// records every state it was asked about.
#[derive(Default)]
pub struct ScriptedDecider {
    answers: Mutex<std::collections::VecDeque<Result<Pick, String>>>,
    states: Mutex<Vec<String>>,
}

impl ScriptedDecider {
    pub fn new(answers: impl IntoIterator<Item = Result<Pick, String>>) -> Self {
        Self { answers: Mutex::new(answers.into_iter().collect()), ..Default::default() }
    }

    /// States of every question asked so far.
    pub fn states(&self) -> Vec<String> {
        self.states.lock().unwrap().clone()
    }
}

impl Decider for ScriptedDecider {
    fn choose<'a>(
        &'a self,
        state: &'a str,
        _instructions: &'a str,
        _options: &'a [(&'a str, &'a str)],
    ) -> BoxFuture<'a, Result<Pick>> {
        Box::pin(async move {
            self.states.lock().unwrap().push(state.to_owned());
            match self.answers.lock().unwrap().pop_front() {
                Some(Ok(p)) => Ok(p),
                Some(Err(e)) => Err(anyhow!(e)),
                None => Err(anyhow!("scripted decider has no answer left")),
            }
        })
    }
}
