//! Which backend serves which model. A session can switch models per turn (`:model`), so the
//! provider handed to agents routes each request by model id: OpenAI ids (`gpt-…`, `o3`,
//! `codex-…`, or anything in `openai_models`) go to OpenAI, the rest to the DeepSeek endpoint,
//! which also serves other OpenAI-compatible APIs through `DEEPSEEK_BASE_URL`.

use std::sync::Arc;

use anyhow::{Result, bail};

use crate::{
    config::Config,
    deepseek::DeepSeek,
    llm::{BoxFuture, Completion, DeltaSink, Provider, Request},
    openai::OpenAi,
};

pub const DEEPSEEK_DEFAULT_MODEL: &str = "deepseek-flash";
pub const DEEPSEEK_MODELS: [&str; 2] = ["deepseek-flash", "deepseek-v4-pro"];
pub const OPENAI_DEFAULT_MODEL: &str = "gpt-6.2-sol";
/// Models a ChatGPT login can use through Codex (and an API key through the API).
pub const OPENAI_MODELS: [&str; 10] = [
    "gpt-6.2-sol", "gpt-6.2-astra", "gpt-6.1-sol", "gpt-6.1-terra", "gpt-6-sol", "gpt-6-astra", "gpt-6-luna",
    "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna",
];

/// Whether `model` names an OpenAI model.
pub fn is_openai(model: &str) -> bool {
    model.starts_with("gpt-")
        || model.starts_with("codex-")
        || (model.starts_with('o') && model[1..].starts_with(|c: char| c.is_ascii_digit()))
}

pub struct Router {
    model: String,
    deepseek: Option<DeepSeek>,
    openai: OpenAi,
    /// Extra ids routed to OpenAI (`openai_models` in the config).
    openai_models: Vec<String>,
}

impl Router {
    fn pick(&self, model: &str) -> Result<&dyn Provider> {
        if is_openai(model) || self.openai_models.iter().any(|m| m == model) {
            return Ok(&self.openai);
        }
        match &self.deepseek {
            Some(d) => Ok(d),
            None => bail!("{model} needs a DeepSeek key: run `rdsh login deepseek` (or set DEEPSEEK_API_KEY)"),
        }
    }
}

impl Provider for Router {
    fn model(&self) -> &str {
        &self.model
    }

    fn complete<'a>(&'a self, req: Request<'a>, on_delta: DeltaSink<'a>) -> BoxFuture<'a, Result<Completion>> {
        let model = req.model.unwrap_or(&self.model);
        match self.pick(model) {
            Ok(p) => p.complete(Request { model: Some(model), ..req }, on_delta),
            Err(e) => Box::pin(async move { Err(e) }),
        }
    }
}

/// Builds the provider for `cfg`; fails up front when nothing can serve the default model.
pub fn build(cfg: &Config) -> Result<Arc<dyn Provider>> {
    let deepseek = cfg.api_key.is_some().then(|| DeepSeek::new(cfg)).transpose()?;
    let router = Router {
        model: cfg.model.clone(),
        deepseek,
        openai: OpenAi::new(cfg, cfg.model.clone())?,
        openai_models: cfg.openai_models.clone(),
    };
    let openai = is_openai(&cfg.model) || cfg.openai_models.contains(&cfg.model);
    if openai && cfg.openai.is_none() {
        bail!("{} needs an OpenAI login: run `rdsh login chatgpt` (subscription) or `rdsh login openai` (API key)", cfg.model);
    }
    if !openai && router.deepseek.is_none() {
        bail!("no API key: run `rdsh login` (or set DEEPSEEK_API_KEY)");
    }
    Ok(Arc::new(router))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn openai_ids_are_recognized() {
        for m in ["gpt-6.2-sol", "o3", "o4-mini", "codex-mini"] {
            assert!(is_openai(m), "{m}");
        }
        for m in ["deepseek-flash", "qwen3", "openrouter/x", "o"] {
            assert!(!is_openai(m), "{m}");
        }
    }

    #[test]
    fn build_needs_a_login_for_the_default_model() {
        let cfg = Config { model: "gpt-6.2-sol".into(), ..Config::default() };
        assert!(build(&cfg).err().unwrap().to_string().contains("rdsh login chatgpt"));
        let cfg = Config { api_key: Some("k".into()), ..Config::default() };
        assert!(build(&cfg).is_ok());
    }
}
