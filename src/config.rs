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

use crate::{decision::KeySource, llm::Effort};

#[derive(Debug, Clone, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// Model id sent to the API (`RDSH_MODEL` overrides).
    pub model: String,
    /// Models offered by `:model`; any other id can still be typed after it.
    pub models: Vec<String>,
    /// Thinking effort new turns start with (`off`, `low`, `high`, `max`); Shift-Tab cycles it.
    pub effort: Effort,
    /// OpenAI-compatible base URL (`DEEPSEEK_BASE_URL` overrides).
    pub base_url: String,
    /// Maximum model requests per user turn in the main agent.
    pub max_steps: usize,
    /// Maximum model requests per subagent task.
    pub subagent_max_steps: usize,
    /// Tool output is truncated (head and tail kept) beyond this many characters.
    pub max_tool_output_chars: usize,
    /// Default `bash` timeout when the model does not pass one.
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
            model: "deepseek-flash".into(),
            models: vec!["deepseek-flash".into(), "deepseek-v4-pro".into()],
            effort: Effort::High,
            base_url: "https://api.deepseek.com".into(),
            max_steps: 200,
            subagent_max_steps: 50,
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
        }
    }
}

impl Config {
    /// Loads configuration for a session rooted at `cwd`.
    ///
    /// @param cwd working directory whose `.env` is consulted.
    /// @returns the validated configuration.
    pub fn load(cwd: &Path) -> Result<Self> {
        let mut cfg = match config_path() {
            Some(path) if path.exists() => {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                serde_json::from_str(&text)
                    .with_context(|| format!("invalid config file {}", path.display()))?
            }
            _ => Config::default(),
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
        if let Some(v) = var("RDSH_MODEL") {
            cfg.model = v;
        }
        (cfg.decision_api_key, cfg.decision_key_source) = match (var("TYPESAFE_API_KEY"), var("OPENROUTER_API_KEY")) {
            (Some(k), _) => (Some(k), Some(KeySource::TypeSafe)),
            (None, Some(k)) => (Some(k), Some(KeySource::OpenRouter)),
            (None, None) => (None, None),
        };
        cfg.api_key = match var("DEEPSEEK_API_KEY") {
            Some(k) => Some(k),
            None => crate::auth::load()?,
        };
        cfg.validate()?;
        Ok(cfg)
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
        if self.max_steps == 0 || self.subagent_max_steps == 0 {
            bail!("config: `max_steps` and `subagent_max_steps` must be at least 1");
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
    fn host_specs() {
        let h = HostConfig::parse("me@build:/srv/app").unwrap();
        assert_eq!((h.name.as_str(), h.ssh.as_str(), h.dir.as_deref()), ("build", "me@build", Some("/srv/app")));
        let h = HostConfig::parse("box").unwrap();
        assert_eq!((h.name.as_str(), h.dir), ("box", None));
        assert!(HostConfig::parse(":/x").is_err());
    }

    #[test]
    fn validation_rejects_zero_steps() {
        let cfg = Config { max_steps: 0, ..Config::default() };
        assert!(cfg.validate().is_err());
        assert!(Config::default().validate().is_ok());
    }
}
