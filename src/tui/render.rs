//! Drawing. Only visible rows are styled, so frame cost is independent of session length.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use unicode_width::UnicodeWidthChar;

use super::{
    app::{App, Mode, View},
    motion::Pos,
    highlight::Run,
    transcript::GUTTER,
};
use crate::{comments::Anchor, diff::Kind as DiffKind};

/// Width of the file view's `old new ●+ ` gutter.
pub const FILE_GUTTER: usize = 14;
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
const SEL_BG: Color = Color::Rgb(58, 68, 112);
pub(crate) const ADD_BG: Color = Color::Rgb(24, 52, 32);
const DEL_BG: Color = Color::Rgb(62, 28, 30);
const MAX_PROMPT_ROWS: usize = 8;

/// What is drawn over a row's base style, all in char offsets of the row text.
#[derive(Clone, Copy, Default)]
pub(crate) struct Marks<'a> {
    /// Syntax-highlight runs, patched over the base style.
    pub syntax: &'a [Run],
    pub links: &'a [(usize, usize)],
    /// Visual selection `[s, e)`; `e` past the text end marks the line end too.
    pub sel: Option<(usize, usize)>,
    pub cursor: Option<usize>,
}

/// Styles one row's text with `marks`, starting at char `hscroll` and clipped to `maxw` columns.
pub(crate) fn styled(text: &str, base: Style, marks: &Marks<'_>, hscroll: usize, maxw: usize) -> Vec<Span<'static>> {
    let Marks { syntax, links, sel, cursor } = *marks;
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current: Option<Style> = None;
    let mut buf = String::new();
    let mut w = 0;
    let mut len = 0;
    for (i, ch) in text.chars().enumerate() {
        len = i + 1;
        if i < hscroll {
            continue;
        }
        let cw = ch.width().unwrap_or(0);
        if w + cw > maxw {
            len = text.chars().count();
            break;
        }
        w += cw;
        let mut st = syntax.iter().filter(|r| r.start <= i && i < r.end).fold(base, |st, r| st.patch(r.style));
        if links.iter().any(|&(s, e)| s <= i && i < e) {
            st = st.fg(Color::LightBlue).add_modifier(Modifier::UNDERLINED);
        }
        if sel.is_some_and(|(s, e)| s <= i && i < e) {
            st = st.bg(SEL_BG);
        }
        if cursor == Some(i) {
            st = st.add_modifier(Modifier::REVERSED);
        }
        if current != Some(st) {
            if let Some(s) = current
                && !buf.is_empty()
            {
                spans.push(Span::styled(std::mem::take(&mut buf), s));
            }
            current = Some(st);
        }
        buf.push(ch);
    }
    if let Some(s) = current
        && !buf.is_empty()
    {
        spans.push(Span::styled(buf, s));
    }
    if w < maxw {
        let past_end = |c: usize| c >= len;
        if cursor.is_some_and(past_end) {
            let mut st = Style::default().add_modifier(Modifier::REVERSED);
            if sel.is_some() {
                st = st.bg(SEL_BG);
            }
            spans.push(Span::styled(" ", st));
        } else if sel.is_some_and(|(s, e)| e > len && s <= len) {
            spans.push(Span::styled(" ", Style::default().bg(SEL_BG)));
        }
    }
    spans
}

/// Selection columns `[s, e)` on `row`, or None when the row is outside the visual selection.
fn sel_range(mode: Mode, cursor: Pos, row: usize, len: usize) -> Option<(usize, usize)> {
    let Mode::Visual { line, anchor } = mode else { return None };
    let (a, b) = if anchor <= cursor { (anchor, cursor) } else { (cursor, anchor) };
    if row < a.row || row > b.row {
        return None;
    }
    if line {
        return Some((0, len + 1));
    }
    let s = if row == a.row { a.col } else { 0 };
    let e = if row == b.row { b.col + 1 } else { len + 1 };
    Some((s, e))
}

fn keep_visible(scroll: &mut usize, row: usize, height: usize, total: usize) {
    if height == 0 {
        return;
    }
    if row < *scroll {
        *scroll = row;
    } else if row >= *scroll + height {
        *scroll = row + 1 - height;
    }
    *scroll = (*scroll).min(total.saturating_sub(1));
}

fn dim() -> Style {
    Style::default().fg(Color::DarkGray)
}

impl App {
    /// Renders one frame.
    pub fn draw(&mut self, f: &mut Frame) {
        let area = f.area();
        let prompt_w = (area.width as usize).saturating_sub(2);
        let (prompt_lines, prompt_cur) = self.prompt.layout(prompt_w);
        let prompt_h = prompt_lines.len().clamp(1, MAX_PROMPT_ROWS) as u16;
        let [main, sep, prompt, status] =
            Layout::vertical([Constraint::Min(1), Constraint::Length(1), Constraint::Length(prompt_h), Constraint::Length(1)])
                .areas(area);
        self.main_area = main;
        self.prompt_area = prompt;

        match self.view {
            View::Chat => self.draw_chat(f, main),
            View::File(_) => self.draw_file(f, main),
            View::Queue { .. } => self.draw_queue(f, main),
            View::Files { .. } => self.draw_files(f, main),
            View::Help => draw_help(f, main),
        }
        if self.mode == Mode::Comment {
            self.draw_comment_popup(f, main);
        }
        self.draw_separator(f, sep);
        self.draw_prompt(f, prompt, &prompt_lines, prompt_cur);
        self.draw_status(f, status);
    }

    fn draw_chat(&mut self, f: &mut Frame, area: Rect) {
        self.transcript.layout(area.width as usize);
        let total = self.transcript.total();
        if total == 0 {
            let text = vec![
                Line::from(Span::styled("rdsh", Style::default().add_modifier(Modifier::BOLD))),
                Line::from(Span::styled(format!("{} · {}", self.model, self.transcript.cwd().display()), dim())),
                Line::from(""),
                Line::from("Type a message and press Enter. Esc switches to normal mode:"),
                Line::from("  j/k w/b gg/G Ctrl-d/u  move      { }  previous/next message      / ?  search"),
                Line::from("  v V  select text, then c to comment — comments are sent with your next message"),
                Line::from("  Enter or gf on a file path (or click it) opens its diff, where v/V/c comment on lines"),
                Line::from("  Q comment queue    F changed files    Tab expand output    :help    Ctrl-C twice quits"),
            ];
            f.render_widget(Paragraph::new(text).wrap(Wrap { trim: false }), area);
            return;
        }
        let h = area.height as usize;
        if self.follow {
            self.chat_cursor.pos = Pos { row: total - 1, col: 0 };
            self.chat_scroll = total.saturating_sub(h);
        }
        self.chat_cursor.clamp(&self.transcript);
        keep_visible(&mut self.chat_scroll, self.chat_cursor.pos.row, h, total);
        let show_cursor = matches!(self.mode, Mode::Normal | Mode::Visual { .. } | Mode::Search { .. } | Mode::Command);
        let text_w = (area.width as usize).saturating_sub(GUTTER);
        let mut lines = Vec::with_capacity(h);
        for r in self.chat_scroll..(self.chat_scroll + h).min(total) {
            let row = self.transcript.row(r);
            let len = row.text.chars().count();
            let links: Vec<(usize, usize)> = row.links.iter().map(|l| (l.start, l.end)).collect();
            let sel = sel_range(self.mode, self.chat_cursor.pos, r, len);
            let cur = (show_cursor && r == self.chat_cursor.pos.row).then_some(self.chat_cursor.pos.col);
            let mut spans = vec![Span::styled(row.gutter, row.gutter_style)];
            spans.extend(styled(&row.text, row.style, &Marks { links: &links, sel, cursor: cur, ..Marks::default() }, 0, text_w));
            if let Some(bg) = row.style.bg {
                let used: usize = spans.iter().map(|s| s.width()).sum();
                let fill = (area.width as usize).saturating_sub(used);
                spans.push(Span::styled(" ".repeat(fill), Style::default().bg(bg)));
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), area);
    }

    fn draw_file(&mut self, f: &mut Frame, area: Rect) {
        let [head, body, foot] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(1), Constraint::Length(1)]).areas(area);
        let mode = self.mode;
        let queue = &self.queue;
        let View::File(fv) = &mut self.view else { return };
        f.render_widget(
            Paragraph::new(Line::from(vec![
                Span::styled(format!(" {} ", fv.title()), Style::default().add_modifier(Modifier::BOLD).bg(Color::Rgb(40, 40, 40))),
                Span::styled("  Tab diff/file · ]c [c hunks · v/V select · c comment · r reload · q back", dim()),
            ])),
            head,
        );
        if fv.lines.is_empty() {
            let msg = fv.file.error.clone().unwrap_or_else(|| "empty file".into());
            f.render_widget(Paragraph::new(Span::styled(msg, dim())), body);
            return;
        }
        let h = body.height as usize;
        let total = fv.lines.len();
        fv.cursor.clamp(&fv.texts);
        keep_visible(&mut fv.scroll, fv.cursor.pos.row, h, total);
        let text_w = (body.width as usize).saturating_sub(FILE_GUTTER).max(1);
        let col = fv.cursor.pos.col;
        if col < fv.hscroll {
            fv.hscroll = col;
        } else if col >= fv.hscroll + text_w {
            fv.hscroll = col + 1 - text_w;
        }
        let commented = |n: Option<usize>| {
            queue.iter().any(|c| match (&c.anchor, n) {
                (Anchor::File { path, lines: Some((lo, hi)), .. }, Some(n)) => *path == fv.file.rel && (*lo..=*hi).contains(&n),
                _ => false,
            })
        };
        let mut lines = Vec::with_capacity(h);
        for r in fv.scroll..(fv.scroll + h).min(total) {
            let l = &fv.lines[r];
            let text = &fv.texts.0[r];
            let num = l.new.or(l.old);
            let syntax = fv.syntax(r);
            // Changed lines get a tinted background so syntax colors stay readable; without
            // highlighting, the text itself is colored as well.
            let tinted = |bg: Color, fg: Color| {
                let s = Style::default().bg(bg);
                if syntax.is_empty() { s.fg(fg) } else { s }
            };
            let (style, sign, sign_style) = match l.kind {
                DiffKind::Add => (tinted(ADD_BG, Color::Green), '+', Style::default().fg(Color::Green).bg(ADD_BG)),
                DiffKind::Del => (tinted(DEL_BG, Color::Red), '-', Style::default().fg(Color::Red).bg(DEL_BG)),
                DiffKind::Ctx => (Style::default(), ' ', Style::default()),
                DiffKind::Hunk => (Style::default().fg(Color::Cyan), ' ', Style::default()),
                DiffKind::Meta => (dim(), ' ', Style::default()),
            };
            let marker = if commented(num) { "●" } else { " " };
            let fmt = |n: Option<usize>| n.map_or(String::new(), |n| n.to_string());
            let gutter = format!("{:>5} {:>5}{marker}{sign} ", fmt(l.old), fmt(l.new));
            let len = text.chars().count();
            let sel = sel_range(mode, fv.cursor.pos, r, len);
            let cur = (matches!(mode, Mode::Normal | Mode::Visual { .. }) && r == fv.cursor.pos.row).then_some(col);
            let mut spans = vec![
                Span::styled(gutter[..gutter.len() - marker.len() - 2].to_owned(), dim()),
                Span::styled(marker.to_owned(), Style::default().fg(Color::Yellow)),
                Span::styled(format!("{sign} "), sign_style),
            ];
            spans.extend(styled(text, style, &Marks { syntax, sel, cursor: cur, ..Marks::default() }, fv.hscroll, text_w));
            if let Some(bg) = style.bg {
                let used: usize = spans.iter().map(|s| s.width()).sum();
                spans.push(Span::styled(" ".repeat((body.width as usize).saturating_sub(used)), Style::default().bg(bg)));
            }
            lines.push(Line::from(spans));
        }
        f.render_widget(Paragraph::new(lines), body);
        let here = fv.lines.get(fv.cursor.pos.row).and_then(|l| l.new.or(l.old));
        let note = queue.iter().find(|c| match (&c.anchor, here) {
            (Anchor::File { path, lines: Some((lo, hi)), .. }, Some(n)) => *path == fv.file.rel && (*lo..=*hi).contains(&n),
            _ => false,
        });
        if let Some(c) = note {
            let body_line = c.body.lines().next().unwrap_or("");
            f.render_widget(
                Paragraph::new(Span::styled(format!("● {}: {body_line}", c.label()), Style::default().fg(Color::Yellow))),
                foot,
            );
        }
    }

    fn draw_queue(&self, f: &mut Frame, area: Rect) {
        let View::Queue { sel } = self.view else { return };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(
                    format!(" Comment queue — {} ", self.queue.len()),
                    Style::default().add_modifier(Modifier::BOLD).bg(Color::Rgb(40, 40, 40)),
                ),
                Span::styled("  sent with your next message · j/k · e edit · dd delete · D clear · q back", dim()),
            ]),
            Line::from(""),
        ];
        if self.queue.is_empty() {
            lines.push(Line::from(Span::styled(
                "No comments yet. In the chat or a file view, select with v/V and press c.",
                dim(),
            )));
        }
        let mut sel_line = 0;
        for (i, c) in self.queue.iter().enumerate() {
            if i == sel {
                sel_line = lines.len();
            }
            let head = Style::default().add_modifier(Modifier::BOLD);
            let head = if i == sel { head.bg(SEL_BG) } else { head };
            lines.push(Line::from(Span::styled(format!("[{}] {}", i + 1, c.label()), head)));
            let excerpt: Vec<&str> = c.excerpt().lines().collect();
            for l in excerpt.iter().take(4) {
                lines.push(Line::from(Span::styled(format!("  > {l}"), dim())));
            }
            if excerpt.len() > 4 {
                lines.push(Line::from(Span::styled(format!("  > … {} more lines", excerpt.len() - 4), dim())));
            }
            for l in c.body.lines() {
                lines.push(Line::from(Span::styled(format!("  {l}"), Style::default().fg(Color::Yellow))));
            }
            lines.push(Line::from(""));
        }
        let h = area.height as usize;
        let scroll = sel_line.saturating_sub(h / 2) as u16;
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
    }

    fn draw_files(&self, f: &mut Frame, area: Rect) {
        let View::Files { items, sel } = &self.view else { return };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(" Changed files ", Style::default().add_modifier(Modifier::BOLD).bg(Color::Rgb(40, 40, 40))),
                Span::styled("  Enter open · r refresh · q back", dim()),
            ]),
            Line::from(""),
        ];
        for (i, (status, path)) in items.iter().enumerate() {
            let color = match status.as_str() {
                "??" | "A" => Color::Green,
                "D" => Color::Red,
                _ => Color::Yellow,
            };
            let mut st = Style::default();
            if i == *sel {
                st = st.bg(SEL_BG);
            }
            lines.push(Line::from(vec![
                Span::styled(format!(" {status:>2} "), st.fg(color)),
                Span::styled(path.clone(), st),
            ]));
        }
        let h = area.height as usize;
        let scroll = (*sel + 2).saturating_sub(h.saturating_sub(1)) as u16;
        f.render_widget(Paragraph::new(lines).scroll((scroll, 0)), area);
    }

    fn draw_comment_popup(&self, f: &mut Frame, main: Rect) {
        let (label, excerpt) = match (&self.comment_target, self.editing.and_then(|i| self.queue.get(i))) {
            (Some(a), _) => {
                let c = crate::comments::Comment { anchor: a.clone(), body: String::new() };
                (c.label(), c.excerpt().to_owned())
            }
            (None, Some(c)) => (c.label(), c.excerpt().to_owned()),
            _ => return,
        };
        let inner_w = (main.width as usize).saturating_sub(4).max(10);
        let (input_lines, cur) = self.comment.layout(inner_w);
        let quote: Vec<&str> = excerpt.lines().collect();
        let shown_quote = quote.len().min(6);
        let height = (shown_quote + usize::from(quote.len() > 6) + 1 + input_lines.len().min(6) + 2) as u16;
        let height = height.min(main.height);
        let area = Rect { x: main.x, y: main.y + main.height - height, width: main.width, height };
        let mut lines: Vec<Line> = quote.iter().take(6).map(|l| Line::from(Span::styled(format!("> {l}"), dim()))).collect();
        if quote.len() > 6 {
            lines.push(Line::from(Span::styled(format!("> … {} more lines", quote.len() - 6), dim())));
        }
        lines.push(Line::from(""));
        let input_start = lines.len();
        let skip = (cur.1 as usize).saturating_sub(5);
        for l in input_lines.iter().skip(skip).take(6) {
            lines.push(Line::from(l.clone()));
        }
        let block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Yellow))
            .title(format!(" Comment on {label} "))
            .title_bottom(Span::styled(" Enter save · Ctrl-Enter save & send · Shift-Enter newline · Esc cancel ", dim()));
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(lines).block(block), area);
        let y = area.y + 1 + input_start as u16 + cur.1 - skip as u16;
        if y < area.y + area.height - 1 {
            f.set_cursor_position((area.x + 1 + cur.0, y));
        }
    }

    fn draw_separator(&self, f: &mut Frame, area: Rect) {
        let mut left: Vec<Span> = Vec::new();
        if !self.queue.is_empty() {
            let n = self.queue.len();
            left.push(Span::styled(
                format!(" ● {n} comment{} queued — sent with your next message (Q to review) ", if n == 1 { "" } else { "s" }),
                Style::default().fg(Color::Yellow),
            ));
        }
        if self.queued_send.is_some() {
            left.push(Span::styled(" ⏳ message waiting for the current turn ", Style::default().fg(Color::Magenta)));
        }
        let right = if self.running {
            let secs = self.turn_started.elapsed().as_secs();
            let act = self.activity.as_deref().unwrap_or("working");
            format!(" {} {act} · {secs}s · Ctrl-C interrupts ", SPINNER[self.spinner % SPINNER.len()])
        } else {
            String::new()
        };
        let used: usize = left.iter().map(|s| s.width()).sum::<usize>() + right.chars().count();
        let fill = (area.width as usize).saturating_sub(used);
        left.push(Span::styled("─".repeat(fill), dim()));
        left.push(Span::styled(right, Style::default().fg(Color::Cyan)));
        f.render_widget(Paragraph::new(Line::from(left)), area);
    }

    fn draw_prompt(&self, f: &mut Frame, area: Rect, lines: &[String], cur: (u16, u16)) {
        if self.prompt.text.is_empty() && self.mode != Mode::Insert {
            let hint = if self.queue.is_empty() {
                "  press i to type · Enter sends · Ctrl-Enter sends from any mode"
            } else {
                "  Ctrl-Enter sends the queued comments · i adds a message"
            };
            f.render_widget(Paragraph::new(Line::from(vec![Span::styled("›", dim()), Span::styled(hint, dim())])), area);
            return;
        }
        let h = area.height as usize;
        let skip = (cur.1 as usize + 1).saturating_sub(h);
        let rows: Vec<Line> = lines
            .iter()
            .enumerate()
            .skip(skip)
            .take(h)
            .map(|(i, l)| {
                let p = if i == 0 { Span::styled("› ", Style::default().fg(Color::Cyan)) } else { Span::raw("  ") };
                Line::from(vec![p, Span::raw(l.clone())])
            })
            .collect();
        f.render_widget(Paragraph::new(rows), area);
        if self.mode == Mode::Insert {
            f.set_cursor_position((area.x + 2 + cur.0, area.y + cur.1 - skip as u16));
        }
    }

    fn draw_status(&self, f: &mut Frame, area: Rect) {
        let (label, color) = match self.mode {
            Mode::Normal => ("NORMAL", Color::Blue),
            Mode::Insert => ("INSERT", Color::Green),
            Mode::Visual { line: false, .. } => ("VISUAL", Color::Magenta),
            Mode::Visual { line: true, .. } => ("V-LINE", Color::Magenta),
            Mode::Comment => ("COMMENT", Color::Yellow),
            Mode::Search { .. } => ("SEARCH", Color::Cyan),
            Mode::Command => ("COMMAND", Color::Cyan),
        };
        let mut spans = vec![Span::styled(format!(" {label} "), Style::default().bg(color).fg(Color::Black).add_modifier(Modifier::BOLD))];
        match self.mode {
            Mode::Search { forward } => {
                let prefix = if forward { "/" } else { "?" };
                spans.push(Span::raw(format!(" {prefix}{}", self.minibuf.text)));
                let x = area.x + label.len() as u16 + 3 + self.minibuf.text[..self.minibuf.cursor].chars().count() as u16;
                f.set_cursor_position((x, area.y));
            }
            Mode::Command => {
                spans.push(Span::raw(format!(" :{}", self.minibuf.text)));
                let x = area.x + label.len() as u16 + 3 + self.minibuf.text[..self.minibuf.cursor].chars().count() as u16;
                f.set_cursor_position((x, area.y));
            }
            _ => {
                let view = match &self.view {
                    View::Chat => "chat",
                    View::File(_) => "file",
                    View::Queue { .. } => "comments",
                    View::Files { .. } => "changes",
                    View::Help => "help",
                };
                spans.push(Span::styled(format!(" {view} "), dim()));
                if let Some(m) = &self.flash {
                    spans.push(Span::styled(format!(" {m}"), Style::default().fg(Color::Yellow)));
                }
            }
        }
        let u = self.usage;
        let short: String = self.session_id.chars().take(15).collect();
        let right = format!("{} · ↑{} ↓{} · {short} ", self.model, compact(u.prompt_tokens), compact(u.completion_tokens));
        let used: usize = spans.iter().map(|s| s.width()).sum();
        let pad = (area.width as usize).saturating_sub(used + right.chars().count());
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(right, dim()));
        f.render_widget(Paragraph::new(Line::from(spans)), area);
    }
}

fn compact(n: u64) -> String {
    match n {
        0..1000 => n.to_string(),
        1000..1_000_000 => format!("{:.1}k", n as f64 / 1000.0),
        _ => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn draw_help(f: &mut Frame, area: Rect) {
    let h = |s: &'static str| Line::from(Span::styled(s, Style::default().add_modifier(Modifier::BOLD)));
    let l = |s: &'static str| Line::from(s);
    let text = vec![
        h("rdsh keys (any key closes)"),
        l(""),
        h("Prompt (INSERT)"),
        l("  Enter send · Shift-Enter newline (also Alt-Enter, Ctrl-J) · Up/Down history · Esc normal mode"),
        l(""),
        h("Navigation (NORMAL, chat and file views)"),
        l("  h j k l · w b e · 0 ^ $ · gg G · 42G · Ctrl-d/u half page · Ctrl-f/b page · Ctrl-e/y scroll"),
        l("  { } or [[ ]]  previous/next message (chat) or hunk (file); ]c [c also jump hunks"),
        l("  H M L  top/middle/bottom of screen · zz zt zb  scroll cursor · / ? n N  search"),
        l(""),
        h("Selecting and commenting"),
        l("  v charwise · V linewise · o swap end · y yank (OSC 52) · c or Enter comment on selection"),
        l("  c in normal mode comments on the current line · mouse drag selects too"),
        l("  Comments queue up; the next message you send carries them, each with its quoted text."),
        l(""),
        h("Files"),
        l("  Enter / gf / click on a path opens it: diff view (Tab toggles full file), comment on lines with v/V c"),
        l("  F lists changed files · :e path[:line] opens any file · r reloads · q or Esc goes back"),
        l(""),
        h("Other"),
        l("  Tab or za expands/collapses tool output · Q comment queue (e edit, dd delete, D clear)"),
        l("  Ctrl-Enter (any mode) sends queued comments + prompt text; in a comment it saves, then sends"),
        l("  :send · :dropcomments · :q quit · Ctrl-C interrupts / clears / quits (twice)"),
    ];
    f.render_widget(Paragraph::new(text), area);
}
