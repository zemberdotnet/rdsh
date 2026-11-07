//! The conversation as display rows: entries are rendered to logical lines, wrapped to the
//! viewport width and cached per entry, so streaming only re-wraps the entry that changed.

use std::path::PathBuf;

use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthChar;

use super::{
    motion::{Lines, Pos},
    theme::*,
};
use crate::{
    links::{Link, LinkResolver},
    util::sanitize,
};

/// Columns reserved left of the text for markers.
pub const GUTTER: usize = 2;
const COLLAPSED_OUTPUT_LINES: usize = 4;
const COLLAPSED_REASONING_LINES: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Ok,
    Failed,
}

#[derive(Clone, Debug)]
pub struct ToolEntry {
    pub call_id: String,
    pub name: String,
    pub summary: String,
    pub status: ToolStatus,
    pub output: String,
    /// Progress lines from a subagent (`task`).
    pub children: Vec<String>,
    pub duration_ms: Option<u64>,
}

#[derive(Clone, Debug)]
pub enum Kind {
    User,
    Assistant,
    Reasoning,
    Tool(Box<ToolEntry>),
    Info,
    Error,
}

pub struct Entry {
    pub kind: Kind,
    pub text: String,
    pub collapsed: bool,
    pub streaming: bool,
    rows: Vec<Row>,
    dirty: bool,
}

impl Entry {
    /// Name used when quoting this entry in a comment.
    pub fn source_label(&self) -> String {
        match &self.kind {
            Kind::User => "my earlier message".into(),
            Kind::Assistant => "your response".into(),
            Kind::Reasoning => "your reasoning".into(),
            Kind::Tool(t) => format!("the {} call ({})", t.name, t.summary),
            Kind::Info | Kind::Error => "a status message".into(),
        }
    }

    /// Whether Tab can expand/collapse this entry.
    pub fn collapsible(&self) -> bool {
        match &self.kind {
            Kind::Tool(t) => t.output.lines().count() > COLLAPSED_OUTPUT_LINES,
            Kind::Reasoning => !self.streaming,
            _ => false,
        }
    }
}

/// One display row. `text` excludes the gutter.
#[derive(Clone, Debug)]
pub struct Row {
    pub text: String,
    pub style: Style,
    pub gutter: &'static str,
    pub gutter_style: Style,
    /// Links with char offsets relative to this row.
    pub links: Vec<Link>,
    pub entry: usize,
    /// Index of the logical line within the entry.
    pub logical: usize,
    /// Char offset of this row within its logical line (0 = first row of the line).
    pub offset: usize,
}

struct LLine {
    text: String,
    style: Style,
    gutter: &'static str,
    gutter_style: Style,
    linkify: bool,
}

impl LLine {
    fn new(text: impl Into<String>, style: Style) -> Self {
        Self { text: text.into(), style, gutter: "  ", gutter_style: Style::default(), linkify: true }
    }
    fn gutter(mut self, g: &'static str, s: Style) -> Self {
        self.gutter = g;
        self.gutter_style = s;
        self
    }
    fn plain(mut self) -> Self {
        self.linkify = false;
        self
    }
}

fn dim() -> Style {
    Style::default().fg(GRAY)
}

fn logical_lines(e: &Entry) -> Vec<LLine> {
    let mut out = Vec::new();
    match &e.kind {
        Kind::User => {
            for (i, l) in sanitize(&e.text).lines().enumerate() {
                let line = LLine::new(l, Style::default().fg(FG).bg(USER_BG));
                let g = Style::default().fg(AQUA).bg(USER_BG).add_modifier(Modifier::BOLD);
                out.push(line.gutter(if i == 0 { "› " } else { "  " }, g));
            }
        }
        Kind::Assistant => {
            let mut in_code = false;
            for (i, l) in sanitize(&e.text).lines().enumerate() {
                let fence = l.trim_start().starts_with("```");
                let style = if fence {
                    in_code = !in_code;
                    dim()
                } else if in_code {
                    Style::default().fg(YELLOW)
                } else if l.starts_with('#') {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                };
                let line = LLine::new(l, style);
                out.push(if i == 0 { line.gutter("● ", Style::default().fg(FG)) } else { line });
            }
        }
        Kind::Reasoning => {
            let style = Style::default().fg(GRAY).add_modifier(Modifier::ITALIC);
            let text = sanitize(&e.text);
            let lines: Vec<&str> = text.lines().collect();
            let g = Style::default().fg(GRAY);
            if e.collapsed && !e.streaming {
                out.push(LLine::new(format!("thought for {} lines (Tab to expand)", lines.len()), style).gutter("∴ ", g).plain());
            } else {
                let skip = if e.streaming { lines.len().saturating_sub(COLLAPSED_REASONING_LINES) } else { 0 };
                for (i, l) in lines.iter().enumerate().skip(skip) {
                    let line = LLine::new(*l, style);
                    out.push(if i == skip { line.gutter("∴ ", g) } else { line });
                }
            }
        }
        Kind::Tool(t) => {
            let (icon, color) = match t.status {
                ToolStatus::Running => ("⋯ ", YELLOW),
                ToolStatus::Ok => ("✓ ", GREEN),
                ToolStatus::Failed => ("✗ ", RED),
            };
            let dur = t.duration_ms.map_or(String::new(), |d| {
                if d >= 1000 { format!("  ({:.1}s)", d as f64 / 1000.0) } else { format!("  ({d}ms)") }
            });
            out.push(
                LLine::new(format!("{} {}{dur}", t.name, t.summary), Style::default().add_modifier(Modifier::BOLD))
                    .gutter(icon, Style::default().fg(color)),
            );
            for c in &t.children {
                out.push(LLine::new(format!("  {c}"), dim()));
            }
            let output = sanitize(&t.output);
            let lines: Vec<&str> = output.lines().collect();
            let shown = if e.collapsed { COLLAPSED_OUTPUT_LINES.min(lines.len()) } else { lines.len() };
            let out_style = if t.status == ToolStatus::Failed { Style::default().fg(RED) } else { Style::default().fg(FG_DIM) };
            for l in &lines[..shown] {
                out.push(LLine::new(*l, out_style).gutter("│ ", dim()));
            }
            if shown < lines.len() {
                out.push(LLine::new(format!("… +{} lines (Tab to expand)", lines.len() - shown), dim()).gutter("│ ", dim()).plain());
            }
        }
        Kind::Info => {
            for l in sanitize(&e.text).lines() {
                out.push(LLine::new(l, dim()).gutter("· ", dim()));
            }
        }
        Kind::Error => {
            for l in sanitize(&e.text).lines() {
                out.push(LLine::new(l, Style::default().fg(RED)).gutter("✗ ", Style::default().fg(RED)));
            }
        }
    }
    if out.is_empty() {
        out.push(LLine::new("", Style::default()).plain());
    }
    // Spacer between entries.
    out.push(LLine::new("", Style::default()).plain());
    out
}

/// Greedy word wrap by display width. Returns (char offset, text) per row; rows are
/// contiguous substrings (spaces stay at the end of the earlier row).
pub fn wrap(text: &str, width: usize) -> Vec<(usize, String)> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return vec![(0, String::new())];
    }
    let width = width.max(1);
    let mut out = Vec::new();
    let mut start = 0;
    while start < chars.len() {
        let mut w = 0;
        let mut end = start;
        let mut last_space = None;
        while end < chars.len() {
            let cw = chars[end].width().unwrap_or(0);
            if w + cw > width && end > start {
                break;
            }
            w += cw;
            if chars[end] == ' ' {
                last_space = Some(end);
            }
            end += 1;
        }
        if end < chars.len()
            && let Some(s) = last_space
            && s > start
        {
            end = s + 1;
        }
        out.push((start, chars[start..end].iter().collect()));
        start = end;
    }
    out
}

fn render_entry(index: usize, e: &Entry, width: usize, resolver: &mut LinkResolver) -> Vec<Row> {
    let mut rows = Vec::new();
    for (li, l) in logical_lines(e).into_iter().enumerate() {
        let links = if l.linkify { resolver.find(&l.text) } else { Vec::new() };
        for (k, (offset, text)) in wrap(&l.text, width).into_iter().enumerate() {
            let len = text.chars().count();
            let row_links = links
                .iter()
                .filter(|lk| lk.start < offset + len && lk.end > offset)
                .map(|lk| Link {
                    start: lk.start.max(offset) - offset,
                    end: lk.end.min(offset + len) - offset,
                    path: lk.path.clone(),
                    line: lk.line,
                })
                .collect();
            rows.push(Row {
                text,
                style: l.style,
                gutter: if k == 0 { l.gutter } else { "  " },
                gutter_style: l.gutter_style,
                links: row_links,
                entry: index,
                logical: li,
                offset,
            });
        }
    }
    rows
}

pub struct Transcript {
    pub entries: Vec<Entry>,
    offsets: Vec<usize>,
    total: usize,
    width: usize,
    resolver: LinkResolver,
    stale: bool,
}

impl Transcript {
    pub fn new(cwd: PathBuf) -> Self {
        Self { entries: Vec::new(), offsets: Vec::new(), total: 0, width: 0, resolver: LinkResolver::new(cwd), stale: false }
    }

    pub fn cwd(&self) -> &std::path::Path {
        self.resolver.cwd()
    }

    /// Appends an entry and returns its index.
    pub fn push(&mut self, kind: Kind, text: impl Into<String>) -> usize {
        let collapsed = match &kind {
            Kind::Reasoning => true,
            Kind::Tool(t) => t.output.lines().count() > COLLAPSED_OUTPUT_LINES,
            _ => false,
        };
        self.entries.push(Entry { kind, text: text.into(), collapsed, streaming: false, rows: Vec::new(), dirty: true });
        self.stale = true;
        self.entries.len() - 1
    }

    /// Mutable access that schedules the entry for re-layout.
    pub fn entry_mut(&mut self, i: usize) -> &mut Entry {
        self.stale = true;
        let e = &mut self.entries[i];
        e.dirty = true;
        e
    }

    /// Re-wraps changed entries for a viewport `width` columns wide (gutter included).
    pub fn layout(&mut self, width: usize) {
        let w = width.saturating_sub(GUTTER).max(8);
        let resized = w != self.width;
        if !resized && !self.stale {
            return;
        }
        self.width = w;
        for (i, e) in self.entries.iter_mut().enumerate() {
            if e.dirty || resized {
                e.rows = render_entry(i, e, w, &mut self.resolver);
                e.dirty = false;
            }
        }
        self.offsets.clear();
        let mut total = 0;
        for e in &self.entries {
            self.offsets.push(total);
            total += e.rows.len();
        }
        self.total = total;
        self.stale = false;
    }

    pub fn total(&self) -> usize {
        self.total
    }

    /// First display row of every entry (for `{`/`}` jumps).
    pub fn entry_starts(&self) -> &[usize] {
        &self.offsets
    }

    pub fn entry_at(&self, row: usize) -> usize {
        self.offsets.partition_point(|&o| o <= row).saturating_sub(1)
    }

    pub fn row(&self, i: usize) -> &Row {
        let e = self.entry_at(i);
        &self.entries[e].rows[i - self.offsets[e]]
    }

    /// Extracts the selected text between `a` and `b` (inclusive, any order).
    ///
    /// @param linewise select whole rows (`V`) instead of a char range (`v`).
    /// @returns where the text came from and the text, or None if the selection is blank.
    pub fn quote(&self, a: Pos, b: Pos, linewise: bool) -> Option<(String, String)> {
        if self.total == 0 {
            return None;
        }
        let (a, b) = if a <= b { (a, b) } else { (b, a) };
        let b = Pos { row: b.row.min(self.total - 1), col: b.col };
        let mut out = String::new();
        let mut prev: Option<(usize, usize)> = None;
        let mut sources: Vec<usize> = Vec::new();
        for r in a.row..=b.row {
            let row = self.row(r);
            let len = row.text.chars().count();
            let (s, e) = if linewise {
                (0, len)
            } else {
                (if r == a.row { a.col.min(len) } else { 0 }, if r == b.row { (b.col + 1).min(len) } else { len })
            };
            let piece: String = row.text.chars().skip(s).take(e.saturating_sub(s)).collect();
            if let Some(p) = prev
                && !(p == (row.entry, row.logical) && row.offset > 0)
            {
                out.push('\n');
            }
            out.push_str(&piece);
            prev = Some((row.entry, row.logical));
            if !piece.trim().is_empty() && sources.last() != Some(&row.entry) {
                sources.push(row.entry);
            }
        }
        let text = out.trim_matches('\n').trim_end().to_owned();
        if text.trim().is_empty() {
            return None;
        }
        let label = match sources.as_slice() {
            [one] => self.entries[*one].source_label(),
            _ => "several messages".into(),
        };
        Some((label, text))
    }
}

impl Lines for Transcript {
    fn count(&self) -> usize {
        self.total
    }
    fn line(&self, row: usize) -> &str {
        &self.row(row).text
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_prefers_spaces_and_keeps_offsets() {
        let w = wrap("hello world foo", 8);
        assert_eq!(w, vec![(0, "hello ".into()), (6, "world ".into()), (12, "foo".into())]);
        assert_eq!(wrap("abcdefghij", 4), vec![(0, "abcd".into()), (4, "efgh".into()), (8, "ij".into())]);
        assert_eq!(wrap("", 4), vec![(0, String::new())]);
    }

    #[test]
    fn quote_rejoins_wrapped_lines_and_links_survive_wrapping() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("main.rs"), "").unwrap();
        let mut t = Transcript::new(dir.path().to_path_buf());
        t.push(Kind::User, "hi");
        t.push(Kind::Assistant, "first line is long enough to wrap\nsee main.rs:3 now");
        t.layout(GUTTER + 14);
        // user: "hi" + spacer; assistant starts at row 2.
        assert_eq!(t.entry_starts(), &[0, 2]);
        assert_eq!(t.row(2).text, "first line is ");
        let (label, q) = t.quote(Pos { row: 2, col: 6 }, Pos { row: 3, col: 10 }, false).unwrap();
        assert_eq!(label, "your response");
        assert_eq!(q, "line is long enough");
        let rows: Vec<_> = (0..t.total()).map(|i| t.row(i)).collect();
        let link_row = rows.iter().find(|r| !r.links.is_empty()).unwrap();
        assert_eq!(link_row.text, "see main.rs:3 ");
        assert_eq!((link_row.links[0].start, link_row.links[0].end, link_row.links[0].line), (4, 13, Some(3)));
        let (_, q) = t.quote(Pos { row: 0, col: 0 }, Pos { row: 2, col: 0 }, true).unwrap();
        assert_eq!(q, "hi\n\nfirst line is");
    }

    #[test]
    fn tool_output_collapses() {
        let mut t = Transcript::new(PathBuf::from("/"));
        let out = (1..=10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let i = t.push(
            Kind::Tool(Box::new(ToolEntry {
                call_id: "c".into(),
                name: "bash".into(),
                summary: "ls".into(),
                status: ToolStatus::Ok,
                output: out,
                children: vec![],
                duration_ms: Some(1500),
            })),
            "",
        );
        t.layout(80);
        assert_eq!(t.total(), 1 + 4 + 1 + 1);
        assert_eq!(t.row(0).text, "bash ls  (1.5s)");
        t.entry_mut(i).collapsed = false;
        t.layout(80);
        assert_eq!(t.total(), 1 + 10 + 1);
    }
}
