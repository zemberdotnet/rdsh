use std::{path::PathBuf, process::ExitCode, sync::Arc};

use anyhow::{Context, Result, bail};
use rdsh::{
    config::Config,
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
    rdsh sessions [--all]          list recorded sessions
    rdsh trace [ID|last]           print a session trace as readable text

OPTIONS:
    -m, --model <MODEL>    model id (default: deepseek-chat, env RDSH_MODEL)
    -r, --resume <ID>      resume a session by id, prefix, or `last`
    -C, --cwd <DIR>        working directory (default: current)
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
}

fn parse_args() -> Result<Option<Args>> {
    let mut a = Args { command: None, positional: Vec::new(), model: None, resume: None, cwd: None, all: false };
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
            "--" => a.positional.extend(it.by_ref()),
            s if s.starts_with('-') && s.len() > 1 => bail!("unknown option {s} (see --help)"),
            "run" | "sessions" | "trace" | "login" | "logout" if a.command.is_none() && a.positional.is_empty() => {
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
            runtime()?.block_on(async {
                let provider: Arc<dyn Provider> = Arc::new(DeepSeek::new(&cfg)?);
                let start = args.resume.as_deref().map_or(Start::New, Start::Resume);
                let agent = session::build(&cfg, &cwd, provider, start)?;
                Ok(headless::run(agent, prompt).await)
            })
        }
        _ => {
            if let Some(p) = args.positional.first() {
                bail!("unexpected argument {p:?} (did you mean `rdsh run`?)");
            }
            runtime()?.block_on(async {
                let provider: Arc<dyn Provider> = Arc::new(DeepSeek::new(&cfg)?);
                let start = args.resume.as_deref().map_or(Start::New, Start::Resume);
                let agent = session::build(&cfg, &cwd, provider, start)?;
                tui::run(agent, cwd).await?;
                Ok(0)
            })
        }
    }
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
