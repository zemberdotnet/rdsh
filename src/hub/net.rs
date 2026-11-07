//! The hub protocol over byte streams: newline-delimited JSON, one [`Request`] or [`Notice`]
//! per line.
//!
//! - [`serve`] listens on a Unix socket and attaches every connection to a [`Hub`].
//! - [`connect_socket`] and [`connect_command`] give a client the same channel pair that
//!   [`Hub::connect`] gives in-process clients. A command's stdin/stdout carry the protocol, so
//!   `ssh host rdsh connect` reaches a hub on another machine.
//! - [`bridge`] is `rdsh connect`: it relays its stdin/stdout to the local hub's socket.
//!
//! The socket's directory is private to the user (0700); that is the access control for local
//! clients. Remote access goes through SSH, so the hub never listens on the network.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

use super::{Hub, Notice, Request};

/// Where the local hub listens: `$XDG_RUNTIME_DIR/rdsh/hub.sock`, or `/tmp/rdsh-<uid>/hub.sock`.
pub fn default_socket() -> PathBuf {
    let base = std::env::var_os("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("rdsh")).unwrap_or_else(|| {
        #[cfg(unix)]
        let uid = unsafe { libc::getuid() };
        #[cfg(not(unix))]
        let uid = 0;
        std::env::temp_dir().join(format!("rdsh-{uid}"))
    });
    base.join("hub.sock")
}

/// Serves `hub` on `socket` until `idle` passes with no clients and no running turns
/// (`None` serves forever).
///
/// Fails when another hub already answers on `socket`; a stale socket file is replaced.
pub async fn serve(hub: Hub, socket: &Path, idle: Option<Duration>) -> Result<()> {
    // sockaddr_un holds 108 bytes including the terminator.
    let len = socket.as_os_str().len();
    if len > 107 {
        bail!("socket path is {len} bytes, the limit is 107: {} (pass --socket or set XDG_RUNTIME_DIR to a shorter directory)", socket.display());
    }
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if socket.exists() {
        if UnixStream::connect(socket).await.is_ok() {
            bail!("a hub is already running on {}", socket.display());
        }
        std::fs::remove_file(socket).with_context(|| format!("removing stale {}", socket.display()))?;
    }
    let listener = UnixListener::bind(socket).with_context(|| format!("listening on {}", socket.display()))?;
    eprintln!("rdsh hub listening on {}", socket.display());
    let mut tick = tokio::time::interval(Duration::from_secs(5));
    let mut idle_since = tokio::time::Instant::now();
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let (stream, _) = accepted.context("accepting a client")?;
                let (r, w) = stream.into_split();
                tokio::spawn(attach(hub.clone(), r, w));
            }
            _ = tick.tick() => {
                if hub.clients() > 0 || hub.busy() {
                    idle_since = tokio::time::Instant::now();
                } else if idle.is_some_and(|d| idle_since.elapsed() >= d) {
                    let _ = std::fs::remove_file(socket);
                    return Ok(());
                }
            }
        }
    }
}

/// Runs one client connection: lines in become requests, notices go out as lines.
pub async fn attach<R, W>(hub: Hub, reader: R, mut writer: W)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    // When the reader ends it drops `req_tx`; the hub then drops this client and `notices`
    // closes, which ends the write loop below.
    let (req_tx, mut notices) = hub.connect();
    let reader_task = tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Request>(&line) {
                Ok(req) => {
                    if req_tx.send(req).is_err() {
                        break;
                    }
                }
                // Unknown or malformed requests are dropped; the client sees no reply.
                Err(_) => continue,
            }
        }
    });
    while let Some(n) = notices.recv().await {
        let Ok(mut line) = serde_json::to_string(&n) else { continue };
        line.push('\n');
        if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
            break;
        }
    }
    reader_task.abort();
}

/// Client side of a byte stream: requests are written as lines, notice lines are parsed.
///
/// @returns the same channel pair as [`Hub::connect`]; the notice receiver closes when the
/// stream ends.
pub fn client<R, W>(reader: R, mut writer: W) -> (UnboundedSender<Request>, UnboundedReceiver<Notice>)
where
    R: AsyncRead + Unpin + Send + 'static,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let (req_tx, mut req_rx) = mpsc::unbounded_channel::<Request>();
    let (ntx, nrx) = mpsc::unbounded_channel::<Notice>();
    tokio::spawn(async move {
        while let Some(req) = req_rx.recv().await {
            let Ok(mut line) = serde_json::to_string(&req) else { continue };
            line.push('\n');
            if writer.write_all(line.as_bytes()).await.is_err() || writer.flush().await.is_err() {
                break;
            }
        }
    });
    tokio::spawn(async move {
        let mut lines = BufReader::new(reader).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            match serde_json::from_str::<Notice>(&line) {
                Ok(n) => {
                    if ntx.send(n).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let message = format!("unreadable message from the hub: {e}");
                    let _ = ntx.send(Notice::Error { session_id: None, message });
                }
            }
        }
    });
    (req_tx, nrx)
}

/// Connects to a hub's socket.
pub async fn connect_socket(socket: &Path) -> Result<(UnboundedSender<Request>, UnboundedReceiver<Notice>)> {
    let stream = UnixStream::connect(socket).await.with_context(|| format!("connecting to {}", socket.display()))?;
    let (r, w) = stream.into_split();
    Ok(client(r, w))
}

/// Runs `program args…` and speaks the protocol over its stdin/stdout (for `ssh host rdsh
/// connect`). The process is killed when the connection is dropped.
pub fn connect_command(program: &str, args: &[String]) -> Result<(UnboundedSender<Request>, UnboundedReceiver<Notice>)> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("starting {program}"))?;
    let stdin = child.stdin.take().context("child stdin")?;
    let stdout = child.stdout.take().context("child stdout")?;
    let mut stderr = child.stderr.take().context("child stderr")?;
    let (req_tx, mut inner) = client(stdout, stdin);
    let (ntx, nrx) = mpsc::unbounded_channel();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<String>();
    // Owns the child for the connection's lifetime; reports why it ended if it failed.
    tokio::spawn(async move {
        let mut err = Vec::new();
        let _ = tokio::io::AsyncReadExt::read_to_end(&mut stderr, &mut err).await;
        let status = child.wait().await;
        let text = String::from_utf8_lossy(&err);
        let tail: Vec<&str> = text.lines().rev().take(3).collect();
        let reason = match status {
            Ok(s) if s.success() => String::new(),
            Ok(s) => format!("{s}: {}", tail.into_iter().rev().collect::<Vec<_>>().join(" / ")),
            Err(e) => e.to_string(),
        };
        let _ = done_tx.send(reason);
    });
    tokio::spawn(async move {
        while let Some(n) = inner.recv().await {
            if ntx.send(n).is_err() {
                return;
            }
        }
        if let Ok(reason) = done_rx.await
            && !reason.is_empty()
        {
            let _ = ntx.send(Notice::Error { session_id: None, message: format!("connection ended ({reason})") });
        }
    });
    Ok((req_tx, nrx))
}

/// Makes sure a hub answers on `socket`, starting `rdsh serve` in the background if not.
///
/// @param exe the rdsh binary to start.
pub async fn ensure_server(socket: &Path, exe: &Path) -> Result<()> {
    if UnixStream::connect(socket).await.is_ok() {
        return Ok(());
    }
    let log = socket.with_file_name("hub.log");
    if let Some(dir) = socket.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let log_file = std::fs::File::options().create(true).append(true).open(&log)?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("serve").arg("--socket").arg(socket).stdin(Stdio::null()).stdout(Stdio::null()).stderr(log_file);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session, so closing the terminal does not stop the hub.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    cmd.spawn().with_context(|| format!("starting {} serve", exe.display()))?;
    for _ in 0..100 {
        if UnixStream::connect(socket).await.is_ok() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    bail!("the hub did not start; see {}", log.display())
}

/// `rdsh connect`: relays stdin/stdout to the hub on `socket`, starting it if needed.
pub async fn bridge(socket: &Path, exe: &Path) -> Result<()> {
    ensure_server(socket, exe).await?;
    let stream = UnixStream::connect(socket).await?;
    let (mut r, mut w) = stream.into_split();
    let mut stdin = tokio::io::stdin();
    let mut stdout = tokio::io::stdout();
    let up = async {
        let _ = tokio::io::copy(&mut stdin, &mut w).await;
        let _ = w.shutdown().await;
    };
    let down = async {
        let _ = tokio::io::copy(&mut r, &mut stdout).await;
    };
    // Either side closing ends the relay.
    tokio::select! {
        _ = up => {}
        _ = down => {}
    }
    Ok(())
}
