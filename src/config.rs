//! Runtime configuration: defaults, `~/.config/rdsh/config.json`, then environment.
//!
//! Precedence (later wins): built-in defaults, config file, `.env` in the working
//! directory, process environment. The API key falls back to `rdsh login` credentials.
//! Invalid config files fail loudly at startup.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::{auth::OpenAiAuth, decision::KeySource, llm::Effort};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Default model for new sessions (`RDSH_MODEL` overrides). Unset: the model of the last
    /// `rdsh login`, else one the stored keys can use.
    pub model: String,
    /// Models offered by `:model`; any other id can still be typed after it. Unset: the models
    /// of every provider logged into.
    pub models: Vec<String>,
    /// Thinking effort new turns start with (`off`, `low`, `high`, `max`); Shift-Tab cycles it.
    pub effort: Effort,
    /// OpenAI-compatible base URL (`DEEPSEEK_BASE_URL` overrides).
    pub base_url: String,
    /// OpenAI API base for API-key logins (`OPENAI_BASE_URL` overrides).
    pub openai_base_url: String,
    /// ChatGPT backend for subscription logins; requests go to `{this}/codex/responses`.
    pub chatgpt_base_url: String,
    /// Model ids beyond `gpt-…`, `o3`-style and `codex-…` that go to OpenAI.
    pub openai_models: Vec<String>,
    /// Maximum model requests per user turn in the main agent; 0 is unlimited.
    pub max_steps: usize,
    /// Maximum model requests per subagent task; 0 (the default) is unlimited.
    pub subagent_max_steps: usize,
    /// Tool output is truncated (head and tail kept) beyond this many characters.
    pub max_tool_output_chars: usize,
    /// Seconds a `bash` command runs in the foreground before it moves to the background (in
    /// subagents: before it is killed), unless the model passes its own timeout.
    pub bash_timeout_secs: u64,
    /// Retries for 429/5xx/connect failures before any output streamed.
    pub max_retries: u32,
    /// Abort a model stream when no bytes arrive for this long.
    pub stream_idle_timeout_secs: u64,
    /// Where session traces are written; defaults to `$XDG_DATA_HOME/rdsh/sessions`.
    pub sessions_dir: Option<PathBuf>,
    /// Ask the decision model how long each reply should be (top-level TUI turns only).
    pub length_routing: bool,
    /// Name new sessions with a short model-written title for the session rail.
    pub session_titles: bool,
    /// Remote machines whose hubs the TUI also attaches to (over SSH).
    pub hosts: Vec<HostConfig>,
    /// SSH client used for remote hosts.
    pub ssh_program: String,
    /// Command run on a remote host to reach its hub.
    pub remote_command: Vec<String>,
    /// `rdsh serve` exits after this long with no clients and no running turns; 0 never exits.
    pub serve_idle_secs: u64,
    /// System One decisions endpoint; defaults to OpenRouter's, or TypeSafe's for a TypeSafe key.
    pub decision_url: Option<String>,
    /// Decision model id; defaults to Jev for the chosen endpoint.
    pub decision_model: Option<String>,
    /// A decision that takes longer is skipped.
    pub decision_timeout_ms: u64,
    /// Decision key from `TYPESAFE_API_KEY` or `OPENROUTER_API_KEY`; never read from the config file.
    #[serde(skip)]
    pub decision_api_key: Option<String>,
    #[serde(skip)]
    pub decision_key_source: Option<KeySource>,
    /// API key from `DEEPSEEK_API_KEY`; never read from the config file.
    #[serde(skip)]
    pub api_key: Option<String>,
    /// OpenAI login from `OPENAI_API_KEY` or `rdsh login chatgpt|openai`; never read from the
    /// config file.
    #[serde(skip)]
    pub openai: Option<OpenAiAuth>,
    /// `openai` came from the environment.
    #[serde(skip)]
    pub openai_from_env: bool,
    /// Kagi key from `KAGI_API_KEY`; enables `web_search` and `web_fetch`. Never read from the
    /// config file.
    #[serde(skip)]
    pub kagi_api_key: Option<String>,
    /// Kagi API root from `KAGI_BASE_URL`.
    #[serde(skip)]
    pub kagi_base_url: Option<String>,
}

/// A remote machine in the session rail.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostConfig {
    /// Label in the rail.
    pub name: String,
    /// SSH destination, e.g. `me@build-box`.
    pub ssh: String,
    /// Directory for new sessions there; only sessions from it are listed. Default: all.
    #[serde(default)]
    pub dir: Option<String>,
}

impl HostConfig {
    /// Parses `--host DEST[:DIR]` (`me@box:/srv/app`); the name is the host part of DEST.
    pub fn parse(spec: &str) -> Result<Self> {
        let (dest, dir) = match spec.split_once(':') {
            Some((d, dir)) if !dir.is_empty() => (d, Some(dir.to_owned())),
            _ => (spec.trim_end_matches(':'), None),
        };
        if dest.is_empty() {
            bail!("--host needs an SSH destination, like me@box or me@box:/path");
        }
        let name = dest.rsplit('@').next().unwrap_or(dest).to_owned();
        Ok(Self { name, ssh: dest.to_owned(), dir })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            model: crate::providers::DEEPSEEK_DEFAULT_MODEL.into(),
            models: Vec::new(),
            effort: Effort::High,
            base_url: "https://api.deepseek.com".into(),
            openai_base_url: "https://api.openai.com/v1".into(),
            chatgpt_base_url: "https://chatgpt.com/backend-api".into(),
            openai_models: Vec::new(),
            max_steps: 200,
            subagent_max_steps: 0,
            max_tool_output_chars: 30_000,
            bash_timeout_secs: 120,
            max_retries: 3,
            stream_idle_timeout_secs: 180,
            sessions_dir: None,
            length_routing: true,
            session_titles: true,
            hosts: Vec::new(),
            ssh_program: "ssh".into(),
            remote_command: vec!["rdsh".into(), "connect".into()],
            serve_idle_secs: 1800,
            decision_url: None,
            decision_model: None,
            decision_timeout_ms: 2500,
            decision_api_key: None,
            decision_key_source: None,
            api_key: None,
            openai: None,
            openai_from_env: false,
            kagi_api_key: None,
            kagi_base_url: None,
        }
    }
}

impl Config {
    /// Loads configuration for a session rooted at `cwd`.
    ///
    /// @param cwd working directory whose `.env` is consulted.
    /// @returns the validated configuration.
    pub fn load(cwd: &Path) -> Result<Self> {
        let (mut cfg, mut explicit_model) = match config_path() {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let raw: serde_json::Value = serde_json::from_str(&text)
                    .with_context(|| format!("invalid config file {}", path.display()))?;
                let explicit = raw.get("model").is_some();
                let cfg = serde_json::from_value(raw)
                    .with_context(|| format!("invalid config file {}", path.display()))?;
                (cfg, explicit)
            }
            _ => (Config::default(), false),
        };
        let dotenv = read_dotenv(&cwd.join(".env"));
        let var = |key: &str| {
            std::env::var(key)
                .ok()
                .filter(|v| !v.is_empty())
                .or_else(|| dotenv.get(key).cloned())
        };
        if let Some(v) = var("DEEPSEEK_BASE_URL") {
            cfg.base_url = v;
        }
        if let Some(v) = var("OPENAI_BASE_URL") {
            cfg.openai_base_url = v;
        }
        if let Some(v) = var("RDSH_MODEL") {
            cfg.model = v;
            explicit_model = true;
        }
        (cfg.decision_api_key, cfg.decision_key_source) = match (var("TYPESAFE_API_KEY"), var("OPENROUTER_API_KEY")) {
            (Some(k), _) => (Some(k), Some(KeySource::TypeSafe)),
            (None, Some(k)) => (Some(k), Some(KeySource::OpenRouter)),
            (None, None) => (None, None),
        };
        cfg.kagi_api_key = var("KAGI_API_KEY");
        cfg.kagi_base_url = var("KAGI_BASE_URL");
        let creds = crate::auth::load()?;
        cfg.api_key = var("DEEPSEEK_API_KEY").or(creds.deepseek_api_key.clone());
        cfg.openai_from_env = var("OPENAI_API_KEY").is_some();
        cfg.openai = match var("OPENAI_API_KEY") {
            Some(key) => Some(OpenAiAuth::ApiKey { key }),
            None => creds.openai.clone(),
        };
        cfg.resolve_models(creds.default_model.as_deref(), explicit_model);
        cfg.validate()?;
        Ok(cfg)
    }

    /// Picks the default model when the config and environment name none (the last login's, if
    /// its provider still has credentials, else whichever provider does) and fills an unset
    /// `models` list from the providers logged into.
    fn resolve_models(&mut self, last_login: Option<&str>, explicit: bool) {
        let openai = |m: &str| crate::providers::is_openai(m) || self.openai_models.iter().any(|o| o == m);
        let usable = |m: &str| if openai(m) { self.openai.is_some() } else { self.api_key.is_some() };
        if !explicit {
            self.model = match last_login.filter(|m| usable(m)) {
                Some(m) => m.to_owned(),
                None if self.api_key.is_none() && self.openai.is_some() => crate::providers::OPENAI_DEFAULT_MODEL.to_owned(),
                None => crate::providers::DEEPSEEK_DEFAULT_MODEL.to_owned(),
            };
        }
        if self.models.is_empty() {
            let mut models: Vec<String> = Vec::new();
            if self.api_key.is_some() || self.openai.is_none() {
                models.extend(crate::providers::DEEPSEEK_MODELS.map(String::from));
            }
            if self.openai.is_some() {
                models.extend(crate::providers::OPENAI_MODELS.map(String::from));
                models.extend(self.openai_models.iter().cloned());
            }
            if !models.contains(&self.model) {
                models.insert(0, self.model.clone());
            }
            self.models = models;
        }
    }

    /// Rejects values that would make the agent unusable.
    pub fn validate(&self) -> Result<()> {
        if self.model.trim().is_empty() {
            bail!("config: `model` must not be empty");
        }
        if self.remote_command.is_empty() {
            bail!("config: `remote_command` must name a program");
        }
        if let Some(h) = self.hosts.iter().find(|h| h.name.is_empty() || h.ssh.is_empty() || h.name == "local") {
            bail!("config: host entries need a `name` (not \"local\") and an `ssh` destination: {h:?}");
        }
        if self.max_tool_output_chars < 1000 {
            bail!("config: `max_tool_output_chars` must be at least 1000");
        }
        if self.bash_timeout_secs == 0 || self.stream_idle_timeout_secs == 0 || self.decision_timeout_ms == 0 {
            bail!("config: timeouts must be at least 1 second");
        }
        Ok(())
    }

    /// Directory holding session trace files.
    pub fn sessions_dir(&self) -> PathBuf {
        self.sessions_dir
            .clone()
            .unwrap_or_else(|| data_home().join("rdsh").join("sessions"))
    }
}

fn home() -> PathBuf {
    std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

/// `$XDG_CONFIG_HOME/rdsh` (default `~/.config/rdsh`).
pub fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".config"))
        .join("rdsh")
}

fn config_path() -> Option<PathBuf> {
    Some(config_dir().join("config.json"))
}

fn data_home() -> PathBuf {
    std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home().join(".local").join("share"))
}

/// Parses a dotenv file (`KEY=value`, optional `export`, optional quotes); missing file → empty.
pub fn read_dotenv(path: &Path) -> HashMap<String, String> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let line = line.strip_prefix("export ").unwrap_or(line);
            let (k, v) = line.split_once('=')?;
            let v = v.trim();
            let v = v
                .strip_prefix('"')
                .and_then(|v| v.strip_suffix('"'))
                .or_else(|| v.strip_prefix('\'').and_then(|v| v.strip_suffix('\'')))
                .unwrap_or(v);
            Some((k.trim().to_owned(), v.to_owned()))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dotenv_parsing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join(".env");
        std::fs::write(&p, "# c\nA=1\nexport B=\"two\"\nC='3'\n\nbad\n").unwrap();
        let m = read_dotenv(&p);
        assert_eq!(m["A"], "1");
        assert_eq!(m["B"], "two");
        assert_eq!(m["C"], "3");
        assert_eq!(m.len(), 3);
    }

    #[test]
    fn default_model_follows_the_last_login() {
        let chatgpt = || Some(OpenAiAuth::ApiKey { key: "k".into() });
        let resolved = |api_key: Option<&str>, openai, last: Option<&str>, explicit: Option<&str>| {
            let mut c = Config { api_key: api_key.map(String::from), openai, ..Config::default() };
            if let Some(m) = explicit {
                c.model = m.into();
            }
            c.resolve_models(last, explicit.is_some());
            (c.model, c.models)
        };
        // Only DeepSeek: unchanged from before.
        let (m, ms) = resolved(Some("k"), None, None, None);
        assert_eq!((m.as_str(), ms.len()), ("deepseek-flash", 2));
        // Only ChatGPT: its default, and only its models.
        let (m, ms) = resolved(None, chatgpt(), None, None);
        assert_eq!(m, "gpt-6.2-sol");
        assert!(ms.iter().all(|m| m.starts_with("gpt-")), "{ms:?}");
        // Both: the last login wins, and the picker offers both.
        let (m, ms) = resolved(Some("k"), chatgpt(), Some("gpt-6-sol"), None);
        assert_eq!((m.as_str(), ms.len()), ("gpt-6-sol", 12));
        // A last login whose credentials are gone is skipped.
        assert_eq!(resolved(Some("k"), None, Some("gpt-6-sol"), None).0, "deepseek-flash");
        // The config or RDSH_MODEL always wins, and an unlisted model joins the picker.
        let (m, ms) = resolved(Some("k"), chatgpt(), Some("gpt-6-sol"), Some("qwen3"));
        assert_eq!((m.as_str(), ms[0].as_str()), ("qwen3", "qwen3"));
    }

    #[test]
    fn host_specs() {
        let h = HostConfig::parse("me@build:/srv/app").unwrap();
        assert_eq!((h.name.as_str(), h.ssh.as_str(), h.dir.as_deref()), ("build", "me@build", Some("/srv/app")));
        let h = HostConfig::parse("box").unwrap();
        assert_eq!((h.name.as_str(), h.dir), ("box", None));
        assert!(HostConfig::parse(":/x").is_err());
    }

    #[test]
    fn validation_rejects_tiny_tool_output() {
        let cfg = Config { max_tool_output_chars: 10, ..Config::default() };
        assert!(cfg.validate().is_err());
        assert!(Config::default().validate().is_ok());
    }
}
