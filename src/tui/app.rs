//! TUI state: modes, views, key and mouse handling, and folding agent events into the transcript.

use std::{collections::HashMap, path::PathBuf, time::Instant};

use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::mpsc::UnboundedSender;
use tokio_util::sync::CancellationToken;

use super::{
    fileview::FileView,
    input::{Input, InputAction},
    motion::{self, Cursor, Lines, Motion, Pos},
    render::FILE_GUTTER,
    sidebar::{Live, LiveSub, Sidebar},
    transcript::{GUTTER, Kind, ToolEntry, ToolStatus, Transcript},
};
use crate::{
    agent::AgentEvent,
    length::Routing,
    comments::{Anchor, Comment, compose_message},
    diff,
    llm::{Message, Usage},
    tools::{resolve_path, summarize},
    util::base64,
};

/// Requests from the UI to the agent task.
pub enum AgentCmd {
    Turn { text: String, cancel: CancellationToken },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Normal,
    Insert,
    /// `line`: linewise (`V`) vs charwise (`v`). `anchor` is where the selection started.
    Visual { line: bool, anchor: Pos },
    Comment,
    Search { forward: bool },
    Command,
}

pub enum View {
    Chat,
    File(Box<FileView>),
    Queue { sel: usize },
    Files { items: Vec<(String, String)>, sel: usize },
    Help,
}

/// The buffer that motions act on in the current view.
struct Active<'a> {
    lines: &'a dyn Lines,
    cursor: &'a mut Cursor,
    scroll: &'a mut usize,
    blocks: &'a [usize],
}

pub struct App {
    pub(crate) transcript: Transcript,
    pub(crate) chat_cursor: Cursor,
    pub(crate) chat_scroll: usize,
    /// Keep the cursor pinned to the newest row while output streams.
    pub(crate) follow: bool,
    pub(crate) view: View,
    pub(crate) mode: Mode,
    mode_before_search: Mode,
    pub(crate) prompt: Input,
    pub(crate) comment: Input,
    pub(crate) minibuf: Input,
    pub(crate) comment_target: Option<Anchor>,
    pub(crate) editing: Option<usize>,
    pub(crate) queue: Vec<Comment>,
    count: Option<usize>,
    pending: Option<char>,
    last_search: Option<(String, bool)>,
    pub(crate) running: bool,
    cancel: Option<CancellationToken>,
    cmd_tx: UnboundedSender<AgentCmd>,
    pub(crate) queued_send: Option<String>,
    pub(crate) usage: Usage,
    pub(crate) model: String,
    pub(crate) session_id: String,
    pub(crate) flash: Option<String>,
    /// The last turn's response-length pick, shown in the status line.
    pub(crate) length: Option<String>,
    pub(crate) activity: Option<String>,
    pub quit: bool,
    ctrl_c_armed: bool,
    pub(crate) main_area: Rect,
    pub(crate) prompt_area: Rect,
    /// Where the session rail is drawn; `None` when it is hidden.
    pub(crate) sidebar_area: Option<Rect>,
    /// Sessions with subagents nested beneath them.
    pub(crate) sidebar: Sidebar,
    /// Subagents this process saw start, keyed by their session id.
    pub(crate) subagent_live: HashMap<String, LiveSub>,
    /// Tool call id → subagent session id, for `task` calls in flight (from `SubagentStarted`).
    pub(crate) subagent_calls: HashMap<String, String>,
    /// Keys go to the rail instead of the main view.
    pub(crate) sidebar_focus: bool,
    /// Selected rail row.
    pub(crate) sidebar_sel: usize,
    /// First visible rail row.
    pub(crate) sidebar_scroll: usize,
    /// Where traces live; the rail is rebuilt from here.
    pub(crate) sessions_dir: Option<PathBuf>,
    /// Whether the session rail is shown.
    pub(crate) sidebar_open: bool,
    history: Vec<String>,
    hist_pos: Option<usize>,
    pub spinner: usize,
    drag_anchor: Option<Pos>,
    streaming_text: Option<usize>,
    streaming_reasoning: Option<usize>,
    tool_entries: HashMap<String, usize>,
    pub(crate) turn_started: Instant,
    /// Text written to the terminal outside ratatui (OSC 52 clipboard) on the next draw.
    pub(crate) osc_out: Option<String>,
}

impl App {
    pub fn new(cwd: PathBuf, model: String, session_id: String, cmd_tx: UnboundedSender<AgentCmd>) -> Self {
        Self {
            transcript: Transcript::new(cwd),
            chat_cursor: Cursor::default(),
            chat_scroll: 0,
            follow: true,
            view: View::Chat,
            mode: Mode::Insert,
            mode_before_search: Mode::Normal,
            prompt: Input::default(),
            comment: Input::default(),
            minibuf: Input::default(),
            comment_target: None,
            editing: None,
            queue: Vec::new(),
            count: None,
            pending: None,
            last_search: None,
            running: false,
            cancel: None,
            cmd_tx,
            queued_send: None,
            usage: Usage::default(),
            model,
            session_id,
            flash: None,
            length: None,
            activity: None,
            quit: false,
            ctrl_c_armed: false,
            main_area: Rect::default(),
            prompt_area: Rect::default(),
            sidebar_area: None,
            sidebar: Default::default(),
            subagent_live: HashMap::new(),
            subagent_calls: HashMap::new(),
            sidebar_focus: false,
            sidebar_sel: 0,
            sidebar_scroll: 0,
            sessions_dir: None,
            sidebar_open: false,
            history: Vec::new(),
            hist_pos: None,
            spinner: 0,
            drag_anchor: None,
            streaming_text: None,
            streaming_reasoning: None,
            tool_entries: HashMap::new(),
            turn_started: Instant::now(),
            osc_out: None,
        }
    }

    /// Rebuilds the transcript from a resumed session's history.
    pub fn load_history(&mut self, history: &[Message]) {
        for m in history {
            match m {
                Message::System { .. } | Message::Context { .. } | Message::Directive { .. } => {}
                Message::User { content } => {
                    self.transcript.push(Kind::User, content.clone());
                    self.history.push(content.clone());
                }
                Message::Assistant { content, reasoning, tool_calls } => {
                    if let Some(r) = reasoning {
                        self.transcript.push(Kind::Reasoning, r.clone());
                    }
                    if !content.is_empty() {
                        self.transcript.push(Kind::Assistant, content.clone());
                    }
                    for c in tool_calls {
                        let i = self.transcript.push(Kind::Tool(Box::new(tool_entry(&c.id, &c.name, &c.arguments))), "");
                        self.tool_entries.insert(c.id.clone(), i);
                    }
                }
                Message::Tool { call_id, content, .. } => {
                    if let Some(&i) = self.tool_entries.get(call_id) {
                        let failed = content.starts_with("Error:");
                        self.finish_tool(i, content.clone(), failed, None);
                    }
                }
            }
        }
        if history.iter().any(|m| matches!(m, Message::User { .. })) {
            self.transcript.push(Kind::Info, format!("resumed session {}", self.session_id));
            self.mode = Mode::Insert;
        }
    }

    /// Rebuilds the session rail from the traces on disk.
    ///
    /// Filesystem work happens only on refresh (turn start/end, subagent lifecycle), never per frame.
    pub fn refresh_sidebar(&mut self) {
        let Some(dir) = &self.sessions_dir else { return };
        let infos = crate::trace::list_sessions(dir);
        let selected = self.sidebar.rows().get(self.sidebar_sel).map(|r| r.id.clone());
        let cwd = self.transcript.cwd().display().to_string();
        self.sidebar = Sidebar::build(&infos, &cwd, &self.subagent_live);
        // Keep the selection on the same session when rows shift.
        let fallback = self.sidebar.index_of(&self.session_id).unwrap_or(0);
        self.sidebar_sel = selected.and_then(|id| self.sidebar.index_of(&id)).unwrap_or(fallback);
    }

    fn set_sidebar_focus(&mut self, focus: bool) {
        if focus && !self.sidebar_open {
            self.sidebar_open = true;
            self.refresh_sidebar();
        }
        self.sidebar_focus = focus && self.sidebar_open;
        self.mode = Mode::Normal;
    }

    /// Keys while the rail has focus. Esc, `l`, Tab or `Ctrl-w l` return to the main view.
    fn sidebar_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let n = self.sidebar.rows().len();
        let last = n.saturating_sub(1);
        let half = (self.sidebar_area.map_or(10, |a| a.height as usize) / 2).max(1);
        if self.pending.take() == Some('W') {
            if matches!(k.code, KeyCode::Char('l' | 'w') | KeyCode::Right) {
                self.sidebar_focus = false;
            }
            return;
        }
        match (k.code, ctrl) {
            (KeyCode::Char('j') | KeyCode::Down, false) => self.sidebar_sel = (self.sidebar_sel + 1).min(last),
            (KeyCode::Char('k') | KeyCode::Up, false) => self.sidebar_sel = self.sidebar_sel.saturating_sub(1),
            (KeyCode::Char('g') | KeyCode::Home, false) => self.sidebar_sel = 0,
            (KeyCode::Char('G') | KeyCode::End, false) => self.sidebar_sel = last,
            (KeyCode::Char('d'), true) | (KeyCode::PageDown, _) => self.sidebar_sel = (self.sidebar_sel + half).min(last),
            (KeyCode::Char('u'), true) | (KeyCode::PageUp, _) => self.sidebar_sel = self.sidebar_sel.saturating_sub(half),
            (KeyCode::Char('w'), true) => self.pending = Some('W'),
            (KeyCode::Enter, _) => self.preview_session(),
            (KeyCode::Char('y'), false) => {
                if let Some(r) = self.sidebar.rows().get(self.sidebar_sel) {
                    let id = r.id.clone();
                    self.osc_out = Some(format!("\x1b]52;c;{}\x07", base64(id.as_bytes())));
                    self.set_flash(format!("copied {id}"));
                }
            }
            (KeyCode::Char('r'), false) => {
                self.refresh_sidebar();
                self.set_flash("sessions reloaded");
            }
            (KeyCode::Char('S'), false) => {
                self.sidebar_open = false;
                self.sidebar_focus = false;
            }
            (KeyCode::Char('i' | 'a'), false) => {
                self.sidebar_focus = false;
                self.mode = Mode::Insert;
            }
            (KeyCode::Char(':'), false) => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            (KeyCode::Esc | KeyCode::Tab | KeyCode::Char('l' | 'q') | KeyCode::Right, _) => self.sidebar_focus = false,
            _ => {}
        }
    }

    /// Enter on a rail row: says what the row is and how to open it. Switching sessions in
    /// place is not supported yet; resuming happens from the command line.
    fn preview_session(&mut self) {
        let Some(row) = self.sidebar.rows().get(self.sidebar_sel).cloned() else { return };
        let msg = if row.id == self.session_id {
            "this is the current session".to_owned()
        } else if row.depth == 0 {
            format!("resume: rdsh --resume {} · y copies the id", row.id)
        } else {
            format!("subagent trace: rdsh trace {} · y copies the id", row.id)
        };
        self.set_flash(msg);
    }

    pub(crate) fn set_flash(&mut self, s: impl Into<String>) {
        self.flash = Some(s.into());
    }

    // ---------------------------------------------------------------- events

    pub fn on_event(&mut self, ev: Event) {
        match ev {
            Event::Key(k) if k.kind != KeyEventKind::Release => self.on_key(k),
            Event::Mouse(m) => self.on_mouse(m),
            Event::Paste(s) => match self.mode {
                Mode::Comment => self.comment.insert_str(&s),
                Mode::Search { .. } | Mode::Command => self.minibuf.insert_str(&s.replace('\n', " ")),
                _ => {
                    self.mode = Mode::Insert;
                    self.prompt.insert_str(&s);
                }
            },
            _ => {}
        }
    }

    pub fn on_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && k.code == KeyCode::Char('c') {
            self.ctrl_c();
            return;
        }
        if ctrl && k.code == KeyCode::Enter {
            self.send_now();
            return;
        }
        self.ctrl_c_armed = false;
        self.flash = None;
        match self.mode {
            Mode::Insert => self.insert_key(k),
            Mode::Comment => self.comment_key(k),
            Mode::Search { forward } => self.search_key(k, forward),
            Mode::Command => self.command_key(k),
            Mode::Normal if self.sidebar_focus => self.sidebar_key(k),
            Mode::Normal | Mode::Visual { .. } => match self.view {
                View::Chat | View::File(_) => self.buffer_key(k),
                View::Queue { .. } => self.queue_key(k),
                View::Files { .. } => self.files_key(k),
                View::Help => self.view = View::Chat,
            },
        }
    }

    fn ctrl_c(&mut self) {
        if self.running {
            if let Some(c) = &self.cancel {
                c.cancel();
            }
            self.set_flash("interrupting…");
            return;
        }
        let cleared = match self.mode {
            Mode::Insert if !self.prompt.text.is_empty() => {
                self.prompt.take();
                true
            }
            Mode::Comment | Mode::Search { .. } | Mode::Command | Mode::Visual { .. } => {
                self.mode = Mode::Normal;
                true
            }
            _ => false,
        };
        if cleared {
            return;
        }
        if self.ctrl_c_armed {
            self.quit = true;
        } else {
            self.ctrl_c_armed = true;
            self.set_flash("press Ctrl-C again to quit");
        }
    }

    fn insert_key(&mut self, k: KeyEvent) {
        match self.prompt.handle(k) {
            InputAction::Submit => self.submit(),
            InputAction::Cancel => self.mode = Mode::Normal,
            InputAction::HistoryPrev if !self.history.is_empty() => {
                let i = self.hist_pos.map_or(self.history.len() - 1, |i| i.saturating_sub(1));
                self.hist_pos = Some(i);
                self.prompt.set(self.history[i].clone());
            }
            InputAction::HistoryNext => {
                if let Some(i) = self.hist_pos {
                    if i + 1 < self.history.len() {
                        self.hist_pos = Some(i + 1);
                        self.prompt.set(self.history[i + 1].clone());
                    } else {
                        self.hist_pos = None;
                        self.prompt.take();
                    }
                }
            }
            InputAction::Edited => self.hist_pos = None,
            _ => {}
        }
    }

    /// Ctrl-Enter from any mode: finish the comment being written, then send the queue and prompt.
    fn send_now(&mut self) {
        self.ctrl_c_armed = false;
        self.flash = None;
        match self.mode {
            Mode::Comment => self.save_comment(),
            Mode::Visual { .. } | Mode::Search { .. } | Mode::Command => self.mode = Mode::Normal,
            Mode::Normal | Mode::Insert => {}
        }
        if self.queue.is_empty() && self.prompt.is_blank() {
            self.set_flash("nothing to send — comment with c or type with i");
            return;
        }
        self.submit();
    }

    /// Sends the prompt plus all queued comments as the next user message.
    pub(crate) fn submit(&mut self) {
        let text = self.prompt.text.trim().to_owned();
        if text.is_empty() && self.queue.is_empty() {
            return;
        }
        self.prompt.take();
        if !text.is_empty() {
            self.history.push(text.clone());
        }
        self.hist_pos = None;
        let msg = compose_message(&self.queue, &text);
        self.queue.clear();
        if self.running {
            self.queued_send = Some(match self.queued_send.take() {
                Some(prev) => format!("{prev}\n\n{msg}"),
                None => msg,
            });
            self.set_flash("queued — sends when the current turn finishes");
        } else {
            self.start_turn(msg);
        }
    }

    fn start_turn(&mut self, msg: String) {
        self.transcript.push(Kind::User, msg.clone());
        let cancel = CancellationToken::new();
        if self.cmd_tx.send(AgentCmd::Turn { text: msg, cancel: cancel.clone() }).is_err() {
            self.transcript.push(Kind::Error, "the agent task has stopped; restart rdsh");
            return;
        }
        self.cancel = Some(cancel);
        self.running = true;
        self.follow = true;
        self.turn_started = Instant::now();
        self.activity = Some("waiting for model".into());
        self.view = View::Chat;
    }

    // --------------------------------------------------------- normal/visual

    fn active(&mut self) -> Option<Active<'_>> {
        match &mut self.view {
            View::Chat => Some(Active {
                lines: &self.transcript,
                cursor: &mut self.chat_cursor,
                scroll: &mut self.chat_scroll,
                blocks: self.transcript.entry_starts(),
            }),
            View::File(fv) => {
                let fv = &mut **fv;
                Some(Active { lines: &fv.texts, cursor: &mut fv.cursor, scroll: &mut fv.scroll, blocks: &fv.blocks })
            }
            _ => None,
        }
    }

    fn page(&self) -> usize {
        let h = self.main_area.height as usize;
        if matches!(self.view, View::File(_)) { h.saturating_sub(2).max(1) } else { h.max(1) }
    }

    fn cursor_pos(&mut self) -> Pos {
        self.active().map_or_else(Pos::default, |a| a.cursor.pos)
    }

    pub(crate) fn motion(&mut self, m: Motion) {
        let count = self.count.take().unwrap_or(1);
        if let Some(a) = self.active() {
            motion::apply(a.cursor, m, a.lines, a.blocks, count);
        }
        self.update_follow();
    }

    fn update_follow(&mut self) {
        if matches!(self.view, View::Chat) {
            self.follow = self.chat_cursor.pos.row + 1 >= self.transcript.total();
        }
    }

    /// Scrolls the viewport by `d` rows, dragging the cursor along when it leaves the view.
    fn scroll_by(&mut self, d: isize) {
        let page = self.page();
        if let Some(a) = self.active() {
            let n = a.lines.count();
            let max = n.saturating_sub(1);
            *a.scroll = ((*a.scroll as isize + d).max(0) as usize).min(max);
            let row = a.cursor.pos.row.clamp(*a.scroll, (*a.scroll + page).saturating_sub(1).min(max));
            if row != a.cursor.pos.row {
                motion::apply(a.cursor, Motion::Row(row), a.lines, &[], 1);
            }
        }
        self.update_follow();
    }

    fn buffer_key(&mut self, k: KeyEvent) {
        let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
        let visual = matches!(self.mode, Mode::Visual { .. });
        let in_file = matches!(self.view, View::File(_));
        if let Some(p) = self.pending.take() {
            self.pending_key(p, k);
            return;
        }
        if let KeyCode::Char(c) = k.code
            && !ctrl
            && c.is_ascii_digit()
            && (c != '0' || self.count.is_some())
        {
            let d = c.to_digit(10).unwrap_or(0) as usize;
            self.count = Some(self.count.unwrap_or(0).saturating_mul(10).saturating_add(d).min(100_000));
            return;
        }
        let page = self.page() as isize;
        match (k.code, ctrl) {
            (KeyCode::Char('j') | KeyCode::Down, false) => self.motion(Motion::Down),
            (KeyCode::Char('k') | KeyCode::Up, false) => self.motion(Motion::Up),
            (KeyCode::Char('h') | KeyCode::Left, false) => self.motion(Motion::Left),
            (KeyCode::Char('l') | KeyCode::Right, false) => self.motion(Motion::Right),
            (KeyCode::Char('w'), false) => self.motion(Motion::WordForward),
            (KeyCode::Char('b'), false) => self.motion(Motion::WordBack),
            (KeyCode::Char('e'), false) => self.motion(Motion::WordEnd),
            (KeyCode::Char('0') | KeyCode::Home, false) => self.motion(Motion::LineStart),
            (KeyCode::Char('^'), false) => self.motion(Motion::FirstNonBlank),
            (KeyCode::Char('$') | KeyCode::End, false) => self.motion(Motion::LineEnd),
            (KeyCode::Char('G'), false) => match self.count.take() {
                Some(n) => self.motion(Motion::Row(n.saturating_sub(1))),
                None => self.motion(Motion::Bottom),
            },
            (KeyCode::Char('g'), false) => self.pending = Some('g'),
            (KeyCode::Char('z'), false) => self.pending = Some('z'),
            (KeyCode::Char(']'), false) => self.pending = Some(']'),
            (KeyCode::Char('['), false) => self.pending = Some('['),
            (KeyCode::Char('}'), false) => self.motion(Motion::NextBlock),
            (KeyCode::Char('{'), false) => self.motion(Motion::PrevBlock),
            (KeyCode::Char('d'), true) => {
                self.motion(Motion::Rows(page / 2));
                self.scroll_by(page / 2);
            }
            (KeyCode::Char('u'), true) => {
                self.motion(Motion::Rows(-page / 2));
                self.scroll_by(-page / 2);
            }
            (KeyCode::Char('f'), true) | (KeyCode::PageDown, _) => {
                self.motion(Motion::Rows(page));
                self.scroll_by(page);
            }
            (KeyCode::Char('b'), true) | (KeyCode::PageUp, _) => {
                self.motion(Motion::Rows(-page));
                self.scroll_by(-page);
            }
            (KeyCode::Char('e'), true) => self.scroll_by(1),
            (KeyCode::Char('y'), true) => self.scroll_by(-1),
            (KeyCode::Char('H'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s));
            }
            (KeyCode::Char('M'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s + self.page() / 2));
            }
            (KeyCode::Char('L'), false) => {
                let s = self.active().map_or(0, |a| *a.scroll);
                self.motion(Motion::Row(s + self.page().saturating_sub(1)));
            }
            (KeyCode::Char('/'), false) => self.start_search(true),
            (KeyCode::Char('?'), false) => self.start_search(false),
            (KeyCode::Char('n'), false) => self.repeat_search(false),
            (KeyCode::Char('N'), false) => self.repeat_search(true),
            (KeyCode::Char('v'), false) => self.toggle_visual(false),
            (KeyCode::Char('V'), false) => self.toggle_visual(true),
            (KeyCode::Char('o'), false) if visual => {
                if let Mode::Visual { line, anchor } = self.mode {
                    let cur = self.cursor_pos();
                    self.mode = Mode::Visual { line, anchor: cur };
                    if let Some(a) = self.active() {
                        a.cursor.jump(anchor, a.lines);
                    }
                }
            }
            (KeyCode::Char('y'), false) if visual => self.yank_selection(),
            (KeyCode::Char('c'), false) => self.begin_comment(),
            (KeyCode::Enter, _) if visual || in_file => self.begin_comment(),
            (KeyCode::Enter, _) => {
                if !self.open_link_at_cursor(true) && !self.toggle_collapse() {
                    self.open_link_at_cursor(false);
                }
            }
            (KeyCode::Tab, _) if in_file => {
                if let View::File(fv) = &mut self.view {
                    fv.toggle_full();
                }
            }
            (KeyCode::Tab, _) => {
                self.toggle_collapse();
            }
            (KeyCode::Esc, _) if visual => self.mode = Mode::Normal,
            (KeyCode::Esc | KeyCode::Char('q'), _) if in_file => self.close_view(),
            (KeyCode::Char('r'), false) if in_file => {
                let cwd = self.transcript.cwd().to_owned();
                if let View::File(fv) = &mut self.view {
                    fv.reload(&cwd);
                }
                self.set_flash("reloaded");
            }
            (KeyCode::Char('i' | 'a' | 'A' | 'I'), false) => self.mode = Mode::Insert,
            (KeyCode::Char(':'), false) => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            (KeyCode::Char('Q'), false) => self.open_queue(),
            (KeyCode::Char('F'), false) => self.open_files(),
            (KeyCode::Char('S'), false) => {
                self.sidebar_open = !self.sidebar_open;
                self.sidebar_focus = false;
                if self.sidebar_open {
                    self.refresh_sidebar();
                }
            }
            // Ctrl-w h / Ctrl-w w: focus the session rail (vim window motions).
            (KeyCode::Char('w'), true) => self.pending = Some('W'),
            (KeyCode::F(1), _) => self.view = View::Help,
            _ => {}
        }
        self.count = None;
    }

    fn pending_key(&mut self, p: char, k: KeyEvent) {
        match (p, k.code) {
            ('g', KeyCode::Char('g')) => match self.count.take() {
                Some(n) => self.motion(Motion::Row(n.saturating_sub(1))),
                None => self.motion(Motion::Top),
            },
            ('W', KeyCode::Char('h' | 'w') | KeyCode::Left) => self.set_sidebar_focus(true),
            ('g', KeyCode::Char('f')) => {
                if !self.open_link_at_cursor(false) {
                    self.set_flash("no file link on this line");
                }
            }
            (']', KeyCode::Char(']' | 'c')) => self.motion(Motion::NextBlock),
            ('[', KeyCode::Char('[' | 'c')) => self.motion(Motion::PrevBlock),
            ('z', KeyCode::Char('a')) => {
                self.toggle_collapse();
            }
            ('z', KeyCode::Char(c @ ('z' | 't' | 'b'))) => {
                let page = self.page();
                if let Some(a) = self.active() {
                    let row = a.cursor.pos.row;
                    *a.scroll = match c {
                        'z' => row.saturating_sub(page / 2),
                        't' => row,
                        _ => row.saturating_sub(page.saturating_sub(1)),
                    };
                }
                self.follow = false;
            }
            _ => {}
        }
        self.count = None;
    }

    fn toggle_visual(&mut self, line: bool) {
        self.mode = match self.mode {
            Mode::Visual { line: l, .. } if l == line => Mode::Normal,
            Mode::Visual { anchor, .. } => Mode::Visual { line, anchor },
            _ => Mode::Visual { line, anchor: self.cursor_pos() },
        };
    }

    fn selection(&mut self) -> (Pos, Pos, bool) {
        let cur = self.cursor_pos();
        match self.mode {
            Mode::Visual { line, anchor } => (anchor, cur, line),
            _ => (cur, cur, true),
        }
    }

    /// Opens the comment editor for the visual selection (or the current line).
    pub(crate) fn begin_comment(&mut self) {
        let (a, b, line) = self.selection();
        let anchor = match &self.view {
            View::Chat => match self.transcript.quote(a, b, line) {
                Some((source, quote)) => Anchor::Chat { source, quote },
                None => {
                    self.set_flash("nothing to comment on here — select text with v or V");
                    return;
                }
            },
            View::File(fv) if !fv.lines.is_empty() => fv.anchor(a.row, b.row),
            _ => return,
        };
        self.comment_target = Some(anchor);
        self.editing = None;
        self.comment = Input::default();
        self.mode = Mode::Comment;
    }

    /// Saves the comment being edited into the queue (or updates the edited one).
    fn save_comment(&mut self) {
        let body = self.comment.take();
        if body.trim().is_empty() {
            self.set_flash("empty comment discarded");
        } else if let Some(i) = self.editing.take() {
            if let Some(c) = self.queue.get_mut(i) {
                c.body = body;
            }
        } else if let Some(anchor) = self.comment_target.take() {
            self.queue.push(Comment { anchor, body });
            let n = self.queue.len();
            self.set_flash(format!("comment queued ({n}) — sent with your next message"));
        }
        self.comment_target = None;
        self.mode = Mode::Normal;
    }

    fn comment_key(&mut self, k: KeyEvent) {
        match self.comment.handle(k) {
            InputAction::Submit => self.save_comment(),
            InputAction::Cancel => {
                self.comment_target = None;
                self.editing = None;
                self.mode = Mode::Normal;
            }
            _ => {}
        }
    }

    fn yank_selection(&mut self) {
        let (a, b, line) = self.selection();
        let text = match &self.view {
            View::Chat => self.transcript.quote(a, b, line).map(|(_, q)| q),
            View::File(fv) if !fv.lines.is_empty() => match fv.anchor(a.row, b.row) {
                Anchor::File { snippet, .. } => Some(snippet),
                Anchor::Chat { quote, .. } => Some(quote),
            },
            _ => None,
        };
        if let Some(t) = text {
            self.osc_out = Some(format!("\x1b]52;c;{}\x07", base64(t.as_bytes())));
            self.set_flash(format!("yanked {} lines to clipboard", t.lines().count()));
        }
        self.mode = Mode::Normal;
    }

    fn start_search(&mut self, forward: bool) {
        self.mode_before_search = match self.mode {
            Mode::Visual { .. } => self.mode,
            _ => Mode::Normal,
        };
        self.minibuf = Input::default();
        self.mode = Mode::Search { forward };
    }

    fn search_key(&mut self, k: KeyEvent, forward: bool) {
        match self.minibuf.handle(k) {
            InputAction::Submit => {
                let needle = self.minibuf.take();
                self.mode = self.mode_before_search;
                let needle = if needle.is_empty() {
                    match &self.last_search {
                        Some((n, _)) => n.clone(),
                        None => return,
                    }
                } else {
                    needle
                };
                self.last_search = Some((needle.clone(), forward));
                self.find(&needle, forward);
            }
            InputAction::Cancel => self.mode = self.mode_before_search,
            _ => {}
        }
    }

    fn repeat_search(&mut self, reverse: bool) {
        if let Some((needle, forward)) = self.last_search.clone() {
            self.find(&needle, forward != reverse);
        }
    }

    fn find(&mut self, needle: &str, forward: bool) {
        let found = match self.active() {
            Some(a) => match motion::search(a.lines, a.cursor.pos, needle, forward) {
                Some(p) => {
                    a.cursor.jump(p, a.lines);
                    true
                }
                None => false,
            },
            None => true,
        };
        if !found {
            self.set_flash(format!("pattern not found: {needle}"));
        }
        self.update_follow();
    }

    fn command_key(&mut self, k: KeyEvent) {
        match self.minibuf.handle(k) {
            InputAction::Submit => {
                let cmd = self.minibuf.take();
                self.mode = Mode::Normal;
                self.run_command(cmd.trim());
            }
            InputAction::Cancel => self.mode = Mode::Normal,
            _ => {}
        }
    }

    pub(crate) fn run_command(&mut self, cmd: &str) {
        let (name, arg) = cmd.split_once(' ').map_or((cmd, ""), |(a, b)| (a, b.trim()));
        match name {
            "" => {}
            "q" | "q!" | "quit" | "qa" => self.quit = true,
            "help" | "h" => self.view = View::Help,
            "files" => self.open_files(),
            "comments" | "queue" => self.open_queue(),
            "dropcomments" => {
                let n = self.queue.len();
                self.queue.clear();
                self.set_flash(format!("dropped {n} comments"));
            }
            "send" => self.submit(),
            "e" | "edit" | "open" if !arg.is_empty() => {
                let (p, line) = match arg.rsplit_once(':') {
                    Some((p, l)) if l.parse::<usize>().is_ok() => (p, l.parse().ok()),
                    _ => (arg, None),
                };
                let path = resolve_path(self.transcript.cwd(), p);
                if path.is_file() {
                    self.open_file(path, line);
                } else {
                    self.set_flash(format!("not a file: {arg}"));
                }
            }
            n if n.parse::<usize>().is_ok() => {
                let line: usize = n.parse().unwrap_or(1);
                match &mut self.view {
                    View::File(fv) => fv.jump_to_line(line),
                    _ => self.motion(Motion::Row(line.saturating_sub(1))),
                }
            }
            _ => self.set_flash(format!("unknown command :{cmd} (see :help)")),
        }
    }

    // ------------------------------------------------------------ views

    fn close_view(&mut self) {
        self.view = View::Chat;
        self.mode = Mode::Normal;
    }

    fn open_queue(&mut self) {
        self.view = View::Queue { sel: 0 };
        self.mode = Mode::Normal;
    }

    fn open_files(&mut self) {
        let items = diff::changed_files(self.transcript.cwd());
        if items.is_empty() {
            self.set_flash("no uncommitted changes");
            return;
        }
        self.view = View::Files { items, sel: 0 };
        self.mode = Mode::Normal;
    }

    pub(crate) fn open_file(&mut self, path: PathBuf, line: Option<usize>) {
        let file = diff::load(self.transcript.cwd(), &path);
        self.view = View::File(Box::new(FileView::new(file, line)));
        self.mode = Mode::Normal;
        self.count = None;
    }

    /// Opens the file link under the cursor, or (when `exact` is false) the first link on the row.
    fn open_link_at_cursor(&mut self, exact: bool) -> bool {
        if !matches!(self.view, View::Chat) || self.transcript.total() == 0 {
            return false;
        }
        let pos = self.chat_cursor.pos;
        let row = self.transcript.row(pos.row);
        let link = row
            .links
            .iter()
            .find(|l| l.start <= pos.col && pos.col < l.end)
            .or(if exact { None } else { row.links.first() })
            .cloned();
        match link {
            Some(l) => {
                self.open_file((*l.path).clone(), l.line);
                true
            }
            None => false,
        }
    }

    fn toggle_collapse(&mut self) -> bool {
        if !matches!(self.view, View::Chat) || self.transcript.total() == 0 {
            return false;
        }
        let e = self.transcript.entry_at(self.chat_cursor.pos.row);
        if !self.transcript.entries[e].collapsible() {
            return false;
        }
        let entry = self.transcript.entry_mut(e);
        entry.collapsed = !entry.collapsed;
        self.transcript.layout(self.main_area.width as usize);
        let start = self.transcript.entry_starts()[e];
        motion::apply(&mut self.chat_cursor, Motion::Row(start), &self.transcript, &[], 1);
        self.update_follow();
        true
    }

    fn queue_key(&mut self, k: KeyEvent) {
        let View::Queue { sel } = &mut self.view else { return };
        let n = self.queue.len();
        if let Some(p) = self.pending.take() {
            if p == 'd' && k.code == KeyCode::Char('d') && *sel < n {
                self.queue.remove(*sel);
                *sel = (*sel).min(self.queue.len().saturating_sub(1));
            }
            return;
        }
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') => *sel = 0,
            KeyCode::Char('G') => *sel = n.saturating_sub(1),
            KeyCode::Char('d') => self.pending = Some('d'),
            KeyCode::Char('x') | KeyCode::Delete if *sel < n => {
                self.queue.remove(*sel);
                *sel = (*sel).min(self.queue.len().saturating_sub(1));
            }
            KeyCode::Char('D') => {
                self.queue.clear();
                *sel = 0;
            }
            KeyCode::Char('e') | KeyCode::Enter if *sel < n => {
                let i = *sel;
                self.editing = Some(i);
                self.comment = Input::with_text(self.queue[i].body.clone());
                self.mode = Mode::Comment;
            }
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Esc | KeyCode::Char('q' | 'Q') => self.close_view(),
            KeyCode::Char(':') => {
                self.minibuf = Input::default();
                self.mode = Mode::Command;
            }
            _ => {}
        }
    }

    fn files_key(&mut self, k: KeyEvent) {
        let View::Files { items, sel } = &mut self.view else { return };
        let n = items.len();
        match k.code {
            KeyCode::Char('j') | KeyCode::Down => *sel = (*sel + 1).min(n.saturating_sub(1)),
            KeyCode::Char('k') | KeyCode::Up => *sel = sel.saturating_sub(1),
            KeyCode::Char('g') => *sel = 0,
            KeyCode::Char('G') => *sel = n.saturating_sub(1),
            KeyCode::Enter | KeyCode::Char('l' | 'o') if *sel < n => {
                let path = resolve_path(self.transcript.cwd(), &items[*sel].1);
                self.open_file(path, None);
            }
            KeyCode::Char('r') => self.open_files(),
            KeyCode::Char('i') => self.mode = Mode::Insert,
            KeyCode::Esc | KeyCode::Char('q') => self.close_view(),
            _ => {}
        }
    }

    // ------------------------------------------------------------- mouse

    fn mouse_pos(&mut self, col: u16, row: u16) -> Option<Pos> {
        let area = self.main_area;
        if self.sidebar_area.is_some_and(|r| r.contains((col, row).into())) || !area.contains((col, row).into()) {
            return None;
        }
        let y = (row - area.y) as usize;
        let x = (col - area.x) as usize;
        match &self.view {
            View::Chat => {
                let r = self.chat_scroll + y;
                (r < self.transcript.total()).then(|| Pos { row: r, col: x.saturating_sub(GUTTER) })
            }
            View::File(fv) => {
                // Row 0 of the main area is the file title.
                let r = fv.scroll + y.checked_sub(1)?;
                (r < fv.lines.len()).then(|| Pos { row: r, col: x.saturating_sub(FILE_GUTTER) + fv.hscroll })
            }
            _ => None,
        }
    }

    pub(crate) fn on_mouse(&mut self, m: MouseEvent) {
        if let Some(rail) = self.sidebar_area
            && rail.contains((m.column, m.row).into())
        {
            let last = self.sidebar.rows().len().saturating_sub(1);
            match m.kind {
                // Row 0 of the rail is its header.
                MouseEventKind::Down(MouseButton::Left) if m.row > rail.y => {
                    let i = self.sidebar_scroll + (m.row - rail.y - 1) as usize;
                    if i <= last && !self.sidebar.rows().is_empty() {
                        self.set_sidebar_focus(true);
                        self.sidebar_sel = i;
                    }
                }
                MouseEventKind::ScrollDown => self.sidebar_sel = (self.sidebar_sel + 1).min(last),
                MouseEventKind::ScrollUp => self.sidebar_sel = self.sidebar_sel.saturating_sub(1),
                _ => {}
            }
            return;
        }
        if matches!(m.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.sidebar_focus = false;
        }
        match m.kind {
            MouseEventKind::ScrollDown => match &mut self.view {
                View::Queue { sel } => *sel = (*sel + 1).min(self.queue.len().saturating_sub(1)),
                _ => self.scroll_by(3),
            },
            MouseEventKind::ScrollUp => match &mut self.view {
                View::Queue { sel } => *sel = sel.saturating_sub(1),
                _ => self.scroll_by(-3),
            },
            MouseEventKind::Down(MouseButton::Left) => {
                if self.prompt_area.contains((m.column, m.row).into()) {
                    self.mode = Mode::Insert;
                    return;
                }
                let Some(pos) = self.mouse_pos(m.column, m.row) else { return };
                if matches!(self.view, View::Chat) {
                    let row = self.transcript.row(pos.row);
                    if let Some(l) = row.links.iter().find(|l| l.start <= pos.col && pos.col < l.end).cloned() {
                        self.open_file((*l.path).clone(), l.line);
                        return;
                    }
                }
                if let Some(a) = self.active() {
                    a.cursor.jump(pos, a.lines);
                }
                self.drag_anchor = Some(pos);
                if !matches!(self.mode, Mode::Normal) {
                    self.mode = Mode::Normal;
                }
                self.update_follow();
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                if let Some(anchor) = self.drag_anchor
                    && let Some(pos) = self.mouse_pos(m.column, m.row)
                {
                    self.mode = Mode::Visual { line: false, anchor };
                    if let Some(a) = self.active() {
                        a.cursor.jump(pos, a.lines);
                    }
                    self.update_follow();
                }
            }
            MouseEventKind::Up(MouseButton::Left) => self.drag_anchor = None,
            _ => {}
        }
    }

    // ------------------------------------------------------- agent events

    fn set_sub_state(&mut self, id: &str, state: Live) {
        if let Some(s) = self.subagent_live.get_mut(id)
            && s.state != state
        {
            s.state = state;
            self.refresh_sidebar();
        }
    }

    fn finish_tool(&mut self, i: usize, output: String, failed: bool, duration_ms: Option<u64>) {
        let e = self.transcript.entry_mut(i);
        if let Kind::Tool(t) = &mut e.kind {
            t.status = if failed { ToolStatus::Failed } else { ToolStatus::Ok };
            t.output = output;
            t.duration_ms = duration_ms;
        }
        e.collapsed = e.collapsible();
    }

    fn close_streams(&mut self) {
        for i in [self.streaming_text.take(), self.streaming_reasoning.take()].into_iter().flatten() {
            self.transcript.entry_mut(i).streaming = false;
        }
    }

    fn stream_into(&mut self, reasoning: bool, delta: &str) {
        let slot = if reasoning { self.streaming_reasoning } else { self.streaming_text };
        let i = match slot {
            Some(i) => i,
            None => {
                if !reasoning {
                    // Reasoning ends when visible text starts.
                    if let Some(r) = self.streaming_reasoning.take() {
                        self.transcript.entry_mut(r).streaming = false;
                    }
                }
                let i = self.transcript.push(if reasoning { Kind::Reasoning } else { Kind::Assistant }, "");
                self.transcript.entry_mut(i).streaming = true;
                if reasoning {
                    self.streaming_reasoning = Some(i);
                } else {
                    self.streaming_text = Some(i);
                }
                i
            }
        };
        self.transcript.entry_mut(i).text.push_str(delta);
    }

    pub fn on_agent(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TextDelta(t) => {
                self.activity = Some("writing".into());
                self.stream_into(false, &t);
            }
            AgentEvent::ReasoningDelta(t) => {
                self.activity = Some("thinking".into());
                self.stream_into(true, &t);
            }
            AgentEvent::ToolCallStreaming { name } => {
                self.activity = Some(format!("preparing {name}"));
            }
            AgentEvent::Assistant { message, usage } => {
                if let Some(u) = usage {
                    self.usage += u;
                }
                if let Message::Assistant { content, .. } = &message {
                    match self.streaming_text {
                        Some(i) => self.transcript.entry_mut(i).text = content.clone(),
                        None if !content.is_empty() => {
                            self.transcript.push(Kind::Assistant, content.clone());
                        }
                        None => {}
                    }
                }
                self.close_streams();
                self.activity = Some("waiting for model".into());
            }
            AgentEvent::ToolStart { call } => {
                self.close_streams();
                let i = self.transcript.push(Kind::Tool(Box::new(tool_entry(&call.id, &call.name, &call.arguments))), "");
                self.tool_entries.insert(call.id, i);
                self.activity = Some(format!("running {}", call.name));
            }
            AgentEvent::SubagentStarted { call_id, session_id, title } => {
                self.subagent_calls.insert(call_id, session_id.clone());
                let parent = self.session_id.clone();
                self.subagent_live.insert(session_id, LiveSub { parent, title, state: Live::Running });
                self.refresh_sidebar();
            }
            AgentEvent::ToolEnd { call_id, output, duration_ms } => {
                if let Some(&i) = self.tool_entries.get(&call_id) {
                    self.finish_tool(i, output.content, output.is_error, Some(duration_ms));
                }
                if let Some(id) = self.subagent_calls.remove(&call_id) {
                    self.set_sub_state(&id, if output.is_error { Live::Failed } else { Live::Done });
                }
            }
            AgentEvent::Subagent { call_id, event } => {
                if let AgentEvent::TurnEnd { cancelled, error } = &*event
                    && let Some(id) = self.subagent_calls.get(&call_id).cloned()
                {
                    self.set_sub_state(&id, if *cancelled || error.is_some() { Live::Failed } else { Live::Done });
                }
                let Some(&i) = self.tool_entries.get(&call_id) else { return };
                let line = match *event {
                    AgentEvent::ToolStart { call } => Some(format!("↳ {} {}", call.name, summarize(&call.name, &call.arguments))),
                    AgentEvent::ToolEnd { output, .. } if output.is_error => {
                        Some(format!("  ✗ {}", output.content.lines().next().unwrap_or("")))
                    }
                    AgentEvent::Assistant { usage: Some(u), .. } => {
                        self.usage += u;
                        None
                    }
                    _ => None,
                };
                if let Some(l) = line
                    && let Kind::Tool(t) = &mut self.transcript.entry_mut(i).kind
                {
                    t.children.push(l);
                }
            }
            AgentEvent::ResponseLength(r) => {
                self.length = Some(match r {
                    Routing::Picked { level, confidence } => format!("{level} {confidence:.2}"),
                    Routing::Unsure { level, confidence } => format!("unsure ({level} {confidence:.2})"),
                    Routing::Unavailable { .. } => "unrouted".into(),
                });
            }
            AgentEvent::Warning(w) => {
                self.transcript.push(Kind::Info, w);
            }
            AgentEvent::TurnEnd { cancelled, error } => {
                self.close_streams();
                self.running = false;
                self.cancel = None;
                self.activity = None;
                let running: Vec<usize> = self
                    .tool_entries
                    .values()
                    .copied()
                    .filter(|&i| matches!(&self.transcript.entries[i].kind, Kind::Tool(t) if t.status == ToolStatus::Running))
                    .collect();
                for i in running {
                    self.finish_tool(i, "interrupted".into(), true, None);
                }
                if let Some(e) = error {
                    self.transcript.push(Kind::Error, e);
                }
                if cancelled {
                    self.transcript.push(Kind::Info, "interrupted");
                }
                // A turn that ended with subagents still in flight was interrupted.
                for (_, id) in std::mem::take(&mut self.subagent_calls) {
                    self.set_sub_state(&id, Live::Failed);
                }
                self.refresh_sidebar();
                if let Some(msg) = self.queued_send.take() {
                    self.start_turn(msg);
                }
            }
        }
    }
}

fn tool_entry(id: &str, name: &str, arguments: &str) -> ToolEntry {
    ToolEntry {
        call_id: id.to_owned(),
        name: name.to_owned(),
        summary: summarize(name, arguments),
        status: ToolStatus::Running,
        output: String::new(),
        children: Vec::new(),
        duration_ms: None,
    }
}
