//! `rdsh web`: serves the browser client and relays its WebSocket to the hubs.
//!
//! The page talks to this process, not to a hub. It is a client: each browser
//! connection gets one hub connection per host (the local socket, `ssh host rdsh connect` for
//! remote ones), and every message on the WebSocket is tagged with the host's index. Hubs keep
//! never listening on the network.
//!
//! Access control. The server binds 127.0.0.1 only. The URL it prints carries a random token;
//! the first visit trades it for an HttpOnly, SameSite=Strict cookie. Every request must name
//! this server in `Host` (against DNS rebinding), and the WebSocket must come from this origin
//! with the cookie, so other sites open in the same browser cannot drive the hubs.

use std::{
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use anyhow::{Context, Result};
use axum::{
    Router,
    extract::{
        Query, Request as HttpRequest, State,
        ws::{Message as WsMessage, WebSocket, WebSocketUpgrade},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Redirect, Response},
    routing::get,
};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::{
    net::TcpListener,
    sync::mpsc::{self, UnboundedReceiver, UnboundedSender},
};

use crate::{
    config::HostConfig,
    hub::{Notice, Request},
    llm::{BoxFuture, Effort},
};

/// A hub the page can reach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostSpec {
    pub name: String,
    /// The hub on this machine; its sessions' files are local.
    pub local: bool,
    /// Directory for new sessions; also limits the rail to sessions from it.
    pub dir: Option<String>,
    /// SSH destination of a remote host.
    #[serde(skip)]
    pub ssh: Option<String>,
}

impl HostSpec {
    pub fn remote(h: &HostConfig) -> Self {
        Self { name: h.name.clone(), local: false, dir: h.dir.clone(), ssh: Some(h.ssh.clone()) }
    }
}

pub type HubLink = (UnboundedSender<Request>, UnboundedReceiver<Notice>);

/// Opens a connection to a host's hub (starting it if needed).
pub type Connector = Arc<dyn Fn(HostSpec) -> BoxFuture<'static, Result<HubLink>> + Send + Sync>;

/// What the page needs besides the hubs.
#[derive(Clone)]
pub struct Settings {
    /// Default directory for new local sessions; the local rail lists sessions from it.
    pub cwd: String,
    pub effort: Effort,
    pub models: Vec<String>,
    pub hosts: Vec<HostSpec>,
    /// Serve the page from this directory instead of the copy built into the binary (for
    /// working on the client: `RDSH_WEB_DIR=web/dist`).
    pub assets_dir: Option<PathBuf>,
}

struct Shared {
    settings: Settings,
    connector: Connector,
    token: String,
    port: u16,
}

/// Page → this server.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum In {
    Request { host: usize, request: Request },
    /// `:host DEST[:DIR]` typed in the page.
    AddHost { spec: String },
}

/// This server → page.
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Out<'a> {
    Welcome { version: &'a str, cwd: &'a str, effort: Effort, models: &'a [String], hosts: &'a [HostSpec] },
    HostAdded { host: &'a HostSpec },
    Notice { host: usize, notice: &'a Notice },
    Disconnected { host: usize, reason: Option<String> },
    Error { message: String },
}

fn encode(o: &Out) -> String {
    serde_json::to_string(o).unwrap_or_default()
}

/// The token from `dir/web.token`, created once so tabs survive a restart of `rdsh web`.
pub fn load_token(dir: &Path) -> Result<String> {
    let path = dir.join("web.token");
    if let Ok(t) = std::fs::read_to_string(&path)
        && t.trim().len() >= 32
    {
        return Ok(t.trim().to_owned());
    }
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let mut bytes = [0u8; 24];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom").context("opening /dev/urandom")?, &mut bytes)?;
    let token: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(token)
}

/// Binds 127.0.0.1:`port`, or any free port when `port` is taken and `fallback` allows it.
pub async fn bind(port: u16, fallback: bool) -> Result<TcpListener> {
    match TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(l) => Ok(l),
        Err(_) if fallback => Ok(TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await?),
        Err(e) => Err(e).with_context(|| format!("listening on 127.0.0.1:{port}")),
    }
}

/// The URL that logs a browser in.
pub fn login_url(listener: &TcpListener, token: &str) -> Result<String> {
    let port = listener.local_addr()?.port();
    Ok(format!("http://127.0.0.1:{port}/?t={token}"))
}

/// Serves until the listener fails.
pub async fn serve(listener: TcpListener, settings: Settings, connector: Connector, token: String) -> Result<()> {
    let port = listener.local_addr()?.port();
    let shared = Arc::new(Shared { settings, connector, token, port });
    let app = Router::new()
        .route("/", get(index))
        .route("/app.js", get(|s: State<Arc<Shared>>| asset(s, "app.js")))
        .route("/app.css", get(|s: State<Arc<Shared>>| asset(s, "app.css")))
        .route("/frame", get(frame))
        .route("/frame.js", get(|s: State<Arc<Shared>>| asset(s, "frame.js")))
        .route("/d3.js", get(|s: State<Arc<Shared>>| asset(s, "d3.js")))
        .route("/fonts/{name}", get(font))
        .route("/ws", get(ws))
        .layer(middleware::from_fn_with_state(shared.clone(), guard))
        .with_state(shared);
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>()).await?;
    Ok(())
}

fn allowed_hosts(port: u16) -> [String; 2] {
    [format!("127.0.0.1:{port}"), format!("localhost:{port}")]
}

/// Refuses requests that do not name this server (DNS rebinding). Diagram frames come from
/// their own `<label>.localhost` names (each its own site, so its own process); those names
/// reach only the frame's files. Browsers resolve every `*.localhost` to this machine themselves.
async fn guard(State(s): State<Arc<Shared>>, req: HttpRequest, next: Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let path = req.uri().path();
    let frame_file = matches!(path, "/frame" | "/frame.js" | "/d3.js") || path.starts_with("/fonts/");
    if !allowed_hosts(s.port).iter().any(|a| a == host) && !(frame_file && is_frame_host(host, s.port)) {
        return (StatusCode::FORBIDDEN, "wrong Host").into_response();
    }
    next.run(req).await
}

/// `<label>.localhost:<port>`, the label of letters, digits and dashes.
fn is_frame_host(host: &str, port: u16) -> bool {
    host.strip_suffix(&format!(".localhost:{port}"))
        .is_some_and(|l| !l.is_empty() && l.len() <= 63 && l.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'))
}

fn cookie_name(port: u16) -> String {
    // Cookies are per host, not per port: two servers on different ports must not clobber.
    format!("rdsh_{port}")
}

/// Compares in time independent of where the strings differ.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

fn has_cookie(s: &Shared, headers: &HeaderMap) -> bool {
    let name = cookie_name(s.port);
    headers.get_all(header::COOKIE).iter().filter_map(|v| v.to_str().ok()).flat_map(|v| v.split(';')).any(|kv| {
        kv.trim().split_once('=').is_some_and(|(k, v)| k == name && same(v, &s.token))
    })
}

#[derive(Deserialize)]
struct Login {
    t: Option<String>,
}

async fn index(State(s): State<Arc<Shared>>, Query(q): Query<Login>, headers: HeaderMap) -> Response {
    if let Some(t) = q.t {
        if !same(&t, &s.token) {
            return (StatusCode::FORBIDDEN, "stale link: open the URL `rdsh web` printed").into_response();
        }
        // Trade the token in the URL for a cookie, and drop it from the address bar and history.
        let cookie = format!("{}={}; HttpOnly; SameSite=Strict; Path=/; Max-Age=31536000", cookie_name(s.port), s.token);
        let mut r = Redirect::to("/").into_response();
        if let Ok(v) = HeaderValue::from_str(&cookie) {
            r.headers_mut().insert(header::SET_COOKIE, v);
        }
        return r;
    }
    if !has_cookie(&s, &headers) {
        return (StatusCode::FORBIDDEN, "not logged in: open the URL `rdsh web` printed").into_response();
    }
    let port = s.port;
    with_csp(asset(State(s), "index.html").await, &page_csp(port))
}

async fn asset(State(s): State<Arc<Shared>>, name: &'static str) -> Response {
    let body: Vec<u8> = match &s.settings.assets_dir {
        Some(dir) => match tokio::fs::read(dir.join(name)).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::NOT_FOUND, format!("{name}: {e}")).into_response(),
        },
        None => match name {
            "index.html" => include_bytes!("../web/dist/index.html").to_vec(),
            "app.js" => include_bytes!("../web/dist/app.js").to_vec(),
            "app.css" => include_bytes!("../web/dist/app.css").to_vec(),
            "frame.html" => include_bytes!("../web/dist/frame.html").to_vec(),
            "frame.js" => include_bytes!("../web/dist/frame.js").to_vec(),
            "d3.js" => include_bytes!("../web/dist/d3.js").to_vec(),
            _ => return StatusCode::NOT_FOUND.into_response(),
        },
    };
    let kind = match name.rsplit('.').next() {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    ([(header::CONTENT_TYPE, kind), (header::CACHE_CONTROL, "no-cache"), (header::X_CONTENT_TYPE_OPTIONS, "nosniff")], body).into_response()
}

async fn ws(State(s): State<Arc<Shared>>, headers: HeaderMap, up: WebSocketUpgrade) -> Response {
    let origin = headers.get(header::ORIGIN).and_then(|h| h.to_str().ok()).unwrap_or("");
    let origin_ok = allowed_hosts(s.port).iter().any(|h| origin == format!("http://{h}"));
    if !origin_ok || !has_cookie(&s, &headers) {
        return (StatusCode::FORBIDDEN, "not allowed").into_response();
    }
    up.on_upgrade(move |socket| relay(socket, s))
}

/// Hub connections of one page, by host index; `None` until connected or after it ended.
type Links = Arc<Mutex<Vec<Option<UnboundedSender<Request>>>>>;

/// One page: hub notices out, requests in, until the page goes away.
async fn relay(socket: WebSocket, s: Arc<Shared>) {
    let (mut sink, mut stream) = socket.split();
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        while let Some(text) = out_rx.recv().await {
            if sink.send(WsMessage::Text(text.into())).await.is_err() {
                break;
            }
        }
    });
    let mut hosts = s.settings.hosts.clone();
    let links: Links = Arc::new(Mutex::new(vec![None; hosts.len()]));
    let _ = out_tx.send(encode(&Out::Welcome {
        version: env!("CARGO_PKG_VERSION"),
        cwd: &s.settings.cwd,
        effort: s.settings.effort,
        models: &s.settings.models,
        hosts: &hosts,
    }));
    let mut tasks = Vec::new();
    for (i, h) in hosts.iter().enumerate() {
        tasks.push(tokio::spawn(link_host(i, h.clone(), s.connector.clone(), links.clone(), out_tx.clone())));
    }
    while let Some(Ok(msg)) = stream.next().await {
        let WsMessage::Text(text) = msg else { continue };
        match serde_json::from_str::<In>(&text) {
            Ok(In::Request { host, request }) => {
                let tx = links.lock().unwrap().get(host).cloned().flatten();
                let sent = tx.is_some_and(|tx| tx.send(request).is_ok());
                if !sent {
                    let name = hosts.get(host).map_or("that host", |h| h.name.as_str());
                    let _ = out_tx.send(encode(&Out::Error { message: format!("not connected to {name}") }));
                }
            }
            Ok(In::AddHost { spec }) => match HostConfig::parse(&spec) {
                Ok(h) if hosts.iter().any(|x| x.name == h.name) => {
                    let _ = out_tx.send(encode(&Out::Error { message: format!("host {} is already in the rail", h.name) }));
                }
                Ok(h) => {
                    let spec = HostSpec::remote(&h);
                    let i = hosts.len();
                    hosts.push(spec.clone());
                    links.lock().unwrap().push(None);
                    let _ = out_tx.send(encode(&Out::HostAdded { host: &spec }));
                    tasks.push(tokio::spawn(link_host(i, spec, s.connector.clone(), links.clone(), out_tx.clone())));
                }
                Err(e) => {
                    let _ = out_tx.send(encode(&Out::Error { message: format!(":host — {e:#}") }));
                }
            },
            Err(e) => {
                let _ = out_tx.send(encode(&Out::Error { message: format!("unreadable message from the page: {e}") }));
            }
        }
    }
    // The page is gone: dropping the senders closes every hub connection (and SSH process).
    links.lock().unwrap().clear();
    for t in tasks {
        t.abort();
    }
    writer.abort();
}

/// Connects host `i` and forwards its notices until the connection ends.
/// Delays between reconnect attempts after a hub connection ends (a `:restart`, a crash).
const RECONNECT_DELAYS_MS: [u64; 4] = [200, 500, 1000, 2000];
const RECONNECT_ATTEMPTS: usize = 32;

async fn link_host(i: usize, spec: HostSpec, connector: Connector, links: Links, out: UnboundedSender<String>) {
    let (mut tx, mut notices) = match connector(spec.clone()).await {
        Ok(link) => link,
        Err(e) => {
            let _ = out.send(encode(&Out::Disconnected { host: i, reason: Some(format!("{e:#}")) }));
            return;
        }
    };
    let mut restart_asked = false;
    loop {
        let _ = tx.send(Request::List);
        if let Some(slot) = links.lock().unwrap().get_mut(i) {
            *slot = Some(tx.clone());
        }
        while let Some(n) = notices.recv().await {
            // A hub older than this binary is asked to drain and restart.
            if let Notice::Hello { version } = &n
                && !restart_asked
                && crate::util::compare_versions(version, env!("CARGO_PKG_VERSION")).is_lt()
            {
                restart_asked = true;
                let _ = tx.send(Request::Restart);
            }
            if out.send(encode(&Out::Notice { host: i, notice: &n })).is_err() {
                return;
            }
        }
        if let Some(slot) = links.lock().unwrap().get_mut(i) {
            *slot = None;
        }
        let _ = out.send(encode(&Out::Disconnected { host: i, reason: None }));
        // Reconnect; the browser reopens its sessions when the new hub says hello.
        let mut last = String::new();
        let mut relinked = None;
        for attempt in 0..RECONNECT_ATTEMPTS {
            tokio::time::sleep(Duration::from_millis(RECONNECT_DELAYS_MS[attempt.min(RECONNECT_DELAYS_MS.len() - 1)])).await;
            if out.is_closed() {
                return;
            }
            match connector(spec.clone()).await {
                // A command that starts but never reaches a hub closes without a notice.
                Ok((t, mut n)) => match tokio::time::timeout(Duration::from_secs(15), n.recv()).await {
                    Ok(Some(first)) => {
                        relinked = Some((t, n, first));
                        break;
                    }
                    Ok(None) => last = "the hub closed the connection".into(),
                    Err(_) => last = "no answer from the hub".into(),
                },
                Err(e) => last = format!("{e:#}"),
            }
        }
        let Some((t, n, first)) = relinked else {
            let _ = out.send(encode(&Out::Disconnected { host: i, reason: Some(format!("could not reconnect: {last}")) }));
            return;
        };
        if out.send(encode(&Out::Notice { host: i, notice: &first })).is_err() {
            return;
        }
        (tx, notices) = (t, n);
    }
}

/// The page: its own scripts and styles, the fonts, its WebSocket, and diagram frames.
fn page_csp(port: u16) -> String {
    let [a, b] = allowed_hosts(port);
    format!(
        "default-src 'none'; script-src 'self'; style-src 'self'; font-src 'self'; \
         img-src 'self' data:; connect-src ws://{a} ws://{b}; frame-src http://*.localhost:{port}/frame; \
         base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    )
}

fn with_csp(mut r: Response, csp: &str) -> Response {
    if let Ok(v) = HeaderValue::from_str(csp) {
        r.headers_mut().insert(header::CONTENT_SECURITY_POLICY, v);
    }
    r
}

/// A diagram frame. The page loads each from its own `<label>.localhost` name, so each is its
/// own site, so the browser runs it in its own process and a runaway script freezes neither the
/// page nor the other diagrams. Its CSP allows D3 and the bootstrap
/// script from this server, `eval` for the diagram itself, and no network at all; only the
/// page may embed it.
async fn frame(State(s): State<Arc<Shared>>, headers: HeaderMap) -> Response {
    let host = headers.get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let [a, b] = allowed_hosts(s.port);
    let csp = format!(
        "default-src 'none'; script-src http://{host}/d3.js http://{host}/frame.js 'unsafe-eval'; style-src 'unsafe-inline'; \
         font-src http://{host}/fonts/; img-src data: blob:; base-uri 'none'; form-action 'none'; frame-ancestors http://{a} http://{b}"
    );
    with_csp(asset(State(s), "frame.html").await, &csp)
}

/// DejaVu Sans Mono (license in `web/dist/fonts/LICENSE`), for the page and the diagram frames.
/// Frames have an opaque origin, so fonts are fetched cross-origin and need CORS.
async fn font(State(s): State<Arc<Shared>>, axum::extract::Path(name): axum::extract::Path<String>) -> Response {
    let embedded: Option<&'static [u8]> = match name.as_str() {
        "DejaVuSansMono.woff2" => Some(include_bytes!("../web/dist/fonts/DejaVuSansMono.woff2")),
        "DejaVuSansMono-Bold.woff2" => Some(include_bytes!("../web/dist/fonts/DejaVuSansMono-Bold.woff2")),
        "DejaVuSansMono-Oblique.woff2" => Some(include_bytes!("../web/dist/fonts/DejaVuSansMono-Oblique.woff2")),
        "DejaVuSansMono-BoldOblique.woff2" => Some(include_bytes!("../web/dist/fonts/DejaVuSansMono-BoldOblique.woff2")),
        "LICENSE" => Some(include_bytes!("../web/dist/fonts/LICENSE")),
        _ => None,
    };
    let Some(embedded) = embedded else { return StatusCode::NOT_FOUND.into_response() };
    let body = match &s.settings.assets_dir {
        Some(dir) => match tokio::fs::read(dir.join("fonts").join(&name)).await {
            Ok(b) => b,
            Err(e) => return (StatusCode::NOT_FOUND, format!("{name}: {e}")).into_response(),
        },
        None => embedded.to_vec(),
    };
    let kind = if name.ends_with(".woff2") { "font/woff2" } else { "text/plain; charset=utf-8" };
    (
        [
            (header::CONTENT_TYPE, kind),
            (header::CACHE_CONTROL, "max-age=86400"),
            (header::ACCESS_CONTROL_ALLOW_ORIGIN, "*"),
            (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        ],
        body,
    )
        .into_response()
}
