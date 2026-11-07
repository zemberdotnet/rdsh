//! Interactive terminal UI.
//!
//! The agent runs in its own task and reports through an unbounded channel; the UI loop
//! coalesces queued events before each redraw, so streaming cost scales with frames, not tokens.

pub mod app;
pub mod fileview;
pub mod highlight;
pub mod markdown;
pub mod theme;
pub mod input;
pub mod motion;
pub mod render;
pub mod transcript;

use std::{io::Write, path::PathBuf, sync::Arc, time::Duration};

use anyhow::Result;
use crossterm::{
    event::{
        DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture, EventStream,
        KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
    },
    execute,
};
use futures_util::StreamExt;
use tokio::sync::mpsc;

use crate::agent::{Agent, AgentEvent, EventSink};
use app::{AgentCmd, App};

/// Runs the TUI until the user quits.
///
/// @param agent the session's agent; its history (when resumed) is shown first.
/// @param cwd working directory used to resolve file links.
pub async fn run(mut agent: Agent, cwd: PathBuf) -> Result<()> {
    let (ev_tx, mut ev_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<AgentCmd>();
    let mut app = App::new(cwd, agent.model().to_owned(), agent.session_id().unwrap_or("").to_owned(), cmd_tx);
    app.load_history(agent.history());

    let sink: EventSink = Arc::new(move |ev| {
        // The receiver only closes when the UI exits; dropping late events is fine then.
        let _ = ev_tx.send(ev);
    });
    let agent_task = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                AgentCmd::Turn { text, cancel } => {
                    agent.run_turn(text, sink.clone(), cancel).await;
                }
            }
        }
    });

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

    let result = event_loop(&mut terminal, &mut app, &mut ev_rx).await;

    if enhanced {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(out, DisableMouseCapture, DisableBracketedPaste);
    ratatui::restore();
    agent_task.abort();
    if !app.session_id.is_empty() {
        println!("session {} — resume with: rdsh --resume {}", app.session_id, app.session_id);
    }
    result
}

async fn event_loop(
    terminal: &mut ratatui::DefaultTerminal,
    app: &mut App,
    ev_rx: &mut mpsc::UnboundedReceiver<AgentEvent>,
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
        tokio::select! {
            ev = events.next() => match ev {
                Some(Ok(ev)) => {
                    app.on_event(ev);
                }
                Some(Err(e)) => return Err(e.into()),
                None => return Ok(()),
            },
            Some(ev) = ev_rx.recv() => {
                app.on_agent(ev);
                while let Ok(ev) = ev_rx.try_recv() {
                    app.on_agent(ev);
                }
            }
            _ = tick.tick(), if app.running => {
                app.spinner = app.spinner.wrapping_add(1);
            }
        }
    }
}
#[cfg(test)]
mod tests;
