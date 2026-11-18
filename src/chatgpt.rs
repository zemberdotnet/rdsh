//! Sign in with ChatGPT: the OAuth flow the Codex CLI uses (and pi follows), so requests draw on
//! the subscription instead of API billing. Browser login runs PKCE against a callback on
//! `localhost:1455`; device-code login suits SSH sessions. Tokens refresh with the refresh token.

use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};

use crate::util::now_ms;

const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const AUTH_BASE: &str = "https://auth.openai.com";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CALLBACK_PORT: u16 = 1455;
const DEVICE_VERIFICATION_URI: &str = "https://auth.openai.com/codex/device";
const DEVICE_REDIRECT_URI: &str = "https://auth.openai.com/deviceauth/callback";
const DEVICE_TIMEOUT: Duration = Duration::from_secs(15 * 60);
const SCOPE: &str = "openid profile email offline_access";
const JWT_CLAIM: &str = "https://api.openai.com/auth";
/// Sent as `originator` on the authorize URL and every Codex request.
pub const ORIGINATOR: &str = "rdsh";

/// A ChatGPT login.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Token {
    pub access: String,
    pub refresh: String,
    /// When `access` expires, in Unix milliseconds.
    pub expires_ms: u64,
    /// Sent as `chatgpt-account-id`; read from the access token's claims.
    pub account_id: String,
}

impl Token {
    /// Expired, or expiring within a minute.
    pub fn stale(&self) -> bool {
        now_ms() + 60_000 >= self.expires_ms
    }
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn random(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("system randomness");
    buf
}

/// PKCE verifier and its S256 challenge.
fn pkce() -> (String, String) {
    let verifier = b64(&random(32));
    let challenge = b64(&Sha256::digest(verifier.as_bytes()));
    (verifier, challenge)
}

/// `application/x-www-form-urlencoded` body.
fn form(pairs: &[(&str, &str)]) -> String {
    let mut url = reqwest::Url::parse("http://x/").expect("static url");
    url.query_pairs_mut().extend_pairs(pairs);
    url.query().unwrap_or_default().to_owned()
}

/// The ChatGPT account id in an access token's claims.
pub fn account_id(jwt: &str) -> Option<String> {
    let payload = jwt.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(payload.trim_end_matches('=')).ok()?;
    let claims: Value = serde_json::from_slice(&bytes).ok()?;
    claims.get(JWT_CLAIM)?.get("chatgpt_account_id")?.as_str().filter(|s| !s.is_empty()).map(str::to_owned)
}

fn client() -> Result<reqwest::Client> {
    reqwest::Client::builder().timeout(Duration::from_secs(30)).build().context("building HTTP client")
}

/// Reads a token endpoint reply; `previous_refresh` is kept when a refresh does not rotate it.
async fn token_reply(resp: reqwest::Response, what: &str, previous_refresh: Option<&str>) -> Result<Token> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        bail!("ChatGPT token {what} failed ({status}): {}", text.trim());
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("ChatGPT token {what}: bad reply {text}"))?;
    let access = v["access_token"].as_str().context("ChatGPT token reply has no access_token")?.to_owned();
    let refresh = v["refresh_token"].as_str().or(previous_refresh).context("ChatGPT token reply has no refresh_token")?;
    let expires_in = v["expires_in"].as_u64().context("ChatGPT token reply has no expires_in")?;
    let account_id = account_id(&access).context("the ChatGPT access token names no account")?;
    Ok(Token { access, refresh: refresh.to_owned(), expires_ms: now_ms() + expires_in * 1000, account_id })
}

async fn exchange(code: &str, verifier: &str, redirect_uri: &str) -> Result<Token> {
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", CLIENT_ID),
        ("code", code),
        ("code_verifier", verifier),
        ("redirect_uri", redirect_uri),
    ]);
    let resp = client()?
        .post(format!("{AUTH_BASE}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("contacting auth.openai.com")?;
    token_reply(resp, "exchange", None).await
}

/// Trades the refresh token for a new access token.
pub async fn refresh(token: &Token) -> Result<Token> {
    let body = form(&[("grant_type", "refresh_token"), ("refresh_token", &token.refresh), ("client_id", CLIENT_ID)]);
    let resp = client()?
        .post(format!("{AUTH_BASE}/oauth/token"))
        .header("content-type", "application/x-www-form-urlencoded")
        .body(body)
        .send()
        .await
        .context("refreshing the ChatGPT login")?;
    token_reply(resp, "refresh", Some(&token.refresh))
        .await
        .map_err(|e| anyhow!("{e:#}; run `rdsh login chatgpt` again"))
}

/// Pulls `code` out of a callback URL (or a bare code), checking `state` when present.
fn code_from(input: &str, state: &str) -> Result<String> {
    let input = input.trim();
    let (code, got_state, error) = match reqwest::Url::parse(input) {
        Ok(url) => {
            let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
            (get("code"), get("state"), get("error"))
        }
        Err(_) if input.contains("code=") => {
            let url = reqwest::Url::parse(&format!("http://x/?{}", input.trim_start_matches('?'))).context("bad callback")?;
            let get = |k: &str| url.query_pairs().find(|(n, _)| n == k).map(|(_, v)| v.into_owned());
            (get("code"), get("state"), get("error"))
        }
        Err(_) => (Some(input.to_owned()).filter(|s| !s.is_empty()), None, None),
    };
    if let Some(e) = error {
        bail!("ChatGPT sign-in failed: {e}");
    }
    if got_state.is_some_and(|s| s != state) {
        bail!("sign-in state mismatch; start `rdsh login chatgpt` again");
    }
    code.context("no authorization code in the callback")
}

const PAGE: &str = "<!doctype html><meta charset=utf-8><title>rdsh</title>\
<body style=\"font-family:system-ui;margin:4em\"><h2>{}</h2><p>You can close this tab.</p>";

/// Serves the redirect on `localhost:1455` until a callback carrying `state` arrives.
async fn callback_server(listeners: Vec<tokio::net::TcpListener>, state: String) -> Result<String> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Result<String>>();
    for listener in listeners {
        let (tx, state) = (tx.clone(), state.clone());
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]);
                let target = head.split_whitespace().nth(1).unwrap_or("/");
                if !target.starts_with("/auth/callback") {
                    let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
                    continue;
                }
                let result = code_from(&format!("http://localhost{target}"), &state);
                let msg = match &result {
                    Ok(_) => "rdsh is signed in to ChatGPT.".to_owned(),
                    Err(e) => format!("Sign-in failed: {e}"),
                };
                let body = PAGE.replace("{}", &msg);
                let status = if result.is_ok() { "200 OK" } else { "400 Bad Request" };
                let reply = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(reply.as_bytes()).await;
                // A stray request (an old tab, a mismatched state) does not end the wait; a
                // refusal from the sign-in page does.
                if result.is_ok() || target.contains("error=") {
                    let _ = tx.send(result);
                }
            }
        });
    }
    drop(tx);
    rx.recv().await.unwrap_or_else(|| Err(anyhow!("callback server stopped")))
}

/// Browser login. Prints the URL (and opens it); a pasted redirect URL also completes it, for when
/// the browser runs on another machine or port 1455 is taken (the Codex CLI uses it too).
pub async fn login_browser() -> Result<Token> {
    let (verifier, challenge) = pkce();
    let state = hex(&random(16));
    let mut url = reqwest::Url::parse(&format!("{AUTH_BASE}/oauth/authorize")).expect("static url");
    url.query_pairs_mut().extend_pairs([
        ("response_type", "code"),
        ("client_id", CLIENT_ID),
        ("redirect_uri", REDIRECT_URI),
        ("scope", SCOPE),
        ("code_challenge", &challenge),
        ("code_challenge_method", "S256"),
        ("state", &state),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
        ("originator", ORIGINATOR),
    ]);
    // `localhost` may resolve to either loopback address; listen on both when we can.
    let mut listeners = Vec::new();
    for addr in ["127.0.0.1", "[::1]"] {
        if let Ok(l) = tokio::net::TcpListener::bind(format!("{addr}:{CALLBACK_PORT}")).await {
            listeners.push(l);
        }
    }
    eprintln!("Sign in to ChatGPT in your browser:\n\n  {url}\n");
    if listeners.is_empty() {
        eprintln!("(port {CALLBACK_PORT} is in use, maybe by the Codex CLI; paste the final redirect URL below)");
    } else {
        crate::util::open_browser(url.as_str());
    }
    eprint!("Waiting for the browser. If it lands on a page that does not load, paste its URL here: ");
    std::io::Write::flush(&mut std::io::stderr())?;
    let pasted = async {
        let mut line = String::new();
        let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
        loop {
            line.clear();
            if stdin.read_line(&mut line).await? == 0 {
                // Piped or closed stdin: only the callback can finish.
                std::future::pending::<()>().await;
            }
            if !line.trim().is_empty() {
                return code_from(&line, &state);
            }
        }
    };
    let code = if listeners.is_empty() {
        pasted.await?
    } else {
        tokio::select! {
            c = callback_server(listeners, state.clone()) => c?,
            c = pasted => c?,
        }
    };
    eprintln!("\nsigned in; exchanging the code…");
    exchange(&code, &verifier, REDIRECT_URI).await
}

/// Device-code login: shows a code to enter at auth.openai.com, then polls until it is approved.
pub async fn login_device() -> Result<Token> {
    let http = client()?;
    let resp = http
        .post(format!("{AUTH_BASE}/api/accounts/deviceauth/usercode"))
        .json(&json!({"client_id": CLIENT_ID}))
        .send()
        .await
        .context("contacting auth.openai.com")?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if status.as_u16() == 404 {
        bail!("device-code login is not enabled for this account; use `rdsh login chatgpt` without --device");
    }
    if !status.is_success() {
        bail!("device-code request failed ({status}): {}", text.trim());
    }
    let v: Value = serde_json::from_str(&text).with_context(|| format!("bad device-code reply: {text}"))?;
    let device_id = v["device_auth_id"].as_str().context("device-code reply has no device_auth_id")?;
    let user_code = v["user_code"].as_str().context("device-code reply has no user_code")?;
    let mut interval = match &v["interval"] {
        Value::String(s) => s.trim().parse().unwrap_or(5),
        n => n.as_u64().unwrap_or(5),
    }
    .max(1);
    eprintln!("Open {DEVICE_VERIFICATION_URI} and enter the code\n\n  {user_code}\n\nWaiting for approval (Ctrl-C cancels)…");
    let deadline = tokio::time::Instant::now() + DEVICE_TIMEOUT;
    loop {
        tokio::time::sleep(Duration::from_secs(interval)).await;
        if tokio::time::Instant::now() > deadline {
            bail!("the device code expired; run `rdsh login chatgpt --device` again");
        }
        let resp = http
            .post(format!("{AUTH_BASE}/api/accounts/deviceauth/token"))
            .json(&json!({"device_auth_id": device_id, "user_code": user_code}))
            .send()
            .await
            .context("polling auth.openai.com")?;
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            let v: Value = serde_json::from_str(&text).with_context(|| format!("bad device token reply: {text}"))?;
            let code = v["authorization_code"].as_str().context("device token reply has no authorization_code")?;
            let verifier = v["code_verifier"].as_str().context("device token reply has no code_verifier")?;
            return exchange(code, verifier, DEVICE_REDIRECT_URI).await;
        }
        if matches!(status.as_u16(), 403 | 404) {
            continue;
        }
        let code = serde_json::from_str::<Value>(&text).ok().and_then(|v| {
            let e = &v["error"];
            e.as_str().or_else(|| e["code"].as_str()).map(str::to_owned)
        });
        match code.as_deref() {
            Some("deviceauth_authorization_pending") => {}
            Some("slow_down") => interval += 5,
            _ => bail!("device-code login failed ({status}): {}", text.trim()),
        }
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn jwt(claims: Value) -> String {
        format!("h.{}.s", b64(claims.to_string().as_bytes()))
    }

    #[test]
    fn account_id_comes_from_the_auth_claim() {
        let t = jwt(json!({JWT_CLAIM: {"chatgpt_account_id": "acct-1"}}));
        assert_eq!(account_id(&t).as_deref(), Some("acct-1"));
        assert_eq!(account_id(&jwt(json!({"sub": "x"}))), None);
        assert_eq!(account_id("garbage"), None);
    }

    #[test]
    fn pkce_challenge_is_s256_of_the_verifier() {
        let (v, c) = pkce();
        assert_eq!(v.len(), 43);
        assert_eq!(c, b64(&Sha256::digest(v.as_bytes())));
    }

    #[test]
    fn callback_codes_are_parsed_and_state_checked() {
        let url = "http://localhost:1455/auth/callback?code=abc&state=s1";
        assert_eq!(code_from(url, "s1").unwrap(), "abc");
        assert!(code_from(url, "other").is_err());
        assert_eq!(code_from("code=xyz&state=s1", "s1").unwrap(), "xyz");
        assert_eq!(code_from("  rawcode \n", "s1").unwrap(), "rawcode");
        assert!(code_from("http://localhost:1455/auth/callback?error=access_denied", "s1").is_err());
    }

    #[test]
    fn form_bodies_are_url_encoded() {
        assert_eq!(form(&[("a", "x y"), ("b", "c&d=")]), "a=x+y&b=c%26d%3D");
    }

    #[tokio::test]
    async fn the_callback_server_answers_the_browser_and_returns_the_code() {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        let server = tokio::spawn(callback_server(vec![l], "st".into()));
        let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
        s.write_all(b"GET /auth/callback?code=c0de&state=st HTTP/1.1\r\nHost: localhost\r\n\r\n").await.unwrap();
        let mut reply = String::new();
        s.read_to_string(&mut reply).await.unwrap();
        assert!(reply.starts_with("HTTP/1.1 200"), "{reply}");
        assert_eq!(server.await.unwrap().unwrap(), "c0de");
    }
}
