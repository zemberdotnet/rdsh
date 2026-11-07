use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context, Result, bail};
use rdsh::{
    config::{Config, HostConfig},
    hub::{Factory, Hub, OpenKind, Request, net},
    deepseek::DeepSeek,
    headless,
    llm::{Message, Provider},
    session::{self, Start},
    trace, tui,
};

const HELP: &str = "\
rdsh — terminal coding agent for DeepSeek models

USAGE:
    rdsh [OPTIONS]                 interactive TUI
    rdsh run [OPTIONS] <PROMPT>    run one turn headless (stdout: answer, stderr: progress)
    rdsh login                     store a DeepSeek API key (~/.config/rdsh/credentials.json)
    rdsh logout                    remove the stored key
    rdsh serve [--socket PATH]     run the session hub in the foreground (rdsh starts it on demand)
    rdsh connect [--socket PATH]   relay stdin/stdout to the local hub (used over SSH)
    rdsh sessions [--all]          list recorded sessions
    rdsh trace [ID|last]           print a session trace as readable text

OPTIONS:
    -m, --model <MODEL>    model id (default: deepseek-chat, env RDSH_MODEL)
    -r, --resume <ID>      resume a session by id, prefix, or `last`
    -C, --cwd <DIR>        working directory (default: current)
        --host <DEST[:DIR]>  also attach to the hub on an SSH host (repeatable; or a config host name)
        --no-daemon        run sessions inside this process instead of the background hub
        --socket <PATH>    hub socket (default $XDG_RUNTIME_DIR/rdsh/hub.sock)
    -h, --help             show this help
    -V, --version          show version

ENVIRONMENT:
    DEEPSEEK_API_KEY       API key; overrides `rdsh login` (also read from ./.env)
    DEEPSEEK_BASE_URL      API base URL (default https://api.deepseek.com)

Config file: ~/.config/rdsh/config.json (see README).";

struct Args {
    command: Option<String>,
    positional: Vec<String>,
    model: Option<String>,
    resume: Option<String>,
    cwd: Option<PathBuf>,
    all: bool,
    hosts: Vec<String>,
    no_daemon: bool,
    socket: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>> {
    let mut a = Args {
        command: None,
        positional: Vec::new(),
        model: None,
        resume: None,
        cwd: None,
        all: false,
        hosts: Vec::new(),
        no_daemon: false,
        socket: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().with_context(|| format!("{name} needs a value"));
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("rdsh {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "-m" | "--model" => a.model = Some(value("--model")?),
            "-r" | "--resume" => a.resume = Some(value("--resume")?),
            "-C" | "--cwd" => a.cwd = Some(PathBuf::from(value("--cwd")?)),
            "--all" => a.all = true,
            "--host" => a.hosts.push(value("--host")?),
            "--no-daemon" => a.no_daemon = true,
            "--socket" => a.socket = Some(PathBuf::from(value("--socket")?)),
            "--" => a.positional.extend(it.by_ref()),
            s if s.starts_with('-') && s.len() > 1 => bail!("unknown option {s} (see --help)"),
            "run" | "sessions" | "trace" | "login" | "logout" | "serve" | "connect" if a.command.is_none() && a.positional.is_empty() => {
                a.command = Some(arg.clone())
            }
            _ => a.positional.push(arg.clone()),
        }
    }
    Ok(Some(a))
}

fn main() -> ExitCode {
    match real_main() {
        Ok(code) => ExitCode::from(code.clamp(0, 255) as u8),
        Err(e) => {
            eprintln!("rdsh: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn real_main() -> Result<i32> {
    let Some(args) = parse_args()? else { return Ok(0) };
    let cwd = match &args.cwd {
        Some(d) => d.canonicalize().with_context(|| format!("--cwd {}", d.display()))?,
        None => std::env::current_dir()?,
    };
    let mut cfg = Config::load(&cwd)?;
    if let Some(m) = &args.model {
        cfg.model = m.clone();
    }
    match args.command.as_deref() {
        Some("login") => runtime()?.block_on(rdsh::auth::login(&cfg.base_url)),
        Some("logout") => {
            if rdsh::auth::logout()? {
                eprintln!("removed {}", rdsh::auth::credentials_path().display());
            } else {
                eprintln!("not logged in");
            }
            Ok(0)
        }
        Some("sessions") => {
            for s in trace::list_sessions(&cfg.sessions_dir()) {
                if s.parent.is_some() && !args.all {
                    continue;
                }
                println!("{}  {}", s.id, s.title);
            }
            Ok(0)
        }
        Some("trace") => {
            let q = args.positional.first().map_or("last", String::as_str);
            print_trace(&trace::find_session(&cfg.sessions_dir(), q)?)?;
            Ok(0)
        }
        Some("run") => {
            let prompt = args.positional.join(" ");
            if prompt.trim().is_empty() {
                bail!("rdsh run needs a prompt");
            }
            // Headless output usually feeds a script or another agent: no length routing.
            cfg.length_routing = false;
            runtime()?.block_on(async {
                let provider: Arc<dyn Provider> = Arc::new(DeepSeek::new(&cfg)?);
                let start = args.resume.as_deref().map_or(Start::New, Start::Resume);
                let agent = session::build(&cfg, &cwd, provider, start)?;
                Ok(headless::run(agent, prompt).await)
            })
        }
        Some("serve") => {
            let socket = args.socket.clone().unwrap_or_else(net::default_socket);
            let provider: Arc<dyn Provider> = Arc::new(DeepSeek::new(&cfg)?);
            let idle = (cfg.serve_idle_secs > 0).then(|| std::time::Duration::from_secs(cfg.serve_idle_secs));
            let sessions_dir = cfg.sessions_dir();
            let factory = factory(cfg, cwd, provider);
            runtime()?.block_on(async {
                net::serve(Hub::new(factory, Some(sessions_dir)), &socket, idle).await?;
                Ok(0)
            })
        }
        Some("connect") => {
            let socket = args.socket.clone().unwrap_or_else(net::default_socket);
            let exe = std::env::current_exe().context("locating the rdsh binary")?;
            runtime()?.block_on(async {
                net::bridge(&socket, &exe).await?;
                Ok(0)
            })
        }
        _ => {
            if let Some(p) = args.positional.first() {
                bail!("unexpected argument {p:?} (did you mean `rdsh run`?)");
            }
            // Fail before the TUI starts when the session to resume does not exist.
            let first = match args.resume.as_deref() {
                Some(q) => {
                    let path = trace::find_session(&cfg.sessions_dir(), q)?;
                    let id = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
                    Request::Open { session_id: id }
                }
                None => Request::New { cwd: Some(cwd.display().to_string()) },
            };
            let mut remotes = cfg.hosts.clone();
            for spec in &args.hosts {
                let h = match cfg.hosts.iter().find(|h| h.name == *spec) {
                    Some(known) => known.clone(),
                    None => HostConfig::parse(spec)?,
                };
                if !remotes.iter().any(|r| r.name == h.name) {
                    remotes.push(h);
                }
            }
            let ssh_program = cfg.ssh_program.clone();
            let remote_command = cfg.remote_command.clone();
            let socket = args.socket.clone().unwrap_or_else(net::default_socket);
            let no_daemon = args.no_daemon;
            runtime()?.block_on(async {
                let (req_tx, notices) = if no_daemon {
                    let provider: Arc<dyn Provider> = Arc::new(DeepSeek::new(&cfg)?);
                    let sessions_dir = cfg.sessions_dir();
                    Hub::new(factory(cfg, cwd.clone(), provider), Some(sessions_dir)).connect()
                } else {
                    if net::connect_socket(&socket).await.is_err() {
                        // Starting the hub: check the key here so the error is readable.
                        DeepSeek::new(&cfg)?;
                        let exe = std::env::current_exe().context("locating the rdsh binary")?;
                        net::ensure_server(&socket, &exe).await?;
                    }
                    net::connect_socket(&socket).await?
                };
                let mut links = vec![tui::HostLink { name: "local".into(), local: true, dir: None, req_tx, notices }];
                for h in remotes {
                    let mut ssh_args = vec!["-T".to_owned(), h.ssh.clone()];
                    ssh_args.extend(remote_command.iter().cloned());
                    let (req_tx, notices) = net::connect_command(&ssh_program, &ssh_args)?;
                    links.push(tui::HostLink { name: h.name, local: false, dir: h.dir.map(PathBuf::from), req_tx, notices });
                }
                tui::run(links, cwd, first).await?;
                Ok(0)
            })
        }
    }
}

/// Builds agents for a hub: new sessions in the requested directory (default `default_cwd`),
/// resumed ones in the directory recorded in their trace.
fn factory(cfg: Config, default_cwd: PathBuf, provider: Arc<dyn Provider>) -> Factory {
    Arc::new(move |kind| match kind {
        OpenKind::New(dir) => {
            let dir = dir.unwrap_or_else(|| default_cwd.clone());
            if !dir.is_dir() {
                bail!("{} is not a directory on this host", dir.display());
            }
            session::build(&cfg, &dir, provider.clone(), Start::New)
        }
        OpenKind::Resume(id) => {
            let path = trace::find_session(&cfg.sessions_dir(), &id)?;
            let recorded = trace::load(&path)?.cwd;
            let dir = if recorded.is_empty() { default_cwd.clone() } else { PathBuf::from(recorded) };
            session::build(&cfg, &dir, provider.clone(), Start::Resume(&id))
        }
    })
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .context("starting async runtime")
}

fn print_trace(path: &std::path::Path) -> Result<()> {
    let loaded = trace::load(path)?;
    println!("session {}  model {}  cwd {}", loaded.session_id, loaded.model, loaded.cwd);
    if let Some(p) = &loaded.parent {
        println!("parent {p}");
    }
    for r in &loaded.records {
        match &r.event {
            trace::TraceEvent::Message { message } => match message {
                Message::System { .. } => println!("\n[system prompt]"),
                Message::User { content } => println!("\n› user\n{content}"),
                Message::Context { content } => {
                    println!("\n[runtime context] {}", content.lines().skip(2).collect::<Vec<_>>().join(" · "))
                }
                Message::Directive { content } => println!("  [directive] {content}"),
                Message::Assistant { content, reasoning, tool_calls } => {
                    if let Some(r) = reasoning {
                        println!("\n∴ reasoning ({} chars)", r.len());
                    }
                    if !content.is_empty() {
                        println!("\n● assistant\n{content}");
                    }
                    for c in tool_calls {
                        println!("  ▸ {} {}", c.name, rdsh::tools::summarize(&c.name, &c.arguments));
                    }
                }
                Message::Tool { name, content, .. } => {
                    let lines = content.lines().count();
                    let first = content.lines().next().unwrap_or("");
                    let more = if lines > 1 { format!("  (+{} lines)", lines - 1) } else { String::new() };
                    println!("    ↳ {name}: {first}{more}");
                }
            },
            trace::TraceEvent::Usage { usage } => {
                println!("    [tokens {} in / {} out]", usage.prompt_tokens, usage.completion_tokens)
            }
            trace::TraceEvent::Error { message } => println!("\n✗ error: {message}"),
            trace::TraceEvent::Cancelled => println!("\n⨯ interrupted"),
            _ => {}
        }
    }
    Ok(())
}
