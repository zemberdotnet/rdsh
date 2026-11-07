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
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender};

use crate::hub::{Notice, Request};
use app::App;

/// Runs the TUI until the user quits.
///
/// @param req_tx / notices the client end of a hub connection.
/// @param cwd working directory used to resolve file links and filter the session rail.
/// @param first `Request::New` or `Request::Open`, the session shown at startup.
pub async fn run(
    req_tx: UnboundedSender<Request>,
    mut notices: UnboundedReceiver<Notice>,
    cwd: PathBuf,
    first: Request,
) -> Result<()> {
    let mut app = App::new(cwd, req_tx.clone());
    app.sidebar_open = true;
    app.expect_open();
    // A closed hub surfaces as a flash on the first request the user makes.
    let _ = req_tx.send(Request::List);
    let _ = req_tx.send(first);

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
    for id in app.open_sessions() {
        println!("session {id} — resume with: rdsh --resume {id}");
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    notices: &mut UnboundedReceiver<Notice>,
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
        let animating = app.cur.running || !app.hub_running.is_empty();
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    app.on_event(ev);
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            Some(n) = notices.recv() => {
                app.on_notice(n);
                while let Ok(n) = notices.try_recv() {
                    app.on_notice(n);
                }
            }
            _ = tick.tick(), if animating => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }
}
#[cfg(test)]
mod tests;
