//! Markdown rendering for assistant messages.
//!
//! Each source line becomes one display line. Markup characters are hidden or replaced
//! (`**`, `` ` ``, `#`, list markers), and the styled ranges become [`Run`]s. A [`RawLine`]
//! keeps the mapping back to the source so a quoted selection carries the original markdown.
//!
//! Inline parsing follows CommonMark's flanking rules for `*` and `_` (so `2 * 3`, `*ptr * 2`
//! and `snake_case` stay literal) and GFM's `~~strike~~`. Fenced code blocks are highlighted
//! with tree-sitter when the fence names a known language, and GFM tables render as aligned
//! columns with box-drawing separators.

use std::sync::Arc;

use ratatui::style::{Modifier, Style};
use unicode_width::UnicodeWidthStr;

use super::{
    highlight::{self, Run},
    theme::*,
};

/// One rendered line.
pub struct Rendered {
    pub text: String,
    pub style: Style,
    pub runs: Vec<Run>,
    /// Present when the display text differs from the source line.
    pub raw: Option<Arc<RawLine>>,
}

/// Maps the display chars of a rendered line to its source chars.
#[derive(Debug, PartialEq)]
pub struct RawLine {
    raw: Vec<char>,
    /// Source index of each display char.
    pos: Vec<usize>,
    /// Whether each source char is shown (false for hidden markup).
    shown: Vec<bool>,
}

impl RawLine {
    /// Source text behind display chars `[start, end)`, widened over the hidden markup that
    /// directly borders the range so `**bold**` or `` `code` `` are quoted whole.
    pub fn slice(&self, start: usize, end: usize) -> String {
        let end = end.min(self.pos.len());
        if start >= end {
            return String::new();
        }
        let mut a = self.pos[start];
        let mut b = self.pos[end - 1] + 1;
        while a > 0 && !self.shown[a - 1] {
            a -= 1;
        }
        while b < self.raw.len() && !self.shown[b] {
            b += 1;
        }
        self.raw[a..b].iter().collect()
    }
}

/// Per-source-char rendering decisions for one line.
struct Line {
    raw: Vec<char>,
    hidden: Vec<bool>,
    replace: Vec<Option<char>>,
    /// Source chars that are literal text (escaped, inside code), never delimiters.
    literal: Vec<bool>,
    runs: Vec<(usize, usize, Style)>,
    style: Style,
}

impl Line {
    fn new(s: &str, style: Style) -> Self {
        let raw: Vec<char> = s.chars().collect();
        let n = raw.len();
        Self { raw, hidden: vec![false; n], replace: vec![None; n], literal: vec![false; n], runs: Vec::new(), style }
    }

    fn hide(&mut self, a: usize, b: usize) {
        self.hidden[a..b].iter_mut().for_each(|h| *h = true);
    }

    fn finish(self) -> Rendered {
        let mut text = String::with_capacity(self.raw.len());
        let mut pos = Vec::with_capacity(self.raw.len());
        // visible[i] = display chars before source index i.
        let mut visible = Vec::with_capacity(self.raw.len() + 1);
        let mut changed = false;
        for (i, &c) in self.raw.iter().enumerate() {
            visible.push(pos.len());
            if self.hidden[i] {
                changed = true;
                continue;
            }
            match self.replace[i] {
                Some(r) => {
                    changed = true;
                    text.push(r);
                }
                None => text.push(c),
            }
            pos.push(i);
        }
        visible.push(pos.len());
        let runs = self
            .runs
            .iter()
            .map(|&(a, b, style)| Run { start: visible[a], end: visible[b], style })
            .filter(|r| r.start < r.end)
            .collect();
        let raw = changed.then(|| {
            let shown = self.hidden.iter().map(|h| !h).collect();
            Arc::new(RawLine { raw: self.raw, pos, shown })
        });
        Rendered { text, style: self.style, runs, raw }
    }
}

fn is_punct(c: char) -> bool {
    c.is_ascii_punctuation() || (!c.is_alphanumeric() && !c.is_whitespace())
}

/// Code, emphasis and strike-through within `line.raw[from..]`.
fn inline(line: &mut Line, from: usize) {
    let n = line.raw.len();
    // Pass 1: backslash escapes and code spans; their contents are not markup.
    let mut i = from;
    while i < n {
        let c = line.raw[i];
        if c == '\\' && i + 1 < n && line.raw[i + 1].is_ascii_punctuation() {
            line.hidden[i] = true;
            line.literal[i + 1] = true;
            i += 2;
            continue;
        }
        if c == '`' {
            let open = run_len(&line.raw, i, '`');
            let mut j = i + open;
            let close = loop {
                if j >= n {
                    break None;
                }
                if line.raw[j] == '`' {
                    let len = run_len(&line.raw, j, '`');
                    if len == open {
                        break Some(j);
                    }
                    j += len;
                } else {
                    j += 1;
                }
            };
            match close {
                Some(j) => {
                    let (mut a, mut b) = (i + open, j);
                    // CommonMark: one space of padding on both sides is stripped.
                    if b - a >= 2 && line.raw[a] == ' ' && line.raw[b - 1] == ' ' && line.raw[a..b].iter().any(|&c| c != ' ') {
                        line.hidden[a] = true;
                        line.hidden[b - 1] = true;
                        a += 1;
                        b -= 1;
                    }
                    line.hide(i, i + open);
                    line.hide(j, j + open);
                    line.literal[i..j + open].iter_mut().for_each(|l| *l = true);
                    line.runs.push((a, b, Style::default().fg(AQUA)));
                    i = j + open;
                }
                None => {
                    line.literal[i..i + open].iter_mut().for_each(|l| *l = true);
                    i += open;
                }
            }
            continue;
        }
        i += 1;
    }

    // Pass 2: delimiter runs.
    struct Delim {
        ch: char,
        lo: usize,
        hi: usize,
        len: usize,
        open: bool,
        close: bool,
    }
    let at = |k: isize| -> char {
        if k < from as isize || k as usize >= n { ' ' } else { line.raw[k as usize] }
    };
    let mut delims: Vec<Delim> = Vec::new();
    let mut i = from;
    while i < n {
        let c = line.raw[i];
        if !matches!(c, '*' | '_' | '~') || line.literal[i] {
            i += 1;
            continue;
        }
        let len = run_len(&line.raw, i, c);
        let (prev, next) = (at(i as isize - 1), at((i + len) as isize));
        let left = !next.is_whitespace() && (!is_punct(next) || prev.is_whitespace() || is_punct(prev));
        let right = !prev.is_whitespace() && (!is_punct(prev) || next.is_whitespace() || is_punct(next));
        let (open, close) = match c {
            '_' => (left && (!right || is_punct(prev)), right && (!left || is_punct(next))),
            '~' if len != 2 => (false, false),
            _ => (left, right),
        };
        if open || close {
            delims.push(Delim { ch: c, lo: i, hi: i + len, len, open, close });
        }
        i += len;
    }

    // Pass 3: match closers to the nearest compatible opener (CommonMark "process emphasis").
    let mut stack: Vec<usize> = Vec::new();
    for k in 0..delims.len() {
        if delims[k].close {
            while delims[k].hi > delims[k].lo {
                let found = stack.iter().rposition(|&o| {
                    let (op, cl) = (&delims[o], &delims[k]);
                    op.ch == cl.ch
                        && op.hi > op.lo
                        // Rule of three: a run that can both open and close must not pair
                        // with one whose combined length is a multiple of 3.
                        && !(op.ch != '~' && (op.close || cl.open) && (op.len + cl.len) % 3 == 0 && !(op.len % 3 == 0 && cl.len % 3 == 0))
                });
                let Some(si) = found else { break };
                let o = stack[si];
                let (orem, crem) = (delims[o].hi - delims[o].lo, delims[k].hi - delims[k].lo);
                let used = if delims[k].ch == '~' || (orem >= 2 && crem >= 2) { 2 } else { 1 };
                let style = match (delims[k].ch, used) {
                    ('~', _) => Style::default().add_modifier(Modifier::CROSSED_OUT),
                    (_, 2) => Style::default().add_modifier(Modifier::BOLD),
                    _ => Style::default().add_modifier(Modifier::ITALIC),
                };
                let inner = (delims[o].hi, delims[k].lo);
                line.hide(delims[o].hi - used, delims[o].hi);
                line.hide(delims[k].lo, delims[k].lo + used);
                line.runs.push((inner.0, inner.1, style));
                delims[o].hi -= used;
                delims[k].lo += used;
                // Openers between the pair can no longer match.
                stack.truncate(if delims[o].hi > delims[o].lo { si + 1 } else { si });
            }
        }
        if delims[k].open && delims[k].hi > delims[k].lo {
            stack.push(k);
        }
    }
}

fn run_len(raw: &[char], i: usize, c: char) -> usize {
    raw[i..].iter().take_while(|&&x| x == c).count()
}

/// Renders one line outside code fences: headings, quotes, rules, list markers, then inline.
fn block(src: &str) -> Rendered {
    let mut line = Line::new(src, Style::default().fg(FG));
    let raw = line.raw.clone();
    let n = raw.len();
    let indent = raw.iter().take_while(|c| **c == ' ').count();
    let rest = &raw[indent..];

    // Thematic break: three or more of the same of - * _ (spaces allowed).
    let marks: Vec<char> = rest.iter().copied().filter(|c| *c != ' ').collect();
    if marks.len() >= 3 && matches!(marks[0], '-' | '*' | '_') && marks.iter().all(|&c| c == marks[0]) {
        for (i, c) in raw.iter().enumerate().skip(indent) {
            if *c == ' ' { line.hidden[i] = true } else { line.replace[i] = Some('─') }
        }
        line.style = Style::default().fg(GRAY);
        return line.finish();
    }

    // ATX heading.
    let hashes = rest.iter().take_while(|c| **c == '#').count();
    if (1..=6).contains(&hashes) && rest.get(hashes).is_none_or(|c| *c == ' ') && indent < 4 {
        let content = (indent + hashes + 1).min(n);
        line.hide(0, content);
        // Optional closing sequence: ` ###` at the end.
        let mut end = n;
        while end > content && raw[end - 1] == ' ' {
            end -= 1;
        }
        let closing = raw[content..end].iter().rev().take_while(|c| **c == '#').count();
        if closing > 0 && (end - closing == content || raw[end - closing - 1] == ' ') {
            let mut cut = end - closing;
            while cut > content && raw[cut - 1] == ' ' {
                cut -= 1;
            }
            line.hide(cut, n);
        }
        line.style = match hashes {
            1 => Style::default().fg(YELLOW).add_modifier(Modifier::BOLD),
            2 => Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
            _ => Style::default().fg(FG).add_modifier(Modifier::BOLD),
        };
        inline(&mut line, content);
        return line.finish();
    }

    // Block quotes (possibly nested): `>` becomes a bar.
    let mut i = indent;
    let mut quoted = false;
    while i < n && raw[i] == '>' {
        line.replace[i] = Some('▎');
        quoted = true;
        i += 1;
        if i < n && raw[i] == ' ' {
            i += 1;
        }
    }
    if quoted {
        line.style = Style::default().fg(FG_DIM).add_modifier(Modifier::ITALIC);
        inline(&mut line, i);
        return line.finish();
    }

    // Unordered list item, with an optional task checkbox.
    if rest.len() >= 2 && matches!(rest[0], '-' | '*' | '+') && rest[1] == ' ' {
        line.replace[indent] = Some('•');
        let mut start = indent + 2;
        if raw.len() >= start + 4 && raw[start] == '[' && raw[start + 2] == ']' && raw[start + 3] == ' ' {
            match raw[start + 1] {
                ' ' => line.replace[start] = Some('☐'),
                'x' | 'X' => line.replace[start] = Some('☑'),
                _ => {}
            }
            if line.replace[start].is_some() {
                line.hide(start + 1, start + 3);
                start += 4;
            }
        }
        inline(&mut line, start);
        return line.finish();
    }

    inline(&mut line, indent);
    line.finish()
}

// ── Tables ─────────────────────────────────────────────────────────────────────
//
// GFM tables become aligned columns: pipes render as `│`, the delimiter row becomes a rule
// (`─────┼──────`) and the header is bold. Each rendered row keeps a [`RawLine`] back to its
// source line, so quoting a whole row reproduces the exact source markdown and selecting a
// cell quotes it with its pipes.

/// Column alignment, taken from the delimiter row's colons.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Align {
    Left,
    Center,
    Right,
}

/// One trimmed table cell and where its chars sit in the source line.
struct Cell {
    /// Trimmed content; `\|` escapes are already resolved to a literal `|`.
    text: String,
    /// Source index of each char in `text`.
    src: Vec<usize>,
    /// Source range of the trimmed content (falls back to the preceding pipe when empty).
    start: usize,
    end: usize,
}

/// A parsed table row.
struct Row {
    cells: Vec<Cell>,
    /// Source index of the pipe between `cells[j]` and `cells[j + 1]`.
    pipes: Vec<usize>,
    /// Opening/closing pipe, present when the row starts/ends with `|`.
    leading: Option<usize>,
    trailing: Option<usize>,
    raw: Vec<char>,
    /// Backslashes consumed by `\|` escapes (hidden in the display).
    escaped: Vec<usize>,
}

fn parse_row(line: &str) -> Row {
    let raw: Vec<char> = line.chars().collect();
    // Segments between pipes: (chars with their source index, pipe index after the segment).
    type Seg = (Vec<(char, usize)>, Option<usize>);
    let mut segs: Vec<Seg> = vec![(Vec::new(), None)];
    let mut escaped = Vec::new();
    let mut i = 0;
    while i < raw.len() {
        match raw[i] {
            '|' => {
                segs.last_mut().unwrap().1 = Some(i);
                segs.push((Vec::new(), None));
            }
            '\\' if i + 1 < raw.len() && raw[i + 1] == '|' => {
                escaped.push(i);
                segs.last_mut().unwrap().0.push(('|', i + 1));
                i += 1;
            }
            c => segs.last_mut().unwrap().0.push((c, i)),
        }
        i += 1;
    }
    let blank = |s: &Seg| s.0.iter().all(|&(c, _)| c.is_whitespace());
    // A leading/trailing `|` shows up as a blank first/last segment.
    let leading = if segs.len() > 1 && blank(&segs[0]) && segs[0].1.is_some() { segs.remove(0).1 } else { None };
    let trailing = if segs.len() > 1 && blank(&segs[segs.len() - 1]) && segs[segs.len() - 1].1.is_none() {
        let last = segs.len() - 1;
        let p = segs[last - 1].1.take();
        segs.remove(last);
        p
    } else {
        None
    };
    let pipes: Vec<usize> = segs.iter().filter_map(|s| s.1).collect();
    let cells = segs
        .into_iter()
        .enumerate()
        .map(|(j, (chars, _))| {
            let before = if j == 0 { leading.unwrap_or(0) } else { pipes[j - 1] };
            let content: Vec<(char, usize)> = match (
                chars.iter().position(|&(c, _)| !c.is_whitespace()),
                chars.iter().rposition(|&(c, _)| !c.is_whitespace()),
            ) {
                (Some(a), Some(b)) => chars[a..=b].to_vec(),
                _ => Vec::new(),
            };
            let (start, end) = match (content.first(), content.last()) {
                (Some(&(_, a)), Some(&(_, b))) => (a, b + 1),
                _ => (before, before),
            };
            let text: String = content.iter().map(|&(c, _)| c).collect();
            let src: Vec<usize> = content.iter().map(|&(_, i)| i).collect();
            Cell { text, src, start, end }
        })
        .collect();
    Row { cells, pipes, leading, trailing, raw, escaped }
}

/// Delimiter cells become column alignments; None if the row is not a delimiter row.
fn aligns(delim: &Row) -> Option<Vec<Align>> {
    let mut out = Vec::with_capacity(delim.cells.len());
    for c in &delim.cells {
        let t = c.text.as_str();
        let inner = t.strip_prefix(':').unwrap_or(t);
        let inner = inner.strip_suffix(':').unwrap_or(inner);
        if inner.is_empty() || !inner.chars().all(|ch| ch == '-') {
            return None;
        }
        out.push(match (t.starts_with(':'), t.ends_with(':')) {
            (true, true) => Align::Center,
            (false, true) => Align::Right,
            _ => Align::Left,
        });
    }
    Some(out)
}

/// Inline markdown on one table cell: code spans, emphasis, strike and escapes.
fn inline_only(content: &str, style: Style) -> Rendered {
    let mut line = Line::new(content, style);
    inline(&mut line, 0);
    line.finish()
}

/// Marks the row's structural chars (`|`, the space right after a pipe, the padding around the
/// outer pipes) as hidden in the [`RawLine`], so selections widen over them and quote whole cells.
fn hidden_structure(shown: &mut [bool], raw: &[char], pipes: &[usize], leading: Option<usize>, trailing: Option<usize>) {
    for &p in pipes {
        shown[p] = false;
        if p + 1 < raw.len() && raw[p + 1] == ' ' {
            shown[p + 1] = false;
        }
    }
    if let Some(t) = trailing {
        let mut k = t;
        while k > 0 && raw[k - 1] == ' ' {
            k -= 1;
            shown[k] = false;
        }
        for i in (t + 1..raw.len()).take_while(|&i| raw[i] == ' ') {
            shown[i] = false;
        }
    }
    if let Some(l) = leading {
        for i in (0..l).rev().take_while(|&i| raw[i] == ' ') {
            shown[i] = false;
        }
    }
}

fn row_render(row: &Row, widths: &[usize], aligns: &[Align], style: Style) -> Rendered {
    let n = widths.len();
    let mut text = String::new();
    let mut pos: Vec<usize> = Vec::new();
    let mut runs: Vec<Run> = Vec::new();
    let mut shown = vec![true; row.raw.len()];
    let fallback = row.raw.len().saturating_sub(1);
    for &e in &row.escaped {
        shown[e] = false;
    }
    let mut pipes: Vec<usize> = Vec::new();
    if let Some(l) = row.leading {
        pipes.push(l);
    }
    pipes.extend(row.pipes.iter().copied());
    if let Some(t) = row.trailing {
        pipes.push(t);
    }
    hidden_structure(&mut shown, &row.raw, &pipes, row.leading, row.trailing);
    // Cells beyond the header's columns are dropped; hide their source so a whole-row
    // selection still reproduces the exact source row.
    if row.cells.len() > n
        && let Some(c) = row.cells.get(n - 1)
    {
        shown[c.end..].iter_mut().for_each(|s| *s = false);
    }
    for j in 0..n {
        if j > 0 {
            let p = row.pipes.get(j - 1).copied().unwrap_or(fallback);
            text.push(' ');
            pos.push(p.saturating_sub(1).min(fallback));
            text.push('│');
            pos.push(p.min(fallback));
            text.push(' ');
            pos.push(p.saturating_add(1).min(fallback));
        }
        let cell = row.cells.get(j);
        let content = cell.map_or("", |c| c.text.as_str());
        let rendered = inline_only(content, style);
        let pad = widths[j].saturating_sub(UnicodeWidthStr::width(rendered.text.as_str()));
        let (left, right) = match aligns[j] {
            Align::Left => (0, pad),
            Align::Right => (pad, 0),
            Align::Center => (pad / 2, pad - pad / 2),
        };
        // No trailing padding on the last column: nothing after it to align.
        let right = if j + 1 == n { 0 } else { right };
        let first = cell.map_or(fallback, |c| c.start);
        let last = cell.map_or(fallback, |c| c.end.saturating_sub(1));
        for _ in 0..left {
            text.push(' ');
            pos.push(first);
        }
        let start = text.chars().count();
        text.push_str(&rendered.text);
        for (k, _) in rendered.text.chars().enumerate() {
            let ci = rendered.raw.as_ref().and_then(|m| m.pos.get(k).copied()).unwrap_or(k);
            pos.push(cell.map_or(fallback, |c| c.src.get(ci).copied().unwrap_or(fallback)));
        }
        if let (Some(m), Some(c)) = (rendered.raw.as_ref(), cell) {
            for (ci, &vis) in m.shown.iter().enumerate() {
                if !vis
                    && let Some(&s) = c.src.get(ci)
                {
                    shown[s] = false;
                }
            }
        }
        for r in &rendered.runs {
            runs.push(Run { start: start + r.start, end: start + r.end, style: r.style });
        }
        for _ in 0..right {
            text.push(' ');
            pos.push(last);
        }
    }
    let raw = Arc::new(RawLine { raw: row.raw.clone(), pos, shown });
    Rendered { text, style, runs, raw: Some(raw) }
}

fn rule_render(row: &Row, widths: &[usize]) -> Rendered {
    let mut text = String::new();
    let mut pos: Vec<usize> = Vec::new();
    let mut shown = vec![true; row.raw.len()];
    let fallback = row.raw.len().saturating_sub(1);
    for &e in &row.escaped {
        shown[e] = false;
    }
    let mut pipes: Vec<usize> = Vec::new();
    if let Some(l) = row.leading {
        pipes.push(l);
    }
    pipes.extend(row.pipes.iter().copied());
    if let Some(t) = row.trailing {
        pipes.push(t);
    }
    hidden_structure(&mut shown, &row.raw, &pipes, row.leading, row.trailing);
    for (j, &w) in widths.iter().enumerate() {
        if j > 0 {
            let p = row.pipes.get(j - 1).copied().unwrap_or(fallback);
            text.push('─');
            pos.push(p.saturating_sub(1).min(fallback));
            text.push('┼');
            pos.push(p.min(fallback));
            text.push('─');
            pos.push(p.saturating_add(1).min(fallback));
        }
        let cell = row.cells.get(j);
        let dashes: Vec<usize> = cell
            .map(|c| c.text.char_indices().filter(|&(_, ch)| ch == '-').map(|(k, _)| c.src[k]).collect())
            .unwrap_or_default();
        let last = dashes.last().copied().unwrap_or(fallback);
        for k in 0..w.max(1) {
            text.push('─');
            pos.push(*dashes.get(k).unwrap_or(&last));
        }
        if let Some(c) = cell {
            // Dashes beyond the drawn rule are hidden, so a whole-rule selection still
            // reproduces the exact delimiter line.
            for (k, &s) in dashes.iter().enumerate() {
                if k >= w {
                    shown[s] = false;
                }
            }
            for (k, ch) in c.text.chars().enumerate() {
                if ch == ':' {
                    shown[c.src[k]] = false;
                }
            }
        }
    }
    let raw = Arc::new(RawLine { raw: row.raw.clone(), pos, shown });
    Rendered { text, style: Style::default().fg(GRAY), runs: Vec::new(), raw: Some(raw) }
}

struct Table {
    header: Row,
    delim: Row,
    aligns: Vec<Align>,
    body: Vec<Row>,
    widths: Vec<usize>,
    /// Line index just after the table.
    end: usize,
}

/// Tries to parse a GFM table starting at `lines[start]`: a header row with at least two
/// columns, a matching delimiter row, and at least one body row.
fn table_at(lines: &[&str], start: usize) -> Option<Table> {
    if start + 2 >= lines.len() {
        return None;
    }
    let header = parse_row(lines[start]);
    if header.cells.len() < 2 {
        return None;
    }
    let delim = parse_row(lines[start + 1]);
    if delim.cells.len() != header.cells.len() {
        return None;
    }
    let aligns = aligns(&delim)?;
    let mut body = Vec::new();
    let mut end = start + 2;
    while end < lines.len() {
        let row = parse_row(lines[end]);
        if row.leading.is_none() && row.trailing.is_none() && row.pipes.is_empty() {
            break;
        }
        body.push(row);
        end += 1;
    }
    if body.is_empty() {
        return None;
    }
    let mut widths = vec![0; header.cells.len()];
    let widest = |cells: &[Cell], widths: &mut [usize]| {
        for (w, c) in widths.iter_mut().zip(cells) {
            let rendered = inline_only(&c.text, Style::default());
            *w = (*w).max(UnicodeWidthStr::width(rendered.text.as_str()));
        }
    };
    widest(&header.cells, &mut widths);
    for b in &body {
        widest(&b.cells, &mut widths);
    }
    Some(Table { header, delim, aligns, body, widths, end })
}

impl Table {
    fn render(self) -> Vec<Rendered> {
        let mut out = Vec::with_capacity(self.body.len() + 2);
        out.push(row_render(&self.header, &self.widths, &self.aligns, Style::default().fg(FG).add_modifier(Modifier::BOLD)));
        out.push(rule_render(&self.delim, &self.widths));
        for b in &self.body {
            out.push(row_render(b, &self.widths, &self.aligns, Style::default().fg(FG)));
        }
        out
    }
}

fn fence(line: &str) -> Option<(char, usize, &str)> {
    let t = line.trim_start();
    let c = t.chars().next()?;
    if c != '`' && c != '~' {
        return None;
    }
    let len = t.chars().take_while(|&x| x == c).count();
    (len >= 3).then(|| (c, len, t[len..].trim()))
}

/// Renders a whole assistant message, one [`Rendered`] per source line.
///
/// An unclosed fence (a message still streaming) renders its remaining lines as code.
pub fn render(text: &str) -> Vec<Rendered> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::with_capacity(lines.len());
    let mut i = 0;
    while i < lines.len() {
        let Some((c, len, info)) = fence(lines[i]) else {
            if let Some(t) = table_at(&lines, i) {
                let end = t.end;
                out.extend(t.render());
                i = end;
            } else {
                out.push(block(lines[i]));
                i += 1;
            }
            continue;
        };
        let fence_style = Style::default().fg(GRAY);
        out.push(Rendered { text: lines[i].to_owned(), style: fence_style, runs: Vec::new(), raw: None });
        let start = i + 1;
        let mut end = start;
        while end < lines.len() && !fence(lines[end]).is_some_and(|(c2, l2, rest)| c2 == c && l2 >= len && rest.is_empty()) {
            end += 1;
        }
        let body = &lines[start..end];
        let lang = info.split_whitespace().next().unwrap_or("");
        let highlighted = (!lang.is_empty()).then(|| highlight::highlight_lang(lang, &body.join("\n"))).flatten();
        let base = if highlighted.is_some() { Style::default().fg(FG) } else { Style::default().fg(YELLOW) };
        for (k, l) in body.iter().enumerate() {
            let runs = highlighted.as_ref().and_then(|h| h.get(k)).cloned().unwrap_or_default();
            out.push(Rendered { text: (*l).to_owned(), style: base, runs, raw: None });
        }
        if end < lines.len() {
            out.push(Rendered { text: lines[end].to_owned(), style: fence_style, runs: Vec::new(), raw: None });
        }
        i = end + 1;
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> Rendered {
        render(s).remove(0)
    }

    fn mods(r: &Rendered, display: &str) -> Vec<Modifier> {
        let start = r.text.find(display).map(|b| r.text[..b].chars().count()).expect("text present");
        let end = start + display.chars().count();
        r.runs.iter().filter(|x| x.start <= start && end <= x.end).map(|x| x.style.add_modifier).collect()
    }

    #[test]
    fn emphasis_code_and_strike() {
        let r = one("a **bold** and *it* and `code` and ~~gone~~ end");
        assert_eq!(r.text, "a bold and it and code and gone end");
        assert_eq!(mods(&r, "bold"), vec![Modifier::BOLD]);
        assert_eq!(mods(&r, "it"), vec![Modifier::ITALIC]);
        assert_eq!(mods(&r, "gone"), vec![Modifier::CROSSED_OUT]);
        let code = r.runs.iter().find(|x| x.style.fg == Some(AQUA)).unwrap();
        assert_eq!(&r.text[code.start..code.end], "code");
    }

    #[test]
    fn code_like_text_stays_literal() {
        for s in ["x = *ptr * 2;", "2 * 3 * 4", "snake_case_name", "a_b_c and MAX_W", "**unclosed", "`open", "1 < 2 ~~ x"] {
            let r = one(s);
            assert_eq!(r.text, s, "{s}");
            assert!(r.runs.is_empty(), "{s}");
        }
    }

    #[test]
    fn nesting_escapes_and_code_contents() {
        let r = one("***both*** and **bold *it* bold**");
        assert_eq!(r.text, "both and bold it bold");
        assert!(mods(&r, "both").contains(&Modifier::BOLD) && mods(&r, "both").contains(&Modifier::ITALIC));
        assert_eq!(mods(&r, "it").len(), 2);
        let r = one(r"\*not\* and `**raw**` and `` a`b ``");
        assert_eq!(r.text, "*not* and **raw** and a`b");
        assert!(r.runs.iter().all(|x| x.style.add_modifier.is_empty()));
    }

    #[test]
    fn blocks() {
        let r = render("## Plan ##\n- one\n  * nested **b**\n- [x] done\n> quoted *it*\n---\n1. first");
        let texts: Vec<_> = r.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["Plan", "• one", "  • nested b", "• ☑ done", "▎ quoted it", "───", "1. first"]);
        assert!(r[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(r[0].style.fg, Some(ORANGE));
    }

    #[test]
    fn fenced_code_is_highlighted_and_not_parsed_as_markdown() {
        let r = render("```rust\nlet x = **y**;\n```\nafter **b**");
        assert_eq!(r[1].text, "let x = **y**;");
        assert!(r[1].runs.iter().any(|x| x.style.fg == Some(RED)), "`let` keyword highlighted");
        assert_eq!(r[3].text, "after b");
        // Unknown language: plain code color. Unclosed fence while streaming: still code.
        let r = render("```\n*a*\n");
        assert_eq!((r[1].text.as_str(), r[1].style.fg), ("*a*", Some(YELLOW)));
    }

    #[test]
    fn tables_align_columns_and_quote_source_rows() {
        let r = render("| name | value |\n|:-----|------:|\n| **a** | 1 |\n| bb | 22 |");
        let texts: Vec<_> = r.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts, vec!["name │ value", "─────┼──────", "a    │     1", "bb   │    22"]);
        assert!(r[0].style.add_modifier.contains(Modifier::BOLD), "header is bold");
        assert_eq!(r[1].style.fg, Some(GRAY), "rule is dim");
        // Inline markdown still applies inside cells.
        let a = r[2].text.find('a').unwrap();
        assert!(r[2].runs.iter().any(|x| x.start <= a && a < x.end && x.style.add_modifier.contains(Modifier::BOLD)));
        // A whole-row selection reproduces the exact source row, and a cell quotes with its pipes.
        for (k, src) in ["| name | value |", "|:-----|------:|", "| **a** | 1 |", "| bb | 22 |"].iter().enumerate() {
            let raw = r[k].raw.as_ref().unwrap();
            assert_eq!(raw.slice(0, r[k].text.chars().count()), *src);
        }
        let raw = r[2].raw.as_ref().unwrap();
        let s = r[2].text[..r[2].text.find('1').unwrap()].chars().count();
        assert_eq!(raw.slice(s, s + 1), "| 1 |");
    }

    #[test]
    fn tables_center_and_right_align() {
        let r = render("| abcd | x |\n|:----:|--:|\n| a | yy |");
        assert_eq!(r[0].text, "abcd │  x");
        assert_eq!(r[2].text, " a   │ yy");
    }

    #[test]
    fn pipe_lines_without_a_delimiter_stay_literal() {
        let r = render("| a | b |\n| c | d |");
        assert_eq!((r[0].text.as_str(), r[1].text.as_str()), ("| a | b |", "| c | d |"));
        // A table needs at least one body row.
        let r = render("| a | b |\n|---|---|");
        assert_eq!((r[0].text.as_str(), r[1].text.as_str()), ("| a | b |", "|---|---|"));
    }

    #[test]
    fn raw_slices_include_bordering_markup() {
        let r = one("see `src/config.rs:17` and **bold** text");
        let raw = r.raw.as_ref().unwrap();
        let find = |s: &str| {
            let a = r.text.find(s).map(|b| r.text[..b].chars().count()).unwrap();
            (a, a + s.chars().count())
        };
        let (a, b) = find("src/config.rs:17");
        assert_eq!(raw.slice(a, b), "`src/config.rs:17`");
        let (a, b) = find("bold");
        assert_eq!(raw.slice(a, b), "**bold**");
        assert_eq!(raw.slice(0, r.text.chars().count()), "see `src/config.rs:17` and **bold** text");
        let h = one("# Title");
        assert_eq!(h.raw.as_ref().unwrap().slice(0, 5), "# Title");
        assert!(one("plain").raw.is_none());
    }
}
