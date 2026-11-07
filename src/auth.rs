//! `rdsh login` / `rdsh logout`: store the DeepSeek API key in
//! `~/.config/rdsh/credentials.json` (mode 0600). Environment and `.env` keys take precedence.

use std::{
    io::{BufRead, IsTerminal, Write},
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use serde::{Deserialize, Serialize};

use crate::config::config_dir;

#[derive(Serialize, Deserialize)]
struct Credentials {
    deepseek_api_key: String,
}

pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// Reads the stored key; Ok(None) when the user has not logged in.
pub fn load() -> Result<Option<String>> {
    let path = credentials_path();
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let c: Credentials = serde_json::from_str(&text).with_context(|| format!("invalid {}", path.display()))?;
    Ok(Some(c.deepseek_api_key).filter(|k| !k.is_empty()))
}

/// Writes the key with owner-only permissions.
pub fn save(key: &str) -> Result<PathBuf> {
    let path = credentials_path();
    std::fs::create_dir_all(config_dir()).context("creating config directory")?;
    let body = serde_json::to_string_pretty(&Credentials { deepseek_api_key: key.to_owned() })?;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        opts.mode(0o600);
        // An existing file keeps its old mode on open; tighten it explicitly.
        if path.exists() {
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    let mut f = opts.open(&path).with_context(|| format!("writing {}", path.display()))?;
    f.write_all(body.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(path)
}

/// Deletes stored credentials. Returns whether a file existed.
pub fn logout() -> Result<bool> {
    match std::fs::remove_file(credentials_path()) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e).context("removing credentials"),
    }
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

/// Interactive login flow.
///
/// @param base_url API base used to verify the key.
/// @returns process exit code.
pub async fn login(base_url: &str) -> Result<i32> {
    if std::env::var("DEEPSEEK_API_KEY").is_ok_and(|v| !v.is_empty()) {
        eprintln!("note: DEEPSEEK_API_KEY is set in the environment and takes precedence over the stored key");
    }
    eprintln!("Create a key at https://platform.deepseek.com/api_keys");
    let key = read_secret("DeepSeek API key: ")?;
    if key.is_empty() {
        bail!("no key entered");
    }
    eprint!("verifying… ");
    verify(base_url, &key).await?;
    let path = save(&key)?;
    eprintln!("ok\nSaved to {}", path.display());
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
        assert_eq!(load().unwrap(), None);
        let p = save("sk-test").unwrap();
        assert_eq!(load().unwrap().as_deref(), Some("sk-test"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&p).unwrap().permissions().mode() & 0o777, 0o600);
        }
        assert!(logout().unwrap());
        assert!(!logout().unwrap());
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
