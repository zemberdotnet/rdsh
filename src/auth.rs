//! `rdsh login` / `rdsh logout`: credentials in `~/.config/rdsh/credentials.json` (mode 0600).
//! It holds a DeepSeek API key, one OpenAI login (an API key or a ChatGPT subscription), and the
//! model of the last login, which is the default when the config names none. Environment and
//! `.env` keys take precedence over stored ones.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::{chatgpt, config::{Config, config_dir}};

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Credentials {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deepseek_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub openai: Option<OpenAiAuth>,
    /// Model of the provider logged into last.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_model: Option<String>,
}

/// How requests to OpenAI are authorized.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum OpenAiAuth {
    /// A platform API key, billed per token at api.openai.com.
    ApiKey { key: String },
    /// Sign in with ChatGPT: the subscription's Codex allowance at chatgpt.com.
    Chatgpt(chatgpt::Token),
}

pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// Reads stored credentials; empty when the user has not logged in.
pub fn load() -> Result<Credentials> {
    let path = credentials_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Credentials::default()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let mut c: Credentials = serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    c.deepseek_api_key = c.deepseek_api_key.filter(|k| !k.is_empty());
    Ok(c)
}

/// Writes credentials with owner-only permissions.
pub fn save(c: &Credentials) -> Result<PathBuf> {
    let path = credentials_path();
    std::fs::create_dir_all(config_dir()).context("creating config directory")?;
    let body = serde_json::to_string_pretty(c)?;
    // Write a sibling and rename, so a process refreshing a token never leaves a torn file.
    let tmp = path.with_extension(format!("json.{}", std::process::id()));
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp).with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(body.as_bytes())?;
    f.write_all(b"\n")?;
    drop(f);
    std::fs::rename(&tmp, &path).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Applies `change` to the stored credentials and saves them.
pub fn update(change: impl FnOnce(&mut Credentials)) -> Result<PathBuf> {
    let mut c = load()?;
    change(&mut c);
    save(&c)
}

/// Removes one provider's login (`deepseek`, `openai`/`chatgpt`), or everything when `None`.
/// Returns whether anything was removed.
pub fn logout(provider: Option<&str>) -> Result<bool> {
    let Some(p) = provider else {
        return match std::fs::remove_file(credentials_path()) {
            Ok(()) => Ok(true),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(e) => Err(e).context("removing credentials"),
        };
    };
    let mut c = load()?;
    let removed = match p {
        "deepseek" => c.deepseek_api_key.take().is_some(),
        "openai" | "chatgpt" => c.openai.take().is_some(),
        other => bail!("unknown provider {other:?}: use deepseek, openai or chatgpt"),
    };
    if c.default_model.as_deref().is_some_and(|m| crate::providers::is_openai(m) == (p != "deepseek")) {
        c.default_model = None;
    }
    if removed {
        save(&c)?;
    }
    Ok(removed)
}

/// Reads a secret from the terminal without echoing it, or one line from piped stdin.
fn read_secret(prompt: &str) -> Result<String> {
    let stdin = std::io::stdin();
    if !stdin.is_terminal() {
        let mut line = String::new();
        stdin.lock().read_line(&mut line)?;
        return Ok(line.trim().to_owned());
    }
    eprint!("{prompt}");
    std::io::stderr().flush()?;
    crossterm::terminal::enable_raw_mode()?;
    let result = (|| -> Result<Option<String>> {
        let mut key = String::new();
        loop {
            let Event::Key(k) = event::read()? else { continue };
            if k.kind == KeyEventKind::Release {
                continue;
            }
            match k.code {
                KeyCode::Enter => return Ok(Some(key)),
                KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => return Ok(None),
                KeyCode::Esc => return Ok(None),
                KeyCode::Backspace => {
                    key.pop();
                }
                KeyCode::Char(c) => key.push(c),
                _ => {}
            }
        }
    })();
    crossterm::terminal::disable_raw_mode()?;
    eprintln!();
    match result? {
        Some(k) => Ok(k.trim().to_owned()),
        None => bail!("login cancelled"),
    }
}

/// Checks a key with `GET {base_url}/models`.
async fn verify(base_url: &str, key: &str) -> Result<()> {
    let client = reqwest::Client::builder().timeout(Duration::from_secs(20)).build()?;
    let url = format!("{}/models", base_url.trim_end_matches('/'));
    let resp = client.get(&url).bearer_auth(key).send().await.with_context(|| format!("contacting {url}"))?;
    let status = resp.status();
    if status.as_u16() == 401 || status.as_u16() == 403 {
        bail!("the API rejected this key ({status})");
    }
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        bail!("could not verify the key: {status} {}", body.trim());
    }
    Ok(())
}

async fn read_key(env: &str, url: &str, label: &str, base_url: &str) -> Result<String> {
    if std::env::var(env).is_ok_and(|v| !v.is_empty()) {
        eprintln!("note: {env} is set in the environment and takes precedence over the stored key");
    }
    eprintln!("Create a key at {url}");
    let key = read_secret(&format!("{label} API key: "))?;
    if key.is_empty() {
        bail!("no key entered");
    }
    eprint!("verifying… ");
    verify(base_url, &key).await?;
    eprintln!("ok");
    Ok(key)
}

/// Asks which provider to log into when `rdsh login` names none.
fn choose() -> Result<(&'static str, bool)> {
    if !std::io::stdin().is_terminal() {
        bail!("name a provider: rdsh login deepseek | chatgpt [--device] | openai");
    }
    eprintln!("Log in to:");
    eprintln!("  1) ChatGPT subscription (opens a browser)");
    eprintln!("  2) ChatGPT subscription with a device code (for SSH or no browser)");
    eprintln!("  3) OpenAI API key");
    eprintln!("  4) DeepSeek API key");
    eprint!("> ");
    std::io::stderr().flush()?;
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line)?;
    Ok(match line.trim() {
        "1" => ("chatgpt", false),
        "2" => ("chatgpt", true),
        "3" => ("openai", false),
        "4" => ("deepseek", false),
        _ => bail!("login cancelled"),
    })
}

/// Interactive login.
///
/// @param provider `deepseek`, `chatgpt` or `openai`; `None` asks.
/// @param device ChatGPT only: use a device code instead of a browser redirect.
/// @returns process exit code.
pub async fn login(cfg: &Config, provider: Option<&str>, device: bool) -> Result<i32> {
    let (provider, device) = match provider {
        Some(p) => (p, device),
        None => choose()?,
    };
    let (path, model) = match provider {
        "deepseek" => {
            let key = read_key("DEEPSEEK_API_KEY", "https://platform.deepseek.com/api_keys", "DeepSeek", &cfg.base_url).await?;
            let model = crate::providers::DEEPSEEK_DEFAULT_MODEL;
            (update(|c| (c.deepseek_api_key, c.default_model) = (Some(key), Some(model.into())))?, model)
        }
        "openai" => {
            let key = read_key("OPENAI_API_KEY", "https://platform.openai.com/api-keys", "OpenAI", &cfg.openai_base_url).await?;
            let model = crate::providers::OPENAI_DEFAULT_MODEL;
            (update(|c| (c.openai, c.default_model) = (Some(OpenAiAuth::ApiKey { key }), Some(model.into())))?, model)
        }
        "chatgpt" => {
            if std::env::var("OPENAI_API_KEY").is_ok_and(|v| !v.is_empty()) {
                eprintln!("note: OPENAI_API_KEY is set in the environment and takes precedence over this login");
            }
            let token = if device { chatgpt::login_device().await? } else { chatgpt::login_browser().await? };
            let model = crate::providers::OPENAI_DEFAULT_MODEL;
            (update(|c| (c.openai, c.default_model) = (Some(OpenAiAuth::Chatgpt(token)), Some(model.into())))?, model)
        }
        other => bail!("unknown provider {other:?}: use deepseek, chatgpt or openai"),
    };
    eprintln!("Saved to {}; new sessions default to {model}", path.display());
    Ok(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn save_load_logout_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: test-only env mutation; no other test reads XDG_CONFIG_HOME concurrently.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        assert_eq!(load().unwrap(), Credentials::default());
        // The original single-key file still loads.
        std::fs::create_dir_all(config_dir()).unwrap();
        std::fs::write(credentials_path(), r#"{"deepseek_api_key": "sk-old"}"#).unwrap();
        assert_eq!(load().unwrap().deepseek_api_key.as_deref(), Some("sk-old"));

        let token = chatgpt::Token { access: "a".into(), refresh: "r".into(), expires_ms: 1, account_id: "acct".into() };
        let p = update(|c| {
            c.openai = Some(OpenAiAuth::Chatgpt(token.clone()));
            c.default_model = Some("gpt-6.2-sol".into());
        })
        .unwrap();
        let c = load().unwrap();
        assert_eq!((c.deepseek_api_key.as_deref(), c.openai), (Some("sk-old"), Some(OpenAiAuth::Chatgpt(token))));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // Logging out of OpenAI keeps DeepSeek and drops OpenAI's default model.
        assert!(logout(Some("chatgpt")).unwrap());
        let c = load().unwrap();
        assert_eq!((c.openai, c.default_model, c.deepseek_api_key.is_some()), (None, None, true));
        assert!(logout(None).unwrap());
        assert!(!logout(None).unwrap());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
