//! Interactive terminal UI.
//!
//! The TUI is a client of the session hub: it sends requests and renders the notices it gets
//! back. The loop coalesces queued notices before each redraw, so streaming cost scales with
//! frames, not tokens.

pub mod app;
pub mod fileview;
pub mod highlight;
pub mod markdown;
pub mod theme;
pub mod input;
pub mod motion;
pub mod render;
pub mod sidebar;
pub mod transcript;

use std::{io::Write, path::PathBuf, time::Duration};

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::StreamExt;
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use crate::{
    hub::{Notice, Request},
    llm::Effort,
};
use app::{App, Host};

/// One hub connection for the TUI.
pub struct HostLink {
    pub name: String,
    /// The hub runs on this machine (its sessions' files can be read directly).
    pub local: bool,
    /// Directory for new sessions there; also filters the rail.
    pub dir: Option<PathBuf>,
    pub req_tx: UnboundedSender<Request>,
    pub notices: UnboundedReceiver<Notice>,
}

/// Runs the TUI until the user quits.
///
/// @param links hub connections; the first one shows the startup session.
/// @param cwd working directory used for local file links and to filter the local rail.
/// @param first `Request::New` or `Request::Open` for the first host.
/// @param effort thinking effort turns start with.
/// @param models ids `:model` lists.
pub async fn run(links: Vec<HostLink>, cwd: PathBuf, first: Request, effort: Effort, models: Vec<String>) -> Result<()> {
    // One stream for all hosts; `None` marks a host whose connection ended.
    let (merged_tx, mut notices) = mpsc::unbounded_channel::<(usize, Option<Notice>)>();
    let mut hosts = Vec::new();
    for (i, link) in links.into_iter().enumerate() {
        // A closed hub surfaces when its notices end (below) and as a flash on later requests.
        let _ = link.req_tx.send(Request::List);
        if i == 0 {
            let _ = link.req_tx.send(first.clone());
        }
        let tx = merged_tx.clone();
        let mut rx = link.notices;
        tokio::spawn(async move {
            while let Some(n) = rx.recv().await {
                if tx.send((i, Some(n))).is_err() {
                    return;
                }
            }
            let _ = tx.send((i, None));
        });
        hosts.push(Host::new(link.name, link.local, link.dir, link.req_tx));
    }
    let mut app = App::new(cwd, hosts);
    app.effort = effort;
    app.models = models;
    app.sidebar_open = true;
    app.expect_open(0);

    let mut terminal = ratatui::init();
    let mut out = std::io::stdout();
    execute!(out, EnableMouseCapture, EnableBracketedPaste)?;
    let enhanced = crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false);
    if enhanced {
        execute!(out, PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES))?;
    }
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = execute!(std::io::stdout(), DisableMouseCapture, DisableBracketedPaste);
        prev_hook(info);
    }));

    let result = event_loop(&mut terminal, &mut app, &mut notices).await;

    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    for (host, id) in app.open_sessions() {
        if host == "local" {
            println!("session {id} — resume with: rdsh --resume {id}");
        } else {
            println!("session {id} on {host}");
        }
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    notices: &mut UnboundedReceiver<(usize, Option<Notice>)>,
) -> Result<()> {
    let mut events = EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // Every wake-up below changes visible state, so each iteration redraws.
    loop {
        terminal.draw(|f| app.draw(f))?;
        if let Some(seq) = app.osc_out.take() {
            let mut out = std::io::stdout();
            out.write_all(seq.as_bytes())?;
            out.flush()?;
        }
        if app.quit {
            return Ok(());
        }
        let animating = app.cur.running || app.hosts.iter().any(|h| !h.running.is_empty());
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    app.on_event(ev);
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            Some((host, n)) = notices.recv() => {
                deliver(app, host, n);
                while let Ok((host, n)) = notices.try_recv() {
                    deliver(app, host, n);
                }
            }
            _ = tick.tick(), if animating => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }
}
fn deliver(app: &mut App, host: usize, n: Option<Notice>) {
    match n {
        Some(n) => app.on_notice(host, n),
        None => app.on_disconnect(host),
    }
}

#[cfg(test)]
mod tests;
